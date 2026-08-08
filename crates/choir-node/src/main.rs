//! choir-node daemon entry point.
//!
//! Usage: `choir-node <repo-root> [port] [--create owner/name.git]...
//! [--auth-file path] [--keys-file path] [--reviewers-file path]
//! [--require-assignment] [--protected-refs path] [--bind addr]
//! [--tls-cert cert.pem --tls-key key.pem]`
//!
//! Binds 127.0.0.1 by default. `--auth-file` points at a
//! `user:token`-per-line file (0600; never in-repo) and turns on
//! mandatory basic auth. `--keys-file` (ed25519 public keys, one
//! 64-char hex line each) turns on the platform API; the op log
//! persists at `<repo-root>/.choir/ops.jsonl`. `--reviewers-file`
//! (one eligible reviewer name per line) lets the node assign
//! reviewers to review requests that name none;
//! `--require-assignment` additionally refuses requests that name
//! their own reviewers, and `--protected-refs` (one
//! `<repo>:<refname>` pattern per line, trailing `*` allowed) refuses
//! them only for reviews landing on a matching ref. `--bind` with a
//! non-loopback address is refused unless TLS is configured.

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
    let bind = flag_value("--bind").cloned().unwrap_or_else(|| "127.0.0.1".into());
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
        for (_, key) in &signers {
            registry
                .register(key)
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
        // Hot-reload, both halves: appending a key line takes effect on
        // the next failed signature check for submissions, and on the
        // next request for push-certificate verification.
        node.watch_keys_file(path.into());
        // Same file the sequencer appends to: readers that fall behind
        // the in-memory /api/log window resync from it.
        let mut platform =
            Platform::start_reloading(registry, Box::new(log), node_key, Some(path.into()))
                .map_err(std::io::Error::other)?
                .with_log_path(log_path);
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
