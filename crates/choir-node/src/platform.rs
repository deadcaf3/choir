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

use choir_identity::{ActorKey, Registry};
use choir_oplog::{ContentHash, OpEntry, OpLog, Witness};
use choir_sequencer::{Sequencer, SequencerHandle, SubmitPolicy, Submission};
use choir_view::{OpKind, View, ViewOp};

/// Sliding window over recent admitted entries: `base` is the seq of
/// the first held entry, so `/api/log?from=` keeps absolute semantics
/// after old entries are dropped (full history lives in the op log).
pub struct LogWindow {
    base: u64,
    entries: Vec<OpEntry>,
}

/// Entries retained in memory for `/api/log`; older reads fall back to
/// the persisted op log (not served over HTTP yet).
const LOG_WINDOW_CAP: usize = 100_000;

impl LogWindow {
    fn push(&mut self, entry: OpEntry) {
        self.entries.push(entry);
        if self.entries.len() > LOG_WINDOW_CAP {
            let drop = self.entries.len() - LOG_WINDOW_CAP;
            self.entries.drain(..drop);
            self.base += drop as u64;
        }
    }
}

/// Verify author signature, then CAS against the shared view. Runs on
/// the sequencer's writer thread; API readers share the view mutex.
struct ChoirPolicy {
    registry: Registry,
    view: Arc<Mutex<View>>,
    entries: Arc<Mutex<LogWindow>>,
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
        self.entries.lock().expect("entries lock").push(entry.clone());
    }
}

/// A running platform: the sequencer plus the shared view it maintains.
pub struct Platform {
    handle: SequencerHandle,
    view: Arc<Mutex<View>>,
    /// The daemon's own key: signs ops it derives from authenticated git
    /// pushes. Attribution: a verified push certificate names the
    /// pusher's key (`key/<principal>`); otherwise the basic-auth user
    /// (`git/<user>`).
    node_key: ActorKey,
    entries: Arc<Mutex<LogWindow>>,
    // Kept alive for the daemon's lifetime; the writer thread exits with
    // the process.
    _sequencer: Sequencer,
}

impl Platform {
    /// Replays `log` into a view and starts the admission sequencer over
    /// it with `registry` as the trusted key set plus the daemon's own
    /// `node_key` (registered automatically, for git-derived ops).
    ///
    /// # Errors
    ///
    /// Returns a description of any replay failure (a log written
    /// through this platform always replays cleanly).
    pub fn start(
        mut registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
    ) -> Result<Self, String> {
        registry
            .register(&node_key.public_key_bytes())
            .map_err(|e| format!("register node key: {e:?}"))?;
        let view = View::materialize(log.as_ref()).map_err(|e| format!("replay: {e:?}"))?;
        let view = Arc::new(Mutex::new(view));
        let existing: Vec<OpEntry> = (0..log.len()).filter_map(|i| log.get(i)).collect();
        let mut window = LogWindow {
            base: 0,
            entries: Vec::new(),
        };
        for e in existing {
            window.push(e);
        }
        let entries = Arc::new(Mutex::new(window));
        let sequencer = Sequencer::spawn_with_policy(
            log,
            Box::new(ChoirPolicy {
                registry,
                view: view.clone(),
                entries: entries.clone(),
            }),
        );
        Ok(Self {
            handle: sequencer.handle(),
            view,
            node_key,
            entries,
            _sequencer: sequencer,
        })
    }

    /// Routes one git ref update (from a repo's `update` hook) through
    /// the sequencer: CAS against the view, node-signed, totally ordered
    /// with API ops. Refs are namespaced `<repo>:<refname>`; git oids
    /// enter the envelope with their own codec ([`ContentHash::from_git_oid`]).
    ///
    /// # Errors
    ///
    /// The policy's rejection reason (stale CAS = concurrent update git
    /// itself would also have refused).
    pub fn git_update(
        &self,
        repo: &str,
        refname: &str,
        old_hex: &str,
        new_hex: &str,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        const ZERO: [char; 2] = ['0', '0'];
        let is_zero = |h: &str| !h.is_empty() && h.chars().all(|c| c == ZERO[0]);
        let name = format!("{repo}:{refname}");
        let prev = if is_zero(old_hex) {
            None
        } else {
            Some(ContentHash::from_git_oid(old_hex).ok_or("bad old oid")?)
        };
        let kind = if is_zero(new_hex) {
            OpKind::DeleteRef { name, prev }
        } else {
            OpKind::SetRef {
                name,
                commit: ContentHash::from_git_oid(new_hex).ok_or("bad new oid")?,
                prev,
            }
        };
        let payload = ViewOp::new(kind).to_payload();
        // Verified push certificate ("G" = good signature) attributes
        // the op to the pusher's own key; otherwise the transport user.
        let workspace = match cert {
            Some(("G", signer)) if !signer.is_empty() => format!("key/{signer}"),
            _ => format!("git/{user}"),
        };
        let sig = self.node_key.sign_submission(&workspace, &payload);
        self.handle
            .try_submit(&workspace, payload, Some(sig))
            .map(|_| ())
    }

