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
//! choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]
//! choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
//! choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
//! choir submit <api> <key-file> <channel> '<op-json>'
//! choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
//! choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
//! choir slash <api> <node-key-file> <id> <reviewer> '<reason>'
//! choir bind <api> <node-key-file> <operator> <key-hex> [channel]
//! choir revoke <api> <node-key-file> <key-hex> '<reason>'
//! choir appeal <api> <attempt-id>
//! choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
//! choir reviews <api> <reviewer>
//! choir view <api>
//! choir triage <api>
//! choir state <api> <channel>
//! choir skill install [--into <dir>]
//! ```
//!
//! Exit codes: 0 = the node accepted, 1 = the node rejected (the JSON
//! error body is printed), 2 = usage error.

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_view::{ArchiveAuthorization, CreateAuthorization, OpKind, Verdict, ViewOp};

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

fn usage() -> ! {
    // Rendered from the surface table, so help can never disagree with
    // the README, the templates, or agents.md.
    eprint!("{}", choir_cli::surface::usage());
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

/// Loads the 32-byte secret key file, creating it (0600) if absent.
fn load_key(path: &str) -> ActorKey {
    if std::path::Path::new(path).exists() {
        let bytes = std::fs::read(path).expect("read key file");
        ActorKey::from_secret_bytes(&bytes.as_slice().try_into().expect("32-byte key file"))
    } else {
        let key = ActorKey::generate();
        std::fs::write(path, key.secret_bytes()).expect("write key file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod key file");
        }
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
fn current_binding(
    api: &str,
    auth: AuthOptions<'_>,
    actor_id: &str,
) -> Option<serde_json::Value> {
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
/// **This is a first-party client and says so.** It decodes into the
/// same `OpEntry` the node encodes from, so a hash agreeing here proves
/// the node agrees with *this build's* definition of the format rather
/// than with an independent reading of `SYNC.md`.
/// `choir-node/tests/it/sync_contract.rs` is the independent one: it
/// rebuilds the canonical bytes by hand and deliberately never calls
/// `content_hash`.
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
    let report = choir_cli::verify::page(&entries, &registry);
    for note in &report.notes {
        eprintln!("choir log: {note}");
    }
    for failure in &report.failures {
        eprintln!("choir log: {failure}");
    }
    eprintln!(
        "choir log: {} entries, chain {}, {} signatures verified, {} unverified",
        entries.len(),
        if report.failures.is_empty() { "holds" } else { "BROKEN" },
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
                        let (status, body) = http(
                            &config.api,
                            auth,
                            "choir_view",
                            serde_json::json!({}),
                        );
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
    let mut index = 0;
    while index < rest.len() {
        let Some(value) = rest.get(index + 1).copied() else {
            usage();
        };
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
    );
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (auth, args) = parse_auth(&args);
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
            let mut body =
                signed_payload_body(key_file, channel, &authorization.to_payload());
            body["repo"] = serde_json::json!(repo);
            body["name"] = serde_json::json!(name);
            body["change"] = serde_json::json!(change_id);
            body["idempotency_key"] = serde_json::json!(idempotency_key);
            let (status, resp) = http(api, auth, "choir_workspace_archive", body);
            finish(status, &resp);
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
                        let Some(value) = it.next().and_then(|v| v.parse().ok()) else { usage() };
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
        ["reviews", api, reviewer] => {
            let (status, resp) = http(
                api,
                auth,
                "choir_reviews",
                serde_json::json!({ "reviewer": reviewer }),
            );
            finish(status, &resp);
        }
        ["view", api] => {
            let (status, resp) = http(api, auth, "choir_view", serde_json::json!({}));
            finish(status, &resp);
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
                if let Err(error) =
                    std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &rendered))
                {
                    eprintln!("choir: cannot write {}: {error}", path.display());
                    std::process::exit(1);
                }
            }
            let doc = serde_json::json!({ "path": path.display().to_string(), "wrote": wrote });
            finish(200, &doc.to_string());
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
