//! `choir` — the agent-facing command line for a choir node.
//!
//! Everything the templates teach an agent to do by hand (mint a key,
//! provision a workspace, sign and submit ops, run a review round) as
//! one binary. All HTTP shells out to `curl`; the daemon base URL is a
//! positional argument (no crate reads environment variables).
//!
//! ```text
//! choir [--auth-file <path>] [--auth-user <name>] <command> ...
//! choir key <key-file> [name]
//! choir git-credential <auth-file> [--auth-user <name>] get|store|erase
//! choir invite <api> <name> <owner/repo> [read|write]
//! choir asks <api>
//! choir grant <api> <request-id> <owner/repo> [read|write]
//! choir decline <api> <request-id>
//! choir join <link> | <api> <invite-file> <key-file> [--user <name>] [--channel <name>] [--key-file <path>] [--ssh-key <path>] [--token-file <path>]
//! choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]
//! choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
//! choir propose [reviewer]... [--key-file <path>] [--channel <name>] [--api <url>] [--repo <owner/repo>] [--onto <branch>]
//! choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
//! choir submit <api> <key-file> <channel> '<op-json>'
//! choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
//! choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
//! choir slash <api> <node-key-file> <id> <reviewer> '<reason>'
//! choir bind <api> <node-key-file> <operator> <key-hex> [channel]
//! choir revoke <api> <node-key-file> <key-hex> '<reason>'
//! choir appeal <api> <attempt-id>
//! choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
//! choir check <api> <key-file> <channel> <git-oid> <name> passed|failed|running|errored [evidence] [--ref <repo:ref>]
//! choir checks <api> <git-oid>
//! choir reviews <api> <reviewer>
//! choir acl render <api> <acl-file>
//! choir view <api>
//! choir triage <api>
//! choir funnel <api>
//! choir state <api> <channel>
//! choir skill install [--into <dir>]
//! choir repair <log-file> --verify | --truncate-tail
//! ```
//!
//! Exit codes: 0 = the node accepted, 1 = the node rejected (the JSON
//! error body is printed), 2 = usage error. `choir checks` adds 3 = the
//! answer is not decided yet and 4 = a check could not be run at all,
//! which is our fault rather than the commit's and wants a re-run
//! rather than a rewrite; see [`check_exit`].
//!
//! `choir doctor` reads the same two codes as everything else — 0
//! nothing required is missing, 1 something is — so that `choir doctor
//! && choir propose …` means what it looks like it means. A degraded
//! but working machine exits 0; see [`choir_cli::doctor`].

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_view::{
    reviewer_operator, ArchiveAuthorization, CheckStatus, CreateAuthorization, OpKind, Verdict,
    ViewOp,
};

#[derive(Clone, Copy)]
struct AuthOptions<'a> {
    file: Option<&'a str>,
    user: Option<&'a str>,
    /// Whether either flag was actually given.
    ///
    /// Separate from the two options because [`AuthOptions::file_for`]
    /// fills `file` in from the layout `choir init` wrote, and the
    /// commands that refuse to take a credential at all — `init`,
    /// `join`, `key`, `git-credential` — must keep asking "did the
    /// reader pass one", not "is there one".
    explicit: bool,
}

/// The credential `choir init` wrote, found once.
///
/// A `OnceLock` rather than a lookup per call: every command that
/// reaches the node asks, and the answer is a `stat` on a path that
/// cannot change while the process runs.
static DEFAULT_AUTH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// `~/.choir/auth` if it exists, or whatever `.choir/config` names.
///
/// `HOME` describes the machine rather than carrying a setting of ours,
/// the same reason `choir init` may read it.
fn discovered_auth_file() -> Option<String> {
    if let Some(named) = configured("auth") {
        return Some(named);
    }
    let path = std::path::PathBuf::from(std::env::var_os("HOME")?)
        .join(".choir")
        .join("auth");
    path.exists().then(|| path.display().to_string())
}

/// Whether a bearer token may be sent to this node without being asked
/// for by name.
///
/// Loopback, or the node `.choir/config` points at. An explicit
/// `--auth-file` is the reader saying which credential goes where and
/// is never second-guessed; this is only about the *implicit* one, and
/// an implicit credential must not follow a URL that merely happened to
/// be typed after a command that takes one.
fn may_hold_credential(api: &str) -> bool {
    let rest = api.split_once("://").map_or(api, |(_, rest)| rest);
    let host = rest.split('/').next().unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    if matches!(name, "127.0.0.1" | "localhost" | "::1" | "[::1]") {
        return true;
    }
    let same = |url: &str| url.trim_end_matches('/') == api.trim_end_matches('/');
    // A seed named in `seeds =` (D80) was named by the reader in the same
    // file as the node, which is the same assertion about where the
    // credential may go.
    configured("node").is_some_and(|node| same(&node))
        || configured_seeds().iter().any(|seed| same(seed))
}

/// The seeds `.choir/config` names, as `seeds = <url>[, <url>]` beside
/// `node =` (D80). Empty when there is no such line.
fn configured_seeds() -> Vec<String> {
    configured("seeds")
        .map(|line| {
            line.split(',')
                .map(|url| url.trim().trim_end_matches('/').to_string())
                .filter(|url| !url.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The home a `421 not_home` answer names, when `(status, body)` is one
/// (D80).
fn not_home(status: u16, body: &str) -> Option<String> {
    if status != 421 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value["code"] != "not_home" {
        return None;
    }
    value["home"]
        .as_str()
        .map(|home| home.trim_end_matches('/').to_string())
}

/// Whether to resend a request a seed at `api` refused to its `home`, and
/// if not, says why on stderr.
///
/// Once, and only to a home this command may hand a credential to: the
/// loopback, or the node `.choir/config` names. A seed chooses the address
/// in its answer, so following any address it names would let a seed send
/// this command's credential wherever it liked.
fn follow_home(api: &str, home: &str) -> bool {
    if home == api.trim_end_matches('/') {
        return false;
    }
    if may_hold_credential(home) {
        eprintln!("choir: {api} is a seed of {home}; sent this to the home instead");
        true
    } else {
        eprintln!(
            "choir: {api} is a seed of {home}, and writes go there. Not following it: \
             {home} is not the node .choir/config names, and this command's credential \
             would go with it. Rerun against {home}."
        );
        false
    }
}

impl<'a> AuthOptions<'a> {
    fn is_empty(self) -> bool {
        !self.explicit
    }

    /// The credential to use when talking to `api`.
    ///
    /// What was given, else the one `choir init` wrote — so the command
    /// that creates a node and the commands that read it agree without
    /// a path being typed in between. Before this, every one of them
    /// answered 401 on a node the same tool had just set up.
    fn file_for(self, api: &str) -> Option<&'a str> {
        if self.file.is_some() {
            return self.file;
        }
        if !may_hold_credential(api) {
            return None;
        }
        DEFAULT_AUTH.get_or_init(discovered_auth_file).as_deref()
    }
}

/// A short human summary on stderr, when a person is looking.
///
/// Never on stdout. The commands that call this answer with JSON, and
/// that JSON is read by agents, `jq`, and the tests -- so it stays byte
/// for byte what it was, whatever is attached. This is the second
/// audience: someone at a prompt who has just run their first choir
/// command and would otherwise be reading a brace.
///
/// Nothing here is load-bearing. A reader who redirects stderr loses
/// decoration and no information: every field printed is already in the
/// document on stdout.
fn note(heading: &str, rows: &[(&str, String)]) {
    let style = choir_cli::style::Style::for_stderr();
    if !style.is_painted() {
        return;
    }
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    eprintln!("\n  {}", style.green(heading));
    for (key, value) in rows {
        let key = format!("{key:width$}");
        eprintln!("  {}  {value}", style.dim(&key));
    }
    eprintln!();
}

/// Both spellings of the request for help.
///
/// `-h` is what a reader tries when `--help` has not occurred to them
/// yet, and answering it with "not a choir command" refuses the one
/// question every command line has to answer.
fn is_help(argument: &str) -> bool {
    argument == "--help" || argument == "-h"
}

/// The command word this invocation was reaching for, if any.
///
/// Reads the process arguments again rather than being handed them:
/// [`usage`] is called from a dozen places, most of them deep inside a
/// command's own flag parsing where the name has long since been
/// destructured away, and threading it through all of them would be a
/// dozen chances to pass the wrong one.
fn invoked_command() -> Option<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The same two-argument skip [`parse_auth`] performs, so that
    // `choir --auth-file f frobnicate` names `frobnicate` and not `f`.
    let mut index = 0;
    while matches!(
        args.get(index).map(String::as_str),
        Some("--auth-file" | "--auth-user")
    ) {
        index += 2;
    }
    let first = args.get(index)?.clone();
    // A reader who typed a two-word command should be told about the
    // command they typed, not about its first word. Read from the
    // surface table rather than listed here: this was written when `acl
    // render` was the only two-word name and hardcoded it, so by the
    // time there were eight, `choir repo list` with no node configured
    // answered "`repo` is not a choir command" — naming a word the
    // reader had not got wrong, and suggesting `repo url`.
    if let Some(second) = args.get(index + 1) {
        let two = format!("{first} {second}");
        if choir_cli::surface::COMMANDS.iter().any(|c| c.name == two) {
            return Some(two);
        }
    }
    // One word that is only ever the first half of a two-word name is
    // still worth naming as such: `choir repo` is not a mistyped
    // command, it is an unfinished one.
    if choir_cli::surface::COMMANDS
        .iter()
        .any(|c| c.name.starts_with(&format!("{first} ")))
    {
        return Some(first);
    }
    Some(first)
}

/// Refuses an invocation, saying the smallest true thing about it.
///
/// The wall of thirty-two commands is the right answer to "what can this
/// do" and the wrong answer to every other question. A reader who
/// mistyped a name needs that name and a candidate; a reader who got a
/// known command's arguments wrong needs *that command's* spec, which
/// the index does not carry. Printing the index at all three was the
/// same as printing nothing: the one line that mattered was buried
/// forty lines from the top, above the prompt, off the screen.
fn usage() -> ! {
    let style = choir_cli::style::Style::for_stderr();
    match invoked_command() {
        // No command at all. On a machine that has been set up, the
        // index is the question this answers. On one that has not, it is
        // the wrong first screen: forty-eight commands, of which exactly
        // one is any use to somebody holding an invite link.
        None if choir_cli::join::Role::of(&state_dir(), discovered_auth_file().as_deref())
            == choir_cli::join::Role::Nothing =>
        {
            eprint!("{}", choir_cli::join::orientation(style));
        }
        None => eprint!("{}", choir_cli::surface::usage_in(style)),
        Some(name) => match choir_cli::surface::command_help_in(&name, style) {
            Some(help) => {
                eprintln!(
                    "{} those arguments do not match `{}`. It takes:\n",
                    style.red("choir:"),
                    name
                );
                eprint!("{help}");
            }
            None => {
                // A word that only ever begins a two-word name is an
                // unfinished command, not a wrong one, and the useful
                // answer is the list of its halves rather than the
                // nearest string to it. `choir repo` used to suggest
                // `repo url`, which is one of the three things it could
                // have meant and no more likely than the others.
                let under: Vec<&str> = choir_cli::surface::COMMANDS
                    .iter()
                    .map(|c| c.name)
                    .filter(|n| n.starts_with(&format!("{name} ")))
                    .collect();
                if under.is_empty() {
                    eprintln!("{} `{}` is not a choir command.", style.red("choir:"), name);
                    let names = choir_cli::surface::COMMANDS.iter().map(|c| c.name);
                    if let Some(near) = choir_cli::style::nearest(&name, names) {
                        eprintln!("       did you mean {}?", style.cyan(near));
                    }
                } else {
                    eprintln!(
                        "{} `{}` is not a command on its own. It has:",
                        style.red("choir:"),
                        name
                    );
                    for one in under {
                        eprintln!("         {}", style.cyan(one));
                    }
                }
                eprintln!("       {} lists every command.", style.cyan("choir --help"));
            }
        },
    }
    std::process::exit(2);
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Refuses to sign an operator-only op with a key file that does not
/// already exist.
///
/// [`load_key`] *creates* a key file when absent, which is right for an
/// agent minting its own identity and wrong here: a typo'd path would
/// silently mint a fresh key, and the node would reject the submission as
/// an unknown signer rather than as the mistake it is.
fn require_node_key_file(path: &str) {
    if !std::path::Path::new(path).is_file() {
        eprintln!("choir: <node-key-file> must name the node's existing key file");
        std::process::exit(2);
    }
}

/// Derives the actor id from a 64-character ed25519 public key hex.
///
/// This is the one derivation the node also performs, so binding a key
/// never asks an operator to hand-compute a hash — they paste the same
/// hex `choir key` printed and the trusted-keys file carries.
fn actor_id_from_hex(key_hex: &str) -> choir_hash::ContentHash {
    let bytes: Option<Vec<u8>> = (key_hex.len() == 64)
        .then(|| {
            (0..64)
                .step_by(2)
                .map(|i| u8::from_str_radix(&key_hex[i..i + 2], 16).ok())
                .collect()
        })
        .flatten();
    let Some(bytes) = bytes else {
        eprintln!("choir: <key-hex> must be a 64-character ed25519 public key hex");
        std::process::exit(2);
    };
    choir_hash::ContentHash::blake3(&bytes)
}

/// `choir repair <log-file> --verify | --truncate-tail`.
///
/// Run against a log **no daemon is holding**. Nothing here takes a lock,
/// because the only safe way to repair a log is for nothing to be
/// appending to it, and a lock would suggest otherwise.
///
/// Exit codes follow the binary's convention: 0 the log is usable (or was
/// made usable), 1 it is damaged and this cannot fix it, 2 usage.
fn repair(log_file: &str, flags: &[&str]) {
    let path = std::path::Path::new(log_file);
    let verify = flags.contains(&"--verify");
    let truncate = flags.contains(&"--truncate-tail");
    if verify == truncate {
        // Neither, or both. Both is the interesting one: it reads like
        // "check and then fix", which is exactly the compound action the
        // operator is supposed to be choosing between.
        eprintln!(
            "choir repair <log-file> --verify | --truncate-tail\n\
             \n\
               --verify         walk the chain and report; changes nothing\n\
               --truncate-tail  quarantine a partly written final record and\n\
                               cut the log back to the last complete one\n\
             \n\
             Exactly one mode, and no default: which of these happens to your\n\
             log is not a decision this tool should make for you."
        );
        std::process::exit(2);
    }

    let report = match choir_oplog::repair::verify(path) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("choir: cannot read {log_file}: {error:?}");
            std::process::exit(1);
        }
    };

    println!("{log_file}");
    println!("  intact records: {}", report.intact_records);
    match &report.head {
        Some(head) => println!("  head:           {}", head.to_hex()),
        None => println!("  head:           (empty log)"),
    }
    if report.torn_tail_bytes > 0 {
        println!(
            "  torn tail:      {} bytes, never acknowledged to any client",
            report.torn_tail_bytes
        );
    }

    if let Some(fault) = &report.fault {
        // Mid-log damage. Say what and where, then say the only thing
        // that actually helps -- and do not offer to truncate, because
        // truncating past this point would drop ops that were
        // acknowledged to somebody.
        println!("  FAULT:          {fault}");
        eprintln!(
            "\nThis is damage to a record that was written whole, not an interrupted\n\
             write, so cutting the end of the file cannot repair it: every record\n\
             after position {} was acknowledged to a client.\n\
             \n\
             Restore from backup:\n\
               1. stop the node (it will refuse to start on this log anyway)\n\
               2. keep this file -- do not delete it; it is the only copy of\n\
                  whatever is still readable\n\
               3. restore the log from the most recent backup\n\
               4. verify the restored copy with `choir repair <log> --verify`\n\
                  before starting the node on it",
            fault.position()
        );
        std::process::exit(1);
    }

    if verify {
        if report.torn_tail_bytes > 0 {
            println!(
                "\nUsable. The torn tail is repairable: re-run with --truncate-tail,\n\
                 or simply start the node, which repairs it on open."
            );
        } else {
            println!("\nIntact.");
        }
        return;
    }

    match choir_oplog::repair::truncate_tail(path) {
        Ok(None) => println!("\nNothing to repair; the log already ends on a record boundary."),
        Ok(Some(repaired)) => println!(
            "\nRepaired.\n  quarantined:    {} ({} bytes)\n  log length:     {}\n\n\
             The removed bytes are in that file, not deleted. Verify before\n\
             starting the node: choir repair {log_file} --verify",
            repaired.quarantine.display(),
            repaired.bytes,
            repaired.length
        ),
        Err(error) => {
            eprintln!("choir: repair refused: {error:?}");
            std::process::exit(1);
        }
    }
}

/// Loads the 32-byte secret key file, creating it (0600) if absent.
fn load_key(path: &str) -> ActorKey {
    if std::path::Path::new(path).exists() {
        let bytes = std::fs::read(path).expect("read key file");
        ActorKey::from_secret_bytes(&bytes.as_slice().try_into().expect("32-byte key file"))
    } else {
        let key = ActorKey::generate();
        // Atomic and 0600 from creation: no window where the secret is
        // world-readable or half-written.
        choir_fs::write_atomic_private(std::path::Path::new(path), key.secret_bytes())
            .expect("write key file");
        key
    }
}

/// One HTTP round trip through the shared endpoint and auth adapter.
fn http(
    api: &str,
    auth: AuthOptions<'_>,
    tool: &str,
    arguments: serde_json::Value,
) -> (u16, String) {
    let (status, body) = http_once(api, auth, tool, &arguments);
    // D80. A seed answers every write with its home; the same request,
    // unchanged, is admissible there, because a seed's view already names
    // the home's log.
    match not_home(status, &body) {
        Some(home) if follow_home(api, &home) => http_once(&home, auth, tool, &arguments),
        _ => (status, body),
    }
}

fn http_once(
    api: &str,
    auth: AuthOptions<'_>,
    tool: &str,
    arguments: &serde_json::Value,
) -> (u16, String) {
    let client = match choir_cli::mcp::HttpClient::new(
        api,
        auth.file_for(api).map(std::path::Path::new),
        auth.user,
    ) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("choir: {error}");
            std::process::exit(2);
        }
    };
    let endpoint = choir_cli::surface::mcp_endpoint(tool).expect("CLI endpoint is in the table");
    match client.request(endpoint, arguments) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("choir: {error}");
            std::process::exit(1);
        }
    }
}

/// Fetches `/api/view` and applies a pure derivation to it, pretty-printed.
///
/// A non-2xx or non-JSON response is fatal before the derivation runs:
/// classifying an error body would produce a confidently empty document,
/// which reads as "nothing to do" — the worst possible failure mode for
/// a next-actions surface.
fn derived_view(
    api: &str,
    auth: AuthOptions<'_>,
    derive: impl Fn(&serde_json::Value) -> serde_json::Value,
) -> String {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        eprintln!("choir: GET /api/view returned {status}: {body}");
        std::process::exit(1);
    }
    let view: serde_json::Value = match serde_json::from_str(&body) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("choir: /api/view response is not JSON: {error}");
            std::process::exit(1);
        }
    };
    serde_json::to_string_pretty(&derive(&view)).expect("derived documents are serializable")
}

/// One authenticated call to the node, for the operator-side commands
/// that are a request and a printed answer and nothing else.
///
/// Exists so the four D72 commands below do not each carry the same
/// twenty lines of client construction and status handling, and so the
/// exit codes they return cannot drift apart: 1 when the node refused,
/// 2 when this machine could not ask.
fn operator_call(
    api: &str,
    auth: AuthOptions<'_>,
    method: &str,
    path: &'static str,
    body: &serde_json::Value,
) -> serde_json::Value {
    let endpoint = choir_cli::surface::endpoint(method, path)
        .unwrap_or_else(|| panic!("{path} is in the endpoint table"));
    let send = |api: &str| {
        let client = match choir_cli::mcp::HttpClient::new(
            api,
            auth.file_for(api).map(std::path::Path::new),
            auth.user,
        ) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("choir: {error}");
                std::process::exit(2);
            }
        };
        match client.request(endpoint, body) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("choir: {error}");
                std::process::exit(1);
            }
        }
    };
    let (status, text) = send(api);
    let (status, text) = match not_home(status, &text) {
        Some(home) if follow_home(api, &home) => send(&home),
        _ => (status, text),
    };
    let parsed = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
    if !(200..300).contains(&status) {
        eprintln!(
            "choir: {method} {path} returned {status}: {}",
            parsed["error"].as_str().unwrap_or(text.trim())
        );
        if status == 403 || status == 401 {
            eprintln!("this needs a credential holding `@node write`.");
        }
        std::process::exit(1);
    }
    parsed
}

/// A repository argument as the ACL spells one.
///
/// `.git` is how a grant names a repository, and leaving it off is the
/// mistake that mints an invite granting nothing. Added rather than
/// refused, since there is exactly one right answer.
fn grant_line(repo: &str, level: &str) -> String {
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    format!("{repo}.git {level}")
}

/// Refuses a level that is not one, before the node has to.
fn checked_level(level: &str) -> &str {
    match level {
        "read" | "propose" | "write" => level,
        other => {
            eprintln!("choir: `{other}` is not a level; use read, propose or write");
            std::process::exit(2);
        }
    }
}

