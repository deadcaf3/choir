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
//! choir workspace <api> <owner/repo> <name>
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
//! ```
//!
//! Exit codes: 0 = the node accepted, 1 = the node rejected (the JSON
//! error body is printed), 2 = usage error.

use choir_hash::ContentHash;
use choir_identity::ActorKey;
use choir_view::{OpKind, Verdict, ViewOp};

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

/// Signs `op` on attribution channel `channel` and posts it.
///
/// The op is scoped to the node's current head first, so the signature
/// is admissible on this log once and nowhere else.
fn submit(api: &str, key_file: &str, channel: &str, op: &ViewOp, auth: AuthOptions<'_>) -> ! {
    let key = load_key(key_file);
    let (node, head) = log_scope(api, auth);
    let op = op.clone().in_scope(node, head);
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    let body = serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    });
    let (status, resp) = http(api, auth, "choir_submit", body);
    finish(status, &resp);
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
        ["workspace", api, repo, name] => {
            let body = serde_json::json!({ "repo": repo, "name": name });
            let (status, resp) = http(api, auth, "choir_workspace", body);
            finish(status, &resp);
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
        _ => usage(),
    }
}
