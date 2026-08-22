//! choir-node daemon entry point.
//!
//! Configured usage: `choir-node <repo-root> <port> [--create owner/name.git]...
//! [--auth-file path] [--acl-file path] [--keys-file path] [--reviewers-file path]
//! [--invite-binds-keys]
//! [--require-assignment] [--protected-refs path] [--require-review]
//! [--require-scope]
//! [--reviewer-conflict-graph path --reviewer-conflict-distance hops]
//! [--review-retention count] [--review-lapse-after-secs seconds]
//! [--newcomer-audit path --newcomer-adjudications path]
//! [--review-adjudications path]
//! [--hooks-file path] [--journal path]
//! [--ci-command path --queue-tree dir]
//! [--request-log path [--request-log-max-bytes n]]
//! [--rate-limit-api per-minute] [--rate-limit-git per-minute]
//! [--quota-push-bytes n] [--quota-workspaces n]
//! [--api-body-limit bytes] [--batch-limit operations] [--ready-min-free-bytes bytes]
//! [--read-only-browser] [--site-repo owner/name]
//! [--bind addr] [--ssh-handoff path]
//! [--accounts-file path [--ssh-authorized-keys path [--ssh-shim path]]]
//! [--tls-cert cert.pem --tls-key key.pem]`. With no arguments it defaults
//! to `./repos` on port 8417; configured invocations must fill the port slot.
//!
//! Binds 127.0.0.1 by default. `--auth-file` points at a
//! `user:token`-per-line file (0600; never in-repo) and turns on
//! mandatory basic auth. It authenticates and nothing more: without
//! `--acl-file` every credential reaches every repository, which the
//! node says out loud at startup. `--acl-file` (D29) adds the
//! per-repository decision — `<user> <repo|*|@node> <level>` per line,
//! `read` < `write` < `own`, `auditor` on `@node`, fail closed, reloaded on
//! mtime — and requires `--auth-file`, since it grades authenticated
//! users. `--keys-file` turns on the platform API; each
//! line is `<64-char hex>` or `<name> <64-char hex>`, where the optional
//! name binds that key to one review channel (a key with no name is
//! unconstrained, as every key was before the column existed). The op log
//! persists at `<repo-root>/.choir/ops.jsonl`. `--reviewers-file`
//! (one eligible reviewer name per line) lets the node assign
//! reviewers to review requests that name none;
//! `--require-assignment` additionally refuses requests that name
//! their own reviewers, and `--protected-refs` (one
//! `<repo>:<refname>` pattern per line, trailing `*` allowed) refuses
//! them only for reviews landing on a matching ref.
//! `--reviewer-conflict-graph` carries undirected `<operator> <operator>`
//! edges and, with the explicitly chosen `--reviewer-conflict-distance`,
//! excludes nearby operators from future draws. The graph is re-read per
//! draw and an unusable graph leaves the review unassigned.
//! `--require-review` additionally refuses to move a protected ref to
//! any commit without approval weight from two distinct operators, and
//! refuses to delete one at all — including for this daemon's own pushes.
//! `--require-scope` admits only ops whose author signed a scope naming
//! this node and a log head still in the window, which is what makes a
//! captured signature unreplayable — on this node after the state it
//! expected returns, and on any other node at all. Off by default because
//! it refuses clients that predate scopes, not because unscoped is safe.
//! `--journal` appends one JSON object per admission decision: the
//! author, workspace, op type, whether it was accepted, the refusal
//! when there was one, and how long the decision took. It also records
//! queue depth, speculation-window moves with their cause, and every
//! lost compare-and-swap. It is derived data — nothing replays it, no
//! hash covers it, and losing it changes no decision — so the writing
//! happens on its own thread and a record is dropped rather than
//! allowed to stall the writer. Without the flag nothing is recorded
//! and nothing is built, which the allocation budget enforces.
//!
//! `--hooks-file` (D32) subscribes operator-named URLs to refs that
//! land: one `<repo:refname pattern> <url> <secret> [allow-private]` per
//! line, the same trailing-`*` patterns `--protected-refs` uses,
//! reloaded on mtime. Delivery runs on its own thread and is
//! best-effort — every attempt and every queue-full drop is recorded in
//! `<repo-root>/.choir/hooks.jsonl` — because a receiver must never be
//! able to delay op admission. It needs `--keys-file`, since refs reach
//! the log through the platform sequencer.
//! `--review-retention` keeps at most that many live reviews when
//! completed reviews can be archived. Incomplete reviews are never killed by default;
//! `--review-lapse-after-secs` is the explicit operator policy that lets
//! an over-limit incomplete review lapse. `--request-log` (D33) records
//! one JSON line per served request — method, path, status, response
//! bytes, elapsed microseconds and the authenticated user — rotating to
//! `<path>.1` past `--request-log-max-bytes` (32 MiB by default), and
//! never writing a header, a body or a query string. `--rate-limit-api`
//! and `--rate-limit-git` are per-user requests-per-minute ceilings, each
//! a token bucket holding one minute's burst, answered `429` with
//! `Retry-After`; the loopback hook callback and any holder of an `@node`
//! grant are exempt, because a limiter that can lock out the operator is
//! worse than no limiter. `--quota-push-bytes` and `--quota-workspaces`
//! (D37) are the per-user ceilings a rate does not imply: the largest git
//! request body one user may send, refused `413` before `git
//! http-backend` is spawned so no `pre-receive` hook ever runs for it,
//! and the most workspaces one user may hold at once, refused `403` and
//! counted from a projection replayed out of the op log so the count
//! survives a restart. They carry the limiter's exemptions unchanged.
//! All five need `--auth-file`, since all five
//! are per authenticated user. `--bind` with a non-loopback
//! address is refused unless TLS is configured.
//! `--ssh-handoff` (D31) writes this daemon's base URL and loopback
//! secret to a 0600 file for the `choir-ssh` forced command, which is how
//! a push arriving over SSH reaches the same sequencer an HTTP push does.
//! `--accounts-file` (D36) turns on invite-only credential self-service:
//! a holder of `@node write` mints a single-use expiring invite at
//! `POST /api/accounts/invite`, its holder redeems it at
//! `POST /api/accounts/redeem` for a token and a registered ssh key, and
//! `POST /api/accounts/revoke` deletes the account so the token stops
//! working on the next request. Issued grants join the `--acl-file`
//! table, which is why the flag needs both `--auth-file` and
//! `--acl-file`; the store can never issue `@node`, so self-service
//! cannot mint an auditor or a rate-limit exemption.
//! `--ssh-authorized-keys` writes the D31 forced commands for every
//! registered key to a file `sshd` is pointed at once — generated, so
//! hand edits are lost — using the `choir-ssh` beside this binary unless
//! `--ssh-shim` names another.
//!
//! Two invocations do not serve anything. `--verify-log <op-log>`
//! refuses an unsupported format version, a broken chain or a torn
//! tail. `--export <repo-root> <dir>` writes a portable copy of a
//! node -- the log, a git bundle per repository, and a versioned
//! manifest, with no secret and no network. `--verify-export <dir>`
//! settles what one claims, and `--import <dir> <repo-root>` places one
//! into a fresh root. See [`choir_node::portable`].