/// `choir invite <api> <name> <owner/repo> [read|write]`
///
/// Prints the join link and nothing else, because the link is the whole
/// artefact: it gets pasted into a chat window, and anything printed
/// beside it invites pasting that too. The `id:secret` pair the API also
/// returns is for `curl -u` and is not what a person receives.
///
/// `name` is a *display* name (D46). The account handle is minted by the
/// node, so the string the op log carries forever is never one an
/// operator typed at half past midnight.
/// `choir host` — a fresh machine to a running node.
///
/// The composition, not a reimplementation: every step here is a call
/// into the thing that already did it, and the value this adds is the
/// order, the refusals between the steps, and the fact that a reader
/// does not have to know which of three orders applies to their machine
/// before they have run anything.
///
/// Two lines go to stdout at the end — the URL and, when one was asked
/// for, the invite link — because those are the two things a person
/// copies out of this. Everything else is stderr, so `choir host` inside
/// a pipeline yields the addresses and nothing else.
fn host(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    let options = match choir_cli::host::parse(rest, state_dir()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{} {error}", style.red("choir host:"));
            std::process::exit(2);
        }
    };
    let layout = choir_cli::serve::Layout::new(&options.state, options.port);
    let user = choir_cli::host::username();
    let mut done: Vec<String> = Vec::new();
    let retry = rerun_line(rest);
    eprintln!();

    // 1. The state directory. Skipped rather than refused when it is
    //    already there: `choir host` is the command people re-run after
    //    pasting a sudo line, and a setup command that cannot be run
    //    twice is one that strands them at step two.
    if layout.missing().is_empty() {
        step(
            "state",
            &format!("{} (already here, kept)", options.state.display()),
        );
    } else {
        let plan = choir_cli::init::Plan::new(&options.state, options.port);
        if let Err(error) = choir_cli::init::run(&plan, false) {
            host_failed(&done, "state", &error, &retry);
        }
        step(
            "state",
            &format!(
                "{} — credential, key, trusted keys",
                options.state.display()
            ),
        );
    }
    done.push("state".to_string());

    // The two files that make this a node other people can join, both
    // created here rather than by `init`, because a node somebody is
    // *hosting* is by definition one others are meant to reach, and a
    // `--invite` that answers 503 is the whole command failing at its
    // last line.
    //
    // They come as a pair and the daemon insists on it: an issued grant
    // with no table to grade it against is a grant to every repository.
    // The table this writes is the smallest one that is not a lie — the
    // operator's own credential keeps everything, and every account
    // issued afterwards holds exactly what its invite carried.
    if !layout.acl.exists() {
        let acl = "# Who may reach what (D29). `<user> <repo|*|@node> <level>`,\n\
                   # levels read < propose < write < own. Issued grants (D36) are\n\
                   # merged with this file; `own` and `@node` are granted here only.\n\
                   choir * own\n\
                   choir @node write\n";
        if let Err(error) = choir_fs::write_atomic_private(&layout.acl, acl) {
            host_failed(
                &done,
                "acl",
                &format!("{}: {error}", layout.acl.display()),
                &retry,
            );
        }
    }
    if !layout.accounts.exists() {
        if let Err(error) = choir_fs::write_atomic_private(&layout.accounts, "") {
            host_failed(
                &done,
                "accounts",
                &format!("{}: {error}", layout.accounts.display()),
                &retry,
            );
        }
    }

    // 2. The certificate, or the reason there is none.
    let name = options.exposure.name();
    match &name {
        None => step("certificate", "not needed — this node binds loopback only"),
        Some(name) => {
            let sudo = tls_line(name, &user, options.port, options.dry_run);
            match layout.tls() {
                Some((cert, _)) if !options.dry_run && std::fs::File::open(&cert).is_ok() => {
                    match choir_cli::tls::expiry(&cert) {
                        Ok(when) => step(
                            "certificate",
                            &format!("already issued, valid until {when}"),
                        ),
                        Err(_) => step(
                            "certificate",
                            &format!("already issued — {}", cert.display()),
                        ),
                    }
                }
                _ => {
                    let firewall = choir_cli::host::firewall_hint(options.port, true);
                    let mut paste = vec![sudo];
                    if let Some(line) = firewall {
                        paste.push(line);
                    }
                    handover(
                        &done,
                        &format!(
                            "a certificate for {name} has to be issued as root.\n  \
                             certbot writes /etc/letsencrypt, and the renewal hook that keeps\n  \
                             this working for the next two years lives there too. Paste this:"
                        ),
                        &paste,
                        &retry,
                    );
                }
            }
            done.push("certificate".to_string());
        }
    }

    // 3. Linger. A `systemd --user` unit without it is stopped when the
    //    user logs out, which on a VPS is roughly one minute after the
    //    node was installed.
    match choir_cli::host::linger(&user) {
        None | Some(true) => {}
        Some(false) if options.yes => {
            eprintln!(
                "  {}  {:14}  off — this node will stop when {user} logs out",
                style.cyan("!!"),
                style.dim("linger")
            );
        }
        Some(false) => handover(
            &done,
            &format!(
                "linger is off for {user}, so systemd stops this node at logout.\n  \
                 One line fixes it for the life of the machine:"
            ),
            &[format!("sudo loginctl enable-linger {user}")],
            &format!("{retry}          (or add --yes to accept a node that dies at logout)"),
        ),
    }
    if choir_cli::host::linger(&user) == Some(true) {
        step("linger", &format!("on for {user}"));
    }

    // 4. The address, written down before the node starts, because it is
    //    what every client command will read back — including the one
    //    that mints the invite, whose link is built from the URL it was
    //    reached at.
    let url = options.exposure.url(options.port);
    if let Err(error) = choir_fs::write_atomic(&layout.public_url, format!("{url}\n")) {
        host_failed(
            &done,
            "address",
            &format!("{}: {error}", layout.public_url.display()),
            &retry,
        );
    }
    if let Err(error) = choir_fs::write_atomic(
        std::path::Path::new(".choir/config"),
        format!(
            "# Which node the `choir` commands talk to when they are not\n\
             # given one. Written by `choir host`: this machine is the node.\n\
             node = {url}\n"
        ),
    ) {
        eprintln!("  {} .choir/config: {error}", style.cyan("!!"));
    }
    step("address", &url);

    // 5. Supervision — or becoming the thing that would have been
    //    supervised. In a container the runtime is the supervisor and
    //    PID 1 should be the daemon, so this execs rather than installs.
    //    Same `exec` as `choir node serve`: signals and the exit code
    //    the daemon uses to ask for supervision (75) reach the real
    //    process rather than a wrapper.
    if options.foreground {
        step(
            "foreground",
            "becoming the daemon; the runtime supervises it",
        );
        eprintln!();
        let program = match choir_cli::serve::find_daemon() {
            Ok(program) => program,
            Err(error) => host_failed(&done, "foreground", &error, &retry),
        };
        match choir_cli::serve::plan(program, &layout, &[], &options.extra) {
            Ok(invocation) => {
                let error = choir_cli::serve::exec(&invocation);
                host_failed(&done, "foreground", &error, &retry);
            }
            Err(error) => host_failed(&done, "foreground", &error, &retry),
        }
    }
    match install_unit(&options.state, options.port, &options.extra) {
        Ok((unit, _)) => step("supervised", &unit.display().to_string()),
        Err(error) => host_failed(&done, "supervised", &error, &retry),
    }
    done.push("supervised".to_string());

    // 6. Readiness. Everything after this is a request to a daemon the
    //    service manager was asked to start a moment ago.
    let client = match choir_cli::mcp::HttpClient::new(&url, Some(&layout.auth), None) {
        Ok(client) => client,
        Err(error) => host_failed(&done, "healthy", &error, &retry),
    };
    match choir_cli::host::wait_healthy(&client, 30) {
        Ok(()) => step("healthy", &format!("{url}/healthz")),
        // The log, not the retry line, is what answers this one: the
        // unit is installed and the service manager is restarting it
        // every two seconds into whatever it is failing on, and running
        // `choir host` again would install the same unit again.
        Err(error) => host_failed(
            &done,
            "healthy",
            &format!(
                "{error}\n           it says why in {}",
                layout.log.display()
            ),
            &format!("choir node logs --state {}", options.state.display()),
        ),
    }
    done.push("healthy".to_string());

    // 7. A first repository, and a first person.
    if let Some(repo) = &options.repo {
        let endpoint = choir_cli::surface::endpoint("POST", "/api/repo")
            .expect("the repo endpoint is in the table");
        match client.request(endpoint, &serde_json::json!({ "name": repo })) {
            Ok((status, _)) if (200..300).contains(&status) => {
                step("repository", &format!("{url}/{repo}"));
            }
            Ok((409, _)) => step("repository", &format!("{repo} was already there")),
            Ok((status, body)) => {
                host_failed(&done, "repository", &format!("{status}: {body}"), &retry)
            }
            Err(error) => host_failed(&done, "repository", &error, &retry),
        }
        done.push("repository".to_string());
    }
    let mut link: Option<String> = None;
    if let Some(who) = &options.invite {
        let repo = options.repo.clone().unwrap_or_default();
        let endpoint = choir_cli::surface::endpoint("POST", "/api/accounts/invite")
            .expect("the invite endpoint is in the table");
        let body = serde_json::json!({
            "display_name": who,
            "grants": [grant_line(&repo, "write")],
        });
        match client.request(endpoint, &body) {
            Ok((status, text)) if (200..300).contains(&status) => {
                let answer: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let issued = answer["join_url"]
                    .as_str()
                    .or_else(|| answer["invite"].as_str())
                    .unwrap_or_default()
                    .to_string();
                step("invited", who);
                link = Some(issued);
            }
            Ok((status, body)) => {
                host_failed(&done, "invited", &format!("{status}: {body}"), &retry)
            }
            Err(error) => host_failed(&done, "invited", &error, &retry),
        }
    }

    // The advisories that are not steps: things this deliberately did
    // not do, each with the one line that does it.
    eprintln!();
    if name.is_some() {
        if let Some(line) = choir_cli::host::firewall_hint(options.port, true) {
            eprintln!(
                "  {} a firewall is running here and this command did not touch it.\n    {line}\n",
                style.cyan("note:")
            );
        }
    } else {
        eprintln!(
            "  {} nobody outside this machine can reach a loopback node. To share it:\n    {}\n",
            style.dim("share:"),
            choir_cli::host::share_hint(options.port)
        );
    }
    eprintln!("  {} choir doctor\n", style.dim("check it:"));

    // The two things a person copies out of this.
    println!("{url}");
    if let Some(link) = link {
        println!("{link}");
    }
    std::process::exit(0)
}

/// `choir seed` — a fresh machine to a running seed of another node's
/// log (D80). Same shape as `choir host`: numbered steps, a handover with
/// exit 3 where something only the home's operator can supply is missing,
/// and the marker `choir node serve` reads so every later command needs
/// no seed-specific argument.
fn seed(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    let options = match choir_cli::seed::parse(rest, state_dir()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{} {error}", style.red("choir seed:"));
            std::process::exit(2);
        }
    };
    let layout = choir_cli::serve::Layout::new(&options.state, options.port);
    let mut done: Vec<String> = Vec::new();
    let retry = {
        let mut line = "choir seed".to_string();
        for argument in rest {
            line.push(' ');
            line.push_str(argument);
        }
        line
    };
    eprintln!();

    // 1. The identity, minted where the daemon will look for it. Kept
    //    when it is already there: the second run of this command is the
    //    one that finishes, and a second key would be a second principal
    //    the home never registered.
    let key_path = choir_cli::seed::key_path(&layout.repos);
    let existed = key_path.exists();
    let key = match choir_cli::seed::identity(&key_path) {
        Ok(key) => key,
        Err(error) => host_failed(&done, "identity", &error, &retry),
    };
    let hex = choir_cli::seed::public_hex(&key);
    step(
        "identity",
        &format!(
            "{} {}{}",
            options.name,
            &hex[..16],
            if existed {
                "… (already here, kept)"
            } else {
                "…"
            }
        ),
    );
    done.push("identity".to_string());

    // 2. The credential the home issued, or the lines that get one.
    let Some(from) = &options.credential else {
        let paste = choir_cli::seed::registration(&options.name, &hex, &options.home);
        handover(
            &done,
            &format!(
                "the home has to admit this seed before it can read a page. On {},
                   its operator adds these to files the node already has (nothing
                   seed-specific), then hands you the auth line as a 0600 file:",
                options.home
            ),
            &paste,
            &format!("{retry} --credential <that file>"),
        );
    };
    let user = match choir_cli::seed::place_credential(from, &layout.seed_credential) {
        Ok(user) => user,
        Err(error) => host_failed(&done, "credential", &error, &retry),
    };
    step(
        "credential",
        &format!("{user} at {}", layout.seed_credential.display()),
    );
    done.push("credential".to_string());

    // 3. The seed's own readers. A serving seed names them exactly as a
    //    home does: a credential and a table. Both are kept when present,
    //    and neither exists for an archival seed, which serves nobody.
    if !options.archival {
        if !layout.auth.exists() {
            let token = match choir_cli::init::mint_token() {
                Ok(token) => token,
                Err(error) => host_failed(&done, "readers", &error, &retry),
            };
            if let Err(error) = choir_fs::write_atomic_private(
                &layout.auth,
                format!(
                    "choir:{token}
"
                ),
            ) {
                host_failed(
                    &done,
                    "readers",
                    &format!("{}: {error}", layout.auth.display()),
                    &retry,
                );
            }
        }
        if !layout.acl.exists() {
            let acl = "# Who may read what on this seed (D29). Every write is answered
                       # 421 not_home whatever this says; `own` here is the operator's
                       # read of everything the seed holds.
                       choir * own
                       choir @node auditor
";
            if let Err(error) = choir_fs::write_atomic_private(&layout.acl, acl) {
                host_failed(
                    &done,
                    "readers",
                    &format!("{}: {error}", layout.acl.display()),
                    &retry,
                );
            }
        }
        step(
            "readers",
            &format!("{} (0600), {}", layout.auth.display(), layout.acl.display()),
        );
    }

    // 4. The marker. From here on `choir node serve` is a seed.
    let marker = choir_cli::serve::Seed {
        home: options.home.clone(),
        credential: layout.seed_credential.clone(),
        serve: !options.archival,
    };
    if let Err(error) = choir_fs::write_atomic(&layout.seed_marker, marker.render()) {
        host_failed(
            &done,
            "marker",
            &format!("{}: {error}", layout.seed_marker.display()),
            &retry,
        );
    }
    step("seed of", &options.home);
    done.push("marker".to_string());

    // 5. The address, and the config that makes this machine's `choir`
    //    talk to the home while `doctor` watches the seed. Written only
    //    when nothing is there: a checkout that names another node keeps
    //    naming it.
    let url = format!("http://127.0.0.1:{}", options.port);
    if !options.archival {
        if let Err(error) = choir_fs::write_atomic(
            &layout.public_url,
            format!(
                "{url}
"
            ),
        ) {
            host_failed(
                &done,
                "address",
                &format!("{}: {error}", layout.public_url.display()),
                &retry,
            );
        }
        let config = std::path::Path::new(".choir/config");
        if !config.exists() {
            if let Err(error) = choir_fs::write_atomic(
                config,
                format!(
                    "# Written by `choir seed`: writes go to the home, and `choir doctor`
                     # compares this seed's statement with what the home shows.
                     node = {}
                     seeds = {url}
",
                    options.home
                ),
            ) {
                eprintln!("  {} .choir/config: {error}", style.cyan("!!"));
            }
        }
        step("address", &url);
    }

    // 6. Supervision, or becoming the daemon.
    if options.foreground {
        step(
            "foreground",
            "becoming the daemon; the runtime supervises it",
        );
        eprintln!();
        let program = match choir_cli::serve::find_daemon() {
            Ok(program) => program,
            Err(error) => host_failed(&done, "foreground", &error, &retry),
        };
        match choir_cli::serve::plan(program, &layout, &[], &options.extra) {
            Ok(invocation) => {
                let error = choir_cli::serve::exec(&invocation);
                host_failed(&done, "foreground", &error, &retry);
            }
            Err(error) => host_failed(&done, "foreground", &error, &retry),
        }
    }
    match install_unit(&options.state, options.port, &options.extra) {
        Ok((unit, _)) => step("supervised", &unit.display().to_string()),
        Err(error) => host_failed(&done, "supervised", &error, &retry),
    }
    done.push("supervised".to_string());

    // 7. Readiness. An archival seed has no port to ask, so its log is
    //    the only receipt.
    if options.archival {
        eprintln!();
        eprintln!(
            "  {} choir node logs --state {}
",
            style.dim("watch it:"),
            options.state.display()
        );
        std::process::exit(0);
    }
    let client = match choir_cli::mcp::HttpClient::new(&url, Some(&layout.auth), None) {
        Ok(client) => client,
        Err(error) => host_failed(&done, "healthy", &error, &retry),
    };
    match choir_cli::host::wait_healthy(&client, 30) {
        Ok(()) => step("healthy", &format!("{url}/healthz")),
        Err(error) => host_failed(
            &done,
            "healthy",
            &format!(
                "{error}
           it says why in {}",
                layout.log.display()
            ),
            &format!("choir node logs --state {}", options.state.display()),
        ),
    }
    eprintln!();
    eprintln!(
        "  {} choir doctor
",
        style.dim("check it:")
    );
    println!("{url}");
    std::process::exit(0)
}

/// `choir node upgrade` — newer binaries, a restart, and the daemon's
/// own word that it took.
fn node_upgrade(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    let command = "choir node upgrade";
    let options = match choir_cli::upgrade::parse(rest, state_dir()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{} {error}", style.red(&format!("{command}:")));
            std::process::exit(2);
        }
    };
    let fail = |what: &str, why: &str| -> ! {
        eprintln!(
            "
  {} {what}: {why}
",
            style.red("failed")
        );
        std::process::exit(1)
    };
    eprintln!();

    // 1. Where the binaries go: where this one runs from, unless told.
    let into = match &options.into {
        Some(into) => into.clone(),
        None => {
            let exe = std::env::current_exe().unwrap_or_default();
            if choir_cli::supervise::in_build_directory(&exe) {
                fail(
                    "into",
                    &format!(
                        "this `choir` lives in a build directory:
    {}
                           name the directory the node runs from: {command} ... --into <dir>",
                        exe.display()
                    ),
                );
            }
            exe.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default()
        }
    };
    let before = choir_cli::upgrade::version_of(&into.join("choir"))
        .map(|(line, _)| line)
        .unwrap_or_else(|| "nothing installed".to_string());
    step("into", &format!("{}  ({before})", into.display()));

    // 2. The node this machine runs, and its address, so the receipt can
    //    be read from it afterwards.
    let layout = choir_cli::serve::Layout::new(&options.state, 8417);
    let api = layout
        .public()
        .or_else(configured_node)
        .unwrap_or_else(|| "http://127.0.0.1:8417".to_string());
    let running_before = choir_cli::node::status(&api, Some(&layout.auth))
        .ok()
        .and_then(|(_, view)| view["build"]["commit"].as_str().map(str::to_string));
    step(
        "running",
        &format!(
            "{api}  {}",
            running_before
                .as_deref()
                .map(|c| c.chars().take(12).collect::<String>())
                .unwrap_or_else(|| "not answering".to_string())
        ),
    );

    if options.dry_run {
        let plan = match &options.source {
            choir_cli::upgrade::Source::Shelf(node) => {
                format!(
                    "sh <({node}/download/install.sh) choir-cli choir-node, into {}",
                    into.display()
                )
            }
            choir_cli::upgrade::Source::Checkout(dir) => format!(
                "cargo build --release in {}, then {} placed into {}",
                dir.display(),
                choir_cli::upgrade::BINARIES.join(", "),
                into.display()
            ),
        };
        step("would", &plan);
        step(
            "then",
            "choir node restart, and the stamp read back from the daemon",
        );
        eprintln!();
        std::process::exit(0);
    }

    // 3. The binaries.
    match &options.source {
        choir_cli::upgrade::Source::Shelf(node) => {
            let cargo_home = match choir_cli::upgrade::cargo_home_for(&into) {
                Ok(home) => home,
                Err(error) => fail("from", &error),
            };
            match choir_cli::upgrade::install_from_shelf(node, &cargo_home) {
                Ok(said) => {
                    for line in said.lines() {
                        eprintln!("      {}", style.dim(line));
                    }
                    step("fetched", &format!("{node}/download/"));
                }
                Err(error) => fail("from", &error),
            }
        }
        choir_cli::upgrade::Source::Checkout(dir) => {
            eprintln!(
                "  {}  {:14}  cargo build --release in {}",
                style.dim(".."),
                style.dim("building"),
                dir.display()
            );
            let (built, head) = match choir_cli::upgrade::build(dir) {
                Ok(built) => built,
                Err(error) => fail("build", &error),
            };
            step("built", &head.chars().take(12).collect::<String>());
            match choir_cli::upgrade::place(&built, &into) {
                Ok(placed) => step("placed", &placed.join(", ")),
                Err(error) => fail("place", &error),
            }
        }
    }
    let (after, wanted) = choir_cli::upgrade::version_of(&into.join("choir"))
        .unwrap_or_else(|| ("unreadable".to_string(), None));
    step("installed", &after);

    // 4. The restart, when there is a unit to restart.
    let Some(supervisor) = choir_cli::supervise::Supervisor::detect() else {
        eprintln!(
            "
  {} no service manager here; restart the node yourself
",
            style.cyan("note:")
        );
        std::process::exit(0);
    };
    let home = home_dir();
    if !supervisor.unit_path(&home).exists() {
        eprintln!(
            "
  {} no unit installed here; restart the node yourself, then: choir node status
",
            style.cyan("note:")
        );
        std::process::exit(0);
    }
    let steps = supervisor.commands(choir_cli::supervise::Action::Install, &home);
    let last = steps.len().saturating_sub(1);
    for (at, cmd) in steps.iter().enumerate() {
        if !run_step(cmd, at != last) {
            fail("restart", "the service manager refused");
        }
    }
    step(
        "restarted",
        &supervisor.unit_path(&home).display().to_string(),
    );

    // 5. The receipt: what the daemon says it is, not what was asked.
    let client = match choir_cli::mcp::HttpClient::new(&api, Some(&layout.auth), None) {
        Ok(client) => client,
        Err(error) => fail("healthy", &error),
    };
    if let Err(error) = choir_cli::host::wait_healthy(&client, 30) {
        fail(
            "healthy",
            &format!(
                "{error}
           it says why in {}",
                layout.log.display()
            ),
        );
    }
    let running = choir_cli::node::status(&api, Some(&layout.auth))
        .ok()
        .and_then(|(_, view)| view["build"]["commit"].as_str().map(str::to_string));
    match (running, wanted) {
        (Some(running), Some(wanted))
            if running.starts_with(&wanted) || wanted.starts_with(&running) =>
        {
            step("serving", &running.chars().take(12).collect::<String>());
            eprintln!();
            println!("{}", running.chars().take(12).collect::<String>());
            std::process::exit(0)
        }
        (Some(running), Some(wanted)) => fail(
            "serving",
            &format!(
                "the node reports {} but the installed choir is {wanted}
                            is the unit pointing at {}? choir node status",
                running.chars().take(12).collect::<String>(),
                into.display()
            ),
        ),
        (Some(running), None) => {
            step(
                "serving",
                &format!(
                    "{} (the installed binary carries no stamp to compare)",
                    running.chars().take(12).collect::<String>()
                ),
            );
            std::process::exit(0)
        }
        (None, _) => fail(
            "serving",
            "the node answers, but its view does not name a build",
        ),
    }
}

/// `choir repo follower add|list|push` — the remotes a node pushes to (D21).
fn repo_follower(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stdout();
    let command = "choir repo follower";
    let mut state: Option<String> = None;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--state" => {
                let Some(value) = rest.get(i + 1) else {
                    eprintln!("{command}: --state needs a value");
                    std::process::exit(2);
                };
                state = Some((*value).to_string());
                i += 2;
            }
            other if other.starts_with('-') => {
                eprintln!("{command}: unknown option {other:?}");
                std::process::exit(2);
            }
            arg => {
                positional.push(arg);
                i += 1;
            }
        }
    }
    let state = state
        .map(std::path::PathBuf::from)
        .unwrap_or_else(state_dir);
    let layout = choir_cli::serve::Layout::new(&state, 0);
    match positional.as_slice() {
        ["add", repo, name, url] => match choir_cli::follower::add(&layout, repo, name, url) {
            Ok(first) => {
                note(
                    "follower added",
                    &[
                        ("repository", choir_cli::follower::repository_name(repo)),
                        ("remote", format!("{name} -> {url}")),
                    ],
                );
                if first {
                    eprintln!(
                        "  {} the node pushes after every landing once restarted:
                                 choir node restart
",
                        style.cyan("next:")
                    );
                }
                eprintln!(
                    "  {} choir repo follower push {}
",
                    style.dim("push now:"),
                    choir_cli::follower::repository_name(repo)
                );
                std::process::exit(0)
            }
            Err(error) => {
                eprintln!("{} {error}", style.red(&format!("{command} add:")));
                std::process::exit(1);
            }
        },
        ["list"] => {
            let listed = choir_cli::follower::list(&layout);
            if listed.is_empty() {
                eprintln!(
                    "no repositories listed in {}",
                    layout.state.join("repos.list").display()
                );
                std::process::exit(1);
            }
            for (repo, remotes) in &listed {
                if remotes.is_empty() {
                    println!("{repo}  {}", style.cyan("no remote: no second copy yet"));
                }
                for (name, url) in remotes {
                    println!("{repo}  {name} -> {url}");
                }
            }
            eprintln!(
                "
  {} {}
",
                style.dim("after each landing:"),
                if choir_cli::follower::following(&layout) {
                    "pushed (--followers)"
                } else {
                    "not pushed; `choir repo follower add` turns it on"
                }
            );
            std::process::exit(0)
        }
        ["push"] | ["push", _] => {
            let only = positional.get(1).copied();
            let outcomes = choir_cli::follower::push(&layout, only);
            if outcomes.is_empty() {
                eprintln!("nothing to push: no repository listed");
                std::process::exit(1);
            }
            let mut failed = false;
            for outcome in &outcomes {
                let line = match outcome.event {
                    "pushed" => format!(
                        "{} {} -> {}",
                        style.green("pushed"),
                        outcome.repo,
                        outcome.remote
                    ),
                    "no_remote" => format!(
                        "{} {}: {}",
                        style.cyan("skipped"),
                        outcome.repo,
                        outcome.detail
                    ),
                    _ => {
                        failed = true;
                        format!(
                            "{} {} -> {}: {}",
                            style.red("FAILED"),
                            outcome.repo,
                            outcome.remote,
                            outcome.detail
                        )
                    }
                };
                println!("{line}");
            }
            std::process::exit(i32::from(failed))
        }
        _ => {
            eprintln!(
                "{command}: add <owner/repo.git> <name> <url> | list | push [<owner/repo.git>]"
            );
            std::process::exit(2);
        }
    }
}

