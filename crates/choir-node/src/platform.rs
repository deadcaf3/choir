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
    /// Entries kept before the oldest is dropped. A field rather than a
    /// constant so a test can drive eviction without writing 100k ops.
    cap: usize,
}

/// Entries retained in memory for `/api/log`; older reads fall back to
/// the persisted op log (not served over HTTP yet).
const LOG_WINDOW_CAP: usize = 100_000;

/// Entries per `/api/log` page, whichever source served them.
const LOG_PAGE: usize = 500;

/// Reviewers drawn per unassigned review: two-person integrity, capped
/// by the pool size (D24 layer 5; design choice, not a measured number).
const ASSIGNMENT_SIZE: usize = 2;

/// Nanosecond clock reading, as a nonzero xorshift seed.
fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64)
        | 1
}

/// FNV-1a, so two reviews drawn in the same nanosecond still diverge.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

impl LogWindow {
    fn push(&mut self, entry: OpEntry) {
        self.entries.push(entry);
        if self.entries.len() > self.cap {
            let drop = self.entries.len() - self.cap;
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
    /// When set, the trusted-keys file is re-read after a failed
    /// signature check if its mtime moved — registering a key becomes
    /// "append a line", no daemon restart.
    keys_file: Option<std::path::PathBuf>,
    keys_mtime: Option<std::time::SystemTime>,
    node_pub: Vec<u8>,
    /// The node key's actor id (hex): the only author allowed to assign
    /// reviewers.
    node_id: String,
}

impl ChoirPolicy {
    /// Rebuilds the registry from the keys file iff its mtime changed
    /// since the last (re)load. Returns whether a reload happened.
    fn reload_keys(&mut self) -> bool {
        let Some(path) = &self.keys_file else {
            return false;
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.keys_mtime {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let mut registry = Registry::new();
        if let Ok(node_pub) = <[u8; 32]>::try_from(self.node_pub.as_slice()) {
            registry.register(&node_pub).ok();
        }
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(key) = hex_decode(line).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                registry.register(&key).ok();
            }
        }
        self.registry = registry;
        self.keys_mtime = mtime;
        true
    }
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        let mut verified = self
            .registry
            .verify_submission(&sub.workspace, &sub.payload, sig);
        // Unknown/failed key: maybe the operator just registered it.
        if verified.is_err() && self.reload_keys() {
            verified = self
                .registry
                .verify_submission(&sub.workspace, &sub.payload, sig);
        }
        verified.map_err(|e| format!("bad signature: {e:?}"))?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| format!("bad op: {e:?}"))?;
        // A verdict's claimed reviewer must be the signature-covered
        // submission channel: the log's author attribution and the
        // view's verdict attribution can never diverge.
        if let OpKind::PostVerdict { reviewer, .. } = &op.kind {
            if *reviewer != sub.workspace {
                return Err(format!(
                    "verdict reviewer {reviewer:?} does not match submission channel {:?}",
                    sub.workspace
                ));
            }
        }
        // D24 layer 5: the requester does not choose who reviews them.
        // Only the daemon's own key may fill in a reviewer list; every
        // other author gets a rejection, so an accepted assignment in
        // the log always came from the node's pool draw.
        if matches!(op.kind, OpKind::AssignReviewers { .. }) && sig.key_id != self.node_id {
            return Err("only the node may assign reviewers".to_string());
        }
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
    /// Operator-curated file of eligible reviewer names, one per line.
    /// Read fresh on every draw, so editing it takes effect at once.
    /// `None` = no pool, and unassigned reviews stay unassigned.
    reviewer_pool: Option<std::path::PathBuf>,
    /// The persisted op log, for readers that have fallen behind the
    /// in-memory window. `None` (an in-memory log) means such a reader
    /// gets a loud gap error instead of a resync.
    log_path: Option<std::path::PathBuf>,
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
        registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
    ) -> Result<Self, String> {
        Self::start_reloading(registry, log, node_key, None)
    }

