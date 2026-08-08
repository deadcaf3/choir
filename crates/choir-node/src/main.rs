//! choir-node daemon entry point.
//!
//! Usage: `choir-node <repo-root> [port] [--create owner/name.git]...
//! [--auth-file path] [--keys-file path]`
//!
//! Binds 127.0.0.1 only. `--auth-file` points at a `user:token`-per-line
//! file (0600; never in-repo) and turns on mandatory basic auth.
//! `--keys-file` (ed25519 public keys, one 64-char hex line each) turns
//! on the platform API; the op log persists at `<repo-root>/.choir/ops.jsonl`.

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

    let mut node = Node::bind_with_auth(&root, port, auth)?;
    if let Some(i) = rest.iter().position(|a| a == "--keys-file") {
        let path = rest.get(i + 1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "--keys-file needs a path")
        })?;
        let mut registry = choir_identity::Registry::new();
        let mut count = 0u32;
        for line in std::fs::read_to_string(path)?.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bytes = choir_node::platform::hex_decode(line)
                .filter(|b| b.len() == 32)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "keys file lines must be 64 hex chars",
                    )
                })?;
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            registry
                .register(&key)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))?;
            count += 1;
        }
        let state_dir = root.join(".choir");
        std::fs::create_dir_all(&state_dir)?;
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
        let log = choir_oplog::FileLog::open(&state_dir.join("ops.jsonl"))
            .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
        let platform = Platform::start(registry, Box::new(log), node_key)
            .map_err(std::io::Error::other)?;
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
        "choir-node serving {} on http://127.0.0.1:{}",
        root.display(),
        node.port()
    );
    node.serve_forever();
    Ok(())
}