/// `<backup-dir> [--state <dir>]`, shared by `backup take` and `backup schedule`.
fn backup_options(command: &str, rest: &[&str]) -> (std::path::PathBuf, std::path::PathBuf, u64) {
    let mut dest: Option<String> = None;
    let mut state: Option<String> = None;
    let mut every: u64 = 3600;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--state" | "--every" => {
                let Some(value) = rest.get(i + 1) else {
                    eprintln!("{command}: {} needs a value", rest[i]);
                    std::process::exit(2);
                };
                if rest[i] == "--state" {
                    state = Some((*value).to_string());
                } else {
                    every = match value.parse::<u64>() {
                        Ok(n) if n >= 60 => n,
                        _ => {
                            eprintln!("{command}: --every is a number of seconds, at least 60, not {value:?}");
                            std::process::exit(2);
                        }
                    };
                }
                i += 2;
            }
            other if other.starts_with('-') => {
                eprintln!("{command}: unknown option {other:?}");
                std::process::exit(2);
            }
            positional => {
                if dest.is_some() {
                    eprintln!("{command}: one backup directory, not two");
                    std::process::exit(2);
                }
                dest = Some(positional.to_string());
                i += 1;
            }
        }
    }
    let Some(dest) = dest else {
        eprintln!(
            "{command}: where should the backup go?

    {command} <backup-dir>"
        );
        std::process::exit(2);
    };
    let dest = std::path::PathBuf::from(dest);
    let dest = std::path::absolute(&dest).unwrap_or(dest);
    let state = state
        .map(std::path::PathBuf::from)
        .unwrap_or_else(state_dir);
    let state = std::path::absolute(&state).unwrap_or(state);
    (dest, state, every)
}

/// `choir backup take` — a backup of this machine's node, verified.
fn backup_take(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stdout();
    let command = "choir backup take";
    let (dest, state, _) = backup_options(command, rest);
    // The backup must not sit on the disk it is meant to survive. Refused
    // only for the one case that is certainly wrong; a separate mount on
    // the same machine is the operator's call.
    if dest.starts_with(&state) {
        eprintln!(
            "{} {} is inside the state directory it would back up

               a backup on the disk it is meant to survive is not one; give another path",
            style.red(&format!("{command}:")),
            dest.display()
        );
        std::process::exit(2);
    }
    let layout = choir_cli::serve::Layout::new(&state, 0);
    let daemon = choir_cli::serve::find_daemon().ok();
    let taken = match choir_cli::backup::take(&layout, &dest, daemon.as_deref()) {
        Ok(taken) => taken,
        Err(error) => {
            eprintln!("{} {error}", style.red(&format!("{command}:")));
            std::process::exit(1);
        }
    };
    let mut rows: Vec<(&str, String)> = vec![
        ("into", taken.dest.display().to_string()),
        (
            "log",
            format!("{} ops, next seq {}", taken.ops, taken.next_seq),
        ),
    ];
    if let Some((was, now)) = taken.grew {
        rows.push(("grew", format!("{was} -> {now} bytes")));
    }
    for line in &taken.bundles {
        rows.push(("bundle", line.clone()));
    }
    note("taken", &rows);
    for warning in &taken.warnings {
        eprintln!("  {} {warning}", style.cyan("!!"));
    }
    if !taken.warnings.is_empty() {
        eprintln!();
    }
    let checks = choir_cli::backup::verify(&dest, daemon.as_deref());
    print!("{}", choir_cli::doctor::report(&checks, style));
    std::process::exit(i32::from(!choir_cli::backup::restorable(&checks)))
}

/// `choir backup schedule` — `backup take` on a timer.
fn backup_schedule(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stdout();
    let command = "choir backup schedule";
    let (dest, state, every) = backup_options(command, rest);
    let supervisor = supervisor(command);
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => {
            eprintln!(
                "{} cannot find my own path",
                style.red(&format!("{command}:"))
            );
            std::process::exit(1);
        }
    };
    if choir_cli::supervise::in_build_directory(&exe) {
        eprintln!(
            "{} this `choir` lives in a build directory:
    {}

               a timer pointing there stops working at the next `cargo clean`. install it first.",
            style.red(&format!("{command}:")),
            exe.display()
        );
        std::process::exit(1);
    }
    let home = home_dir();
    let units = supervisor.backup_units(&home);
    let bodies = supervisor.render_backup(&exe, &state, &dest, every);
    for (unit, body) in units.iter().zip(bodies) {
        if let Some(parent) = unit.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "{} create {}: {error}",
                    style.red(&format!("{command}:")),
                    parent.display()
                );
                std::process::exit(1);
            }
        }
        if let Err(error) = choir_fs::write_atomic(unit, body) {
            eprintln!(
                "{} write {}: {error}",
                style.red(&format!("{command}:")),
                unit.display()
            );
            std::process::exit(1);
        }
    }
    let steps = supervisor.backup_commands(choir_cli::supervise::Action::Install, &home);
    let last = steps.len().saturating_sub(1);
    for (at, step) in steps.iter().enumerate() {
        if !run_step(step, at != last) {
            eprintln!(
                "{} the service manager refused",
                style.red(&format!("{command}:"))
            );
            std::process::exit(1);
        }
    }
    let rows: Vec<(&str, String)> = vec![
        ("every", format!("{every}s")),
        ("into", dest.display().to_string()),
        (
            "unit",
            units
                .iter()
                .map(|u| u.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        ("log", state.join("backup.log").display().to_string()),
    ];
    note("scheduled", &rows);
    eprintln!(
        "  {} choir backup verify {}
",
        style.dim("check one"),
        dest.display()
    );
    std::process::exit(0)
}

/// The `sudo` line `choir host` asks for, spelled out.
fn tls_line(domain: &str, user: &str, port: u16, dry_run: bool) -> String {
    let mut line = format!("sudo choir node tls {domain} --user {user} --port {port}");
    if dry_run {
        line.push_str(" --dry-run");
    }
    line
}

/// The same `choir host` invocation, to print as the thing to run next.
fn rerun_line(rest: &[&str]) -> String {
    let mut line = "choir host".to_string();
    for argument in rest {
        line.push(' ');
        line.push_str(argument);
    }
    line
}

/// `choir node tls` — obtain the certificate and wire up its renewal.
///
/// The only command in this binary that expects to be run as root, and
/// the only one that writes outside the state directory. Both facts are
/// checked before anything happens rather than discovered halfway
/// through, because a half-done certificate step leaves a marker naming
/// files that are not there — and the node reads that marker at every
/// start.
fn node_tls(rest: &[&str]) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    let command = "choir node tls";
    let mut domain: Option<&str> = None;
    let mut user: Option<String> = None;
    let mut port = 8417u16;
    let mut issuance = choir_cli::tls::Issuance::Live;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--dry-run" => {
                issuance = choir_cli::tls::Issuance::DryRun;
                i += 1;
            }
            "--staging" => {
                issuance = choir_cli::tls::Issuance::Staging;
                i += 1;
            }
            flag @ ("--user" | "--port") => {
                let Some(value) = rest.get(i + 1) else {
                    eprintln!("{command}: {flag} needs a value");
                    std::process::exit(2);
                };
                match flag {
                    "--user" => user = Some((*value).to_string()),
                    _ => match value.parse() {
                        Ok(n) => port = n,
                        Err(_) => {
                            eprintln!("{command}: --port needs a port number, not {value:?}");
                            std::process::exit(2);
                        }
                    },
                }
                i += 2;
            }
            other if !other.starts_with('-') && domain.is_none() => {
                domain = Some(other);
                i += 1;
            }
            other => {
                eprintln!("{command}: unknown option {other:?}");
                std::process::exit(2);
            }
        }
    }
    let Some(domain) = domain else {
        eprintln!(
            "{command}: which name is the certificate for?\n\n  \
             sudo choir node tls <domain> --user <the account the node runs as>"
        );
        std::process::exit(2);
    };
    // Required, never inferred. Under `sudo` the running account is
    // root, and falling back to `SUDO_USER` would name the operator's
    // own account — which is exactly the account the node does not run
    // as. The failure would be silent: a marker and a cert pair landing
    // in the wrong home while the node keeps serving plaintext.
    let Some(user) = user else {
        eprintln!(
            "{command}: --user is required — which unprivileged account does the node\n  \
             run as? Under sudo this process is root, and guessing would put the\n  \
             certificate in the wrong home while the node kept serving plaintext.\n\n  \
             sudo choir node tls {domain} --user $(id -un)"
        );
        std::process::exit(2);
    };
    let home = home_of(&user).unwrap_or_else(|| {
        eprintln!("{command}: no such user: {user}");
        std::process::exit(1);
    });
    let plan = choir_cli::tls::Plan::new(domain, port, &user, &home);
    let challenge = choir_cli::tls::Challenge::for_state(&plan.state);
    let uid = choir_cli::tls::uid();
    if let Err(problems) = choir_cli::tls::preflight(&plan, challenge, &uid) {
        eprintln!("\n  {} {problems}\n", style.red(&format!("{command}:")));
        std::process::exit(1);
    }
    eprintln!();
    if challenge == choir_cli::tls::Challenge::Http01 {
        eprintln!(
            "  {} HTTP-01: port 80 must be reachable from the internet now and at\n         \
             every renewal. There is no $HOME/.choir/cloudflare.ini here, which is\n         \
             what selects DNS-01 instead.\n",
            style.dim("method:")
        );
    }
    let (steps, failure) = match choir_cli::tls::apply(&plan, challenge, issuance, &uid) {
        Ok(steps) => (steps, None),
        Err((steps, error)) => (steps, Some(error)),
    };
    for one in &steps {
        match &one.outcome {
            Ok(detail) => step(&one.what, detail),
            Err(error) => eprintln!(
                "  {}  {:14}  {error}",
                style.red("--"),
                style.dim(&one.what)
            ),
        }
    }
    if let Some(error) = failure {
        eprintln!(
            "\n  {} {error}\n\n  \
             nothing the node reads was changed, so it is still serving whatever it\n  \
             was serving before.\n",
            style.red("stopped:")
        );
        std::process::exit(1);
    }
    if issuance == choir_cli::tls::Issuance::DryRun {
        eprintln!(
            "\n  {} the path works and no certificate was issued. Run it again\n  \
             without --dry-run.\n",
            style.green("dry run:")
        );
        std::process::exit(0);
    }
    eprintln!(
        "\n  {} renewal is certbot's own timer; the hook above re-projects the pair\n  \
         and restarts the node, because the daemon reads its certificate once at\n  \
         bind and has no reload.\n\n  \
         {} back as {user}: choir host --domain {domain} --port {port}\n",
        style.dim("renewal:"),
        style.dim("then:")
    );
    println!("{}", plan.url());
    std::process::exit(0)
}

/// One account's home directory, out of the password database.
///
/// `getent passwd` where it exists, falling back to `dscl` on macOS,
/// because `choir node tls` names an account other than the one running
/// it and `HOME` describes the wrong one.
fn home_of(user: &str) -> Option<std::path::PathBuf> {
    if let Some(out) = std::process::Command::new("getent")
        .args(["passwd", user])
        .output()
        .ok()
        .filter(|out| out.status.success())
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let field = text.trim().split(':').nth(5)?;
        if !field.is_empty() {
            return Some(std::path::PathBuf::from(field));
        }
    }
    let out = std::process::Command::new("dscl")
        .args([".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = text.trim().strip_prefix("NFSHomeDirectory:")?.trim();
    match field.is_empty() {
        true => None,
        false => Some(std::path::PathBuf::from(field)),
    }
}

fn invite(api: &str, auth: AuthOptions<'_>, name: &str, repo: &str, level: &str) -> ! {
    let answer = operator_call(
        api,
        auth,
        "POST",
        "/api/accounts/invite",
        &serde_json::json!({
            "display_name": name,
            "grants": [grant_line(repo, checked_level(level))],
        }),
    );
    match answer["join_url"].as_str() {
        Some(url) => println!("{url}"),
        // A node reached without a `Host` header gets no link rather
        // than a guessed one, so say what there is: the pair, which is
        // what `curl -u` wants.
        None => println!("{}", answer["invite"].as_str().unwrap_or_default()),
    }
    std::process::exit(0)
}

/// `choir asks <api>` — the queue, oldest first.
///
/// One line each: the id to answer, how long they have been waiting, and
/// what they wrote. No address, because none was collected (D72).
fn asks(api: &str, auth: AuthOptions<'_>) -> ! {
    let answer = operator_call(api, auth, "GET", "/api/accounts", &serde_json::json!({}));
    let rows = answer["requests"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if rows.is_empty() {
        println!("Nobody is waiting.");
        std::process::exit(0);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for row in rows {
        let asked_at = row["asked_at"].as_u64().unwrap_or(now);
        let waited = now.saturating_sub(asked_at) / 3600;
        println!(
            "{}  {}  ({waited}h)  {}",
            row["request_id"].as_str().unwrap_or("?"),
            row["display_name"].as_str().unwrap_or("?"),
            row["about"].as_str().unwrap_or("")
        );
    }
    std::process::exit(0)
}

/// `choir grant <api> <request-id> <owner/repo> [read|write]`
///
/// Nothing to send afterwards, and that is the point: the request
/// becomes an invite under the id and secret the asker already holds, so
/// the link they were given is the link that starts working.
fn grant(api: &str, auth: AuthOptions<'_>, id: &str, repo: &str, level: &str) -> ! {
    let answer = operator_call(
        api,
        auth,
        "POST",
        "/api/accounts/request/grant",
        &serde_json::json!({
            "request_id": id,
            "grants": [grant_line(repo, checked_level(level))],
        }),
    );
    println!(
        "{} is in as {}. The link they already hold now works; send nothing.",
        answer["display_name"].as_str().unwrap_or("they"),
        answer["user"].as_str().unwrap_or("?")
    );
    std::process::exit(0)
}

/// `choir decline <api> <request-id>`
fn decline(api: &str, auth: AuthOptions<'_>, id: &str) -> ! {
    operator_call(
        api,
        auth,
        "POST",
        "/api/accounts/request/decline",
        &serde_json::json!({ "request_id": id }),
    );
    println!("Declined. Their link now reads as one that was never valid.");
    std::process::exit(0)
}

/// Rewrites an ACL file's trailing comments from the node's roster (D46).
///
/// The decisions all live in [`choir_cli::acl::render`]; this is the IO
/// around it. Three things it deliberately does:
///
/// - **Reads the roster before touching the file.** A failed fetch must
///   leave the ACL exactly as it was, because the failure mode of the
///   alternative is an authorization file emptied of its names by a
///   network error.
/// - **Writes only on a change**, so a re-run on a current file produces
///   no churn and no new mtime for the node's hot-reload to notice.
/// - **Prints the counts.** A file that renders no comments at all looks
///   exactly like a file whose handles are all unnamed, and the second
///   is the one worth knowing about.
fn acl_render(api: &str, auth: AuthOptions<'_>, acl_file: &str) -> ! {
    let endpoint = choir_cli::surface::endpoint("GET", "/api/accounts")
        .expect("the accounts roster is in the endpoint table");
    let client = match choir_cli::mcp::HttpClient::new(
        api,
        auth.file_for(api).map(std::path::Path::new),
        auth.user,
    ) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("choir: {error}");
            std::process::exit(2);
        }
    };
    let (status, body) = match client.request(endpoint, &serde_json::json!({})) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("choir: {error}");
            std::process::exit(1);
        }
    };
    if !(200..300).contains(&status) {
        eprintln!(
            "choir: GET /api/accounts returned {status}: {body}\n\
             reading the roster needs a credential with `@node auditor`."
        );
        std::process::exit(1);
    }
    let roster = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(doc) => doc["accounts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|account| {
                Some((
                    account["user"].as_str()?.to_string(),
                    account["display_name"].as_str()?.to_string(),
                ))
            })
            .collect::<choir_cli::acl::Roster>(),
        Err(error) => {
            eprintln!("choir: /api/accounts response is not JSON: {error}");
            std::process::exit(1);
        }
    };

    let path = std::path::Path::new(acl_file);
    let before = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("choir: cannot read {acl_file}: {error}");
            std::process::exit(1);
        }
    };
    let after = choir_cli::acl::render(&before, &roster);
    let (grants, named) = choir_cli::acl::counts(&before, &roster);
    let wrote = after != before;
    if wrote {
        // Private, not the plain atomic write: the replacement file is
        // created 0600 before any bytes reach it. An ACL file is 0600
        // by operator convention, and a rewrite that restored it at the
        // umask's mercy would widen an authorization file as a side
        // effect of making it readable.
        if let Err(error) = choir_fs::write_atomic_private(path, &after) {
            eprintln!("choir: cannot write {acl_file}: {error}");
            std::process::exit(1);
        }
    }
    let doc = serde_json::json!({
        "path": path.display().to_string(),
        "wrote": wrote,
        "grants": grants,
        "named": named,
        "unresolved": grants - named,
    });
    finish(200, &doc.to_string());
}

/// Where the credential that redeems an invite came from.
///
/// Two shapes because there are two readers. An agent is handed a file
/// by whatever provisioned it; a person is handed a link in a chat
/// window, and the link *is* the credential — writing it to a file first
/// so this could read it back would be asking them to do by hand the one
/// step this command exists to remove.
enum Invite<'a> {
    /// A file holding one `<id>:<secret>` line.
    File(&'a str),
    /// The two halves, lifted straight out of a join link.
    Pair(String, String),
}