use choir_node::platform::ReviewRetention;
use choir_node::{AuthTable, Node, Platform};

/// Renders an export report as the one line both `--export` and
/// `--verify-export` print, so the two are comparable by eye.
fn describe(report: &choir_node::portable::Report) -> String {
    format!(
        "{} records through {}, {} bundles, {} refs matched{}",
        report.records,
        report.head.as_deref().unwrap_or("an empty log"),
        report.bundles,
        report.refs,
        match report.ahead {
            0 => String::new(),
            n => format!(", {n} ahead of the log"),
        }
    )
}

/// Renders a refusal as a sentence and leaves with a nonzero status.
///
/// `fn main() -> io::Result<()>` prints a returned error with `Debug`,
/// so every mistyped flag in this binary came out as
/// `Error: Custom { kind: InvalidInput, error: "..." }` — the message
/// intact and wrapped in the name of the type that carried it. D58 made
/// this the rule for `choir-cli` and never reached the daemon, which is
/// the only reason it survived: a reader who mistypes a flag is told
/// what to fix, not which enum variant holds the telling.
///
/// The status stays 1, unchanged. `scripts/restore_from_backup.sh` and
/// `scripts/pull_backup.sh` branch on nonzero and not on a value, so
/// splitting usage out to 2 would be a behaviour change nobody asked
/// for. `Node::serve_forever` still leaves 75 for a supervisor on a
/// durability failure, from inside the library and past this function
/// entirely.
fn main() {
    if let Err(error) = run() {
        // A usage line already opens with the binary's name, because it
        // is meant to be copied; prefixing it would print that name
        // twice in one sentence.
        let text = error.to_string();
        if text.starts_with("usage:") {
            eprintln!("{text}");
        } else {
            eprintln!("choir-node: {text}");
        }
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--verify-log") {
        let path = args.get(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--verify-log needs an op-log path",
            )
        })?;
        if args.len() != 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: choir-node --verify-log <op-log>",
            ));
        }
        let report = choir_oplog::repair::verify(std::path::Path::new(path)).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("could not verify {path}: {error:?}"),
            )
        })?;
        if let Some(fault) = report.fault {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("op log refused at record {}: {fault}", fault.position()),
            ));
        }
        if report.torn_tail_bytes != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "op log has an unterminated {}-byte tail; backups and restores require a complete record boundary",
                    report.torn_tail_bytes
                ),
            ));
        }
        eprintln!(
            "verified {} op-log records through {}",
            report.intact_records,
            report
                .head
                .as_ref()
                .map_or_else(|| "empty".to_string(), choir_hash::ContentHash::to_hex)
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--export") {
        let (Some(root), Some(dest), 3) = (args.get(1), args.get(2), args.len()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: choir-node --export <repo-root> <export-dir>",
            ));
        };
        let report =
            choir_node::portable::export(std::path::Path::new(root), std::path::Path::new(dest))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        eprintln!("exported {} to {dest}: {}", root, describe(&report));
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--import") {
        let (Some(dir), Some(root), 3) = (args.get(1), args.get(2), args.len()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: choir-node --import <export-dir> <repo-root>",
            ));
        };
        let report =
            choir_node::portable::import(std::path::Path::new(dir), std::path::Path::new(root))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        eprintln!("imported {dir} into {root}: {}", describe(&report));
        eprintln!("the secrets a boot needs are not in an export: see docs/runbook-restore.md");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--verify-export") {
        let (Some(dir), 2) = (args.get(1), args.len()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: choir-node --verify-export <export-dir>",
            ));
        };
        let report = choir_node::portable::verify(std::path::Path::new(dir))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        eprintln!("verified {dir}: {}", describe(&report));
        return Ok(());
    }
    let root = args
        .first()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("./repos"));
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(8417);

    let rest = &args[2.min(args.len())..];
    let auth = match rest.iter().position(|a| a == "--auth-file") {
        Some(i) => {
            let path = rest.get(i + 1).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "--auth-file needs a path")
            })?;
            let mut table = AuthTable::new();
            for line in std::fs::read_to_string(path)?.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (user, token) = line.split_once(':').ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "auth file lines must be user:token",
                    )
                })?;
                table.insert(user.to_string(), token.to_string());
            }
            eprintln!("auth enabled ({} users)", table.len());
            Some(table)
        }
        None => None,
    };

    let flag_value = |name: &str| -> Option<&String> {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
    };
    let bind = flag_value("--bind")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".into());
    for flag in [
        "--review-retention",
        "--review-lapse-after-secs",
        "--reviewer-conflict-graph",
        "--reviewer-conflict-distance",
        "--newcomer-audit",
        "--newcomer-adjudications",
        "--review-adjudications",
        "--acl-file",
        "--hooks-file",
        "--journal",
        "--ssh-handoff",
        "--request-log",
        "--request-log-max-bytes",
        "--rate-limit-api",
        "--rate-limit-git",
        "--accounts-file",
        "--ssh-authorized-keys",
        "--ssh-shim",
        "--quota-push-bytes",
        "--quota-workspaces",
        "--api-body-limit",
        "--batch-limit",
        "--ready-min-free-bytes",
        "--site-repo",
        "--ci-command",
        "--queue-tree",
    ] {
        if rest.iter().any(|arg| arg == flag) && flag_value(flag).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{flag} needs a value"),
            ));
        }
    }
    let review_retention_count = flag_value("--review-retention")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--review-retention needs a non-negative integer",
                )
            })
        })
        .transpose()?;
    let review_lapse_after = flag_value("--review-lapse-after-secs")
        .map(|value| {
            value
                .parse::<u64>()
                .map(std::time::Duration::from_secs)
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "--review-lapse-after-secs needs a non-negative integer",
                    )
                })
        })
        .transpose()?;
    let reviewer_conflict_graph = flag_value("--reviewer-conflict-graph");
    let reviewer_conflict_distance = flag_value("--reviewer-conflict-distance")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--reviewer-conflict-distance needs a non-negative integer",
                )
            })
        })
        .transpose()?;
    let reviewer_conflict_policy = match (reviewer_conflict_graph, reviewer_conflict_distance) {
        (Some(path), Some(distance)) => Some((std::path::PathBuf::from(path), distance)),
        (None, None) => None,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--reviewer-conflict-graph and --reviewer-conflict-distance must be given together",
            ));
        }
    };
    let newcomer_policy = match (
        flag_value("--newcomer-audit"),
        flag_value("--newcomer-adjudications"),
    ) {
        (Some(audit), Some(adjudications)) => Some((
            std::path::PathBuf::from(audit),
            std::path::PathBuf::from(adjudications),
        )),
        (None, None) => None,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--newcomer-audit and --newcomer-adjudications must be given together",
            ));
        }
    };
    let review_adjudications = flag_value("--review-adjudications").map(std::path::PathBuf::from);
    let api_body_limit = flag_value("--api-body-limit")
        .map(|value| {
            value.parse::<std::num::NonZeroU64>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--api-body-limit needs a positive integer (bytes per API request)",
                )
            })
        })
        .transpose()?
        .unwrap_or_else(|| {
            std::num::NonZeroU64::new(choir_node::DEFAULT_API_BODY_BYTES)
                .expect("default API body limit is nonzero")
        });
    let batch_limit = flag_value("--batch-limit")
        .map(|value| {
            value.parse::<std::num::NonZeroUsize>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--batch-limit needs a positive integer (operations per batch)",
                )
            })
        })
        .transpose()?
        .unwrap_or_else(|| {
            std::num::NonZeroUsize::new(choir_node::platform::DEFAULT_BATCH_OPS)
                .expect("default batch limit is nonzero")
        });
    let ready_min_free_bytes = flag_value("--ready-min-free-bytes")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--ready-min-free-bytes needs a non-negative integer",
                )
            })
        })
        .transpose()?
        .unwrap_or(choir_node::DEFAULT_READY_MIN_FREE_BYTES);
    if review_lapse_after.is_some() && review_retention_count.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--review-lapse-after-secs needs --review-retention",
        ));
    }
    let review_retention = review_retention_count.map(|count| {
        let retention = ReviewRetention::keep(count);
        review_lapse_after.map_or(retention, |age| retention.lapse_incomplete_after(age))
    });
    let tls = match (flag_value("--tls-cert"), flag_value("--tls-key")) {
        (Some(cert), Some(key)) => Some((std::fs::read(cert)?, std::fs::read(key)?)),
        (None, None) => None,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--tls-cert and --tls-key must be given together",
            ))
        }
    };
    let tls_on = tls.is_some();

    if newcomer_policy.is_some() && !rest.iter().any(|arg| arg == "--keys-file") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "newcomer instrumentation needs --keys-file",
        ));
    }

    let auth_enabled = auth.is_some();
    let mut node = Node::bind_full(&root, &bind, port, auth, tls)?;
    node.enable_api_body_limit(api_body_limit);
    node.enable_ready_min_free_bytes(ready_min_free_bytes);
    eprintln!("limits: {api_body_limit} API body bytes, {batch_limit} operations per batch");
    eprintln!("readiness: at least {ready_min_free_bytes} free storage bytes");
    if rest.iter().any(|arg| arg == "--read-only-browser") {
        node.disable_browser_writes();
        eprintln!("browser: read-only; mutations require the signed CLI");
    }
    // One repository as the whole browser surface, for a node serving a
    // project's own domain. Validated against the same grammar a repo
    // path takes, because an unchecked value here would be a name the
    // router looks up on disk.
    if let Some(site) = flag_value("--site-repo") {
        node.serve_single_repository(site)?;
        eprintln!("site: this node presents {site} and no repository index");
    }
    // One writer per state dir, process-enforced: a second daemon on the
    // same root would append to the same ops.jsonl and fork the chain.
    // Held until after serve_forever; a stale lock from a dead process
    // is reaped automatically.
    let mut state_lock = None;
    if let Some(i) = rest.iter().position(|a| a == "--keys-file") {
        let path = rest.get(i + 1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "--keys-file needs a path")
        })?;
        let signers = choir_node::parse_keys_file(std::path::Path::new(path))?;
        let count = signers.len();
        let mut registry = choir_identity::Registry::new();
        for signer in &signers {
            registry.register(&signer.key).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"))
            })?;
        }
        let state_dir = root.join(".choir");
        std::fs::create_dir_all(&state_dir)?;
        state_lock = Some(choir_fs::WorkdirLock::acquire(&state_dir).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                match e {
                    choir_fs::LockError::Locked => format!(
                        "{} is in use by another running choir-node; two writers \
                         on one op log would fork the chain",
                        state_dir.display()
                    ),
                    choir_fs::LockError::Io(e) => format!("locking {}: {e}", state_dir.display()),
                },
            )
        })?);
        // Same keys, OpenSSH form: what push-certificate verification
        // (git push --signed) checks signers against.
        choir_node::write_allowed_signers(&root, &signers)?;
        // Node key: persisted so git-derived ops keep one author across
        // restarts. 32 secret bytes, file readable by the daemon user only.
        let key_path = state_dir.join("node.key");
        let node_key = if key_path.exists() {
            let bytes = std::fs::read(&key_path)?;
            let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "node.key must be 32 bytes")
            })?;
            choir_identity::ActorKey::from_secret_bytes(&bytes)
        } else {
            let key = choir_identity::ActorKey::generate();
            // Atomic and 0600 from creation: no window where the secret
            // is world-readable or half-written.
            choir_fs::write_atomic_private(&key_path, key.secret_bytes())?;
            key
        };
        // The node's identity is pinned to the log it writes into. The
        // branch above mints a fresh key whenever the key file is absent,
        // which is correct on a first start and catastrophic on a
        // migration: copy `ops.jsonl` without the key and the daemon comes
        // up happily, signing every subsequent git-derived op as a
        // different actor than the entries already in the log. Nothing
        // downstream notices, because both identities are individually
        // valid — the log simply changes author mid-stream.
        //
        // So the fingerprint (the actor id, a hash of the public key —
        // public, never the secret) is recorded beside the log on first
        // start and compared on every start after. A mismatch is refused
        // rather than warned about: a node that has already lost its
        // identity should not be allowed to append under a new one.
        // The same reasoning appears in other peer-to-peer node
        // implementations that pin an identity beside their state.
        let fingerprint_path = state_dir.join("node.fingerprint");
        let fingerprint = node_key.actor_id().to_hex();
        match std::fs::read_to_string(&fingerprint_path) {
            Ok(pinned) if pinned.trim() != fingerprint => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "node identity changed: {} pins {}, but the loaded key is {}. \
                         The op log was almost certainly moved without its key. \
                         Restore the original key, or if the change is intended, \
                         delete {} and accept that the log changes author here.",
                        fingerprint_path.display(),
                        pinned.trim(),
                        fingerprint,
                        fingerprint_path.display(),
                    ),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Atomic: a torn pin would refuse every later start.
                choir_fs::write_atomic(&fingerprint_path, format!("{fingerprint}\n"))?;
            }
            Err(e) => return Err(e),
        }

        let log_path = state_dir.join("ops.jsonl");
        let log = choir_oplog::FileLog::open(&log_path)
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        // An unclean stop is a fact the operator has to be told, even
        // though recovery already succeeded: these bytes were never
        // acknowledged to a submitter, so nothing downstream is missing,
        // but a power cut that leaves no trace reads as a clean restart.
        if log.torn_tail_bytes() > 0 {
            eprintln!(
                "choir: recovered {} from an unclean stop: {} byte(s) of a partly written \
                 trailing op were discarded. They were never acknowledged to a submitter, \
                 so no accepted operation was lost.",
                log_path.display(),
                log.torn_tail_bytes(),
            );
        }
        // Hot-reload, all three halves: appending or removing a key line
        // takes effect before the next submission signature check, and on
        // the next request for push-certificate verification and channel
        // name bindings.
        node.watch_keys_file(path.into());
        // Same file the sequencer appends to: readers that fall behind
        // the in-memory /api/log window resync from it.
        let mut platform = match review_retention {
            Some(retention) => Platform::start_reloading_with_review_retention(
                registry,
                Box::new(log),
                node_key,
                Some(path.into()),
                retention,
            ),
            None => Platform::start_reloading(registry, Box::new(log), node_key, Some(path.into())),
        }
        .map_err(std::io::Error::other)?
        .with_log_path(log_path)
        // On by default, not behind a flag: the point of measuring the
        // latency gate in production is that nobody has to remember to
        // turn it on before the node is slow. Separate from ops.jsonl
        // because a breach is an observation about this node, not part of
        // the ordered history anyone else replays.
        .with_lag_log(state_dir.join("lag.jsonl"));
        platform = platform.with_batch_limit(batch_limit.get());
        if let Some((audit, adjudications)) = newcomer_policy {
            let incumbents = signers
                .iter()
                .map(|signer| signer.actor_id.clone())
                .collect();
            platform = platform
                .with_newcomer_audit(audit, adjudications, incumbents)
                .map_err(std::io::Error::other)?;
            eprintln!("newcomer harm audit enabled");
        }
        if let Some(path) = review_adjudications {
            platform = platform
                .with_review_adjudications(path)
                .map_err(std::io::Error::other)?;
            eprintln!("review adjudications enabled (T2 stays indeterminate)");
        }
        // D32. Delivery records go beside the lag log, for the same
        // reason: an attempt to notify somebody is an observation about
        // this node, not part of the ordered history anyone replays.
        // Derived data, so it is installed like a log sink rather than
        // like policy: nothing downstream reads it, and a node that
        // fails to open it should say so at startup rather than
        // silently record nothing.
        if let Some(path) = flag_value("--journal") {
            platform = platform.with_journal(std::path::Path::new(&path))?;
        }
        if let Some(path) = flag_value("--hooks-file") {
            platform = platform
                .with_hooks(path.into(), state_dir.join("hooks.jsonl"))
                .map_err(std::io::Error::other)?;
        }
        if rest.iter().any(|a| a == "--require-scope") {
            platform = platform.with_required_scope();
            eprintln!(
                "signed op scopes required (a signature is admissible on this log \
                 once, and on no other node)"
            );
        }
        if let Some(count) = review_retention_count {
            match review_lapse_after {
                Some(age) => eprintln!(
                    "review retention enabled ({count} live, incomplete lapse after {}s)",
                    age.as_secs()
                ),
                None => eprintln!(
                    "review retention enabled ({count} live, incomplete reviews never lapse)"
                ),
            }
        }
        if let Some(pool) = flag_value("--reviewers-file") {
            platform = platform.with_reviewer_pool(pool.into());
            eprintln!("reviewer assignment enabled ({pool})");
            if let Some((graph, distance)) = reviewer_conflict_policy {
                platform = platform.with_reviewer_conflict_graph(graph, distance);
                eprintln!("reviewer conflict graph enabled (distance {distance})");
            }
            if rest.iter().any(|a| a == "--require-assignment") {
                platform = platform.with_required_assignment();
                eprintln!("reviewer assignment required (self-named reviewers refused)");
            }
            if let Some(refs) = flag_value("--protected-refs") {
                platform = platform.with_protected_refs(refs.into());
                eprintln!("protected refs enabled ({refs})");
                if rest.iter().any(|a| a == "--require-review") {
                    platform = platform.with_required_review();
                    eprintln!(
                        "protected refs require approval from two distinct operators to land \
                         (this daemon's own pushes included)"
                    );
                    // D42: the same file the HTTP layer authorizes
                    // against, read by admission for `own` grants alone.
                    // Announced only when it changes a landing rule,
                    // which is here: without `--require-review` nothing
                    // gates a landing and ownership decides nothing.
                    if let Some(acl) = flag_value("--acl-file") {
                        platform = platform.with_acl_file(acl.into());
                        eprintln!(
                            "repository ownership enabled ({acl}): on a repo somebody holds \
                             `own` over, one owner's assent lands a protected ref and the \
                             two-operator rule does not also apply"
                        );
                    }
                }
            } else if rest.iter().any(|a| a == "--require-review") {
                // Nothing is protected, so the flag would silently do
                // nothing — and a gate that silently does nothing is
                // worse than no gate.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--require-review needs --protected-refs",
                ));
            }
        } else if rest.iter().any(|a| a == "--require-assignment")
            || flag_value("--protected-refs").is_some()
            || reviewer_conflict_policy.is_some()
        {
            // Without a pool nothing can ever be assigned, so every
            // review would stall unassigned. Refuse the combination
            // rather than serve a review system that cannot finish.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "review assignment policy needs --reviewers-file",
            ));
        }
        node.enable_platform(platform);
        eprintln!("platform API enabled ({count} actor keys)");

        // D5/D68. The merge queue, and only when both halves are named:
        // a CI command with nowhere to run it, or a tree with nothing to
        // run in it, is a flag that would quietly do nothing.
        match (flag_value("--ci-command"), flag_value("--queue-tree")) {
            (Some(command), Some(tree)) => {
                let spec =
                    choir_queue::differential_ledger::load_command(std::path::Path::new(&command))
                        .map_err(std::io::Error::other)?;
                node.enable_queue(choir_node::queue_api::QueueConfig {
                    tree: std::path::PathBuf::from(tree),
                    command: spec,
                });
                eprintln!("merge queue enabled (POST /api/queue/run)");
            }
            (None, None) => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--ci-command and --queue-tree are needed together",
                ))
            }
        }
    } else if rest.iter().any(|a| a == "--require-scope") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--require-scope needs --keys-file",
        ));
    } else if review_retention.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--review-retention needs --keys-file",
        ));
    } else if flag_value("--hooks-file").is_some() {
        // Refs reach the log through the platform sequencer, git pushes
        // included, so without it nothing could ever fire and the flag
        // would be decorative.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--hooks-file needs --keys-file",
        ));
    }
    // D29. Authorization is keyed on the authenticated username, so an
    // ACL without authentication would grade everybody as `anon` and
    // grant them whatever `anon` holds. Refusing the combination is the
    // difference between a fail-closed table and a decorative one.
    match flag_value("--acl-file") {
        Some(path) if auth_enabled => {
            node.watch_acl_file(path.into())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        }
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--acl-file needs --auth-file: authorization is per authenticated user",
            ))
        }
        // Said out loud rather than assumed. Every operator running
        // without an ACL should know that one credential reaches
        // everything, especially before issuing a second one.
        None => {
            eprintln!("acl: no --acl-file, so every authenticated actor reaches every repository")
        }
    }
    // D36. Credential self-service. After the ACL, because it refuses to
    // start without one — a token issued on a node with nothing to grade
    // it against is a token to every repository. Before the handoff,
    // because the handoff carries the store's path to the SSH shim.
    if let Some(path) = flag_value("--accounts-file") {
        let keys_out = match flag_value("--ssh-authorized-keys") {
            Some(out) => {
                let Some(handoff) = flag_value("--ssh-handoff") else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "--ssh-authorized-keys needs --ssh-handoff: a forced command without \
                         one refuses pushes rather than running them unsequenced",
                    ));
                };
                // Beside this binary unless told otherwise, which is
                // where an install that shipped the pair together puts
                // it. Named rather than searched for on `PATH`: what
                // ends up in a forced command should be a path the
                // operator can read back, not a lookup done later.
                let shim = match flag_value("--ssh-shim") {
                    Some(shim) => std::path::PathBuf::from(shim),
                    None => std::env::current_exe()?
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join("choir-ssh"),
                };
                Some(choir_node::accounts::SshKeysOut {
                    path: out.into(),
                    shim,
                    root: root.clone(),
                    handoff: handoff.into(),
                })
            }
            None => None,
        };
        let generated = keys_out.as_ref().map(|out| out.path.clone());
        // The file the flag binds into is the one already being watched,
        // not a second list: a key admitted here has to be admitted the
        // same way an operator's paste is, or "revoke" would mean two
        // different edits depending on how the key arrived.
        let actor_keys = if rest.iter().any(|arg| arg == "--invite-binds-keys") {
            let Some(keys_file) = flag_value("--keys-file") else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--invite-binds-keys needs --keys-file: there is no trusted-keys file to \
                     bind a redeemed key into",
                ));
            };
            eprintln!(
                "accounts: redemption may bind one actor key into {keys_file}. The invite is \
                 still the operator's assertion, and revoking is still deleting that line"
            );
            Some(std::path::PathBuf::from(keys_file))
        } else {
            None
        };
        node.enable_accounts(path.into(), keys_out, actor_keys)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        eprintln!(
            "accounts: invite-only self-service at /api/accounts (an @node write grant issues; \
             the store can never grant @node)"
        );
        if let Some(path) = generated {
            eprintln!(
                "accounts: writing {} — point sshd's AuthorizedKeysFile at it once, and never \
                 edit it: it is regenerated on every account change",
                path.display()
            );
        }
    } else if flag_value("--ssh-authorized-keys").is_some() {
        // Nothing would ever be written to it, and a generated file that
        // is never generated reads as "no keys are registered".
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--ssh-authorized-keys needs --accounts-file: the keys it writes are registered \
             through self-service",
        ));
    }
    // D31. The SSH shim runs as a separate process with no way to learn
    // an ephemeral port or a per-process secret, so the daemon writes
    // both where the shim's `--handoff` points. Written on every start,
    // because both values change on every start.
    if let Some(path) = flag_value("--ssh-handoff") {
        node.write_ssh_handoff(std::path::Path::new(path))?;
        eprintln!("ssh handoff written to {path} (git-over-ssh pushes reach the sequencer)");
    }
    // D33. Both halves key on the authenticated username, so both need
    // `--auth-file` for the reason `--acl-file` does: without one there is
    // no subject to attribute a line to or to meter, and a shared `anon`
    // bucket is a self-inflicted denial of service rather than a limit.
    let request_log_max_bytes = flag_value("--request-log-max-bytes")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--request-log-max-bytes needs a non-negative integer",
                )
            })
        })
        .transpose()?
        .unwrap_or(choir_node::limits::DEFAULT_LOG_MAX_BYTES);
    match flag_value("--request-log") {
        Some(path) if auth_enabled => {
            node.enable_request_log(path.into(), request_log_max_bytes)?;
            eprintln!(
                "request log enabled ({path}; rotates to <path>.1 past {request_log_max_bytes} \
                 bytes, and never records a header, a body or a query string)"
            );
        }
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--request-log needs --auth-file: the log attributes each request to a user",
            ))
        }
        None => {}
    }
    let rate_flag = |name: &str| -> std::io::Result<Option<std::num::NonZeroU32>> {
        flag_value(name)
            .map(|value| {
                value.parse::<std::num::NonZeroU32>().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{name} needs a positive integer (requests per minute)"),
                    )
                })
            })
            .transpose()
    };
    let api_rate = rate_flag("--rate-limit-api")?;
    let git_rate = rate_flag("--rate-limit-git")?;
    if (api_rate.is_some() || git_rate.is_some()) && !auth_enabled {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--rate-limit-api/--rate-limit-git need --auth-file: the bucket is per user",
        ));
    }
    node.enable_rate_limit(api_rate, git_rate);
    if let Some(per_minute) = api_rate {
        eprintln!("rate limit: {per_minute} API requests per minute per user");
    }
    if let Some(per_minute) = git_rate {
        eprintln!("rate limit: {per_minute} git requests per minute per user");
    }
    if api_rate.is_some() || git_rate.is_some() {
        // Said out loud, because it is the difference between a limiter
        // and a lockout, and an operator should know which one they have.
        eprintln!(
            "rate limit: the loopback hook callback and any holder of an @node grant are exempt"
        );
    }
    // D37. Same subject as the D33 flags, so the same requirement: a
    // quota with nobody to charge it to is not a quota.
    let push_bytes = flag_value("--quota-push-bytes")
        .map(|value| {
            value.parse::<std::num::NonZeroU64>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--quota-push-bytes needs a positive integer (bytes per git request)",
                )
            })
        })
        .transpose()?;
    let max_workspaces = flag_value("--quota-workspaces")
        .map(|value| {
            value.parse::<std::num::NonZeroU32>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--quota-workspaces needs a positive integer (workspaces per user)",
                )
            })
        })
        .transpose()?;
    if (push_bytes.is_some() || max_workspaces.is_some()) && !auth_enabled {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--quota-push-bytes/--quota-workspaces need --auth-file: the quota is per user",
        ));
    }
    node.enable_quotas(push_bytes, max_workspaces);
    if let Some(bytes) = push_bytes {
        eprintln!("quota: at most {bytes} bytes in one git request per user");
    }
    if let Some(count) = max_workspaces {
        eprintln!("quota: at most {count} workspaces held per user");
    }
    if push_bytes.is_some() || max_workspaces.is_some() {
        eprintln!("quota: exempt exactly where the rate limit is — the loopback hook callback and any holder of an @node grant");
    }
    let mut create_next = false;
    for a in rest {
        if create_next {
            match node.create_repo(a) {
                Ok(()) => eprintln!("created {a}"),
                // Already there is not nothing to do. The repo may have
                // arrived from a backup bundle, which carries objects and
                // refs and no hook at all — served normally, and every push
                // into it bypassing the sequencer. It may also predate a
                // move, leaving `gpg.ssh.allowedSignersFile` pointed at a
                // root that no longer exists. Adoption is idempotent, so
                // the ordinary restart pays a few `git config` calls and
                // says nothing.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => node.adopt_repo(a)?,
                Err(e) => return Err(e),
            }
            create_next = false;
        } else if a == "--create" {
            create_next = true;
        }
    }
    // First line of every start, so the log says which build produced
    // everything below it.
    eprintln!("choir-node {}", choir_node::build_line());
    // Before the first request, so nothing races the repair. Silent when
    // the log and the repos already agree, which is every ordinary start.
    let repair = node.reconcile_refs();
    for name in &repair.applied {
        eprintln!("choir: reconciled {name} — git was behind the log and has been moved to it");
    }
    for name in &repair.retracted {
        eprintln!(
            "choir: retracted {name} — the log named a commit this repo does not have, so the \
             log now agrees with git"
        );
    }
    for note in &repair.unreconciled {
        eprintln!(
            "choir: UNRECONCILED {note} — the log and this repo disagree and only an \
             operator can say which is right"
        );
    }
    eprintln!(
        "choir-node serving {} on {}://{}:{}",
        root.display(),
        if tls_on { "https" } else { "http" },
        bind,
        node.port()
    );
    node.serve_forever();
    drop(state_lock);
    Ok(())
}