    /// Points `workspace` at git oid `head_hex` with a node-signed op,
    /// using the view's current head as the CAS `prev` (a lost race is
    /// a sequencer rejection, not a clobber). `attribution` is the
    /// submission channel (e.g. `git/<user>`), as for git-derived ops.
    ///
    /// # Errors
    ///
    /// Bad oid, or the policy's rejection reason.
    pub fn set_workspace_head(
        &self,
        workspace: &str,
        head_hex: &str,
        attribution: &str,
    ) -> Result<(), String> {
        let commit = ContentHash::from_git_oid(head_hex).ok_or("bad head oid")?;
        let prev = self
            .view
            .lock()
            .expect("view lock")
            .workspaces
            .get(workspace)
            .cloned();
        let payload = ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: workspace.to_string(),
            commit,
            prev,
        })
        .to_payload();
        let sig = self.node_key.sign_submission(attribution, &payload);
        self.handle.try_submit(attribution, payload, Some(sig)).map(|_| ())
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
            ("POST", "/api/submit-batch") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return (400, format!(r#"{{"error":"bad json: {e}"}}"#)),
                };
                let Some(ops) = req.get("ops").and_then(|v| v.as_array()) else {
                    return (400, r#"{"error":"need ops array"}"#.to_string());
                };
                // Ops are admitted in array order; each result is
                // independent (a rejection does not abort the batch).
                let mut accepted = 0u64;
                let mut rejected = 0u64;
                let results: Vec<serde_json::Value> = ops
                    .iter()
                    .map(|op| {
                        let bytes = op.to_string().into_bytes();
                        let (status, body) = self.submit(&bytes);
                        if status == 200 {
                            accepted += 1;
                        } else {
                            rejected += 1;
                        }
                        serde_json::from_str(&body)
                            .unwrap_or_else(|_| serde_json::json!({ "error": body }))
                    })
                    .collect();
                (
                    200,
                    serde_json::json!({
                        "accepted": accepted,
                        "rejected": rejected,
                        "results": results,
                    })
                    .to_string(),
                )
            }
            ("POST", "/api/git-update") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return (400, format!(r#"{{"error":"bad json: {e}"}}"#)),
                };
                let f = |k: &str| req.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let (status, signer) = (f("cert_status"), f("signer"));
                match self.git_update(
                    &f("repo"),
                    &f("refname"),
                    &f("old"),
                    &f("new"),
                    &f("user"),
                    Some((status.as_str(), signer.as_str())),
                ) {
                    Ok(()) => (200, r#"{"ok":true}"#.to_string()),
                    Err(reason) => (400, serde_json::json!({ "error": reason }).to_string()),
                }
            }
            ("GET", path) if path.starts_with("/api/log") => {
                let from: usize = path
                    .split_once("from=")
                    .and_then(|(_, v)| v.split('&').next()?.parse().ok())
                    .unwrap_or(0);
                let window = self.entries.lock().expect("entries lock");
                let skip = from.saturating_sub(window.base as usize);
                let rows: Vec<serde_json::Value> = window
                    .entries
                    .iter()
                    .skip(skip)
                    .take(500)
                    .map(|e| {
                        serde_json::json!({
                            "seq": e.seq,
                            "workspace": e.workspace,
                            "payload_hex": hex_encode(&e.payload),
                            "author_key": e.author_sig.as_ref().map(|w| w.key_id.clone()),
                        })
                    })
                    .collect();
                (
                    200,
                    serde_json::json!({ "entries": rows, "window_base": window.base })
                        .to_string(),
                )
            }
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