/// Creates `~/.choir` at 0700 if it is not there.
///
/// 0700 rather than the umask's answer because everything this directory
/// is about to hold — an actor key, a bearer token — is a secret, and a
/// directory somebody else can list is a directory whose filenames tell
/// them what to come back for.
fn ensure_state_dir() -> Result<(), String> {
    let dir = state_dir();
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod 700 {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// The `next` line out of a node's structured rejection.
///
/// Every rejection body carries `code`, `error` and `next` (see
/// `ERRORS.md`), and `next` is the only one of the three that says what
/// to do. Printing the body alone left it as the fourth field of a JSON
/// object, which is where a reader who is already stuck stops reading.
fn refusal_next(body: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(body).ok()?;
    doc.get("next")?.as_str().map(str::to_string)
}

/// The repair for a refused redemption, worked out from the node's
/// message.
///
/// Reads the `error` string because this endpoint has no rejection code
/// to branch on. Substring matching is the weaker test and is used
/// knowingly: the fallback is a true sentence for every message that
/// does not match, so a reworded node message costs a specific line and
/// never produces a wrong one.
fn redeem_next(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|doc| doc.get("error")?.as_str().map(str::to_string))
        .unwrap_or_default();
    if message.contains("no such invite") {
        return "ask the operator for a new link; this one was never valid, or has been used"
            .to_string();
    }
    if message.contains("expired") {
        return "ask the operator for a new link; this one has expired".to_string();
    }
    if message.contains("--invite-binds-keys") {
        return "send the operator the line `choir key <key-file> <channel>` prints, \
                and ask them to register it"
            .to_string();
    }
    if message.contains("already taken") || message.contains("username") {
        return "choir join '<link>' --user <another-name>".to_string();
    }
    "send the operator that message; it describes their node, not your machine".to_string()
}

/// `choir join <link>`, or `choir join <api> <invite-file> <key-file>`
///
/// Admission in one command: mint an actor key, redeem the operator's
/// invite, store the token where the rest of the CLI reads it, and — for
/// the link form — leave git and `~/.choir/config` set up and a clone of
/// each granted repository in the current directory, so that the next
/// thing the reader types is `cd` and the thing after it is
/// `choir propose`.
///
/// **The link is the whole input.** It carries the node and the invite,
/// which is why the one thing a person was actually sent is now the one
/// thing they have to paste. It reaches `curl` over stdin like every
/// other credential here, never on an argv, where `ps` would show it.
/// Pasted again on the machine that redeemed it, it finishes the join
/// that already happened (see [`rejoin`]) rather than refusing.
///
/// The three-argument form stays because agents and provisioning
/// scripts hold those paths. It answers with JSON, writes the token
/// beside the key rather than into `~/.choir`, and touches neither git
/// nor the home directory: a command that edits `~/.gitconfig` without
/// being asked is one nobody can run under an automation account twice.
///
/// What this does **not** do is decide admission. The operator issued
/// the invite, and the invite carries the grants; this only spares them
/// the second out-of-band step of pasting a key line into a file. A node
/// started without `--invite-binds-keys` refuses the key and says so,
/// and admission there still ends with an operator's edit.
fn join(api: &str, invite: Invite<'_>, key_file: Option<&str>, rest: &[&str]) -> ! {
    let (mut channel, mut ssh_key, mut token_file, mut chosen_user) = (None, None, None, None);
    let mut key_flag = None;
    let mut no_clone = false;
    let mut index = 0;
    while index < rest.len() {
        // The one flag that takes no value: the copy goes somewhere else,
        // or nowhere yet.
        if rest[index] == "--no-clone" && !no_clone {
            no_clone = true;
            index += 1;
            continue;
        }
        let Some(value) = rest.get(index + 1).copied() else {
            usage();
        };
        let slot = match rest[index] {
            "--channel" if channel.is_none() => &mut channel,
            "--ssh-key" if ssh_key.is_none() => &mut ssh_key,
            "--token-file" if token_file.is_none() => &mut token_file,
            // Says "yes, that path, I know what is there" -- the one way
            // past the refusal below, and the reason the refusal can be
            // flat rather than a prompt.
            "--key-file" if key_flag.is_none() => &mut key_flag,
            // The name this account will hold forever (D75). Required by
            // an invite that left the seat open, which is the ordinary
            // kind: the node no longer picks on anybody's behalf.
            "--user" if chosen_user.is_none() => &mut chosen_user,
            _ => usage(),
        };
        *slot = Some(value);
        index += 2;
    }
    let from_link = key_file.is_none();
    if from_link {
        if let Err(error) = ensure_state_dir() {
            eprintln!("choir join: {error}\nnext: choir join '<link>' --key-file <path>");
            std::process::exit(1);
        }
    }
    // Three ways to name the key, most explicit first.
    //
    // The default path is refused when this machine has *already
    // joined*, because a key is an identity the node has bound and
    // overwriting one silently would strand every operation the old one
    // signed. The test for "already joined" is the token beside it, not
    // the key alone: `load_key` mints before the request, so any
    // redemption that was refused -- a name this invite left open, a
    // node that does not bind keys, a network that dropped -- leaves a
    // key behind with no token. Refusing on the key alone made every one
    // of those refusals permanent, which is the opposite of what this
    // guard is for. A key with no token was never successfully redeemed,
    // so redeeming with it is exactly right, and is what makes
    // `load_key`'s idempotency reachable.
    let default_key = state_dir().join("agent.key").display().to_string();
    let key_file = match (key_flag, key_file) {
        (Some(named), _) | (None, Some(named)) => named.to_string(),
        (None, None) => {
            let joined =
                std::path::Path::new(&default_key).exists() && state_dir().join("auth").exists();
            // Except for the link this machine already redeemed, on the
            // node it redeemed it at: that is a join that already
            // happened, run again by somebody unsure the first one worked,
            // so it is finished rather than refused. Any other link still
            // stops here, because what a second invite should do to an
            // account that exists is the node's to decide.
            if let (true, Invite::Pair(id, _)) = (joined, &invite) {
                let home = state_dir().join("config");
                if configured_in(&home, "node").as_deref() == Some(api.trim_end_matches('/'))
                    && configured_in(&home, "invite").as_deref() == Some(id.as_str())
                {
                    rejoin(api, no_clone);
                }
            }
            if joined {
                eprintln!(
                    "choir join: this machine has already joined a node: there is a key at \
                     {default_key} and a token beside it.\n\
                     A key is an identity the node has bound, so this will not replace one.\n\
                     next: choir join '<link>' --key-file <another-path> --token-file <another-path>"
                );
                std::process::exit(1);
            }
            default_key
        }
    };
    let key_file = key_file.as_str();
    let invite_file = match invite {
        Invite::File(path) => {
            if !std::path::Path::new(path).is_file() {
                eprintln!(
                    "choir join: {path} does not exist.\n\
                     Write the invite the operator sent you into it, as one line: <id>:<secret>\n\
                     next: choir join '<link>'   (the link needs no file at all)"
                );
                std::process::exit(2);
            }
            Some(path)
        }
        Invite::Pair(..) => None,
    };
    // Minted before the request, so the key exists whatever the node
    // answers. `load_key` is idempotent, which makes a retry after a
    // network failure redeem the key that already exists rather than a
    // second one the operator never saw.
    let key = load_key(key_file);
    let mut body = serde_json::json!({ "actor_key": hex_encode(&key.public_key_bytes()) });
    if let Some(user) = chosen_user {
        body["user"] = serde_json::json!(user);
    }
    if let Some(channel) = channel {
        body["channel"] = serde_json::json!(channel);
    }
    if let Some(path) = ssh_key {
        match std::fs::read_to_string(path) {
            Ok(line) => body["ssh_key"] = serde_json::json!(line.trim()),
            Err(error) => {
                eprintln!("choir join: cannot read {path}: {error}");
                std::process::exit(2);
            }
        }
    }

    let endpoint = choir_cli::surface::endpoint("POST", "/api/accounts/redeem")
        .expect("redemption is in the endpoint table");
    let client = match &invite {
        Invite::File(_) => {
            choir_cli::mcp::HttpClient::new(api, invite_file.map(std::path::Path::new), None)
        }
        Invite::Pair(id, secret) => choir_cli::mcp::HttpClient::with_credential(api, id, secret),
    };
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            eprintln!("choir join: {error}\nnext: choir join '<link>'");
            std::process::exit(2);
        }
    };
    let send = |body: &serde_json::Value| match client.request(endpoint, body) {
        Ok(response) => response,
        Err(error) => {
            eprintln!(
                "choir join: {error}\n\
                 next: choir doctor   (it says which of curl, the network or the node is at fault)"
            );
            std::process::exit(1);
        }
    };
    let (mut status, mut response) = send(&body);
    // An invite that left the seat open is the ordinary kind, and the
    // node says so by refusing rather than by advertising it beforehand
    // -- so the name is asked for here, after the refusal, and only ever
    // once. The refusal happens before the invite is consumed, which is
    // what makes retrying it safe.
    if status == 400 && response.contains("this invite lets you pick your name") {
        match choir_cli::prompt::ask("Pick the name this account keeps (letters, digits, - and _):")
        {
            Some(name) => {
                body["user"] = serde_json::json!(name);
                (status, response) = send(&body);
            }
            None => {
                eprintln!(
                    "choir join: this invite lets you pick your name, and nothing here can \
                     ask for one.\n\
                     next: choir join '<link>' --user <name>"
                );
                std::process::exit(2);
            }
        }
    }
    if !(200..300).contains(&status) {
        println!("{response}");
        // `/api/accounts/redeem` predates the structured-rejection table
        // and answers with a bare `error`, so the repair is worked out
        // here rather than read off the body. Four of them, because
        // those are the four a contributor can actually reach: the rest
        // are the operator's node being misconfigured, and the generic
        // line is the true thing to say about those.
        eprintln!(
            "\nnext: {}",
            refusal_next(&response).unwrap_or_else(|| redeem_next(&response))
        );
        std::process::exit(1);
    }
    let account: serde_json::Value = match serde_json::from_str(&response) {
        Ok(account) => account,
        Err(error) => {
            // Both of these describe the operator's node rather than
            // this machine, so the repair is theirs and the `next:` says
            // whose it is. Retrying changes nothing.
            eprintln!(
                "choir join: the node's answer did not parse: {error}\n\
                 next: send the operator that line; their node answered something \
                 this version cannot read"
            );
            std::process::exit(1);
        }
    };
    let (Some(user), Some(token)) = (account["user"].as_str(), account["token"].as_str()) else {
        eprintln!(
            "choir join: the node issued no token\n\
             next: send the operator this; the invite was accepted and nothing was handed back"
        );
        std::process::exit(1);
    };

    // An invite is single-use, so the token is shown exactly once and
    // this is the only chance to keep it. Written 0600 before anything
    // is printed: a token this process holds and never stored is one the
    // contributor has to ask for a second invite to replace.
    // The link form defaults to the path every other command already
    // looks for, which is what makes "and nothing else" true: a
    // credential written next to the key would need `--auth-file` typed
    // on every command after it. The positional form keeps the name it
    // has always written, because agents and scripts hold that path.
    let token_path = match (token_file, from_link) {
        (Some(named), _) => std::path::PathBuf::from(named),
        (None, true) => state_dir().join("auth"),
        (None, false) => std::path::Path::new(key_file)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("choir.auth"),
    };
    if let Err(error) = choir_fs::write_atomic_private(&token_path, format!("{user}:{token}\n")) {
        eprintln!(
            "choir join: the node issued a token but it could not be stored at {}: {error}\n\
             The invite is spent, so this token cannot be reissued.\n\
             next: ask the operator for another link, then run \
             `choir join '<link>' --token-file <a-writable-path>`",
            token_path.display()
        );
        std::process::exit(1);
    }

    let bound = account["actor_key_bound"] == serde_json::Value::Bool(true);
    let channel = account["channel"].as_str().unwrap_or(user);
    let grants: Vec<&str> = account["grants"]
        .as_array()
        .map(|rows| rows.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    // Only the link form writes to git and to `~/.choir`. The positional
    // form is what tests and provisioning scripts run, often as a user
    // whose `~/.gitconfig` belongs to somebody else's automation, and a
    // command that edits it without being asked is one nobody can run
    // twice safely.
    let (git_config, home_config) = match (from_link, &invite) {
        (true, Invite::Pair(id, _)) => (
            configure_git_credential(api, &token_path.display().to_string()),
            write_home_config(api, channel, key_file, id, &grants),
        ),
        _ => (None, None),
    };
    let next = if bound {
        "choir propose".to_string()
    } else {
        // Said as an instruction rather than a warning, because it is
        // the step that is still outstanding: nothing this contributor
        // does next will work until the operator registers the key.
        format!("choir key {key_file} {channel}   — send the operator that line")
    };
    let summary = serde_json::json!({
        "user": user,
        "channel": account["channel"],
        "grants": account["grants"],
        "auth_file": token_path.display().to_string(),
        "key_file": key_file,
        "actor_key_bound": bound,
        "git_config": git_config,
        "config": home_config,
        "next": next,
    });
    if !from_link {
        note(
            &format!("joined as {user}"),
            &[
                ("token", format!("{} (0600)", token_path.display())),
                ("key", key_file.to_string()),
                ("next", next),
            ],
        );
        finish(
            200,
            &serde_json::to_string_pretty(&summary).expect("summary is serializable"),
        );
    }

    // The link form's reader is a person, so the answer is the report
    // rather than the JSON. What was written is listed in full because
    // every line of it is a file on their machine they did not choose,
    // and the last line is one they can copy.
    let style = choir_cli::style::Style::for_stdout();
    println!("\n  {}\n", style.green(&format!("Joined {api} as {user}.")));
    println!("  {}  {}", style.dim("channel "), channel);
    println!(
        "  {}  {} {}",
        style.dim("key     "),
        key_file,
        style.dim("(0600)")
    );
    println!(
        "  {}  {} {}",
        style.dim("token   "),
        token_path.display(),
        style.dim("(0600)")
    );
    if let Some(path) = &home_config {
        println!(
            "  {}  {} {}",
            style.dim("node    "),
            path,
            style.dim("(so no command has to be told the node again)")
        );
    }
    say_git(git_config.as_deref(), api, &token_path, style);
    let copies = if no_clone {
        Vec::new()
    } else {
        clone_granted(api, &grants, style)
    };
    if bound {
        say_where_to_go(&copies, style);
    } else {
        println!(
            "\n  {}\n  {}\n",
            style.dim(
                "This node does not register keys at redemption. Send the operator this line:"
            ),
            style.cyan(&format!("choir key {key_file} {channel}")),
        );
    }
    std::process::exit(0);
}

/// `choir join` with the link this machine already redeemed, on the node
/// it redeemed it at.
///
/// The node forgets an invite once it is spent, so redeeming again would
/// only be refused as a link that was never valid. Instead the join is
/// finished from what the first run recorded: nothing is redeemed and
/// neither the key nor the token is touched. The git helper is set again
/// because it is the same value and every clone below needs it: a first
/// run on a machine that had no git yet recorded the account and could
/// neither configure git nor clone.
fn rejoin(api: &str, no_clone: bool) -> ! {
    let token_path = state_dir().join("auth");
    let user = std::fs::read_to_string(&token_path)
        .ok()
        .and_then(|line| line.split_once(':').map(|(user, _)| user.to_string()))
        .unwrap_or_default();
    let repos = configured_in(&state_dir().join("config"), "repos").unwrap_or_default();
    let repos: Vec<&str> = repos.split_whitespace().collect();
    let style = choir_cli::style::Style::for_stdout();
    println!(
        "\n  {}\n",
        style.green(&format!("Already joined {api} as {user}."))
    );
    let git_config = configure_git_credential(api, &token_path.display().to_string());
    say_git(git_config.as_deref(), api, &token_path, style);
    let copies = if no_clone {
        Vec::new()
    } else {
        clone_granted(api, &repos, style)
    };
    say_where_to_go(&copies, style);
    std::process::exit(0);
}

/// The report's `git` row: where the helper was written, or the line
/// that writes it by hand.
fn say_git(
    git_config: Option<&str>,
    api: &str,
    token_path: &std::path::Path,
    style: choir_cli::style::Style,
) {
    match git_config {
        Some(path) => println!(
            "  {}  {} {}",
            style.dim("git     "),
            path,
            style.dim("(clone and push need no token in the URL)")
        ),
        None => println!(
            "  {}  {}",
            style.dim("git     "),
            style.cyan(&format!(
                "not configured; run: git config --global credential.{api}.helper \
                 '!choir git-credential {}'",
                token_path.display()
            )),
        ),
    }
}

/// The report's last lines: into a copy, then `choir propose`.
fn say_where_to_go(copies: &[String], style: choir_cli::style::Style) {
    match copies.first() {
        Some(folder) => println!(
            "\n  {}\n  {}\n  {}\n",
            style.dim(if copies.len() == 1 {
                "Go into your copy, commit on a branch as you always would, then propose it:"
            } else {
                "Go into a copy, commit on a branch as you always would, then propose it:"
            }),
            style.cyan(&format!("cd {folder}")),
            style.cyan("choir propose")
        ),
        None => println!(
            "\n  {}\n  {}\n",
            style.dim("Clone anything you were granted, commit on a branch, then:"),
            style.cyan("choir propose")
        ),
    }
}

/// Clones each repository `grants` names into the current directory, one
/// report row per repository, and returns the folders that hold a copy.
///
/// Runs after the token and the credential helper are written, so the
/// clone authenticates the way every later `git pull` and `git push` in
/// that folder will: through the helper, from a URL that carries no
/// credential. A folder already there that pulls from the same URL is the
/// copy, which is the ordinary case on a second run; any other folder by
/// that name is somebody's work and is left as it was. A clone that fails
/// is reported with the line that finishes it rather than failing the
/// join, because the account already exists, and the same link run again
/// tries only what is still missing.
fn clone_granted(api: &str, grants: &[&str], style: choir_cli::style::Style) -> Vec<String> {
    let label = style.dim("copy    ");
    let mut copies = Vec::new();
    for (repo, folder) in choir_cli::join::repositories(grants) {
        let url = format!("{}/{repo}.git", api.trim_end_matches('/'));
        if std::path::Path::new(folder).exists() {
            let origin = std::process::Command::new("git")
                .args(["-C", folder, "remote", "get-url", "origin"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
            if origin.as_deref() == Some(url.as_str()) {
                println!(
                    "  {label}  ./{folder} {}",
                    style.dim("is already your copy")
                );
                copies.push(folder.to_string());
            } else {
                println!(
                    "  {label}  ./{folder} {}",
                    style.dim("is already here, so it was left as it was")
                );
            }
            continue;
        }
        // Never a prompt: the helper answers, and a question for a
        // password would ask the reader for something they were never
        // shown. `--` because the folder is the node's word, not ours.
        let cloned = std::process::Command::new("git")
            .args(["clone", "-q", "--", &url, folder])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output();
        match cloned {
            Ok(out) if out.status.success() => {
                println!("  {label}  ./{folder} {}", style.dim(&format!("({repo})")));
                copies.push(folder.to_string());
            }
            outcome => {
                let why = match &outcome {
                    Ok(out) => String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .rfind(|line| !line.trim().is_empty())
                        .unwrap_or("git gave no reason")
                        .trim()
                        .to_string(),
                    Err(error) => format!("git did not run: {error}"),
                };
                println!("  {label}  {repo} was not cloned: {why}");
                println!(
                    "  {:8}  {}",
                    "",
                    style.cyan(&format!("to try again: git clone {url}"))
                );
            }
        }
    }
    copies
}

/// Points git at the token for one node, and returns the file it wrote.
///
/// Scoped to the node's origin with `credential.<origin>.helper` rather
/// than set as the bare `credential.helper`, so this cannot answer for
/// GitHub or for anybody else's server: a helper configured unscoped is
/// asked about every host git ever talks to.
///
/// It goes in `~/.gitconfig` because that is the only config a clone
/// that does not exist yet will read, and the whole point is that the
/// next `git clone` works. `None` when git could not be run or refused,
/// which is reported rather than fatal — the join itself succeeded, and
/// the line to run by hand is printed instead.
fn configure_git_credential(api: &str, auth_file: &str) -> Option<String> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "choir".to_string());
    let origin = api.trim_end_matches('/');
    let out = std::process::Command::new("git")
        .args([
            "config",
            "--global",
            &format!("credential.{origin}.helper"),
            &format!("!{exe} git-credential {auth_file}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args([
            "config",
            "--global",
            "--list",
            "--show-origin",
            "--name-only",
        ])
        .output()
        .ok()?;
    // `--show-origin` prints `file:<path>\t<name>`; the path is the same
    // for every line, so the first will do. Asking git rather than
    // spelling `~/.gitconfig` here is what keeps this honest on a
    // machine using `$XDG_CONFIG_HOME/git/config`.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .and_then(|origin| origin.strip_prefix("file:"))
        .map(str::to_string)
}

/// Records the node, channel and key in `~/.choir/config`, and the invite
/// redeemed with the repositories it named.
///
/// The fallback the `.choir/config` walk reaches when no directory above
/// the working one names a node — see [`configured`]. Existing keys are
/// preserved rather than the file being rewritten, because `choir init`
/// writes this same file when it is run from `$HOME`.
///
/// The invite is its id and never its secret: enough to tell the same
/// link run again from a second invite, and nothing anybody can redeem.
/// The repositories are what a run again clones when a copy is missing.
///
/// `None` when it could not be written, which is reported rather than
/// fatal for the same reason [`configure_git_credential`]'s failure is.
fn write_home_config(
    api: &str,
    channel: &str,
    key_file: &str,
    invite: &str,
    grants: &[&str],
) -> Option<String> {
    let path = state_dir().join("config");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = String::new();
    let repos: Vec<&str> = choir_cli::join::repositories(grants)
        .into_iter()
        .map(|(repo, _)| repo)
        .collect();
    let repos = repos.join(" ");
    let written = [
        ("node", api.trim_end_matches('/')),
        ("channel", channel),
        ("key", key_file),
        ("invite", invite),
        ("repos", repos.as_str()),
    ];
    for line in existing.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim()).unwrap_or("");
        if !written.iter().any(|(name, _)| *name == key) {
            out.push_str(line);
            out.push('\n');
        }
    }
    for (name, value) in written {
        out.push_str(&format!("{name} = {value}\n"));
    }
    choir_fs::write_atomic_private(&path, out).ok()?;
    Some(path.display().to_string())
}

/// `choir git-credential <auth-file> [--auth-user <name>] <operation>`
///
/// A git credential helper, so a token reaches git over stdin instead of
/// living in a remote URL.
///
/// A URL-embedded credential is written into `.git/config`, echoed by
/// `git remote -v`, and copied into every shell history and bug report
/// that quotes a clone line. Git's helper protocol exists to avoid
/// exactly that: git runs the helper as a subprocess, writes the request
/// as `key=value` lines on stdin, and reads the answer the same way.
///
/// Configure it once per checkout:
///
/// ```text
/// git config credential.helper '!choir git-credential ~/.choir/auth'
/// ```
///
/// `store` and `erase` are accepted and do nothing, deliberately. The
/// auth file is written by `choir join` and owned by the contributor;
/// a helper that honoured `erase` would let a routine authentication
/// failure delete the credential the operator issued once.
fn git_credential(auth_file: &str, user: Option<&str>, operation: &str) -> ! {
    match operation {
        // Git ignores unknown operations from a helper, and so does
        // this: answering a `store` with a credential would be a helper
        // volunteering one nobody asked for.
        "store" | "erase" => std::process::exit(0),
        "get" => {}
        _ => {
            eprintln!("choir git-credential: unknown operation `{operation}`");
            std::process::exit(2);
        }
    }
    // Git's request arrives on stdin and is *not* echoed back: replying
    // with a host or path git did not ask about is how a helper hands a
    // credential to the wrong server. Only the two fields git wants are
    // printed, and git matches them against the request itself.
    let mut request = String::new();
    use std::io::Read;
    if std::io::stdin().read_to_string(&mut request).is_err() {
        std::process::exit(1);
    }
    let (user, token) = match choir_cli::mcp::credential_pair(std::path::Path::new(auth_file), user)
    {
        Ok(pair) => pair,
        Err(error) => {
            // Exit 0 with no output: git reads that as "this helper
            // has nothing", and falls through to the next one or to
            // prompting. Exiting nonzero would abort the whole
            // operation over a helper that simply does not apply.
            eprintln!("choir git-credential: {error}");
            std::process::exit(0);
        }
    };
    println!("username={user}");
    println!("password={token}");
    std::process::exit(0);
}

/// Runs `git` in `dir` and returns its trimmed stdout.
///
/// Failure carries git's own stderr rather than a paraphrase. Every
/// error this can hit -- not a repository, no such remote, no upstream
/// -- already has a message git words better than a wrapper would, and
/// the contributor is going to fix it with git.
fn git_capture(dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs `git` in `dir` for effect, streaming its output to this
/// process's own.
fn git_run(dir: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {} failed", args.join(" ")))
    }
}

/// Aborts a proposal, naming the step that failed.
///
/// Every abort here is mid-sequence by construction, so it says which
/// step stopped and leaves the contributor's checkout untouched. A
/// proposal is resumable precisely because its identifiers are derived
/// rather than minted: running the same command again re-reaches the
/// same change instead of forking a second one.
fn propose_abort(step: &str, detail: &str) -> ! {
    eprintln!("choir propose: {step}: {detail}");
    // Every refusal in this command ends with a line the reader can act
    // on. Some details carry their own -- the ones that know which flag
    // is missing -- and the rest get the step-shaped one, which is the
    // most specific true thing left to say. `choir doctor` is the
    // fallback rather than a shrug: it is the command that tells apart
    // "the node is down" from "git is not installed", and those are what
    // the remaining steps fail on.
    if !detail.contains("next:") {
        let next = match step {
            "checkout" => "run this from inside a git checkout",
            "identity" => "choir join '<link>'",
            "remote" | "base" => {
                "git fetch, then re-run; or name it: choir propose --api <url> --repo <owner/repo>"
            }
            "push" => "check the push error above; the credential comes from ~/.choir/auth",
            _ => "choir doctor",
        };
        eprintln!("next: {next}");
    }
    std::process::exit(1);
}

/// Flags of `choir propose`, after parsing.
struct ProposeOptions<'a> {
    api: Option<&'a str>,
    repo: Option<&'a str>,
    remote: &'a str,
    onto: Option<&'a str>,
    change: Option<&'a str>,
    key_file: Option<&'a str>,
    channel: Option<&'a str>,
    cone: Vec<String>,
    reviewers: Vec<String>,
}

/// Splits off the deprecated `<key-file> <channel>` prefix, if this
/// invocation carries one.
///
/// The zero-argument form and the positional form cannot be told apart
/// by counting, because a bare `choir propose alice bea` names two
/// reviewers. They *can* be told apart by what the first argument is: a
/// key file is a file that exists, and a reviewer is a channel name.
/// Requiring the file to exist rather than merely to look like a path is
/// deliberate — a typo'd key path then reads as a reviewer and is
/// refused by the node by name, instead of being opened and minting a
/// key nobody registered.
fn positional_propose<'a>(rest: &'a [&'a str]) -> Option<(&'a str, &'a str, &'a [&'a str])> {
    let [key_file, channel, tail @ ..] = rest else {
        return None;
    };
    let positional = !key_file.starts_with("--")
        && !channel.starts_with("--")
        && std::path::Path::new(key_file).is_file();
    positional.then_some((key_file, channel, tail))
}

fn parse_propose<'a>(rest: &[&'a str]) -> ProposeOptions<'a> {
    let (mut api, mut repo, mut onto, mut change) = (None, None, None, None);
    let (mut key_file, mut channel) = (None, None);
    let mut remote = "origin";
    let (mut cone, mut reviewers) = (Vec::new(), Vec::new());
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index];
        if !flag.starts_with("--") {
            reviewers.push(flag.to_string());
            index += 1;
            continue;
        }
        let Some(value) = rest.get(index + 1).copied() else {
            usage();
        };
        match flag {
            "--api" if api.is_none() => api = Some(value),
            "--repo" if repo.is_none() => repo = Some(value),
            "--remote" => remote = value,
            "--onto" if onto.is_none() => onto = Some(value),
            "--change" if change.is_none() => change = Some(value),
            "--path" => cone.push(value.to_string()),
            // The two the command used to take positionally. Both are
            // inferred when absent; these override the inference for a
            // machine holding more than one identity.
            "--key-file" if key_file.is_none() => key_file = Some(value),
            "--channel" if channel.is_none() => channel = Some(value),
            _ => usage(),
        }
        index += 2;
    }
    // Same canonical ordering `choir workspace` applies: the node
    // rebuilds these bytes to verify the owner signature, so two
    // clients naming the same subtrees in a different order must sign
    // identical authorizations.
    cone.sort();
    cone.dedup();
    ProposeOptions {
        api,
        repo,
        remote,
        onto,
        change,
        key_file,
        channel,
        cone,
        reviewers,
    }
}

