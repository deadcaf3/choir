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
//! choir join <api> <invite-file> <key-file> [--channel <name>] [--ssh-key <path>] [--token-file <path>]
//! choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]
//! choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
//! choir propose <key-file> <channel> [--api <url>] [--repo <owner/repo>] [--onto <branch>] [reviewer]...
//! choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
//! choir submit <api> <key-file> <channel> '<op-json>'
//! choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
//! choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
//! choir slash <api> <node-key-file> <id> <reviewer> '<reason>'
//! choir bind <api> <node-key-file> <operator> <key-hex> [channel]
//! choir revoke <api> <node-key-file> <key-hex> '<reason>'
//! choir appeal <api> <attempt-id>
//! choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
//! choir check <api> <key-file> <channel> <git-oid> <name> passed|failed|running [evidence] [--ref <repo:ref>]
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
//! answer is not decided yet; see [`check_exit`].

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_view::{ArchiveAuthorization, CheckStatus, CreateAuthorization, OpKind, Verdict, ViewOp};

#[derive(Clone, Copy)]
struct AuthOptions<'a> {
    file: Option<&'a str>,
    user: Option<&'a str>,
}

impl AuthOptions<'_> {
    fn is_empty(self) -> bool {
        self.file.is_none() && self.user.is_none()
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
    // `acl render` is the one two-word command, and a reader who typed
    // only half of it should be told about the half they typed.
    if first == "acl" && args.get(index + 1).map(String::as_str) == Some("render") {
        return Some("acl render".to_string());
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
        // No command at all: this is the question the index answers.
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
                eprintln!("{} `{}` is not a choir command.", style.red("choir:"), name);
                let names = choir_cli::surface::COMMANDS.iter().map(|c| c.name);
                if let Some(near) = choir_cli::style::nearest(&name, names) {
                    eprintln!("       did you mean {}?", style.cyan(near));
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
    let client = match choir_cli::mcp::HttpClient::new(
        api,
        auth.file.map(std::path::Path::new),
        auth.user,
    ) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("choir: {error}");
            std::process::exit(2);
        }
    };
    let endpoint = choir_cli::surface::mcp_endpoint(tool).expect("CLI endpoint is in the table");
    match client.request(endpoint, &arguments) {
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
        auth.file.map(std::path::Path::new),
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

/// `choir join <api> <invite-file> <key-file> [--channel <name>] [--ssh-key <path>] [--token-file <path>]`
///
/// Admission in one command: mint an actor key, redeem the operator's
/// invite, and store the token where the rest of the CLI reads it.
///
/// The invite is read from a file rather than taken as an argument, for
/// the reason every credential here is: an argv is readable by every
/// process on the host through `ps`. The file's format is the node's
/// own `user:token` auth-file spelling, so the invite an operator sent
/// can be pasted straight into one.
///
/// What this does **not** do is decide admission. The operator issued
/// the invite, and the invite carries the grants; this only spares them
/// the second out-of-band step of pasting a key line into a file. A node
/// started without `--invite-binds-keys` refuses the key and says so,
/// and admission there still ends with an operator's edit.
fn join(api: &str, invite_file: &str, key_file: &str, rest: &[&str]) -> ! {
    let (mut channel, mut ssh_key, mut token_file) = (None, None, None);
    let mut index = 0;
    while index < rest.len() {
        let Some(value) = rest.get(index + 1).copied() else {
            usage();
        };
        let slot = match rest[index] {
            "--channel" if channel.is_none() => &mut channel,
            "--ssh-key" if ssh_key.is_none() => &mut ssh_key,
            "--token-file" if token_file.is_none() => &mut token_file,
            _ => usage(),
        };
        *slot = Some(value);
        index += 2;
    }
    if !std::path::Path::new(invite_file).is_file() {
        eprintln!(
            "choir join: {invite_file} does not exist.\n\
             Write the invite the operator sent you into it, as one line: <id>:<secret>"
        );
        std::process::exit(2);
    }

    // Minted before the request, so the key exists whatever the node
    // answers. `load_key` is idempotent, which makes a retry after a
    // network failure redeem the key that already exists rather than a
    // second one the operator never saw.
    let key = load_key(key_file);
    let mut body = serde_json::json!({ "actor_key": hex_encode(&key.public_key_bytes()) });
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
    let client =
        match choir_cli::mcp::HttpClient::new(api, Some(std::path::Path::new(invite_file)), None) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("choir join: {error}");
                std::process::exit(2);
            }
        };
    let (status, response) = match client.request(endpoint, &body) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("choir join: {error}");
            std::process::exit(1);
        }
    };
    if !(200..300).contains(&status) {
        println!("{response}");
        std::process::exit(1);
    }
    let account: serde_json::Value = match serde_json::from_str(&response) {
        Ok(account) => account,
        Err(error) => {
            eprintln!("choir join: the node's answer did not parse: {error}");
            std::process::exit(1);
        }
    };
    let (Some(user), Some(token)) = (account["user"].as_str(), account["token"].as_str()) else {
        eprintln!("choir join: the node issued no token");
        std::process::exit(1);
    };

    // An invite is single-use, so the token is shown exactly once and
    // this is the only chance to keep it. Written 0600 before anything
    // is printed: a token this process holds and never stored is one the
    // contributor has to ask for a second invite to replace.
    let token_path = token_file.map_or_else(
        || {
            std::path::Path::new(key_file)
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("choir.auth")
        },
        std::path::PathBuf::from,
    );
    if let Err(error) = choir_fs::write_atomic_private(&token_path, format!("{user}:{token}\n")) {
        eprintln!(
            "choir join: the node issued a token but it could not be stored at {}: {error}\n\
             The invite is spent. Ask the operator for another.",
            token_path.display()
        );
        std::process::exit(1);
    }

    let bound = account["actor_key_bound"] == serde_json::Value::Bool(true);
    let summary = serde_json::json!({
        "user": user,
        "channel": account["channel"],
        "grants": account["grants"],
        "auth_file": token_path.display().to_string(),
        "key_file": key_file,
        "actor_key_bound": bound,
        "next": if bound {
            "choir --auth-file <auth_file> propose <key_file> <channel>"
        } else {
            // Said as an instruction rather than a warning, because it
            // is the step that is still outstanding: nothing this
            // contributor does next will work until the operator
            // registers the key.
            "ask the operator to register your key: run `choir key <key_file> <channel>` and send them the line"
        },
    });
    note(
        &format!("joined as {user}"),
        &[
            ("token", format!("{} (0600)", token_path.display())),
            ("key", key_file.to_string()),
            (
                "next",
                summary["next"].as_str().unwrap_or_default().to_string(),
            ),
        ],
    );
    finish(
        200,
        &serde_json::to_string_pretty(&summary).expect("summary is serializable"),
    );
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
    std::process::exit(1);
}

