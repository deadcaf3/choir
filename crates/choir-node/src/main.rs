//! choir-node daemon entry point.
//!
//! Usage: `choir-node <repo-root> [port] [--create owner/name.git]...
//! [--auth-file path] [--keys-file path] [--reviewers-file path]
//! [--require-assignment] [--protected-refs path] [--require-review]
//! [--review-retention count] [--review-lapse-after-secs seconds]
//! [--bind addr]
//! [--tls-cert cert.pem --tls-key key.pem]`
//!
//! Binds 127.0.0.1 by default. `--auth-file` points at a
//! `user:token`-per-line file (0600; never in-repo) and turns on
//! mandatory basic auth. `--keys-file` turns on the platform API; each
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
//! `--require-review` additionally refuses to move a protected ref to
//! any commit no approved review named, and refuses to delete one at
//! all — including for this daemon's own pushes. `--review-retention`
//! keeps at most that many live reviews when completed reviews can be
//! archived. Incomplete reviews are never killed by default;
//! `--review-lapse-after-secs` is the explicit operator policy that lets
//! an over-limit incomplete review lapse. `--bind` with a non-loopback
//! address is refused unless TLS is configured.

use choir_node::platform::ReviewRetention;
use choir_node::{AuthTable, Node, Platform};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    for flag in ["--review-retention", "--review-lapse-after-secs"] {
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

    let mut node = Node::bind_full(&root, &bind, port, auth, tls)?;
    if let Some(i) = rest.iter().position(|a| a == "--keys-file") {
        let path = rest.get(i + 1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "--keys-file needs a path")
        })?;
        let signers = choir_node::parse_keys_file(std::path::Path::new(path))?;
        let count = signers.len();
        let mut registry = choir_identity::Registry::new();
        for signer in &signers {
            registry
                .register(&signer.key)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))?;
        }
        let state_dir = root.join(".choir");
        std::fs::create_dir_all(&state_dir)?;
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
            std::fs::write(&key_path, key.secret_bytes())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
            }
            key
        };
        let log_path = state_dir.join("ops.jsonl");
        let log = choir_oplog::FileLog::open(&log_path)
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        // Hot-reload, all three halves: appending a key line takes effect
        // on the next failed signature check for submissions, and on the
        // next request for push-certificate verification and for channel
        // name bindings (a tightening, so it must not wait for a failure).
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
        .with_log_path(log_path);
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
                        "protected refs require an approved review to land \
                         (this daemon's own pushes included)"
                    );
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
        {
            // Without a pool nothing can ever be assigned, so every
            // review would stall unassigned. Refuse the combination
            // rather than serve a review system that cannot finish.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--require-assignment and --protected-refs need --reviewers-file",
            ));
        }
        node.enable_platform(platform);
        eprintln!("platform API enabled ({count} actor keys)");
    } else if review_retention.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--review-retention needs --keys-file",
        ));
    }
    let mut create_next = false;
    for a in rest {
        if create_next {
            match node.create_repo(a) {
                Ok(()) => eprintln!("created {a}"),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            create_next = false;
        } else if a == "--create" {
            create_next = true;
        }
    }
    eprintln!(
        "choir-node serving {} on {}://{}:{}",
        root.display(),
        if tls_on { "https" } else { "http" },
        bind,
        node.port()
    );
    node.serve_forever();
    Ok(())
}
