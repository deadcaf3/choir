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
//! choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
//! choir reviews <api> <reviewer>
//! choir view <api>
//! ```
//!
//! Exit codes: 0 = the node accepted, 1 = the node rejected (the JSON
//! error body is printed), 2 = usage error.

use choir_identity::ActorKey;
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

/// Signs `op` on attribution channel `channel` and posts it.
fn signed_body(key_file: &str, channel: &str, op: &ViewOp) -> serde_json::Value {
    signed_payload_body(key_file, channel, &op.to_payload())
}

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
    let body = signed_body(key_file, channel, op);
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