/// `choir propose [flags] [reviewer]...`
///
/// The five-step path -- provision, commit, push, checkpoint, request
/// review -- as one command, run from the contributor's own checkout.
/// Nothing here is a new endpoint; the command is the inference that
/// supplies each step's identifiers from what the checkout already
/// knows, plus the ordering between them.
///
/// Unlike every other subcommand, the API base is a flag rather than the
/// first positional. That is the point of the command: the git remote
/// already names the node, and asking a newcomer to repeat it is the
/// friction being removed. `--api` and `--repo` override the inference
/// for a checkout whose remote is an `ssh://` URL or a proxy.
///
/// The sequence stops at the first refusal and says which step stopped.
/// It is safe to re-run: the change, workspace and idempotency key are
/// derived from the branch name, so a second run resumes the same
/// proposal rather than opening a second one.
fn propose(rest: &[&str], auth: AuthOptions<'_>) -> ! {
    let positional = positional_propose(rest);
    let options = parse_propose(positional.map_or(rest, |(_, _, tail)| tail));
    // Most explicit first: the two positionals the command used to take,
    // then their flags, then what `choir join` left behind. Nothing is
    // guessed -- an identity that cannot be found is refused with the
    // command that creates one, because signing as the wrong actor is
    // worse than not signing.
    let key_file = positional
        .map(|(key_file, _, _)| key_file.to_string())
        .or_else(|| options.key_file.map(str::to_string))
        .or_else(|| configured("key"))
        .unwrap_or_else(|| state_dir().join("agent.key").display().to_string());
    if !std::path::Path::new(&key_file).is_file() {
        propose_abort(
            "identity",
            &format!(
                "no key at {key_file}\nnext: choir join '<link>'   \
                 (or name one: choir propose --key-file <path>)"
            ),
        );
    }
    // The channel, in the same order. The auth file's user is the last
    // resort rather than the first because an account's channel is not
    // always its user name -- `--channel` at join time makes them
    // differ, and `choir join` writes the answer down for exactly this.
    let channel = positional
        .map(|(_, channel, _)| channel.to_string())
        .or_else(|| options.channel.map(str::to_string))
        .or_else(|| configured("channel"))
        .or_else(|| {
            let path = discovered_auth_file()?;
            choir_cli::mcp::credential_pair(std::path::Path::new(&path), auth.user)
                .ok()
                .map(|(user, _)| user)
        })
        .unwrap_or_else(|| {
            propose_abort(
                "identity",
                "nothing here says which channel to sign as\nnext: choir propose --channel <name>",
            )
        });
    let (key_file, channel) = (key_file.as_str(), channel.as_str());
    let cwd = std::env::current_dir().unwrap_or_else(|e| propose_abort("checkout", &e.to_string()));
    let top = match git_capture(&cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => std::path::PathBuf::from(top),
        Err(error) => propose_abort("checkout", &error),
    };

    // 1. Where to send it. An explicit --api wins; otherwise the remote
    //    URL carries both the node and the repository.
    let remote_url = git_capture(&top, &["remote", "get-url", options.remote]);
    let inferred = match (&options.api, &options.repo, &remote_url) {
        // Both named explicitly: the remote need not even exist, which
        // is what makes this work from a checkout cloned from elsewhere.
        (Some(api), Some(repo), _) => choir_cli::propose::Remote {
            api: (*api).to_string(),
            repo: (*repo).to_string(),
        },
        (_, _, Ok(url)) => match choir_cli::propose::Remote::parse(url) {
            Ok(remote) => remote,
            Err(failure) => propose_abort("remote", &failure.message),
        },
        (_, _, Err(error)) => propose_abort("remote", error),
    };
    let api = options.api.map_or(inferred.api, str::to_string);
    let repo = options.repo.map_or(inferred.repo, str::to_string);

    // 2. What is being proposed, and onto what. The branch name is the
    //    change identity, so an amend or a rebase reaches the same
    //    change rather than forking a second one.
    let branch =
        git_capture(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap_or_default();
    let onto = options.onto.map_or_else(
        || {
            // The remote's own default branch when the clone recorded
            // one, and `main` only as the last resort. Guessing first
            // would silently propose onto the wrong branch on a
            // repository whose default is `master` or `trunk`.
            git_capture(
                &top,
                &[
                    "symbolic-ref",
                    "--short",
                    &format!("refs/remotes/{}/HEAD", options.remote),
                ],
            )
            .ok()
            .and_then(|head| head.rsplit('/').next().map(str::to_string))
            .unwrap_or_else(|| "main".to_string())
        },
        str::to_string,
    );
    let proposal = match choir_cli::propose::Proposal::derive(&repo, &branch, &onto) {
        Ok(proposal) => proposal,
        Err(failure) => propose_abort("branch", &failure.message),
    };
    let change_id = options
        .change
        .map_or(proposal.identity.change_id.clone(), str::to_string);
    let head = match git_capture(&top, &["rev-parse", "HEAD"]) {
        Ok(head) => head,
        Err(error) => propose_abort("checkout", &error),
    };

    // 3. Does this change already exist? Reading first is what makes a
    //    re-run resume. Provisioning again would be refused for a
    //    rebased proposal, whose merge base has moved since the change
    //    was bound to the older one.
    let (status, body) = http(&api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        propose_abort("view", &format!("GET /api/view returned {status}: {body}"));
    }
    let view: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| propose_abort("view", &format!("response is not JSON: {e}")));
    // Read before writing anything: whether a review is already open
    // decides step 5, and asking after the checkpoint would race a
    // reviewer's verdict landing in between.
    let review_open = view["reviews"]
        .get(&change_id)
        .is_some_and(|review| !review.is_null() && review["status"] != "archived");
    let existing = view["changes"]
        .get(&change_id)
        .filter(|state| !state.is_null());
    let workspace_id = match existing {
        Some(state) => {
            let workspace = state["active_workspace"]
                .as_str()
                .unwrap_or_else(|| {
                    propose_abort(
                        "change",
                        "this change's workspace has been archived; propose from a new branch",
                    )
                })
                .to_string();
            eprintln!(
                "choir propose: updating change {}",
                choir_cli::propose::short_change_id(&change_id)
            );
            workspace
        }
        None => {
            // The fork point, not the local branch tip: the node can only
            // bind a base its bare repository already has, and the merge
            // base is the newest commit both sides are known to share.
            let base = git_capture(
                &top,
                &["merge-base", "HEAD", &format!("{}/{onto}", options.remote)],
            )
            .unwrap_or_else(|error| propose_abort(
                "base",
                &format!("{error}\ncannot find where this branch left {}/{onto}; fetch first, or pass --onto", options.remote),
            ));
            let workspace_name = proposal.identity.workspace_name.clone();
            let mut body = serde_json::json!({
                "repo": repo,
                "name": workspace_name,
                "base": base,
                "owner": channel,
                "change": change_id,
                "idempotency_key": proposal.identity.idempotency_key,
            });
            let Some(base_revision) = choir_hash::ContentHash::from_git_oid(&base) else {
                propose_abort("base", "merge-base did not return a git object id");
            };
            let authorization = CreateAuthorization::new(
                change_id.clone(),
                channel.into(),
                format!("{repo}/{workspace_name}"),
                base_revision,
                proposal.identity.idempotency_key.clone(),
            )
            .with_cone(options.cone.clone());
            let signed = signed_payload_body(key_file, channel, &authorization.to_payload());
            for field in ["channel", "payload_hex", "key_id", "signature_hex"] {
                body[field] = signed[field].clone();
            }
            let (status, response) = http(&api, auth, "choir_workspace", body);
            if !(200..300).contains(&status) {
                propose_abort("create change", &response);
            }
            eprintln!(
                "choir propose: created change {} on {base}",
                choir_cli::propose::short_change_id(&change_id)
            );
            format!("{repo}/{workspace_name}")
        }
    };

    // 4. Send the objects. A checkpoint records identity and a CAS; it
    //    does not transfer objects, so a checkpoint of a commit the node
    //    does not have would name a revision nothing can check out.
    //    The ref is named after the commit, so this adds one and never
    //    moves one -- see `Proposal::revision_ref`.
    let revision_ref = proposal.revision_ref(&head);
    let refspec = format!("HEAD:{revision_ref}");
    if let Err(error) = git_run(&top, &["push", options.remote, &refspec]) {
        propose_abort("push", &error);
    }

    // 5. Publish the revision, then ask for review. In that order: a
    //    review names a commit, and a reviewer drawn onto a revision the
    //    change has not published yet is being asked about work the node
    //    cannot show them.
    let Some(revision) = choir_hash::ContentHash::from_git_oid(&head) else {
        propose_abort("checkpoint", "HEAD is not a git object id");
    };
    let prev_revision = current_change_revision(&api, auth, &change_id);
    if prev_revision != revision {
        let op = ViewOp::new(OpKind::CheckpointChange {
            id: change_id.clone(),
            workspace: workspace_id.clone(),
            revision: revision.clone(),
            prev_revision,
        });
        let signed = signed_body(&api, key_file, channel, &op, auth);
        let (status, response) = http(&api, auth, "choir_submit", signed);
        if !(200..300).contains(&status) {
            propose_abort("checkpoint", &response);
        }
    }

    // A review is one long-lived object per change, not one per
    // revision: re-posting a verdict *is* the re-review flow, so asking
    // again would be refused and, if it were not, would discard the
    // discussion and the verdicts already posted. What advances instead
    // is the change's revision, which is where a reviewer reads the
    // current commit from.
    if !review_open {
        let op = ViewOp::new(OpKind::RequestReview {
            id: change_id.clone(),
            target: revision,
            reviewers: options.reviewers.clone(),
            target_ref: Some(proposal.review_target(&repo)),
        });
        let signed = signed_body(&api, key_file, channel, &op, auth);
        let (status, response) = http(&api, auth, "choir_submit", signed);
        if !(200..300).contains(&status) {
            propose_abort("request review", &response);
        }
    }

    let summary = serde_json::json!({
        "change": change_id,
        "workspace": workspace_id,
        "commit": head,
        "pushed_ref": revision_ref,
        "fetch": format!("git fetch {} {revision_ref}", options.remote),
        "target_ref": proposal.review_target(&repo),
        "reviewers": if options.reviewers.is_empty() {
            serde_json::json!("drawn by the node")
        } else {
            serde_json::json!(options.reviewers)
        },
        "review": if review_open {
            // Said plainly because the review's own `target` still names
            // the commit it was opened on. The change's `revision_id` is
            // the live pointer, and this is the sentence that stops a
            // reviewer reading the superseded one.
            "already open; it now needs re-review against this revision"
        } else {
            "opened"
        },
        "next": format!("choir state {api} {channel}"),
    });
    finish(
        200,
        &serde_json::to_string_pretty(&summary).expect("summary is serializable"),
    );
}

/// Prints the checks on `subject` and exits with the trichotomy.
///
/// The only command here that has a third answer, and it is the reason
/// the third answer exists: a caller asking "may I land this" gets three
/// materially different instructions back, and two exit codes cannot
/// carry three instructions. `0` land it, `1` do not, `3` not yet.
///
/// **Nothing reported exits `1`, not `3`.** `3` means a check said it
/// was running, which is a promise that an outcome is coming. No check
/// at all is not that promise -- there may be no runner configured, and
/// a caller that waited would wait forever. Both `1` cases print a
/// distinguishing `verdict`, so a human is never left guessing which of
/// the two they hit.
fn check_exit(subject: &ContentHash, body: &str) -> ! {
    let view: serde_json::Value = match serde_json::from_str(body) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("choir checks: the node's view did not parse: {error}");
            std::process::exit(1);
        }
    };
    let prefix = format!("{}:", subject.to_hex());
    let rows: serde_json::Map<String, serde_json::Value> = view
        .get("checks")
        .and_then(serde_json::Value::as_object)
        .map(|checks| {
            checks
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(key, value)| (key[prefix.len()..].to_string(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let status_of = |value: &serde_json::Value| {
        value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(str::to_lowercase)
    };
    // Ranked the way `View::checks_verdict` ranks, and for its reasons:
    // failed, then errored, then running. Each rank has its own exit
    // code because each implies a different next command -- 1 fix it,
    // 4 re-run it, 3 wait -- and a script that only asks whether the
    // code is zero is unaffected by the new one.
    let (verdict, code) = if rows.is_empty() {
        ("unreported", 1)
    } else if rows
        .values()
        .any(|v| status_of(v).as_deref() == Some("failed"))
    {
        ("failed", 1)
    } else if rows
        .values()
        .any(|v| status_of(v).as_deref() == Some("errored"))
    {
        ("errored", 4)
    } else if rows
        .values()
        .any(|v| status_of(v).as_deref() == Some("running"))
    {
        ("running", 3)
    } else {
        ("passed", 0)
    };
    println!(
        "{}",
        serde_json::json!({
            "subject": subject.to_hex(),
            "verdict": verdict,
            "checks": rows,
        })
    );
    std::process::exit(code);
}

/// Prints the response body and exits nonzero unless the status is 2xx.
fn finish(status: u16, body: &str) -> ! {
    println!("{body}");
    // The body stays exactly what the node said, on stdout, for whatever
    // is parsing it. The repair is repeated on stderr because `next` is
    // the fourth field of a JSON object and a reader who is already
    // stuck does not read that far.
    if !(200..300).contains(&status) {
        if let Some(next) = refusal_next(body) {
            eprintln!("\nnext: {next}");
        }
    }
    std::process::exit(if (200..300).contains(&status) { 0 } else { 1 });
}

/// The binding `/api/view` currently reports for `actor_id`, if any.
///
/// Best-effort by design: any failure to read returns `None` so the bind
/// still goes out. Refusing to submit because a *read* failed would turn
/// a reporting problem into an availability problem, and the node is the
/// authority on whether a binding is admissible regardless of what this
/// saw.
fn current_binding(api: &str, auth: AuthOptions<'_>, actor_id: &str) -> Option<serde_json::Value> {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        return None;
    }
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    let binding = view.get("bindings")?.get(actor_id)?;
    (!binding.is_null()).then(|| binding.clone())
}

/// The log identity to sign a scope against: `(node, head)` from
/// `/api/view`.
///
/// Unlike [`current_binding`], a failed read here is fatal. The two are
/// different kinds of read: that one reports on a decision the node will
/// make anyway, while this one is *part of what gets signed*. Guessing a
/// scope, or quietly signing without one, would produce a signature that
/// is either refused or — worse, on a node that does not require scopes —
/// admissible forever and everywhere.
/// The id of the ref-state attestation this node is currently serving,
/// read from `/api/view` (D67).
///
/// Read rather than accepted as an argument, and fatal on failure, for
/// the same reason as [`log_scope`]: this value is *what gets signed*.
/// A witness that pasted a stale id would be attesting a ref-state that
/// is no longer current, which is the one thing a witness must never do
/// by accident — and the node would refuse it, so the only outcome of
/// allowing it is a confusing error instead of a correct signature.
fn latest_snapshot(api: &str, auth: AuthOptions<'_>) -> ContentHash {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    let fail = |why: &str| -> ! {
        eprintln!("choir: cannot read the ref-state attestation from {api}: {why}");
        eprintln!("choir: not signing a witness statement about a snapshot nobody read.");
        std::process::exit(1);
    };
    if !(200..300).contains(&status) {
        fail(&format!("GET /api/view returned {status}"));
    }
    let view: serde_json::Value = match serde_json::from_str(&body) {
        Ok(view) => view,
        Err(error) => fail(&format!("response is not JSON: {error}")),
    };
    match view["snapshot"]["id"].as_str().and_then(hash_from_hex) {
        Some(id) => id,
        // Two different absences, one message: a node that has taken no
        // snapshot yet, and a reader whose grants hide the section. Both
        // mean the same thing to a witness -- there is nothing here to
        // attest -- and distinguishing them would disclose the section
        // to somebody the ACL just withheld it from.
        None => fail(
            "no `snapshot.id` in the view: either the node has attested no ref-state yet, \
             or this credential may not read node-wide sections (`@node auditor`)",
        ),
    }
}

fn log_scope(api: &str, auth: AuthOptions<'_>) -> (ContentHash, Option<ContentHash>) {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    let fail = |why: &str| -> ! {
        eprintln!("choir: cannot read the log scope from {api}: {why}");
        eprintln!("choir: not signing an op that names no log. Retry when the node answers.");
        std::process::exit(1);
    };
    if !(200..300).contains(&status) {
        fail(&format!("GET /api/view returned {status}"));
    }
    let view: serde_json::Value = match serde_json::from_str(&body) {
        Ok(view) => view,
        Err(error) => fail(&format!("response is not JSON: {error}")),
    };
    let Some(node) = view["log"]["node"].as_str().and_then(hash_from_hex) else {
        fail("response has no `log.node`; this node predates op scopes");
    };
    let head = match view["log"]["head"].as_str() {
        Some(hex) => match hash_from_hex(hex) {
            Some(head) => Some(head),
            None => fail("`log.head` is not a content hash"),
        },
        // A null head is an empty log, which is a real state and the one
        // a genesis op is signed against.
        None => None,
    };
    (node, head)
}

/// Parses a `<codec>-<digest>` content hash as served by the node.
fn hash_from_hex(hex: &str) -> Option<ContentHash> {
    let (codec, digest) = hex.split_once('-')?;
    let codec = u8::from_str_radix(codec, 16).ok()?;
    let digest: Option<Vec<u8>> = (0..digest.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(digest.get(i..i + 2)?, 16).ok())
        .collect();
    Some(ContentHash {
        codec,
        digest: digest?,
    })
}

/// Signs `op` on attribution channel `channel` and builds its submission body.
///
/// The op is scoped to the node's current head first, so the signature
/// is admissible on this log once and nowhere else.
fn signed_body(
    api: &str,
    key_file: &str,
    channel: &str,
    op: &ViewOp,
    auth: AuthOptions<'_>,
) -> serde_json::Value {
    let (node, head) = log_scope(api, auth);
    let op = op.clone().in_scope(node, head);
    signed_payload_body(key_file, channel, &op.to_payload())
}

/// Signs raw `payload` bytes on `channel`. Owner authorizations travel
/// inside a request body rather than the op log, so unlike [`signed_body`]
/// this carries no log scope.
fn signed_payload_body(key_file: &str, channel: &str, payload: &[u8]) -> serde_json::Value {
    let key = load_key(key_file);
    let sig = key.sign_submission(channel, payload);
    serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
}

fn submit(api: &str, key_file: &str, channel: &str, op: &ViewOp, auth: AuthOptions<'_>) -> ! {
    let body = signed_body(api, key_file, channel, op, auth);
    let (status, resp) = http(api, auth, "choir_submit", body);
    finish(status, &resp);
}

/// The trusted keys this client holds, in the operator's own file
/// format: one key per line, hex, with an optional channel name before
/// it.
///
/// Reusing that spelling means the file an operator already keeps is the
/// file a verifying client already has, rather than a second format that
/// can disagree with the first.
fn load_registry(path: &str) -> Registry {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("choir log: {path}: {error}");
            std::process::exit(2);
        }
    };
    let mut registry = Registry::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // `<name> <hex>` or bare `<hex>`: the name is admission's
        // business and this command only needs the key material.
        let hex = line.split_whitespace().last().unwrap_or_default();
        let Some(bytes) = hex_decode(hex).and_then(|b| <[u8; 32]>::try_from(b).ok()) else {
            eprintln!("choir log: {path}:{}: not 64 hex characters", number + 1);
            std::process::exit(2);
        };
        if registry.register(&bytes).is_err() {
            eprintln!("choir log: {path}:{}: not a valid ed25519 key", number + 1);
            std::process::exit(2);
        }
    }
    registry
}

