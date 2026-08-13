//! `allowed_signers` tracks the trusted-keys file without a restart:
//! registering a *signing* key is appending a line, the same mechanism
//! submission keys already had. A broken keys file must not wipe the
//! signer list.

use choir_node::{Node, parse_keys_file, ssh_ed25519_pubkey, write_allowed_signers};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn poke(port: u16) {
    // Any request drives the accept loop, which is where the refresh
    // runs; the response itself does not matter.
    std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", &format!("http://127.0.0.1:{port}/")])
        .output()
        .expect("curl runs");
}

#[test]
fn appending_a_key_updates_allowed_signers_without_a_restart() {
    let work = std::env::temp_dir().join(format!("choir-node-signers-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let root = work.join("repos");
    let keys = work.join("keys");

    let first = choir_identity::ActorKey::generate();
    std::fs::write(&keys, format!("{}\n", hex(&first.public_key_bytes()))).unwrap();

    // Startup path: the signer list is generated once, as before.
    std::fs::create_dir_all(&root).unwrap();
    let signers = parse_keys_file(&keys).unwrap();
    let signers_path = write_allowed_signers(&root, &signers).unwrap();
    assert_eq!(std::fs::read_to_string(&signers_path).unwrap().lines().count(), 1);

    let mut node = Node::bind(&root, 0).unwrap();
    node.watch_keys_file(keys.clone());
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Append a second key, the way an operator registers a reviewer.
    let second = choir_identity::ActorKey::generate();
    // mtime has 1 s granularity on some filesystems; make the edit
    // unambiguous rather than racing the clock.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        &keys,
        format!(
            "{}\n# a comment\n{}\n",
            hex(&first.public_key_bytes()),
            hex(&second.public_key_bytes())
        ),
    )
    .unwrap();

    poke(port);
    let text = std::fs::read_to_string(&signers_path).unwrap();
    assert_eq!(text.lines().count(), 2, "no restart should be needed: {text}");
    // Principal is the actor id; the key is in OpenSSH form, which is
    // what git checks a push certificate against.
    assert!(text.contains(&second.actor_id().to_hex()), "{text}");
    assert!(
        text.contains(&ssh_ed25519_pubkey(&second.public_key_bytes())),
        "{text}"
    );

    // A broken keys file keeps the previous list rather than truncating
    // it: a partial signer list silently stops verifying someone.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&keys, "not-a-key\n").unwrap();
    poke(port);
    let after = std::fs::read_to_string(&signers_path).unwrap();
    assert_eq!(after, text, "a bad keys file must not wipe the signers");

    // And a repaired file is picked up on the next request.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&keys, format!("{}\n", hex(&second.public_key_bytes()))).unwrap();
    poke(port);
    let repaired = std::fs::read_to_string(&signers_path).unwrap();
    assert_eq!(repaired.lines().count(), 1, "{repaired}");
    assert!(repaired.contains(&second.actor_id().to_hex()), "{repaired}");

    node.unblock();
}
