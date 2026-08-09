//! Minimal op-signing client: turns a key file + attribution channel +
//! op JSON into a `POST /api/submit` request body on stdout.
//!
//! Usage: `sign_submit <key-file> <channel> '<op-json>'`
//!
//! `<op-json>` is a serialized [`choir_view::ViewOp`], e.g.
//! `{"format_version":1,"kind":{"RequestReview":{"id":"r1","target":...,
//! "reviewers":["ana"]}}}`. The key file holds 32 secret bytes and is
//! created (0600) if absent; print its public key for the daemon's
//! `--keys-file` with `choir-bridge --pubkey <key-file>`.

use choir_identity::ActorKey;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn submission_body(key: &ActorKey, channel: &str, payload: &[u8]) -> serde_json::Value {
    let sig = key.sign_submission(channel, payload);
    serde_json::json!({
        "channel": channel,
        "workspace": channel,
        "payload_hex": hex_encode(payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [key_file, channel, op_json] = match args.as_slice() {
        [a, b, c] => [a, b, c],
        _ => {
            eprintln!("usage: sign_submit <key-file> <channel> '<op-json>'");
            std::process::exit(2);
        }
    };
    let key = if std::path::Path::new(key_file).exists() {
        let bytes = std::fs::read(key_file).expect("read key file");
        ActorKey::from_secret_bytes(&bytes.as_slice().try_into().expect("32-byte key file"))
    } else {
        let key = ActorKey::generate();
        std::fs::write(key_file, key.secret_bytes()).expect("write key file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key_file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod key file");
        }
        key
    };
    // Round-trip through ViewOp so payload bytes are exactly what the
    // daemon will decode (signature covers the serialized bytes).
    let op: choir_view::ViewOp = serde_json::from_str(op_json).expect("valid op json");
    let payload = op.to_payload();
    println!("{}", submission_body(&key, channel, &payload));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_carries_both_channel_spellings() {
        let key = ActorKey::from_secret_bytes(&[23; 32]);
        let body = submission_body(&key, "operator/example", b"payload");

        assert_eq!(body["channel"], "operator/example");
        assert_eq!(body["workspace"], body["channel"]);
    }
}