/// Reads log entries from a cursor and, with `--verify`, checks them
/// the way `SYNC.md` says a client should (D17).
///
/// `/api/log` was the last agent-facing endpoint with no command, and it
/// is the one where that cost most: the repository ships a 177-line
/// contract telling clients how to establish that the pages they were
/// handed really are the chain — continuity, hash recomputation,
/// authorship — and every step of it was prose. An agent following it
/// hand-rolled hash-chain and ed25519 checking, and the doc has to warn
/// about the subtleties it gets wrong.
///
/// **What `--verify` establishes, and what it does not.** Continuity and
/// recomputation need nothing but the page: they are fully independent
/// of the node. Authorship needs the public key, which this command only
/// has for actors named in `--keys`; an entry whose key it does not hold
/// is reported as **unverified**, never as verified. Saying "checked"
/// for a signature nobody could check is the one failure that would make
/// this worse than no command at all.
///
/// A passkey entry carries its own credential key (D45), so it needs
/// nothing from `--keys` — and is counted separately for the same
/// reason: the key came with the signature, so the bytes are proven
/// intact and nothing proves the credential was that channel's.
///
/// **This is a first-party client and says so.** It decodes into the
/// same `OpEntry` the node encodes from, so a hash agreeing here proves
/// the node agrees with *this build's* definition of the format rather
/// than with an independent reading of `SYNC.md`.
/// `choir-node/tests/it/sync_contract.rs` is the independent one: it
/// rebuilds the canonical bytes by hand and deliberately never calls
/// `content_hash`.
/// The revocation positions `/api/view` reports, keyed by actor id.
///
/// An empty map on any failure, including a node that cannot be reached
/// or serves no bindings. That is the honest default: it verifies fewer
/// claims rather than more, and the alternative — treating an
/// unanswerable question as "revoked" — would report a sound log as
/// broken.
fn revocations(api: &str, auth: AuthOptions<'_>) -> choir_identity::sync::Revocations {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        eprintln!("choir log: cannot read bindings ({status}); revocations not checked");
        return choir_identity::sync::Revocations::new();
    }
    let view: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    view["bindings"]
        .as_object()
        .map(|bindings| {
            bindings
                .iter()
                .filter_map(|(key_id, binding)| {
                    Some((key_id.clone(), binding["revoked"]["at"].as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn log(api: &str, from: u64, verify: bool, keys: Option<&str>, auth: AuthOptions<'_>) -> ! {
    let (status, body) = http(api, auth, "choir_log", serde_json::json!({ "from": from }));
    if !(200..300).contains(&status) {
        // A 409 is the gap rule in `SYNC.md`: this node cannot reach
        // back that far. Pass its own words through rather than
        // paraphrasing a refusal that names the window it does have.
        println!("{body}");
        std::process::exit(1);
    }
    let page: serde_json::Value = match serde_json::from_str(&body) {
        Ok(page) => page,
        Err(error) => {
            eprintln!("choir log: response is not JSON: {error}");
            std::process::exit(1);
        }
    };
    let entries = page["entries"].as_array().cloned().unwrap_or_default();
    for entry in &entries {
        println!("{entry}");
    }
    if !verify {
        eprintln!("choir log: {} entries, not verified", entries.len());
        std::process::exit(0);
    }

    let registry = keys.map(load_registry).unwrap_or_default();
    // A second request, and a deliberate one. Revocations decide whether
    // a good signature was still authorized at the position it sits at
    // (D44), and they are not on the log page -- the `RevokeKey` that
    // matters may be outside the window. Asking the node costs a round
    // trip and does not cost trust: a node that hides a revocation only
    // makes its own log verify, while one that invents one is caught by
    // the entry it points at.
    let revoked = revocations(api, auth);
    let report = choir_identity::sync::page(&entries, &registry, &revoked);
    for note in &report.notes {
        eprintln!("choir log: {note}");
    }
    for failure in &report.failures {
        eprintln!("choir log: {failure}");
    }
    // The passkey count is named separately rather than folded into
    // "verified" (D45). It is a real result — the bytes are intact and
    // were signed by the credential named — and it is not the same
    // result: no key set vouches for a credential the entry carries
    // itself. One number covering both would report the weaker claim in
    // the stronger word, on every line, forever.
    let passkeys = if report.integrity_only == 0 {
        String::new()
    } else {
        format!(
            ", {} passkey signatures intact but unanchored",
            report.integrity_only
        )
    };
    eprintln!(
        "choir log: {} entries, chain {}, {} signatures verified, {} unverified{passkeys}",
        entries.len(),
        if report.failures.is_empty() {
            "holds"
        } else {
            "BROKEN"
        },
        report.checked,
        report.unverified
    );
    std::process::exit(i32::from(!report.failures.is_empty()));
}

/// Signs every op in `source` on one channel and submits them as one
/// batch (D17).
///
/// The node has told agents since D26 that `/api/submit-batch` is the
/// primary path for their workloads — one durability barrier per batch
/// against one per operation — and until now the CLI could not reach it.
/// An agent taking that advice had to hand-roll ed25519 signing and
/// `curl`, which is the thing this binary exists to prevent.
///
/// **The log scope is read once, not once per op.** `submit` reads it
/// per call because it sends one op; doing that here would put an HTTP
/// round trip in front of every operation and spend exactly what the
/// batch endpoint saves. One read is also correct rather than merely
/// cheaper: admission checks that the head an op names is still *in the
/// window*, not that it is the current head, so ops signed against one
/// head are admissible in sequence behind each other.
///
/// **Output is one line per op, in request order**, so a script can read
/// line *n* for op *n* without counting brackets — and the accepted and
/// rejected totals go to stderr, following the rule the runner already
/// documents: machine-facing on stdout, human-facing on stderr.
fn batch(api: &str, key_file: &str, channel: &str, source: &str, auth: AuthOptions<'_>) -> ! {
    let text = if source == "-" {
        let mut buffer = String::new();
        if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer) {
            eprintln!("choir batch: cannot read stdin: {error}");
            std::process::exit(2);
        }
        buffer
    } else {
        match std::fs::read_to_string(source) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("choir batch: {source}: {error}");
                std::process::exit(2);
            }
        }
    };

    // One op per line. Blank lines are skipped so a generated file may
    // end with a newline, or be built by appending, without the last
    // entry being a parse error nobody can see.
    let mut ops: Vec<ViewOp> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ViewOp>(line) {
            Ok(op) => ops.push(op),
            Err(error) => {
                // Named by line, because the whole point of a batch is
                // that there are many and "bad op json" would not say
                // which.
                eprintln!("choir batch: {source}:{}: {error}", number + 1);
                std::process::exit(2);
            }
        }
    }
    if ops.is_empty() {
        eprintln!("choir batch: {source} contains no operations");
        std::process::exit(2);
    }

    let (node, head) = log_scope(api, auth);
    let signed: Vec<serde_json::Value> = ops
        .into_iter()
        .map(|op| {
            let op = op.in_scope(node.clone(), head.clone());
            signed_payload_body(key_file, channel, &op.to_payload())
        })
        .collect();
    let count = signed.len();
    let (status, resp) = http(
        api,
        auth,
        "choir_submit_batch",
        serde_json::json!({ "ops": signed }),
    );

    let parsed: serde_json::Value = match serde_json::from_str(&resp) {
        Ok(value) => value,
        Err(_) => {
            // A batch the node refused before reading the array answers
            // in its own words rather than with per-op results. Pass it
            // through whole rather than inventing results for ops that
            // were never considered.
            println!("{resp}");
            std::process::exit(if (200..300).contains(&status) { 0 } else { 1 });
        }
    };
    let Some(results) = parsed["results"].as_array() else {
        println!("{resp}");
        std::process::exit(if (200..300).contains(&status) { 0 } else { 1 });
    };
    for result in results {
        println!("{result}");
    }
    let accepted = parsed["accepted"].as_u64().unwrap_or(0);
    let rejected = parsed["rejected"].as_u64().unwrap_or(0);
    eprintln!("choir batch: {accepted} accepted, {rejected} rejected, {count} submitted");
    // Nonzero when any op was refused, so `set -e` stops. The per-op
    // lines say which, which is the thing an exit code cannot carry.
    std::process::exit(
        if (200..300).contains(&status) && rejected == 0 && results.len() == count {
            0
        } else {
            1
        },
    );
}

/// Emits a runner result and exits, data on stdout and nothing else.
///
/// An orchestrator parses stdout, so a diagnostic written there would be
/// indistinguishable from a result. Every human-facing word goes to
/// stderr and every machine-facing one to stdout, which is the same rule
/// the rest of the machine surface follows.
fn runner_finish(result: &Result<serde_json::Value, choir_cli::runner::Failure>) -> ! {
    match result {
        Ok(value) => {
            println!("{value}");
            std::process::exit(0);
        }
        Err(failure) => {
            println!("{}", failure.to_json());
            eprintln!("choir runner: {}: {}", failure.code, failure.message);
            std::process::exit(1);
        }
    }
}

/// Reads a JSON document, mapping every failure to a typed refusal.
fn runner_json(
    source: &str,
    code: &str,
    what: &str,
) -> Result<serde_json::Value, choir_cli::runner::Failure> {
    serde_json::from_str(source)
        .map_err(|error| choir_cli::runner::Failure::terminal(code, format!("{what}: {error}")))
}

/// One lifecycle step for an orchestrator adapter.
///
/// The adapter supplies its own wire format and its namespace; every
/// decision that is about the lifecycle rather than the orchestrator is
/// made in [`choir_cli::runner`], where it is unit-tested. This function
/// is the I/O around those decisions and deliberately holds none of them.
fn runner(config_file: &str, auth: AuthOptions<'_>) -> ! {
    use choir_cli::runner::{Config, Failure, Operation, Request};

    let outcome = (|| -> Result<serde_json::Value, Failure> {
        let raw = std::fs::read_to_string(config_file).map_err(|error| {
            Failure::terminal("invalid_config", format!("cannot read config: {error}"))
        })?;
        let config = Config::parse(&runner_json(&raw, "invalid_config", "config is not JSON")?)?;

        let mut stdin = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).map_err(|error| {
            Failure::terminal("invalid_request", format!("cannot read stdin: {error}"))
        })?;
        let request = Request::parse(
            &runner_json(&stdin, "invalid_request", "request is not JSON")?,
            &config,
        )?;

        // Configured credentials win over inherited flags: the operator
        // chose them at install time, and a scheduler's environment is
        // not a place to pick up an identity from.
        let auth = AuthOptions {
            file: config.auth_file.as_deref().or(auth.file_for(&config.api)),
            user: config.auth_user.as_deref().or(auth.user),
            explicit: true,
        };
        let id = &request.identity;

        match request.operation {
            Operation::Ensure => {
                let base = match &request.base {
                    Some(base) => base.clone(),
                    None => {
                        let base_ref = config.base_ref.as_ref().ok_or_else(|| {
                            Failure::terminal(
                                "invalid_config",
                                "ensure needs either a request base or a config base_ref",
                            )
                        })?;
                        let (status, body) =
                            http(&config.api, auth, "choir_view", serde_json::json!({}));
                        if !(200..300).contains(&status) {
                            return Err(choir_cli::runner::failure_from_response(
                                &body,
                                "base revision lookup",
                            ));
                        }
                        choir_cli::runner::base_from_view(
                            &runner_json(&body, "invalid_response", "view is not JSON")?,
                            base_ref,
                        )?
                    }
                };
                let flags: Vec<&str> = vec![
                    "--base",
                    &base,
                    "--owner",
                    &config.owner,
                    "--key-file",
                    &config.key_file,
                    "--change",
                    &id.change_id,
                    "--idempotency-key",
                    &id.idempotency_key,
                ];
                let body = workspace_body(&config.repo, &id.workspace_name, &flags);
                let (status, resp) = http(&config.api, auth, "choir_workspace", body);
                if !(200..300).contains(&status) {
                    return Err(choir_cli::runner::failure_from_response(
                        &resp,
                        "workspace creation",
                    ));
                }
                let response = runner_json(&resp, "invalid_response", "creation is not JSON")?;
                choir_cli::runner::verify_binding(&response, id)?;
                Ok(serde_json::json!({
                    "protocol_version": choir_cli::runner::PROTOCOL_VERSION,
                    "operation": "ensure",
                    "workspace": {
                        "path": response.get("path").cloned().unwrap_or(serde_json::Value::Null),
                        "created_now": response.get("created") == Some(&serde_json::json!(true)),
                    },
                    "binding": binding_json(
                        id,
                        &config,
                        Some(&choir_cli::runner::bound_base(&response, &base)),
                    ),
                    "receipt": response.get("operation").cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                }))
            }
            Operation::Checkpoint => {
                let revision = request.base.clone().ok_or_else(|| {
                    Failure::terminal(
                        "invalid_request",
                        "checkpoint needs the exact committed and pushed Git object id as base",
                    )
                })?;
                let Some(revision_hash) = choir_hash::ContentHash::from_git_oid(&revision) else {
                    return Err(Failure::terminal(
                        "invalid_request",
                        "checkpoint base must be a 40- or 64-char hex Git object id",
                    ));
                };
                let prev_revision = current_change_revision(&config.api, auth, &id.change_id);
                let op = ViewOp::new(OpKind::CheckpointChange {
                    id: id.change_id.clone(),
                    workspace: id.workspace_id.clone(),
                    revision: revision_hash,
                    prev_revision,
                });
                let body = signed_body(&config.api, &config.key_file, &config.owner, &op, auth);
                let (status, resp) = http(&config.api, auth, "choir_submit", body);
                if !(200..300).contains(&status) {
                    return Err(choir_cli::runner::failure_from_response(
                        &resp,
                        "revision checkpoint",
                    ));
                }
                Ok(serde_json::json!({
                    "protocol_version": choir_cli::runner::PROTOCOL_VERSION,
                    "operation": "checkpoint",
                    "checkpoint": {
                        "change_id": id.change_id,
                        "workspace_id": id.workspace_id,
                        "revision_id": revision,
                    },
                    "receipt": runner_json(&resp, "invalid_response", "checkpoint is not JSON")
                        .unwrap_or_else(|_| serde_json::json!({})),
                }))
            }
            Operation::Archive => {
                let prev_revision = current_change_revision(&config.api, auth, &id.change_id);
                let authorization = ArchiveAuthorization::new(
                    id.change_id.clone(),
                    id.workspace_id.clone(),
                    prev_revision,
                );
                let mut body = signed_payload_body(
                    &config.key_file,
                    &config.owner,
                    &authorization.to_payload(),
                );
                body["repo"] = serde_json::json!(config.repo);
                body["name"] = serde_json::json!(id.workspace_name);
                body["change"] = serde_json::json!(id.change_id);
                body["idempotency_key"] = serde_json::json!(id.idempotency_key);
                let (status, resp) = http(&config.api, auth, "choir_workspace_archive", body);
                if !(200..300).contains(&status) {
                    return Err(choir_cli::runner::failure_from_response(
                        &resp,
                        "workspace archive",
                    ));
                }
                let response = runner_json(&resp, "invalid_response", "archive is not JSON")?;
                choir_cli::runner::verify_binding(&response, id)?;
                Ok(serde_json::json!({
                    "protocol_version": choir_cli::runner::PROTOCOL_VERSION,
                    "operation": "archive",
                    "archive": {
                        "workspace_id": id.workspace_id,
                        "change_id": id.change_id,
                        "archived_path": response.get("archived_path").cloned()
                            .unwrap_or(serde_json::Value::Null),
                        "already_archived":
                            response.get("already_archived") == Some(&serde_json::json!(true)),
                    },
                    "receipt": response.get("operation").cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                }))
            }
        }
    })();
    runner_finish(&outcome);
}

/// The binding an adapter echoes back so its orchestrator can store it.
fn binding_json(
    id: &choir_cli::runner::Identity,
    config: &choir_cli::runner::Config,
    base: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "repo": config.repo,
        "owner": config.owner,
        "scheme": id.scheme.as_str(),
        "workspace_id": id.workspace_id,
        "workspace_name": id.workspace_name,
        "change_id": id.change_id,
        "idempotency_key": id.idempotency_key,
        "base": base,
    })
}

fn parse_auth(args: &[String]) -> (AuthOptions<'_>, &[String]) {
    let mut file = None;
    let mut user = None;
    let mut index = 0;
    while let Some(flag) = args.get(index) {
        let slot = match flag.as_str() {
            "--auth-file" if file.is_none() => &mut file,
            "--auth-user" if user.is_none() => &mut user,
            "--auth-file" | "--auth-user" => usage(),
            _ => break,
        };
        let Some(value) = args.get(index + 1) else {
            usage();
        };
        *slot = Some(value.as_str());
        index += 2;
    }
    if user.is_some() && file.is_none() {
        eprintln!("choir: --auth-user needs --auth-file");
        std::process::exit(2);
    }
    (
        AuthOptions {
            file,
            user,
            explicit: index > 0,
        },
        &args[index..],
    )
}

fn workspace_body(repo: &str, name: &str, rest: &[&str]) -> serde_json::Value {
    if rest.is_empty() {
        return serde_json::json!({ "repo": repo, "name": name });
    }
    let (mut base, mut owner, mut key_file, mut change, mut idempotency_key) =
        (None, None, None, None, None);
    // Repeatable, unlike the five above: a change works within as many
    // subtrees as it works within, and one flag per prefix is the same
    // spelling `git sparse-checkout set` takes.
    let mut cone: Vec<String> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let Some(value) = rest.get(index + 1).copied() else {
            usage();
        };
        if rest[index] == "--path" {
            cone.push(value.to_string());
            index += 2;
            continue;
        }
        let slot = match rest[index] {
            "--base" if base.is_none() => &mut base,
            "--owner" if owner.is_none() => &mut owner,
            "--key-file" if key_file.is_none() => &mut key_file,
            "--change" if change.is_none() => &mut change,
            "--idempotency-key" if idempotency_key.is_none() => &mut idempotency_key,
            _ => usage(),
        };
        *slot = Some(value);
        index += 2;
    }
    // Sorted and deduplicated before signing so that two clients naming
    // the same subtrees in different orders produce the same
    // authorization bytes. Canonical ordering is not decoration here:
    // the node rebuilds these bytes to verify the signature.
    cone.sort();
    cone.dedup();
    let (Some(base), Some(owner), Some(key_file), Some(change), Some(idempotency_key)) =
        (base, owner, key_file, change, idempotency_key)
    else {
        usage();
    };
    let Some(base_revision) = choir_hash::ContentHash::from_git_oid(base) else {
        eprintln!("<git-oid> must be a 40- or 64-char hex object id");
        std::process::exit(2);
    };
    let authorization = CreateAuthorization::new(
        change.into(),
        owner.into(),
        format!("{repo}/{name}"),
        base_revision,
        idempotency_key.into(),
    )
    .with_cone(cone);
    let mut body = signed_payload_body(key_file, owner, &authorization.to_payload());
    body["repo"] = serde_json::json!(repo);
    body["name"] = serde_json::json!(name);
    body["base"] = serde_json::json!(base);
    body["owner"] = serde_json::json!(owner);
    body["change"] = serde_json::json!(change);
    body["idempotency_key"] = serde_json::json!(idempotency_key);
    body
}

fn parse_content_hash_hex(value: &str) -> Option<choir_hash::ContentHash> {
    let (codec, digest) = value.split_once('-')?;
    let codec = u8::from_str_radix(codec, 16).ok()?;
    if digest.is_empty() || digest.len() % 2 != 0 {
        return None;
    }
    let digest = (0..digest.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(digest.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(choir_hash::ContentHash { codec, digest })
}

fn current_change_revision(
    api: &str,
    auth: AuthOptions<'_>,
    change_id: &str,
) -> choir_hash::ContentHash {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        finish(status, &body);
    }
    let view: serde_json::Value = match serde_json::from_str(&body) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("choir: view returned invalid JSON: {error}");
            std::process::exit(1);
        }
    };
    let Some(revision) = view["changes"][change_id]["revision_id"].as_str() else {
        eprintln!("choir: no such change or revision in GET /api/view");
        std::process::exit(1);
    };
    match parse_content_hash_hex(revision) {
        Some(revision) => revision,
        None => {
            eprintln!("choir: change revision has an invalid content-hash envelope");
            std::process::exit(1);
        }
    }
}

/// The node URL configured for this directory, if any.
///
/// Walks up from the working directory looking for `.choir/config`, the
/// way git finds a repository. A *file* rather than an environment
/// variable on purpose: this workspace takes configuration from flags
/// and files, and the handful of environment reads that exist are
/// deliberately not configuration.
///
/// Walking up rather than reading one fixed path means a checkout can
/// name the node it belongs to, which is the same thing a git remote
/// does and needs no explaining to anybody who has used one.
/// What `choir node serve` and `choir node install` were told, after
/// defaults.
///
/// One parser for both because they describe the same node: a
/// supervised node and a hand-started one must be the same command with
/// the same arguments, and two parsers is how they stop being.
struct NodeOptions {
    state: std::path::PathBuf,
    port: u16,
    create: Vec<String>,
    extra: Vec<String>,
}

/// Parses the options both node-starting commands take.
///
/// Everything after `--` belongs to the daemon and is not looked at, so
/// a daemon flag this side has never heard of still reaches it. Without
/// that, every new daemon flag would be a reason to stop using these
/// commands and go back to spelling out the whole invocation.
fn node_options(command: &str, rest: &[&str]) -> NodeOptions {
    let (mine, extra) = match rest.iter().position(|a| *a == "--") {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    let mut state: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut create: Vec<String> = Vec::new();
    let mut i = 0;
    while i < mine.len() {
        let name = mine[i];
        let Some(value) = mine.get(i + 1) else {
            eprintln!("{command}: {name} needs a value");
            std::process::exit(2);
        };
        match name {
            "--state" => state = Some((*value).to_string()),
            "--create" => create.push((*value).to_string()),
            "--port" => match value.parse::<u16>() {
                Ok(n) => port = Some(n),
                Err(_) => {
                    eprintln!("{command}: --port needs a port number, not {value:?}");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!(
                    "{command}: unknown option {other:?}\n\n  \
                     daemon flags go after `--`: {command} -- {other} ..."
                );
                std::process::exit(2);
            }
        }
        i += 2;
    }
    NodeOptions {
        state: state
            .map(std::path::PathBuf::from)
            .unwrap_or_else(state_dir),
        // The configured node names the port every client command will
        // use, so reading it back is what keeps the daemon and its
        // clients agreeing without the port being written down twice.
        port: port
            .or_else(|| {
                configured_node()
                    .as_deref()
                    .and_then(choir_cli::serve::port_of)
            })
            .unwrap_or(8417),
        create,
        extra: extra.iter().map(|a| (*a).to_string()).collect(),
    }
}

/// `~/.choir`, the layout `choir init` writes.
///
/// `HOME` describes the machine rather than carrying a setting of ours,
/// which is the same reason `choir init` may read it.
fn state_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".choir")
}

/// The home directory the service manager keeps its units under.
fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

/// The service manager, or a refusal naming what this machine is.
fn supervisor(command: &str) -> choir_cli::supervise::Supervisor {
    match choir_cli::supervise::Supervisor::detect() {
        Some(supervisor) => supervisor,
        None => {
            eprintln!(
                "{command}: no service manager known for {}\n\n  \
                 run it in the foreground instead: choir node serve",
                std::env::consts::OS
            );
            std::process::exit(1);
        }
    }
}

/// One line of `choir host`'s progress.
///
/// Every sub-step prints one of these as it happens rather than a
/// summary at the end, because the steps have wildly different
/// durations — `init` is a few files, `certbot` is a network round trip
/// with a challenge in it — and a command that prints nothing for
/// fifteen seconds is a command people interrupt.
fn step(what: &str, detail: &str) {
    let style = choir_cli::style::Style::for_stderr();
    eprintln!("  {}  {:14}  {detail}", style.green("ok"), style.dim(what));
}

/// `choir host` stopping to hand one thing back to the person running it.
///
/// Exit 3, not 1: "everything up to here worked and something only you
/// can supply is missing" is a different outcome from "this failed", and
/// `choir restore` already spends 3 on exactly that distinction. A
/// `sudo` line and a re-run is the shape, every time.
fn handover(done: &[String], why: &str, paste: &[String], retry: &str) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    eprintln!("\n  {} {why}\n", style.cyan("next:"));
    for line in paste {
        eprintln!("    {line}");
    }
    eprintln!("\n  {} {retry}", style.dim("then:"));
    if !done.is_empty() {
        eprintln!("\n  {} {}", style.dim("already done:"), done.join(", "));
    }
    eprintln!();
    std::process::exit(3)
}

/// `choir host` giving up, having said what it got through.
fn host_failed(done: &[String], what: &str, why: &str, retry: &str) -> ! {
    let style = choir_cli::style::Style::for_stderr();
    eprintln!("\n  {} {what}: {why}\n", style.red("failed"));
    if !done.is_empty() {
        eprintln!("  {} {}\n", style.dim("already done:"), done.join(", "));
    }
    eprintln!("  {} {retry}\n", style.dim("retry:"));
    std::process::exit(1)
}

/// Renders the supervision file and hands the node to the service
/// manager, returning the unit written and the `choir` it runs.
///
/// Factored out of `choir node install` when `choir host` needed to do
/// exactly this as one of its steps. Not "install, but quieter": the
/// same refusals in the same order, because the way a first-run command
/// goes wrong is by being a *second* implementation of the thing it is
/// composing, one refusal short.
///
/// # Errors
///
/// Returns the sentence the caller should print, for a machine with no
/// service manager, a `choir` in a build directory, a state directory
/// with no node in it, or a service manager that refused.
fn install_unit(
    state: &std::path::Path,
    port: u16,
    extra: &[String],
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let Some(supervisor) = choir_cli::supervise::Supervisor::detect() else {
        return Err(format!(
            "no service manager known for {}\n\n  \
             run it in the foreground instead: choir node serve",
            std::env::consts::OS
        ));
    };
    // Refused rather than warned about: a unit pointing into a build
    // directory breaks on the next `cargo clean`, and it breaks at
    // reboot, which is the moment nobody is watching.
    let exe = std::env::current_exe().map_err(|_| "cannot find my own path".to_string())?;
    if choir_cli::supervise::in_build_directory(&exe) {
        return Err(format!(
            "this `choir` lives in a build directory:\n    {}\n\n  \
             a unit pointing there stops working at the next `cargo clean`,\n  \
             and it stops working at reboot. install it first:\n\n    \
             cargo build --release -p choir-cli -p choir-node\n    \
             cp target/release/choir target/release/choir-node ~/.local/bin/",
            exe.display()
        ));
    }
    // The same refusal `serve` makes, made before a unit exists rather
    // than after the service manager has started failing to run it every
    // ten seconds.
    let layout = choir_cli::serve::Layout::new(state, port);
    if !layout.missing().is_empty() {
        return Err(format!(
            "no node in {} yet\n\n  create one: choir init",
            state.display()
        ));
    }
    let home = home_dir();
    let unit = supervisor.unit_path(&home);
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let body = supervisor.render(&exe, state, port, extra);
    choir_fs::write_atomic(&unit, body)
        .map_err(|error| format!("write {}: {error}", unit.display()))?;
    let steps = supervisor.commands(choir_cli::supervise::Action::Install, &home);
    // The teardown step fails when nothing is loaded, which is exactly
    // the first-install case.
    let last = steps.len().saturating_sub(1);
    for (at, step) in steps.iter().enumerate() {
        if !run_step(step, at != last) {
            return Err("the service manager refused".to_string());
        }
    }
    Ok((unit, exe))
}