/// Flags of `choir propose`, after parsing.
struct ProposeOptions<'a> {
    api: Option<&'a str>,
    repo: Option<&'a str>,
    remote: &'a str,
    onto: Option<&'a str>,
    change: Option<&'a str>,
    cone: Vec<String>,
    reviewers: Vec<String>,
}

fn parse_propose<'a>(rest: &[&'a str]) -> ProposeOptions<'a> {
    let (mut api, mut repo, mut onto, mut change) = (None, None, None, None);
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
        cone,
        reviewers,
    }
}

/// `choir propose <key-file> <channel> [flags] [reviewer]...`
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
fn propose(key_file: &str, channel: &str, rest: &[&str], auth: AuthOptions<'_>) -> ! {
    let options = parse_propose(rest);
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
    // Failed outranks Running for the reason `View::checks_verdict`
    // gives: only one of the two can still turn green.
    let (verdict, code) = if rows.is_empty() {
        ("unreported", 1)
    } else if rows
        .values()
        .any(|v| status_of(v).as_deref() == Some("failed"))
    {
        ("failed", 1)
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
fn revocations(api: &str, auth: AuthOptions<'_>) -> choir_cli::verify::Revocations {
    let (status, body) = http(api, auth, "choir_view", serde_json::json!({}));
    if !(200..300).contains(&status) {
        eprintln!("choir log: cannot read bindings ({status}); revocations not checked");
        return choir_cli::verify::Revocations::new();
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
    let report = choir_cli::verify::page(&entries, &registry, &revoked);
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
            file: config.auth_file.as_deref().or(auth.file),
            user: config.auth_user.as_deref().or(auth.user),
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
    (AuthOptions { file, user }, &args[index..])
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
fn configured_node() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join(".choir/config")) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    if key.trim() == "node" {
                        let value = value.trim();
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
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
    let Some(name) = args.first() else {
        return args.to_vec();
    };
    let takes_api = choir_cli::surface::COMMANDS
        .iter()
        .any(|c| c.name == name && c.args.starts_with("<api>"));
    if !takes_api {
        return args.to_vec();
    }
    let given = args.get(1).map(String::as_str).unwrap_or("");
    if given.starts_with("http://") || given.starts_with("https://") {
        return args.to_vec();
    }
    let Some(node) = configured_node() else {
        return args.to_vec();
    };
    let mut filled = Vec::with_capacity(args.len() + 1);
    filled.push(args[0].clone());
    filled.push(node);
    filled.extend(args[1..].iter().cloned());
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
        Some(2) if args[0] == "acl" => Some(format!("{} {}", args[0], args[1])),
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
        ["propose", key_file, channel, rest @ ..] => propose(key_file, channel, rest, auth),
        ["join", api, invite_file, key_file, rest @ ..] if auth.is_empty() => {
            join(api, invite_file, key_file, rest)
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