    /// [`Platform::start`] with a trusted-keys file that is hot-reloaded
    /// (on mtime change) whenever a signature check fails: registering a
    /// key is appending a line, no restart. The file's contents replace
    /// the whole registry on reload, so key *removal* also takes effect.
    ///
    /// # Errors
    ///
    /// Same as [`Platform::start`].
    pub fn start_reloading(
        mut registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
        keys_file: Option<std::path::PathBuf>,
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
            cap: LOG_WINDOW_CAP,
        };
        for e in existing {
            window.push(e);
        }
        let entries = Arc::new(Mutex::new(window));
        let keys_mtime = keys_file
            .as_ref()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let sequencer = Sequencer::spawn_with_policy(
            log,
            Box::new(ChoirPolicy {
                registry,
                view: view.clone(),
                entries: entries.clone(),
                keys_file,
                keys_mtime,
                node_pub: node_key.public_key_bytes().to_vec(),
                node_id: node_key.actor_id().to_hex(),
            }),
        );
        Ok(Self {
            handle: sequencer.handle(),
            view,
            node_key,
            entries,
            reviewer_pool: None,
            log_path: None,
            _sequencer: sequencer,
        })
    }

    /// Points the platform at the JSON-lines file its op log persists to,
    /// so `/api/log?from=` can serve entries that have already been
    /// evicted from the in-memory window. Without it, a reader that has
    /// fallen further behind than the window is told so and cannot
    /// resync.
    ///
    /// The file is append-only, so reading a prefix while the writer
    /// thread appends is safe: already-written lines never change.
    #[must_use]
    pub fn with_log_path(mut self, path: std::path::PathBuf) -> Self {
        self.log_path = Some(path);
        self
    }

    /// Shrinks the in-memory `/api/log` window. Exists so tests can
    /// exercise eviction and the resync path without writing 100k ops.
    #[must_use]
    pub fn with_log_window_cap(self, cap: usize) -> Self {
        self.entries.lock().expect("entries lock").cap = cap.max(1);
        self
    }

    /// Points the platform at an operator-curated pool of eligible
    /// reviewer names (one per line, `#` comments allowed). With a pool
    /// set, a `RequestReview` carrying an empty reviewer list is
    /// answered by a node-signed [`OpKind::AssignReviewers`] drawn from
    /// the pool, excluding the requester — D24 layer 5, so a requester
    /// cannot pick a friendly reviewer.
    #[must_use]
    pub fn with_reviewer_pool(mut self, path: std::path::PathBuf) -> Self {
        self.reviewer_pool = Some(path);
        self
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

    /// Draws reviewers for unassigned review `id` and records them with
    /// a node-signed op. Candidates are the pool minus `requester`;
    /// [`ASSIGNMENT_SIZE`] are drawn, or all of them if the pool is
    /// smaller (a one-person pool still beats self-selection).
    ///
    /// # Errors
    ///
    /// No pool configured, an unreadable or empty-after-exclusion pool,
    /// or the sequencer's rejection reason (e.g. the review was assigned
    /// by a concurrent request).
    pub fn assign_reviewers(&self, id: &str, requester: &str) -> Result<Vec<String>, String> {
        let path = self.reviewer_pool.as_ref().ok_or("no reviewer pool configured")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("read reviewer pool: {e}"))?;
        let mut pool: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != requester)
            .map(String::from)
            .collect();
        if pool.is_empty() {
            return Err("reviewer pool has nobody but the requester".to_string());
        }
        // Partial Fisher-Yates with a hand-rolled xorshift (no rand
        // dep). The draw is node-side and recorded in the log, so
        // replay reproduces it from the op, not from this seed.
        let mut state = seed_from_clock() ^ fnv1a(id.as_bytes());
        let take = ASSIGNMENT_SIZE.min(pool.len());
        for i in 0..take {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = i + (state as usize) % (pool.len() - i);
            pool.swap(i, j);
        }
        pool.truncate(take);
        pool.sort();

        let payload = ViewOp::new(OpKind::AssignReviewers {
            id: id.to_string(),
            reviewers: pool.clone(),
        })
        .to_payload();
        let channel = "node/assign";
        let sig = self.node_key.sign_submission(channel, &payload);
        self.handle.try_submit(channel, payload, Some(sig))?;
        Ok(pool)
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
                let reviews: std::collections::BTreeMap<_, _> = view
                    .reviews
                    .iter()
                    .map(|(id, r)| (id.clone(), review_json(r)))
                    .collect();
                let body = serde_json::json!({
                    "workspaces": ws,
                    "refs": refs,
                    "reviews": reviews,
                    "provenance": view.provenance,
                });
                (200, body.to_string())
            }
            // Pending queue for one reviewer: reviews that fanned out to
            // them and are still unanswered by them.
            ("GET", path) if path.starts_with("/api/reviews") => {
                let reviewer = path
                    .split_once("reviewer=")
                    .map(|(_, v)| v.split('&').next().unwrap_or(v))
                    .unwrap_or("");
                let view = self.view.lock().expect("view lock");
                let pending: std::collections::BTreeMap<_, _> = view
                    .reviews
                    .iter()
                    .filter(|(_, r)| {
                        r.reviewers.iter().any(|x| x == reviewer)
                            && !r.verdicts.contains_key(reviewer)
                    })
                    .map(|(id, r)| (id.clone(), review_json(r)))
                    .collect();
                (200, serde_json::json!({ "pending": pending }).to_string())
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
                let base = window.base as usize;
                // Behind the window: the entries the reader still needs
                // are no longer in memory. Serving from `base` here
                // would hand back a normal-looking page with a silent
                // hole in it, so take the persisted log instead — and
                // if there is none, say so loudly rather than lie.
                if from < base {
                    drop(window);
                    let Some(path) = &self.log_path else {
                        return (
                            409,
                            serde_json::json!({
                                "error": "requested entries have been evicted and no persisted log is configured",
                                "window_base": base,
                            })
                            .to_string(),
                        );
                    };
                    return match replay_from_disk(path, from, LOG_PAGE) {
                        Ok(rows) => (
                            200,
                            serde_json::json!({
                                "entries": rows,
                                "window_base": base,
                                "source": "log",
                            })
                            .to_string(),
                        ),
                        Err(e) => (
                            500,
                            serde_json::json!({ "error": format!("resync: {e}") }).to_string(),
                        ),
                    };
                }
                let rows: Vec<serde_json::Value> = window
                    .entries
                    .iter()
                    .skip(from - base)
                    .take(LOG_PAGE)
                    .map(entry_json)
                    .collect();
                (
                    200,
                    serde_json::json!({
                        "entries": rows,
                        "window_base": window.base,
                        "source": "window",
                    })
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
        // An unassigned review request is answered with a node-signed
        // assignment draw once the request itself is admitted.
        let unassigned = match ViewOp::from_payload(&payload) {
            Ok(op) => match op.kind {
                OpKind::RequestReview { id, reviewers, .. } if reviewers.is_empty() => Some(id),
                _ => None,
            },
            Err(_) => None,
        };
        match self.handle.try_submit(
            &workspace,
            payload,
            Some(Witness { key_id, signature }),
        ) {
            Ok(acc) => {
                let mut resp =
                    serde_json::json!({ "seq": acc.seq, "hash": acc.hash.to_hex() });
                if let Some(id) = unassigned {
                    match self.assign_reviewers(&id, &workspace) {
                        Ok(reviewers) => resp["reviewers"] = serde_json::json!(reviewers),
                        // The request stands; it is visibly unassigned,
                        // which is a state no verdict can complete.
                        Err(e) => resp["assignment_error"] = serde_json::json!(e),
                    }
                }
                (200, resp.to_string())
            }
            Err(reason) => (
                400,
                serde_json::json!({ "error": reason }).to_string(),
            ),
        }
    }
}