/// Runs one service-manager command, reporting the ones that matter.
///
/// `launchctl bootout` on a job that is not loaded fails, and that
/// failure is the normal case on a first install — so a step is allowed
/// to fail only when the caller says which one.
fn run_step(argv: &[String], allow_failure: bool) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return true;
    };
    match std::process::Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            if !allow_failure {
                let text = String::from_utf8_lossy(&out.stderr);
                eprintln!("  {} {}", argv.join(" "), text.trim());
            }
            allow_failure
        }
        Err(error) => {
            if !allow_failure {
                eprintln!("  {}: {error}", argv.join(" "));
            }
            allow_failure
        }
    }
}

fn configured_node() -> Option<String> {
    configured("node")
}

/// One key out of the nearest `.choir/config`.
///
/// Generalised from the `node` lookup when the credential gained the
/// same need: a checkout that names its node and a checkout that names
/// the credential for it are the same question asked twice, and two
/// parsers for one file is one of them drifting.
/// The walk order, stated once so the surface can quote it: every
/// `.choir/config` from the working directory up to the filesystem root,
/// then `~/.choir/config`.
///
/// The home file is the fallback and not the first stop, so a checkout
/// that names its own node still wins on a machine that has joined a
/// different one. It exists because the walk alone cannot answer for a
/// contributor who joins in one directory and clones into another:
/// `~/src/foo` is not under `~` in any sense the walk can see once they
/// have `cd`'d into it — it is, but only because `$HOME` happens to be a
/// parent, which stops being true the moment they clone into `/srv` or
/// onto another volume.
///
/// The alternative considered was writing the node into each clone at
/// clone time through the credential helper. It was rejected because git
/// gives a helper no hook that fires on `git clone` — the helper is
/// asked for a credential, not told a repository was created — so the
/// write would have to happen on the first *authenticated* fetch, which
/// is after the contributor has already run a command that needed it.
fn configured(want: &str) -> Option<String> {
    let mut dir = std::env::current_dir().ok();
    while let Some(here) = dir {
        if let Some(found) = configured_in(&here.join(".choir/config"), want) {
            return Some(found);
        }
        let mut up = here;
        if !up.pop() {
            break;
        }
        dir = Some(up);
    }
    configured_in(&state_dir().join("config"), want)
}

