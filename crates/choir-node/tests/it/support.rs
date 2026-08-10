//! Helpers shared by the harness modules, extracted from byte-identical
//! copies that lived in each pre-merge test binary. Near-duplicates whose
//! differences carry coverage (each module's `git`, `submit`, `view`
//! variants) deliberately stay where they are.

use choir_identity::ActorKey;
use choir_node::platform::hex_encode;
use choir_view::ViewOp;

/// Runs curl against a node endpoint, returning (status, parsed JSON body).
pub fn curl(args: &[&str]) -> (u16, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body)
            .unwrap_or_else(|error| panic!("JSON response ({error}): {body:?}")),
    )
}

/// A signed `/api/submit` body using the current `channel` field.
pub fn submit_body(key: &ActorKey, channel: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

/// Same submission signed the same way, but sent under the legacy
/// `workspace` field name. The modules using this keep the alias
/// exercised; if the node drops it, these tests are the notice.
pub fn submit_body_legacy(key: &ActorKey, channel: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}
