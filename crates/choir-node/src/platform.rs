//! The platform API: signed op submission and view queries over HTTP.
//!
//! This is the production composition the sequencer's `policy.rs` test
//! proved: signature verification (choir-identity) + cached-view CAS
//! (choir-view) running inside the single-writer thread, now fronted by
//! two endpoints on the daemon:
//!
//! - `POST /api/submit` — body `{"workspace", "payload_hex",
//!   "key_id", "signature_hex"}`; payload bytes are a serialized
//!   [`ViewOp`]. Admitted ops answer `{"seq", "hash"}`; rejections are
//!   HTTP 400 with the policy's reason.
//! - `GET /api/view` — the current materialized view as
//!   `{"workspaces": {name: id}, "refs": {name: id}}`.
//!
//! Hex (not JSON-embedding) carries the payload because the signature
//! covers the exact bytes the author serialized; re-encoding through a
//! JSON tree could legally reorder/respace them and break verification.

use std::sync::{Arc, Mutex};

use choir_identity::Registry;
use choir_oplog::{OpEntry, OpLog, Witness};
use choir_sequencer::{Sequencer, SequencerHandle, SubmitPolicy, Submission};
use choir_view::{View, ViewOp};

/// Verify author signature, then CAS against the shared view. Runs on
/// the sequencer's writer thread; API readers share the view mutex.
struct ChoirPolicy {
    registry: Registry,
    view: Arc<Mutex<View>>,
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        self.registry
            .verify_submission(&sub.workspace, &sub.payload, sig)
            .map_err(|e| format!("bad signature: {e:?}"))?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| format!("bad op: {e:?}"))?;
        let mut trial = self.view.lock().expect("view lock").clone();
        trial.apply(&op).map_err(|e| format!("stale head: {e:?}"))
    }

    fn accepted(&mut self, entry: &OpEntry) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked in check()");
        self.view
            .lock()
            .expect("view lock")
            .apply(&op)
            .expect("checked in check()");
    }
}

/// A running platform: the sequencer plus the shared view it maintains.
pub struct Platform {
    handle: SequencerHandle,
    view: Arc<Mutex<View>>,
    // Kept alive for the daemon's lifetime; the writer thread exits with
    // the process.
    _sequencer: Sequencer,
}

impl Platform {
    /// Replays `log` into a view and starts the admission sequencer over
    /// it with `registry` as the trusted key set.
    ///
    /// # Errors
    ///
    /// Returns a description of any replay failure (a log written
    /// through this platform always replays cleanly).
    pub fn start(registry: Registry, log: Box<dyn OpLog>) -> Result<Self, String> {
        let view = View::materialize(log.as_ref()).map_err(|e| format!("replay: {e:?}"))?;
        let view = Arc::new(Mutex::new(view));
        let sequencer = Sequencer::spawn_with_policy(
            log,
            Box::new(ChoirPolicy {
                registry,
                view: view.clone(),
            }),
        );
        Ok(Self {
            handle: sequencer.handle(),
            view,
            _sequencer: sequencer,
        })
    }

    /// Handles one `/api/...` request, returning `(status, json_body)`.
    pub fn handle_api(&self, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        match (method, path) {
            ("GET", "/api/view") => {
                let view = self.view.lock().expect("view lock");
                let ws: std::collections::BTreeMap<_, _> = view
                    .workspaces
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_hex()))
                    .collect();
                let refs: std::collections::BTreeMap<_, _> =
                    view.refs.iter().map(|(k, v)| (k.clone(), v.to_hex())).collect();
                let body = serde_json::json!({ "workspaces": ws, "refs": refs });
                (200, body.to_string())
            }
            ("POST", "/api/submit") => self.submit(body),
            _ => (404, r#"{"error":"no such endpoint"}"#.to_string()),
        }
    }

    fn submit(&self, body: &[u8]) -> (u16, String) {
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!(r#"{{"error":"bad json: {e}"}}"#)),
        };
        let field = |name: &str| -> Option<String> {
            req.get(name).and_then(|v| v.as_str()).map(String::from)
        };
        let (Some(workspace), Some(payload_hex), Some(key_id), Some(signature_hex)) = (
            field("workspace"),
            field("payload_hex"),
            field("key_id"),
            field("signature_hex"),
        ) else {
            return (
                400,
                r#"{"error":"need workspace, payload_hex, key_id, signature_hex"}"#.to_string(),
            );
        };
        let (Some(payload), Some(signature)) =
            (hex_decode(&payload_hex), hex_decode(&signature_hex))
        else {
            return (400, r#"{"error":"bad hex"}"#.to_string());
        };
        match self.handle.try_submit(
            &workspace,
            payload,
            Some(Witness { key_id, signature }),
        ) {
            Ok(acc) => (
                200,
                serde_json::json!({ "seq": acc.seq, "hash": acc.hash.to_hex() }).to_string(),
            ),
            Err(reason) => (
                400,
                serde_json::json!({ "error": reason }).to_string(),
            ),
        }
    }
}

/// Decodes lowercase/uppercase hex; `None` on any bad input.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Encodes bytes as lowercase hex (client-side convenience, used by
/// tests and the demo).
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