/// One key out of one `.choir/config` file.
fn configured_in(path: &std::path::Path, want: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == want {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Fills in the node URL for a command that takes one and was not given
/// one.
///
/// Every command whose spec begins with `<api>` takes it as its first
/// argument, and an api is always a URL, so "the first argument is not a
/// URL" is an unambiguous test rather than a guess. The set of such
/// commands is read from the surface table rather than listed here,
/// because a second list is a second thing to forget.
///
/// An explicit URL always wins: this only ever fills a gap.
fn with_configured_node(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        return args.to_vec();
    }
    // A command name can be two words -- `acl render`, `node status`,
    // `repo create` -- and the api follows the whole name, not the first
    // word of it. Matching only `args[0]` meant every two-word command
    // silently lost `.choir/config`: `choir repo create me/thing.git`
    // was read as a one-word command with a repository where its node
    // should be, and refused.
    let two = (args.len() >= 2).then(|| format!("{} {}", args[0], args[1]));
    let (name, words) = match two {
        Some(two) if choir_cli::surface::COMMANDS.iter().any(|c| c.name == two) => (two, 2),
        _ => (args[0].clone(), 1),
    };
    let takes_api = choir_cli::surface::COMMANDS
        .iter()
        .any(|c| c.name == name && c.args.starts_with("<api>"));
    if !takes_api {
        return args.to_vec();
    }
    let given = args.get(words).map(String::as_str).unwrap_or("");
    if given.starts_with("http://") || given.starts_with("https://") {
        return args.to_vec();
    }
    let Some(node) = configured_node() else {
        return args.to_vec();
    };
    let mut filled = Vec::with_capacity(args.len() + 1);
    filled.extend(args[..words].iter().cloned());
    filled.push(node);
    filled.extend(args[words..].iter().cloned());
    filled
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `choir <command> --help` before anything else parses: a reader
    // asking what a command takes must not have to satisfy its argument
    // rules to be told.
    let style = choir_cli::style::Style::for_stdout();
    // What this binary was built from, before anything else parses.
    //
    // The stamp is the node crate's, which is this workspace's, which is
    // the commit this file was compiled at -- not a `git rev-parse` in
    // whatever directory the reader happens to be standing in. That
    // difference is the whole point: "I rebuilt it" and "the rebuild is
    // what is running" are separate claims, and only the binary can
    // settle the second.
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("choir {}", choir_node::build_line());
        std::process::exit(0);
    }
    // `choir acl render --help` names a two-word command; every other
    // command's help is under args[1].
    //
    // Recognised in place rather than by rewriting `-h` to `--help`
    // first: a rewrite pass has to guess how far right a flag can stand
    // before it becomes somebody's argument, and it guesses wrong.
    // `choir key <file> -h` names an actor `-h`, and the rewriting
    // version of this printed a binding for an actor called `--help`.
    let asked = match args.iter().position(|a| is_help(a)) {
        Some(1) => Some(args[0].clone()),
        // Any two-word command, not just `acl render`: the table knows
        // which names have a space in them, and a second list here is a
        // second thing to forget when one is added.
        Some(2)
            if choir_cli::surface::COMMANDS
                .iter()
                .any(|c| c.name == format!("{} {}", args[0], args[1])) =>
        {
            Some(format!("{} {}", args[0], args[1]))
        }
        _ => None,
    };
    if let Some(name) = asked {
        if let Some(help) = choir_cli::surface::command_help_in(&name, style) {
            print!("{help}");
            std::process::exit(0);
        }
    }
    if args.first().is_some_and(|a| is_help(a)) {
        print!("{}", choir_cli::surface::usage_in(style));
        std::process::exit(0);
    }
    let (auth, args) = parse_auth(&args);
    let args = with_configured_node(args);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        // With a name, prints the line that *binds* this key to one
        // review channel; without, the unconstrained form. Either way the
        // operator appends the output to the node's trusted-keys file.
        ["key", key_file, rest @ ..] if rest.len() <= 1 && auth.is_empty() => {
            let key = load_key(key_file);
            let hex = hex_encode(&key.public_key_bytes());
            match rest.first() {
                Some(name) => println!("{name} {hex}"),
                None => println!("{hex}"),
            }
        }
        // The first command anybody runs, so it takes no <api>: there
        // is no node yet to name.
        ["init", rest @ ..] if auth.is_empty() => {
            let (mut dir, mut port, mut force) = (None, 8417u16, false);
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                match *arg {
                    "--force" => force = true,
                    "--port" => {
                        let Some(value) = it.next().and_then(|v| v.parse().ok()) else {
                            usage()
                        };
                        port = value;
                    }
                    other if !other.starts_with('-') && dir.is_none() => dir = Some(other),
                    _ => usage(),
                }
            }
            let style = choir_cli::style::Style::for_stdout();
            // `HOME` describes the machine rather than carrying a
            // setting of ours, which is the same reason `choir-queue`
            // may read it. A state directory can still be given
            // explicitly, and is the only way to get one elsewhere.
            let state = match dir {
                Some(dir) => std::path::PathBuf::from(dir),
                None => match std::env::var_os("HOME") {
                    Some(home) => std::path::PathBuf::from(home).join(".choir"),
                    None => {
                        eprintln!(
                            "{} no HOME, so there is no default state directory\n\
                             \n  choir init <state-dir>",
                            style.red("choir init:")
                        );
                        std::process::exit(2);
                    }
                },
            };
            let plan = choir_cli::init::Plan::new(&state, port);
            match choir_cli::init::run(&plan, force) {
                Ok(made) => {
                    let rows: Vec<(&str, String)> = vec![
                        ("repos", plan.repos.display().to_string()),
                        ("auth", format!("{} (0600)", plan.auth.display())),
                        ("key", format!("{} (0600)", plan.key.display())),
                        ("trusted", plan.trusted.display().to_string()),
                        (
                            "config",
                            format!("{} -> {}", plan.config.display(), plan.node_url()),
                        ),
                    ];
                    note("ready", &rows);
                    if !made.replaced.is_empty() {
                        eprintln!(
                            "  {} replaced {} existing file(s); the previous credential is gone\n",
                            style.red("--force:"),
                            made.replaced.len()
                        );
                    }
                    // The two commands that follow, because knowing what
                    // was created is not the same as knowing what to do
                    // with it. On stdout so `$(choir init)` is the
                    // command it names; the commentary around it is not.
                    //
                    // `choir node serve`, not the daemon's own argv:
                    // this printed the six-flag `choir-node` line until
                    // there was a command that derived those flags, and
                    // a first run that begins by pasting paths teaches
                    // the paths rather than the tool.
                    let serve = match dir {
                        Some(_) => format!("choir node serve --state {}", state.display()),
                        None => "choir node serve".to_string(),
                    };
                    println!("{serve}");
                    eprintln!(
                        "  {} run the line above, then:\n    choir repo create {} me/thing.git\n",
                        style.dim("next"),
                        plan.node_url()
                    );
                    let _ = made.user;
                }
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir init:"));
                    std::process::exit(1);
                }
            }
        }
        // Beside `init` rather than under `node`, and for the same
        // reason: `choir node …` is the family for a node that exists,
        // and this is the command you run when there is not one yet.
        ["host", rest @ ..] if auth.is_empty() => host(rest),
        ["seed", rest @ ..] if auth.is_empty() => seed(rest),
        // The privileged step, with a name, so that it appears in
        // `--help`, in the shell history and in sudo's log as itself
        // rather than as an argument to something friendlier.
        ["node", "tls", rest @ ..] => node_tls(rest),
        ["node", "upgrade", rest @ ..] => node_upgrade(rest),
        // The one command whose whole point is that the node is
        // already running: everything else about a repository assumes
        // it exists, and until this there was no way to make one
        // without stopping the daemon and naming it in `--create`.
        ["repo", "create", api, name] => {
            let client = match choir_cli::mcp::HttpClient::new(
                api,
                auth.file_for(api).map(std::path::Path::new),
                auth.user,
            ) {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("choir: {error}");
                    std::process::exit(2);
                }
            };
            let endpoint = choir_cli::surface::endpoint("POST", "/api/repo")
                .expect("the repo endpoint is in the table");
            let body = serde_json::json!({ "name": name });
            match client.request(endpoint, &body) {
                Ok((status, response)) => {
                    // The clone URL is assembled here rather than by the
                    // node, because the node does not know how the
                    // caller reached it: behind a proxy its own base is
                    // not the one that works from out here.
                    //
                    // On stderr, through `note`, because stdout is the
                    // node's own JSON byte for byte -- read by agents,
                    // by `jq` and by the tests. A convenience line
                    // printed above it would break all three.
                    if (200..300).contains(&status) {
                        note(
                            "repository created",
                            &[("clone", format!("{}/{name}", api.trim_end_matches('/')))],
                        );
                    }
                    finish(status, &response);
                }
                Err(error) => {
                    eprintln!("choir: {error}");
                    std::process::exit(1);
                }
            }
        }
        // Two words, like `acl render`: `node` is a family rather than
        // a command, and `choir node` alone should say so rather than
        // guessing which member was meant.
        // Everything after `--` belongs to the daemon, so a flag this
        // command has never heard of is still reachable. The split is
        // done here rather than in `serve::plan` because only this side
        // sees the raw argv.
        ["node", "serve", rest @ ..] => {
            let style = choir_cli::style::Style::for_stdout();
            let options = node_options("choir node serve", rest);
            let layout = choir_cli::serve::Layout::new(&options.state, options.port);
            let port = options.port;
            let program = match choir_cli::serve::find_daemon() {
                Ok(program) => program,
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir node serve:"));
                    std::process::exit(1);
                }
            };
            let invocation =
                match choir_cli::serve::plan(program, &layout, &options.create, &options.extra) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        eprintln!("{} {error}", style.red("choir node serve:"));
                        std::process::exit(1);
                    }
                };
            if choir_cli::serve::port_taken(port) {
                eprintln!(
                    "{} something is already listening on 127.0.0.1:{port}\n\n  \
                     is it yours?  choir node status\n  \
                     use another:  choir node serve --port <n>",
                    style.red("choir node serve:")
                );
                std::process::exit(1);
            }
            // On stderr: stdout belongs to the daemon from the next line
            // onwards, and a reader piping it should not receive ours.
            eprintln!("{}", style.dim(&invocation.display()));
            eprintln!("{} {}", style.dim("serving"), layout.port);
            eprintln!("{}", style.red(&choir_cli::serve::exec(&invocation)));
            std::process::exit(1);
        }
        ["repo", "list", api] => {
            let style = choir_cli::style::Style::for_stdout();
            let client = match choir_cli::mcp::HttpClient::new(
                api,
                auth.file_for(api).map(std::path::Path::new),
                auth.user,
            ) {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir repo list:"));
                    std::process::exit(2);
                }
            };
            let (status, body) = match client.get("/api/repos") {
                Ok(answer) => answer,
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir repo list:"));
                    std::process::exit(1);
                }
            };
            if status != 200 {
                eprintln!("{} {status}: {body}", style.red("choir repo list:"));
                std::process::exit(1);
            }
            let answer: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let names: Vec<&str> = answer["repos"]
                .as_array()
                .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            for name in &names {
                println!("{name}");
            }
            // The two empty answers are different problems, and a bare
            // blank line does not say which one this is.
            if names.is_empty() {
                if answer["narrowed"].as_bool().unwrap_or(false) {
                    eprintln!(
                        "  {}\n",
                        style.dim("no repositories this credential can read")
                    );
                } else {
                    eprintln!(
                        "  {}\n    choir repo create {api} me/thing.git\n",
                        style.dim("no repositories on this node yet — make one:")
                    );
                }
            }
        }
        // The credential is deliberately not in the URL. A clone URL is
        // pasted into shells, screenshots and issue trackers, and a
        // token in one is a token in all three; the credential helper
        // line beside it does the same job and leaves no copy behind.
        ["repo", "url", api, name] => {
            let style = choir_cli::style::Style::for_stdout();
            let name = if name.ends_with(".git") {
                (*name).to_string()
            } else {
                format!("{name}.git")
            };
            let url = format!("{}/{name}", api.trim_end_matches('/'));
            println!("{url}");
            let credential = auth
                .file_for(api)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}/auth", state_dir().display()));
            eprintln!(
                "\n  {}\n    git clone {url}\n    git -C {} config credential.helper \\\n      \
                 '!choir git-credential {credential}'\n",
                style.dim("clone it, then teach git the credential:"),
                name.trim_end_matches(".git")
                    .rsplit('/')
                    .next()
                    .unwrap_or("repo")
            );
        }
        // The unit runs `choir node serve`, so it names a command
        // rather than a configuration: a daemon flag changing later
        // never means re-rendering supervision, which is how a plist
        // that lints clean and restarts cleanly ends up launching
        // yesterday's arguments.
        ["node", "install", rest @ ..] => {
            let style = choir_cli::style::Style::for_stdout();
            let command = "choir node install";
            let options = node_options(command, rest);
            let layout = choir_cli::serve::Layout::new(&options.state, options.port);
            let unit = match install_unit(&options.state, options.port, &options.extra) {
                Ok((unit, _)) => unit,
                Err(error) => {
                    eprintln!("{} {error}", style.red(&format!("{command}:")));
                    std::process::exit(1);
                }
            };
            note(
                "installed",
                &[
                    ("unit", unit.display().to_string()),
                    ("state", options.state.display().to_string()),
                    ("log", layout.log.display().to_string()),
                ],
            );
            eprintln!("  {} choir node status\n", style.dim("check it"));
        }
        ["node", "stop"] => {
            let style = choir_cli::style::Style::for_stdout();
            let supervisor = supervisor("choir node stop");
            let home = home_dir();
            for step in supervisor.commands(choir_cli::supervise::Action::Stop, &home) {
                if !run_step(&step, false) {
                    eprintln!("{} nothing was running", style.red("choir node stop:"));
                    std::process::exit(1);
                }
            }
            eprintln!(
                "stopped — the unit is still installed, so it returns at next login\n  \
                 to end it: choir node uninstall"
            );
        }
        // Torn down and re-bootstrapped, never kicked: a restart
        // relaunches the definition the service manager cached, so a
        // unit whose arguments changed restarts cleanly into the old
        // ones.
        ["node", "restart"] => {
            let style = choir_cli::style::Style::for_stdout();
            let command = "choir node restart";
            let supervisor = supervisor(command);
            let home = home_dir();
            let unit = supervisor.unit_path(&home);
            if !unit.exists() {
                eprintln!(
                    "{} nothing is installed here\n\n  install it: choir node install",
                    style.red(&format!("{command}:"))
                );
                std::process::exit(1);
            }
            let steps = supervisor.commands(choir_cli::supervise::Action::Install, &home);
            let last = steps.len().saturating_sub(1);
            for (at, step) in steps.iter().enumerate() {
                if !run_step(step, at != last) {
                    eprintln!(
                        "{} the service manager refused",
                        style.red(&format!("{command}:"))
                    );
                    std::process::exit(1);
                }
            }
            eprintln!("restarted {}", unit.display());
        }
        // The state directory is kept. It holds the keys, the
        // repositories and the op log, and no command of ours deletes
        // those — an uninstall that took the data with it would be the
        // one mistake this whole tool cannot undo.
        ["node", "uninstall"] => {
            let style = choir_cli::style::Style::for_stdout();
            let supervisor = supervisor("choir node uninstall");
            let home = home_dir();
            let unit = supervisor.unit_path(&home);
            for step in supervisor.commands(choir_cli::supervise::Action::Uninstall, &home) {
                run_step(&step, true);
            }
            match std::fs::remove_file(&unit) {
                Ok(()) => eprintln!("uninstalled {}", unit.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("nothing was installed at {}", unit.display());
                }
                Err(error) => {
                    eprintln!(
                        "{} remove {}: {error}",
                        style.red("choir node uninstall:"),
                        unit.display()
                    );
                    std::process::exit(1);
                }
            }
            eprintln!(
                "  {} {} — keys, repositories and the op log\n",
                style.dim("kept"),
                state_dir().display()
            );
            // The one thing `choir host` created that is not under the
            // state directory, and the one this command cannot remove:
            // it belongs to root. Left in place it is harmless — it
            // exits early when the marker is gone — but a renewal hook
            // nobody knows about is a renewal hook nobody removes, so it
            // is named rather than merely survived.
            let hook =
                std::path::Path::new(choir_cli::tls::HOOK_DIR).join(choir_cli::tls::HOOK_NAME);
            if hook.exists() {
                eprintln!(
                    "  {} {}\n    it does nothing once {} is gone; to remove it and the\n    \
                     certificate as well:\n\n      sudo rm {}\n      sudo certbot delete\n",
                    style.cyan("renewal hook still installed:"),
                    hook.display(),
                    state_dir().join("tls.enabled").display(),
                    hook.display(),
                );
            }
        }
        ["node", "logs", rest @ ..] => {
            let style = choir_cli::style::Style::for_stdout();
            let (lines, rest) = match rest.split_first() {
                Some((first, tail)) if !first.starts_with('-') => match first.parse::<usize>() {
                    Ok(n) => (n, tail),
                    Err(_) => {
                        eprintln!("choir node logs: <lines> must be a number, not {first:?}");
                        std::process::exit(2);
                    }
                },
                _ => (30, rest),
            };
            let options = node_options("choir node logs", rest);
            let log = choir_cli::serve::Layout::new(&options.state, options.port).log;
            match std::fs::read_to_string(&log) {
                Ok(text) => {
                    let all: Vec<&str> = text.lines().collect();
                    for line in all.iter().skip(all.len().saturating_sub(lines)) {
                        println!("{line}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "{} no log at {}\n\n  \
                         a node started by hand writes to your terminal, not here;\n  \
                         the log is written by a supervised node: choir node install",
                        style.red("choir node logs:"),
                        log.display()
                    );
                    std::process::exit(1);
                }
                Err(error) => {
                    eprintln!(
                        "{} {}: {error}",
                        style.red("choir node logs:"),
                        log.display()
                    );
                    std::process::exit(1);
                }
            }
        }
        ["node", "status", rest @ ..] if rest.len() <= 1 => {
            let api = rest
                .first()
                .copied()
                .map(str::to_string)
                .or_else(configured_node);
            let Some(api) = api else {
                eprintln!(
                    "choir node status: no node given and none configured\n\
                     \n\
                       write `node = <url>` to .choir/config, or pass the URL"
                );
                std::process::exit(2);
            };
            let style = choir_cli::style::Style::for_stdout();
            match choir_cli::node::status(&api, auth.file_for(&api).map(std::path::Path::new)) {
                Ok((health, view)) => {
                    print!(
                        "{}",
                        choir_cli::node::status_report(&api, health, &view, style)
                    );
                    std::process::exit(health.exit_code());
                }
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir node status:"));
                    std::process::exit(1);
                }
            }
        }
        // The only command that takes its node as an *optional*
        // argument. Everything else refuses without one; this one has
        // to keep working on a machine that has no node yet, because
        // "there is no node configured" is one of the things it reports.
        ["doctor", rest @ ..] if rest.len() <= 3 => {
            // `--state` for the same reason `node serve` takes one: a
            // machine can hold more than one node's state directory, and
            // the host half of this report is entirely about one of them.
            let (state, rest) = match rest {
                [head @ .., "--state", dir] | ["--state", dir, head @ ..] => {
                    (std::path::PathBuf::from(*dir), head.to_vec())
                }
                other if other.len() <= 1 => (state_dir(), other.to_vec()),
                _ => usage(),
            };
            let configured = configured_node();
            let api = rest.first().copied().map(str::to_string).or(configured);
            // The effective credential, not merely a given one: the
            // check that reports "no auth file" must not report it about
            // a node whose credential this command would have used.
            let effective = api.as_deref().and_then(|api| auth.file_for(api));
            let credential = effective.or(auth.file);
            let mut checks = choir_cli::doctor::run(api.as_deref(), credential);
            // D80: a fork check per seed `.choir/config` names, against
            // the node it names.
            let seeds = configured_seeds();
            if !seeds.is_empty() {
                checks.extend(choir_cli::doctor::seeds(
                    api.as_deref(),
                    &seeds,
                    &|url: &str| auth.file_for(url).map(str::to_string),
                    auth.user,
                ));
            }
            // The six host facts, and only on a machine that has a node:
            // "linger is off" told to a laptop that only ever clones is
            // six rows of noise on a report whose whole value is that
            // every row means something.
            if choir_cli::serve::Layout::new(&state, 8417)
                .missing()
                .is_empty()
            {
                checks.extend(choir_cli::doctor::host(&state, credential));
            }
            let style = choir_cli::style::Style::for_stdout();
            // Above the checks, because the first question is "what is
            // this machine" and every check below reads differently
            // depending on the answer: a missing `choir-node` matters to
            // an operator and is nothing to a contributor.
            let role = choir_cli::join::Role::of(&state_dir(), credential);
            print!("{}", choir_cli::doctor::heading(role, style));
            print!("{}", choir_cli::doctor::report(&checks, style));
            std::process::exit(choir_cli::doctor::exit_code(&checks));
        }
        // Exit 3 is its own outcome and not a failure: "the backup is
        // fine and something only you can supply is missing" is the
        // documented recovery path — stop, supply it, re-run — and
        // collapsing it into 1 would make that indistinguishable from a
        // corrupt backup.
        ["backup", "restore", src, root] => {
            let style = choir_cli::style::Style::for_stdout();
            let (src, root) = (std::path::Path::new(src), std::path::Path::new(root));
            if !src.is_dir() {
                eprintln!(
                    "{} no backup directory at {}",
                    style.red("choir backup restore:"),
                    src.display()
                );
                std::process::exit(2);
            }
            let daemon = match choir_cli::serve::find_daemon() {
                Ok(daemon) => daemon,
                Err(error) => {
                    eprintln!("{} {error}", style.red("choir backup restore:"));
                    std::process::exit(1);
                }
            };
            // The restored node's own credential, in its own root. Not
            // the caller's: a restore is building somebody else's node,
            // and the credential it will serve with lives beside the
            // log it will serve.
            let auth = auth
                .file
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| root.join(".choir/auth"));
            let mut say = |line: &str| eprintln!("  {} {line}", style.dim("restore:"));
            match choir_cli::restore::run(src, root, &daemon, &auth, &mut say) {
                Ok(done) => {
                    // On stdout, and not through `note`, which prints
                    // only to a terminal. This is the receipt: it is
                    // read once, on a bad day, and pasted into an
                    // incident log, so it has to survive a pipe.
                    println!(
                        "restore: {} ops replayed, {} repos unbundled, canary landed at seq {}",
                        done.ops, done.repos, done.ops
                    );
                    println!(
                        "restore: the canary ref is {} in {} — it is evidence, delete it when you no longer want it",
                        done.canary, done.landed_in
                    );
                    println!(
                        "restore: root is {} — start it under your supervisor:",
                        root.display()
                    );
                    println!("  choir node install --state {}", root.display());
                }
                Err(refusal) => {
                    let tag = if refusal.code == 3 {
                        style.red("choir backup restore: DECIDE")
                    } else {
                        style.red("choir backup restore:")
                    };
                    eprintln!("{tag} {}", refusal.message);
                    std::process::exit(refusal.code);
                }
            }
        }
        // A backup you can only verify by asking the thing it is a
        // backup of is not a backup, so nothing here opens a connection.
        ["repo", "follower", rest @ ..] => repo_follower(rest),
        ["backup", "take", rest @ ..] => backup_take(rest),
        ["backup", "schedule", rest @ ..] => backup_schedule(rest),
        ["backup", "unschedule"] => {
            let style = choir_cli::style::Style::for_stdout();
            let supervisor = supervisor("choir backup unschedule");
            let home = home_dir();
            for step in supervisor.backup_commands(choir_cli::supervise::Action::Uninstall, &home) {
                run_step(&step, true);
            }
            let mut removed = 0;
            for unit in supervisor.backup_units(&home) {
                match std::fs::remove_file(&unit) {
                    Ok(()) => removed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        eprintln!(
                            "{} remove {}: {error}",
                            style.red("choir backup unschedule:"),
                            unit.display()
                        );
                        std::process::exit(1);
                    }
                }
            }
            if removed == 0 {
                eprintln!("no backup timer was scheduled");
            } else {
                eprintln!("unscheduled; the backups already taken are kept");
            }
        }
        ["backup", "verify", dir] => {
            let style = choir_cli::style::Style::for_stdout();
            let dir = std::path::Path::new(dir);
            if !dir.is_dir() {
                eprintln!(
                    "{} {} is not a directory",
                    style.red("choir backup verify:"),
                    dir.display()
                );
                std::process::exit(2);
            }
            let daemon = choir_cli::serve::find_daemon().ok();
            let checks = choir_cli::backup::verify(dir, daemon.as_deref());
            print!("{}", choir_cli::doctor::report(&checks, style));
            std::process::exit(i32::from(!choir_cli::backup::restorable(&checks)));
        }
        // The mode is required, never defaulted. A repair tool that
        // picks its own action is the one thing this must not be: the
        // difference between "tell me what is wrong" and "change my log"
        // is the operator's to make, and a default would make it by
        // habit.
        ["repair", log_file, rest @ ..] => repair(log_file, rest),
        ["workspace", api, repo, name, rest @ ..] => {
            let body = workspace_body(repo, name, rest);
            let (status, resp) = http(api, auth, "choir_workspace", body);
            finish(status, &resp);
        }
        ["checkpoint", api, key_file, channel, change_id, workspace, oid] => {
            let Some(revision) = choir_hash::ContentHash::from_git_oid(oid) else {
                eprintln!("<git-oid> must be a 40- or 64-char hex object id");
                std::process::exit(2);
            };
            let prev_revision = current_change_revision(api, auth, change_id);
            let op = ViewOp::new(OpKind::CheckpointChange {
                id: (*change_id).into(),
                workspace: (*workspace).into(),
                revision,
                prev_revision,
            });
            submit(api, key_file, channel, &op, auth);
        }
        ["workspace-archive", api, key_file, channel, repo, name, change_id, idempotency_key] => {
            let prev_revision = current_change_revision(api, auth, change_id);
            let authorization = ArchiveAuthorization::new(
                (*change_id).into(),
                format!("{repo}/{name}"),
                prev_revision,
            );
            let mut body = signed_payload_body(key_file, channel, &authorization.to_payload());
            body["repo"] = serde_json::json!(repo);
            body["name"] = serde_json::json!(name);
            body["change"] = serde_json::json!(change_id);
            body["idempotency_key"] = serde_json::json!(idempotency_key);
            let (status, resp) = http(api, auth, "choir_workspace_archive", body);
            finish(status, &resp);
        }
        // Deliberately not `<api>`-first like its neighbours: the git
        // remote already names the node, and repeating it is exactly the
        // friction this command exists to remove.
        ["propose", rest @ ..] => propose(rest, auth),
        // The link form first, and tested on the *path* rather than on
        // the argument count: `choir join <link> --user bea` has three
        // arguments too, and read as the positional form it would name
        // an invite file `--user`.
        ["join", link, rest @ ..] if auth.is_empty() && choir_cli::join::Link::looks_like(link) => {
            match choir_cli::join::Link::parse(link) {
                Ok(choir_cli::join::Link { api, id, secret }) => {
                    join(&api, Invite::Pair(id, secret), None, rest)
                }
                Err(why) => {
                    eprintln!("choir join: {why}");
                    std::process::exit(2);
                }
            }
        }
        ["join", api, invite_file, key_file, rest @ ..] if auth.is_empty() => {
            join(api, Invite::File(invite_file), Some(key_file), rest)
        }
        // Argument order is git's, not ours: it appends the operation
        // to whatever the configured helper line already carried.
        ["git-credential", auth_file, operation] if auth.is_empty() => {
            git_credential(auth_file, None, operation)
        }
        ["git-credential", auth_file, "--auth-user", user, operation] if auth.is_empty() => {
            git_credential(auth_file, Some(user), operation)
        }
        ["runner", config_file] => runner(config_file, auth),
        // The description a client generates against, and what this
        // node will accept. In the CLI so the shell library never needs
        // raw `curl` with a credential on its command line — a secret on
        // an argv is visible to every process through `ps`.
        ["schema", api] => {
            let (status, body) = http(api, auth, "choir_schema", serde_json::json!({}));
            finish(status, &body);
        }
        ["log", api, rest @ ..] => {
            let (mut from, mut verify, mut keys) = (0u64, false, None);
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                match *arg {
                    "--from" => {
                        let Some(value) = it.next().and_then(|v| v.parse().ok()) else {
                            usage()
                        };
                        from = value;
                    }
                    "--verify" => verify = true,
                    "--keys" => {
                        let Some(path) = it.next() else { usage() };
                        keys = Some(*path);
                    }
                    _ => usage(),
                }
            }
            log(api, from, verify, keys, auth);
        }
        ["batch", api, key_file, channel, ops_file] => {
            batch(api, key_file, channel, ops_file, auth);
        }
        ["submit", api, key_file, channel, op_json] => {
            // Round-trip through ViewOp so the signed bytes are exactly
            // what the daemon will decode.
            let op: ViewOp = match serde_json::from_str(op_json) {
                Ok(op) => op,
                Err(e) => {
                    eprintln!("bad op json: {e}");
                    std::process::exit(2);
                }
            };
            submit(api, key_file, channel, &op, auth);
        }
        // No reviewer names = ask the node to assign them (D24 layer 5;
        // needs the daemon started with --reviewers-file). `--ref` says
        // where the change wants to land, which is what per-ref policy
        // reads; omitting it leaves the review unbound.
        ["review", api, key_file, channel, id, oid, rest @ ..] => {
            let Some(target) = choir_hash::ContentHash::from_git_oid(oid) else {
                eprintln!("<git-oid> must be a 40- or 64-char hex object id");
                std::process::exit(2);
            };
            let mut target_ref = None;
            let mut reviewers = Vec::new();
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                if *arg == "--ref" {
                    let Some(name) = it.next() else { usage() };
                    target_ref = Some((*name).to_string());
                } else {
                    reviewers.push((*arg).to_string());
                }
            }
            let op = ViewOp::new(OpKind::RequestReview {
                id: (*id).into(),
                target,
                reviewers,
                target_ref,
            });
            submit(api, key_file, channel, &op, auth);
        }
        ["verdict", api, key_file, reviewer, id, verdict, rest @ ..] if rest.len() <= 1 => {
            let verdict = match *verdict {
                "approve" => Verdict::Approve,
                "request-changes" => Verdict::RequestChanges,
                _ => usage(),
            };
            let op = ViewOp::new(OpKind::PostVerdict {
                id: (*id).into(),
                reviewer: (*reviewer).into(),
                verdict,
                note: rest.first().copied().unwrap_or("").into(),
            });
            // The channel is the reviewer name: admission policy rejects
            // any verdict whose reviewer differs from the signed channel.
            submit(api, key_file, reviewer, &op, auth);
        }
        // D38. The comment id is caller-chosen and refused if the review
        // already holds it, so resubmitting a comment whose response was
        // lost is safe and never doubles it. Nothing here can be edited
        // or deleted afterwards: a correction is another comment.
        ["comment", api, key_file, channel, id, comment, body] => {
            let op = ViewOp::new(OpKind::PostComment {
                id: (*id).into(),
                comment: (*comment).into(),
                author: (*channel).into(),
                body: (*body).into(),
            });
            // The channel is the author: admission rejects any comment
            // whose author differs from the signed channel.
            submit(api, key_file, channel, &op, auth);
        }
        // A read receipt: lets the review's
        // author tell "reviewed and ignored" from "nobody looked yet".
        // First read only; resubmitting is refused, so a lost response
        // is safe to retry and a receipt never doubles.
        ["viewed", api, key_file, viewer, id] => {
            let op = ViewOp::new(OpKind::ViewedReview {
                id: (*id).into(),
                viewer: (*viewer).into(),
            });
            // The channel is the viewer: admission rejects any receipt
            // whose viewer differs from the signed channel.
            submit(api, key_file, viewer, &op, auth);
        }
        // D65. The voucher is not an argument: it is the operator half
        // of the channel being signed on, derived here so the two can
        // never be given different values. Admission checks the same
        // derivation, so a hand-rolled submission that disagrees is
        // refused rather than believed.
        ["witness", api, key_file, channel] => {
            let op = ViewOp::new(OpKind::CountersignSnapshot {
                witness: reviewer_operator(channel).into(),
                snapshot: latest_snapshot(api, auth),
            });
            submit(api, key_file, channel, &op, auth);
        }
        ["vouch", api, key_file, channel, subject, rest @ ..] if rest.len() <= 1 => {
            let op = ViewOp::new(OpKind::Vouch {
                voucher: reviewer_operator(channel).into(),
                subject: (*subject).into(),
                note: rest.first().copied().unwrap_or("").into(),
            });
            submit(api, key_file, channel, &op, auth);
        }
        ["unvouch", api, key_file, channel, subject, reason] => {
            let op = ViewOp::new(OpKind::WithdrawVouch {
                voucher: reviewer_operator(channel).into(),
                subject: (*subject).into(),
                reason: (*reason).into(),
            });
            submit(api, key_file, channel, &op, auth);
        }
        ["slash", api, node_key_file, id, reviewer, reason] => {
            require_node_key_file(node_key_file);
            let op = ViewOp::new(OpKind::SlashApproval {
                id: (*id).into(),
                reviewer: (*reviewer).into(),
                reason: (*reason).into(),
            });
            // This operator-only command uses the same signed-op endpoint
            // as every other mutation. Admission checks the key identity,
            // not this attribution string.
            submit(api, node_key_file, "node/slash", &op, auth);
        }
        // The operator's retention verb for a review that will never
        // finish: settle it as lapsed-unapproved. `lapsed` is hardwired
        // true because freezing a *complete* review is the retention
        // policy's job, and the fold refuses the other two shapes
        // anyway. Node-key-only at admission, same reasoning as slash:
        // archiving drops verdicts, so an unguarded verb would let an
        // agent erase a RequestChanges it did not like.
        ["abandon", api, node_key_file, id] => {
            require_node_key_file(node_key_file);
            let op = ViewOp::new(OpKind::ArchiveReview {
                id: (*id).into(),
                lapsed: true,
            });
            submit(api, node_key_file, "node/abandon", &op, auth);
        }
        // The operator's path to the durable identity record. Without
        // this, `BindKey` is node-only and the node has no CLI, so the
        // record stays empty and D24 T3 attribution — which reads it —
        // reports `indeterminate` with no way for an operator to fix it.
        //
        // `<key-hex>` is the *public key* hex that `choir key` prints and
        // the trusted-keys file already carries, not a content hash. The
        // actor id is derived here, the same way the node derives it, so
        // an operator never hand-computes a hash to bind a key.
        ["bind", api, node_key_file, operator, key_hex, rest @ ..] if rest.len() <= 1 => {
            require_node_key_file(node_key_file);
            let key = actor_id_from_hex(key_hex);
            let channel = rest.first().map(|c| (*c).to_string());
            // A re-bind that changes nothing is admissible -- the fold
            // allows re-binding so a channel typo can be corrected -- so
            // it costs a log entry and moves no state. That is a poor
            // reason to add a fold rule: refusing A->A inside `validate`
            // means distinguishing it from A->B in persisted semantics.
            // Catching it here keeps the record's rules unchanged.
            //
            // Deliberately narrow: only an exact match on operator and
            // channel, and only while unrevoked. Anything else is the
            // node's call, and the fold already refuses a cross-operator
            // move and a re-bind of a revoked key with `identity_state`.
            if let Some(existing) = current_binding(api, auth, &key.to_hex()) {
                if existing["operator"] == serde_json::json!(operator)
                    && existing["channel"] == serde_json::json!(channel)
                    && existing["revoked"].is_null()
                {
                    finish(
                        200,
                        &serde_json::json!({
                            "already_bound": true,
                            "operator": operator,
                            "channel": channel,
                            "bound_at": existing["bound_at"],
                        })
                        .to_string(),
                    );
                }
            }
            let op = ViewOp::new(OpKind::BindKey {
                operator: (*operator).into(),
                key,
                channel,
            });
            submit(api, node_key_file, "node/bind", &op, auth);
        }
        ["revoke", api, node_key_file, key_hex, reason] => {
            require_node_key_file(node_key_file);
            let op = ViewOp::new(OpKind::RevokeKey {
                key: actor_id_from_hex(key_hex),
                reason: (*reason).into(),
            });
            submit(api, node_key_file, "node/revoke", &op, auth);
        }
        ["appeal", api, attempt_id] => {
            let attempt_id = attempt_id.parse::<u64>().unwrap_or_else(|_| usage());
            let (status, resp) = http(
                api,
                auth,
                "choir_appeal",
                serde_json::json!({ "attempt_id": attempt_id }),
            );
            finish(status, &resp);
        }
        ["intent", api, key_file, channel, subject, kind, body] => {
            // D22 provenance record: task spec / plan / rationale for
            // `subject`; latest per (subject, kind) wins in the view.
            let op = ViewOp::new(OpKind::RecordProvenance {
                subject: (*subject).into(),
                kind: (*kind).into(),
                body: (*body).into(),
            });
            submit(api, key_file, channel, &op, auth);
        }
        // The write half of D49. `channel` is the reporter: admission
        // rejects a report whose reporter differs from the signed
        // channel, the same binding `viewed` relies on.
        ["check", api, key_file, channel, oid, name, status, rest @ ..] if rest.len() <= 3 => {
            let Some(subject) = choir_hash::ContentHash::from_git_oid(oid) else {
                eprintln!("<git-oid> must be a 40- or 64-char hex object id");
                std::process::exit(2);
            };
            let Some(status) = CheckStatus::parse(status) else {
                eprintln!("<status> must be passed, failed or running");
                std::process::exit(2);
            };
            let mut evidence = String::new();
            let mut target_ref = None;
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                if *arg == "--ref" {
                    let Some(name) = it.next() else { usage() };
                    target_ref = Some((*name).to_string());
                } else {
                    evidence = (*arg).to_string();
                }
            }
            let op = ViewOp::new(OpKind::RecordCheck {
                subject,
                name: (*name).into(),
                status,
                evidence,
                reporter: (*channel).into(),
                target_ref,
            });
            submit(api, key_file, channel, &op, auth);
        }
        // The read half, and the one command in this binary that exits 3.
        // See `check_exit`.
        ["checks", api, oid] => {
            let Some(subject) = choir_hash::ContentHash::from_git_oid(oid) else {
                eprintln!("<git-oid> must be a 40- or 64-char hex object id");
                std::process::exit(2);
            };
            let (status, resp) = http(api, auth, "choir_view", serde_json::json!({}));
            if !(200..300).contains(&status) {
                finish(status, &resp);
            }
            check_exit(&subject, &resp);
        }
        ["profile", api, channel] => {
            let (status, resp) = http(
                api,
                auth,
                "choir_profile",
                serde_json::json!({ "channel": channel }),
            );
            finish(status, &resp);
        }
        ["search", api, term, rest @ ..] => {
            // The flags are optional and the node validates every one of
            // them, so they are forwarded rather than re-checked here: a
            // second copy of "in must be one of files, code, commits"
            // is a second copy that can drift from the first.
            let mut arguments = serde_json::Map::new();
            arguments.insert("q".into(), serde_json::json!(term));
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                let field = match *arg {
                    "--in" => "in",
                    "--repo" => "repo",
                    "--rev" => "rev",
                    "--limit" => "limit",
                    _ => usage(),
                };
                let Some(value) = it.next() else { usage() };
                // `limit` is a number in the schema and a string on the
                // command line. Sent as a string it would fail schema
                // validation before it ever reached the node, which
                // would report a type error about an argument the caller
                // spelled correctly.
                let value = match field {
                    "limit" => match value.parse::<u64>() {
                        Ok(n) => serde_json::json!(n),
                        Err(_) => usage(),
                    },
                    _ => serde_json::json!(value),
                };
                arguments.insert(field.to_string(), value);
            }
            let (status, resp) = http(api, auth, "choir_search", arguments.into());
            finish(status, &resp);
        }
        ["reviews", api, reviewer] => {
            let (status, resp) = http(
                api,
                auth,
                "choir_reviews",
                serde_json::json!({ "reviewer": reviewer }),
            );
            finish(status, &resp);
        }
        ["view", api, rest @ ..] => {
            // The view is bounded by default, so the CLI has to be able
            // to reach page two: a command that could only ever print the
            // first 200 rows of each section would hide the rest behind a
            // `paging.next` it gave the caller no way to follow.
            let mut arguments = serde_json::Map::new();
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                let field = match *arg {
                    "--limit" => "limit",
                    "--offset" => "offset",
                    _ => usage(),
                };
                let Some(value) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
                    usage()
                };
                arguments.insert(field.to_string(), serde_json::json!(value));
            }
            let (status, resp) = http(api, auth, "choir_view", arguments.into());
            finish(status, &resp);
        }
        ["docs", rest @ ..] => {
            let open = match rest {
                [] => false,
                ["--open"] => true,
                _ => usage(),
            };
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                eprintln!("choir: cannot read the working directory: {e}");
                std::process::exit(1);
            });
            let Some(root) = choir_cli::docs::find_root(&cwd) else {
                eprintln!("choir: {}", choir_cli::docs::Failure::NotACheckout);
                std::process::exit(1);
            };
            let built = match choir_cli::docs::build(&root) {
                Ok(built) => built,
                Err(failure) => {
                    eprintln!("choir: {failure}");
                    std::process::exit(1);
                }
            };
            let opened = open && choir_cli::docs::open(&built.book.join("index.html"));
            note(
                "documentation built",
                &[
                    ("book", built.book.join("index.html").display().to_string()),
                    ("api", built.api.join("index.html").display().to_string()),
                    ("crates", built.crates.len().to_string()),
                ],
            );
            let doc = serde_json::json!({
                "root": built.root.display().to_string(),
                "book": built.book.display().to_string(),
                "api": built.api.display().to_string(),
                "crates": built.crates,
                "opened": opened,
            });
            finish(200, &doc.to_string());
        }
        ["skill", "install", rest @ ..] => {
            let into = match rest {
                [] => ".claude/skills",
                ["--into", dir] => dir,
                _ => usage(),
            };
            let dir = std::path::Path::new(into).join(choir_cli::surface::SKILL_DIR);
            let path = dir.join("SKILL.md");
            let rendered = choir_cli::surface::skill_md();
            // Byte-compare before writing: a re-install after `cargo
            // install` refreshes a stale skill and leaves a current one
            // untouched, so repeated installs produce no churn.
            let wrote = std::fs::read_to_string(&path).ok().as_deref() != Some(rendered.as_str());
            if wrote {
                if let Err(error) = choir_fs::write_atomic(&path, &rendered) {
                    eprintln!("choir: cannot write {}: {error}", path.display());
                    std::process::exit(1);
                }
            }
            let doc = serde_json::json!({ "path": path.display().to_string(), "wrote": wrote });
            note(
                if wrote {
                    "skill installed"
                } else {
                    "skill already current"
                },
                &[("path", path.display().to_string())],
            );
            finish(200, &doc.to_string());
        }
        ["invite", api, name, repo] => invite(api, auth, name, repo, "write"),
        ["invite", api, name, repo, level] => invite(api, auth, name, repo, level),
        ["asks", api] => asks(api, auth),
        ["grant", api, id, repo] => grant(api, auth, id, repo, "write"),
        ["grant", api, id, repo, level] => grant(api, auth, id, repo, level),
        ["decline", api, id] => decline(api, auth, id),
        ["acl", "render", api, acl_file] => acl_render(api, auth, acl_file),
        ["funnel", api] => {
            println!("{}", derived_view(api, auth, choir_cli::triage::funnel));
        }
        ["triage", api] => {
            let doc = derived_view(api, auth, choir_cli::triage::triage);
            finish(200, &doc);
        }
        ["state", api, channel] => {
            let doc = derived_view(api, auth, |view| {
                choir_cli::triage::next_actions(view, api, channel)
            });
            finish(200, &doc);
        }
        _ => usage(),
    }
}