/// JSON shape of one log entry, shared by the in-memory window and the
/// on-disk resync path so a catching-up reader cannot tell them apart.
fn entry_json(e: &OpEntry) -> serde_json::Value {
    serde_json::json!({
        "seq": e.seq,
        "workspace": e.workspace,
        "payload_hex": hex_encode(&e.payload),
        "author_key": e.author_sig.as_ref().map(|w| w.key_id.clone()),
    })
}

/// Reads up to `take` entries starting at `from` straight out of the
/// persisted JSON-lines log, for readers behind the in-memory window.
///
/// Line number is sequence number, so this skips rather than parses the
/// prefix — O(bytes before `from`) per call, which is the price of a
/// resync and is paid only by readers that fell behind.
fn replay_from_disk(
    path: &std::path::Path,
    from: usize,
    take: usize,
) -> Result<Vec<serde_json::Value>, String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).map_err(|e| format!("open log: {e}"))?;
    std::io::BufReader::new(file)
        .lines()
        .skip(from)
        .take(take)
        .map(|line| {
            let line = line.map_err(|e| format!("read log: {e}"))?;
            let entry: OpEntry =
                serde_json::from_str(&line).map_err(|e| format!("decode log line: {e}"))?;
            Ok(entry_json(&entry))
        })
        .collect()
}

/// JSON shape of one review's state (shared by /api/view and
/// /api/reviews).
fn review_json(r: &choir_view::ReviewState) -> serde_json::Value {
    let verdicts: std::collections::BTreeMap<_, _> = r
        .verdicts
        .iter()
        .map(|(who, (v, note))| {
            (
                who.clone(),
                serde_json::json!({ "verdict": format!("{v:?}"), "note": note }),
            )
        })
        .collect();
    serde_json::json!({
        "target": r.target.as_ref().map(choir_oplog::ContentHash::to_hex),
        "reviewers": r.reviewers,
        "verdicts": verdicts,
        "complete": r.complete(),
        "approved": r.approved(),
    })
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
