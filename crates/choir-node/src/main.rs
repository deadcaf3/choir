//! choir-node daemon entry point.
//!
//! Usage: `choir-node <repo-root> [port] [--create owner/name.git]...
//! [--auth-file path]`
//!
//! Binds 127.0.0.1 only. `--auth-file` points at a `user:token`-per-line
//! file (0600; never in-repo) and turns on mandatory basic auth.

use choir_node::{AuthTable, Node};

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

    let node = Node::bind_with_auth(&root, port, auth)?;
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
