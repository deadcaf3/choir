//! A seed (D80): a node whose log is a copy of another node's, taken one
//! way, verified before it is kept, and never written to by anybody here.
//!
//! **Seeds, not peers.** A home is the one node whose sequencer admits
//! submissions for its log. A seed replicates that log page by page over
//! `GET /api/log`, checks every page with SYNC.md's three checks, and
//! appends what passed through its *own* writer thread with
//! [`Platform::replicate`] — so invariant 5 holds on a seed exactly as on
//! a home, and nothing here is a second writer. It fetches the git
//! objects the log points at, checks every ref the view names against the
//! bare repository at the same oid, and serves what it holds.
//!
//! # One key, one grant
//!
//! A seed is a named reader of its home, never an anonymous one: `/api/log`
//! needs a node-wide read grant and the ACL parser refuses to give that to
//! `@anon`. So the home's operator registers the seed's node public key,
//! binds it as an operator, and grants that operator `@node auditor` (the
//! node-wide read) plus `read` on the repositories it should hold. The
//! credential the home issued for that principal is what this module
//! presents, and the node key is what a seed signs witness statements
//! with, so the name in the home's bindings and the key on a statement are
//! one identity.
//!
//! Public keys come from the home's `GET /api/signers`, because the log
//! carries a key's id and never the key. The home's own node key is
//! pinned beside this seed's log on first contact, the same shape a node
//! uses to pin its own key: a later start against a home signing with a
//! different key refuses, since a copy that quietly follows its source to
//! a new identity is not a copy of anything in particular.
//!
//! # What stops replication, and what does not
//!
//! A page that fails a check, and an entry this build cannot fold, both
//! **halt** replication at that seq. What was verified before it stays and
//! is still served; nothing after it is attempted, and the halt holds
//! until the process restarts. Skipping the entry instead would make this
//! node a fork of its home that looks like a copy. An unreachable home is
//! not a halt: the next round tries again.
//!
//! An entry whose author key the home does not list is **unverified**,
//! counted, and still replicated. Continuity and recomputation passed, and
//! the home admitted it; what nobody here can say is who signed it, and
//! the count is how a reader is told so.
//!
//! A home that has evicted the entries this seed needs (`409
//! log_evicted`) cannot be followed at all. Its window resumes at a later
//! seq, and this log's next position is fixed by what it holds: the writer
//! appends only at `log.len()` and the view folds only from genesis, so a
//! chain with a hole in it cannot be held here, and SYNC.md's gap rule
//! forbids reporting two pieces as one. The seed records `gap`, stops, and
//! keeps serving what it has. A home with a persisted log never answers
//! that way.

use crate::platform::Platform;
use choir_hash::ContentHash;
use choir_identity::{sync, ActorKey, Registry};
use choir_sequencer::Sequenced;
use choir_view::{OpKind, ViewOp};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Seconds between replication rounds in the daemon.
///
/// One small request per round when nothing moved, so there is no reason
/// for a long-poll yet; tests call [`Replica::replicate_once`] directly and
/// never wait on this.
pub const INTERVAL_SECS: u64 = 5;

/// Statements `GET /api/witness` serves beside the latest.
pub const WITNESS_HISTORY: usize = 64;

/// The channel a witness statement is signed on.
///
/// Fixed, so the bytes a seed signs are the op scheme's own
/// `[channel, payload]` with nothing the signer chooses, and a signature
/// over a statement can never verify as an op on any channel an actor
/// could be bound to: `/` is refused in an operator name.
pub const WITNESS_CHANNEL: &str = "seed/witness";

/// Most log pages one round reads before it turns to git.
///
/// A home admitting faster than one page per round would otherwise keep
/// a round in its log loop forever and never fetch an object. The next
/// round continues where this one stopped.
const MAX_PAGES_PER_ROUND: usize = 64;

/// The home a seed copies: its base URL and the node key it signs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// Base URL, without a trailing slash: `<url>/api/log`, and
    /// `<url>/<owner>/<name>.git` for git.
    pub url: String,
    /// The home's node actor id, as its `GET /api/signers` named it and
    /// this seed pinned it.
    pub node_id: ContentHash,
}

/// The credential a home issued to a seed's principal: one `user:token`
/// line, the shape of a line in the home's `--auth-file`.
///
/// It never reaches an argv, where `ps` would show it, and never a URL:
/// curl reads it from stdin, and git from its environment.
#[derive(Clone)]
pub struct Credential {
    user: String,
    token: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({}:<redacted>)", self.user)
    }
}

impl Credential {
    /// Reads `path`: its first non-blank, non-comment line, as `user:token`.
    ///
    /// # Errors
    ///
    /// The file cannot be read, holds no such line, or carries a quote,
    /// backslash or control character, any of which could turn a quoted
    /// curl config line into a second directive.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading the seed credential {}: {e}", path.display()))?;
        let line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .ok_or_else(|| format!("{} holds no `user:token` line", path.display()))?;
        Self::parse(line).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses one `user:token` line.
    ///
    /// # Errors
    ///
    /// As [`Credential::read`].
    pub fn parse(line: &str) -> Result<Self, String> {
        let (user, token) = line
            .split_once(':')
            .ok_or("a seed credential is one `user:token` line")?;
        if user.is_empty() || token.is_empty() {
            return Err("a seed credential needs both a user and a token".to_string());
        }
        if line
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
        {
            return Err(
                "a seed credential may not carry quotes, backslashes or control \
                        characters"
                    .to_string(),
            );
        }
        Ok(Self {
            user: user.to_string(),
            token: token.to_string(),
        })
    }

    /// The principal the home knows this seed as.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The value of an `Authorization` header carrying this credential.
    fn basic(&self) -> String {
        format!(
            "Basic {}",
            crate::base64_encode(format!("{}:{}", self.user, self.token).as_bytes())
        )
    }
}

/// One authenticated `GET` against the home, as `(status, body)`.
///
/// `curl` with the credential on stdin, the workspace's one way out: no
/// HTTP client crate, and no secret on a command line.
fn get(base: &str, credential: &Credential, path: &str) -> Result<(u16, String), String> {
    let mut child = std::process::Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--config", "-"])
        .arg(format!("{base}{path}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start curl: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped curl stdin")
        .write_all(format!("user = \"{}:{}\"\n", credential.user, credential.token).as_bytes())
        .map_err(|e| format!("could not configure curl: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not wait for curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not reach the home: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text
        .rsplit_once('\n')
        .ok_or("curl returned no HTTP status")?;
    let status = status
        .trim()
        .parse()
        .map_err(|_| "curl returned an invalid HTTP status".to_string())?;
    Ok((status, body.to_string()))
}

/// The home's signer table, as `GET /api/signers` served it.
#[derive(Debug, Clone)]
pub struct Signers {
    /// The home's node actor id.
    pub node_id: ContentHash,
    /// Every public key the table lists, the node's first.
    pub keys: Vec<[u8; 32]>,
}

impl Signers {
    /// A registry holding every key this table lists.
    fn registry(&self) -> Registry {
        let mut registry = Registry::new();
        for key in &self.keys {
            // A key the table lists but ed25519 refuses verifies nothing,
            // which is what leaving it out does too: its entries count as
            // unverified rather than halting a copy over a bad row.
            let _ = registry.register(key);
        }
        registry
    }
}

/// `GET /api/signers` on the home at `base`, checked: every key's id is
/// the hash of the key it is listed beside, and the node's too.
///
/// # Errors
///
/// The home cannot be reached, refuses the grant, or serves a body this
/// build does not read.
pub fn fetch_signers(base: &str, credential: &Credential) -> Result<Signers, String> {
    let (status, body) = get(base, credential, "/api/signers")?;
    if status != 200 {
        return Err(format!(
            "GET /api/signers answered {status}; a seed needs the home's `@node auditor` \
             grant for its principal `{}`: {}",
            credential.user,
            body.trim()
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("GET /api/signers served a body that is not JSON: {e}"))?;
    if value["format_version"].as_u64() != Some(1) {
        return Err(format!(
            "GET /api/signers is format_version {}, and this build reads 1",
            value["format_version"]
        ));
    }
    let key = |row: &serde_json::Value| -> Result<(ContentHash, [u8; 32]), String> {
        let id = row["actor_id"]
            .as_str()
            .and_then(ContentHash::from_hex)
            .ok_or("a signer row names no actor id")?;
        let bytes: [u8; 32] = row["public_key_hex"]
            .as_str()
            .and_then(crate::platform::hex_decode)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("a signer row carries no 32-byte public key")?;
        if ContentHash::blake3(&bytes) != id {
            return Err(format!(
                "signer {} is listed beside a key that does not hash to it",
                id.to_hex()
            ));
        }
        Ok((id, bytes))
    };
    let (node_id, node_key) = key(&value["node"])?;
    let mut keys = vec![node_key];
    for row in value["signers"].as_array().into_iter().flatten() {
        keys.push(key(row)?.1);
    }
    Ok(Signers { node_id, keys })
}

/// Where a seed pins its home's node key: beside its own log.
fn pin_path(root: &Path) -> PathBuf {
    root.join(".choir").join("home.fingerprint")
}

/// Pins `node_id` as this seed's home on first contact, and refuses a
/// different one afterwards.
///
/// # Errors
///
/// The pin names another key, or cannot be read or written.
pub fn pin_home(root: &Path, url: &str, node_id: &ContentHash) -> Result<(), String> {
    let path = pin_path(root);
    let fingerprint = node_id.to_hex();
    match std::fs::read_to_string(&path) {
        Ok(pinned) if pinned.trim() == fingerprint => Ok(()),
        Ok(pinned) => Err(format!(
            "home identity changed: {} pins {}, but {url} now signs as {fingerprint}. A seed \
             does not follow its home to a new key; if the home really was re-keyed, delete \
             the pin and accept that this copy now claims a different home",
            path.display(),
            pinned.trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root.join(".choir"))
                .map_err(|e| format!("creating {}: {e}", root.join(".choir").display()))?;
            // Atomic: a torn pin would refuse every later start.
            choir_fs::write_atomic(&path, format!("{fingerprint}\n"))
                .map_err(|e| format!("writing {}: {e}", path.display()))
        }
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// First contact with a home: its signer table fetched, and its node key
/// pinned beside this seed's log (or checked against the pin already
/// there).
///
/// # Errors
///
/// The home cannot be read, or signs with a key other than the pinned one.
pub fn contact(root: &Path, url: &str, credential: &Credential) -> Result<Home, String> {
    let url = url.trim_end_matches('/').to_string();
    match fetch_signers(&url, credential) {
        Ok(signers) => {
            pin_home(root, &url, &signers.node_id)?;
            Ok(Home {
                url,
                node_id: signers.node_id,
            })
        }
        // A seed that has met its home before starts on the pin when the
        // home is down: a dead home means a frozen copy that still serves,
        // not a copy that refuses to come up. Every round re-checks the
        // pin once the home answers again.
        Err(unreachable) => match pinned(root) {
            Some(node_id) => Ok(Home { url, node_id }),
            None => Err(format!(
                "{unreachable}; a seed's first start needs its home, to pin the key it \
                 signs with"
            )),
        },
    }
}

/// The home key this seed pinned, if it has met its home before.
#[must_use]
pub fn pinned(root: &Path) -> Option<ContentHash> {
    std::fs::read_to_string(pin_path(root))
        .ok()
        .and_then(|text| ContentHash::from_hex(text.trim()))
}

/// A seed's word that it folded the home's attestation `snapshot` (D80).
///
/// Its own object, under its own `format_version`, served and never
/// written into anybody's log: a seed that cosigned through the home
/// would have to win a race against every new attestation, and a
/// statement compared by ancestry does not care how many landed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessStatement {
    /// Version of this object's shape; 1.
    pub format_version: u16,
    /// The seed's actor id: the hash of the key that signs this.
    pub witness: String,
    /// The home's node actor id, which signed the attestation.
    pub home_node_id: String,
    /// The attestation's id, [`choir_view::RefSnapshot::id`], as hex.
    pub snapshot: String,
    /// The log position the attestation occupies.
    pub at_seq: u64,
    /// The last seq the seed had folded when it signed.
    pub seen_at_seq: u64,
}

/// A JSON string literal for `text`.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

impl WitnessStatement {
    /// The bytes that are signed: compact JSON, fields in declaration
    /// order.
    ///
    /// Written out rather than left to a map's iteration order, because
    /// whether `serde_json` sorts object keys depends on a feature any
    /// crate in a build can switch on, and a signature over bytes that
    /// move with the build is a signature over nothing.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{{\"format_version\":{},\"witness\":{},\"home_node_id\":{},\"snapshot\":{},\"at_seq\":{},\"seen_at_seq\":{}}}",
            self.format_version,
            quoted(&self.witness),
            quoted(&self.home_node_id),
            quoted(&self.snapshot),
            self.at_seq,
            self.seen_at_seq
        )
        .into_bytes()
    }

    /// Reads one back from its JSON form.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let text = |key: &str| value.get(key)?.as_str().map(str::to_string);
        Some(Self {
            format_version: u16::try_from(value.get("format_version")?.as_u64()?).ok()?,
            witness: text("witness")?,
            home_node_id: text("home_node_id")?,
            snapshot: text("snapshot")?,
            at_seq: value.get("at_seq")?.as_u64()?,
            seen_at_seq: value.get("seen_at_seq")?.as_u64()?,
        })
    }

    /// Its JSON form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.canonical_bytes()).expect("canonical bytes are JSON")
    }
}

/// A statement, the public key that signed it, and the signature.
///
/// The key travels with the statement because a reader checking a seed
/// may hold nothing else, and it costs no trust: the statement names its
/// witness by the key's hash, so a substituted key names somebody else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedStatement {
    /// What was said.
    pub statement: WitnessStatement,
    /// The seed's ed25519 public key.
    pub public_key: [u8; 32],
    /// Its signature, made the way an op's author signs one: over the
    /// signing hash of `[WITNESS_CHANNEL, canonical_bytes]`.
    pub signature: Vec<u8>,
}

impl SignedStatement {
    /// Signs `statement` with `key`.
    #[must_use]
    pub fn sign(key: &ActorKey, statement: WitnessStatement) -> Self {
        let witness = key.sign_submission(WITNESS_CHANNEL, &statement.canonical_bytes());
        Self {
            statement,
            public_key: key.public_key_bytes(),
            signature: witness.signature,
        }
    }

    /// Checks the statement is one this build reads, is signed by the key
    /// it carries, and that key is the witness it names.
    ///
    /// # Errors
    ///
    /// Which of the three failed.
    pub fn verify(&self) -> Result<(), String> {
        if self.statement.format_version != 1 {
            return Err(format!(
                "statement is format_version {}, and this build reads 1",
                self.statement.format_version
            ));
        }
        let id = ContentHash::blake3(&self.public_key).to_hex();
        if id != self.statement.witness {
            return Err(format!(
                "the statement names witness {}, and the key it carries is {id}",
                self.statement.witness
            ));
        }
        let mut registry = Registry::new();
        registry
            .register(&self.public_key)
            .map_err(|e| format!("the statement's key is not an ed25519 key: {e:?}"))?;
        registry
            .verify_submission(
                WITNESS_CHANNEL,
                &self.statement.canonical_bytes(),
                &choir_oplog::Witness::ed25519(id, self.signature.clone()),
            )
            .map(|_| ())
            .map_err(|e| format!("the statement's signature does not verify: {e:?}"))
    }

    /// Its JSON form, as `/api/witness` serves it.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "statement": self.statement.to_json(),
            "public_key_hex": crate::platform::hex_encode(&self.public_key),
            "signature_hex": crate::platform::hex_encode(&self.signature),
        })
    }

    /// Reads one back from its JSON form, unchecked: see
    /// [`SignedStatement::verify`].
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            statement: WitnessStatement::from_json(value.get("statement")?)?,
            public_key: value
                .get("public_key_hex")?
                .as_str()
                .and_then(crate::platform::hex_decode)?
                .try_into()
                .ok()?,
            signature: value
                .get("signature_hex")?
                .as_str()
                .and_then(crate::platform::hex_decode)?,
        })
    }
}

/// The home's attestation chain, as far as a reader has read its log:
/// each `RecordRefSnapshot`'s id and the id its `prev_snapshot` names.
///
/// Built from `/api/log` pages, which a reader holds, rather than from
/// anything a seed says about itself.
#[derive(Debug, Default, Clone)]
pub struct SnapshotChain {
    prev: std::collections::BTreeMap<String, Option<String>>,
}

/// How two attestations stand by the fork rule (D80).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The same attestation.
    Same,
    /// The first is an ancestor of the second, this many links back.
    Behind(u64),
    /// The second is an ancestor of the first, this many links back.
    Ahead(u64),
    /// Neither is an ancestor of the other: two histories.
    Fork,
}

impl SnapshotChain {
    /// Adds every attestation in a page of `/api/log` entries, and says
    /// how many it found.
    pub fn read_page(&mut self, entries: &[serde_json::Value]) -> usize {
        let mut found = 0;
        for entry in entries {
            let Some(payload) = entry["payload_hex"]
                .as_str()
                .and_then(crate::platform::hex_decode)
            else {
                continue;
            };
            if let Ok(ViewOp {
                kind: OpKind::RecordRefSnapshot { snapshot },
                ..
            }) = ViewOp::from_payload(&payload)
            {
                self.insert(
                    snapshot.id().to_hex(),
                    snapshot.prev_snapshot.as_ref().map(ContentHash::to_hex),
                );
                found += 1;
            }
        }
        found
    }

    /// Records one link: attestation `id` follows `prev`.
    pub fn insert(&mut self, id: String, prev: Option<String>) {
        self.prev.insert(id, prev);
    }

    /// How many `prev_snapshot` links lead from `from` back to `to`, or
    /// `None` when `to` is not an ancestor of `from` in what was read.
    #[must_use]
    pub fn steps_back(&self, from: &str, to: &str) -> Option<u64> {
        let mut here = from;
        let mut steps = 0u64;
        loop {
            if here == to {
                return Some(steps);
            }
            here = self.prev.get(here)?.as_deref()?;
            steps += 1;
            // A cycle is impossible in a log a fold accepted, and this
            // reads pages nobody has checked.
            if steps > self.prev.len() as u64 {
                return None;
            }
        }
    }

    /// The fork rule: `a` and `b` agree when one is the other's ancestor,
    /// or they are equal, and are a fork when neither is.
    ///
    /// A statement over `S` endorses `S` and everything reachable from it
    /// by `prev_snapshot`, so how many attestations landed between two
    /// readers' looks does not matter, and a seed need not race the home.
    /// For the walk to be complete, the chain must hold every attestation
    /// from the lower of the two positions onward.
    #[must_use]
    pub fn relate(&self, a: &str, b: &str) -> Relation {
        if a == b {
            return Relation::Same;
        }
        if let Some(steps) = self.steps_back(b, a) {
            return Relation::Behind(steps);
        }
        if let Some(steps) = self.steps_back(a, b) {
            return Relation::Ahead(steps);
        }
        Relation::Fork
    }
}

/// Where replication stands, as `/api/view` reports it on a seed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The seq of the last entry this seed holds; `None` while empty.
    pub head_seq: Option<u64>,
    /// The highest seq the home has served this seed. A statement about
    /// the last contact, not about now: the home may have moved since.
    pub home_head_seq: Option<u64>,
    /// Refs the view names that the bare repositories here hold at the
    /// same oid.
    pub refs_verified: usize,
    /// Refs the view names that are not held here at that oid yet, or
    /// that no repository could hold.
    pub refs_pending: usize,
    /// Entries replicated whose author key the home's table does not
    /// list. Never silently zero: this is the reader's warning.
    pub unverified_entries: u64,
    /// Set after a `409 log_evicted`, and never cleared: this seed's copy
    /// cannot continue the home's chain, so it is a copy and not a
    /// witness.
    pub gap: bool,
    /// The seq replication halted at and why, once it has.
    pub halted: Option<(u64, String)>,
    /// The last round's transient failure, if the last round failed.
    pub last_error: Option<String>,
}

impl Status {
    /// The `replica` section of `/api/view` on a seed, under its own
    /// `format_version`.
    #[must_use]
    pub fn to_json(&self, home: &Home) -> serde_json::Value {
        serde_json::json!({
            "format_version": 1,
            "home": home.url,
            "home_node_id": home.node_id.to_hex(),
            "head_seq": self.head_seq,
            "home_head_seq": self.home_head_seq,
            "refs_verified": self.refs_verified,
            "refs_pending": self.refs_pending,
            "unverified_entries": self.unverified_entries,
            "gap": self.gap,
            "halted": self.halted.as_ref().map(|(seq, reason)| serde_json::json!({
                "seq": seq,
                "reason": reason,
            })),
            "last_error": self.last_error,
        })
    }
}

/// What one successful round did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Entries appended this round.
    pub appended: u64,
    /// The status after the round.
    pub status: Status,
}

/// Why a round stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaError {
    /// The home could not be reached or answered something this round
    /// cannot use. Transient: the next round tries again.
    Unreachable(String),
    /// A page failed a check, an entry could not be folded, or the home
    /// changed key. Replication stops at `seq` for the life of the
    /// process; everything before it is kept and served.
    Halted {
        /// The first seq not taken.
        seq: u64,
        /// Why.
        reason: String,
    },
    /// The home evicted entries this seed needs and has no persisted log
    /// to serve them from. Replication stops; see the module docs.
    Gap {
        /// Where the home's window resumes.
        window_base: u64,
    },
}

impl ReplicaError {
    /// Whether this stops replication for the life of the process, as
    /// opposed to a round the next one retries.
    #[must_use]
    pub fn is_halt(&self) -> bool {
        !matches!(self, Self::Unreachable(_))
    }
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "replication round failed: {why}"),
            Self::Halted { seq, reason } => {
                write!(f, "replication halted at seq {seq}: {reason}")
            }
            Self::Gap { window_base } => write!(
                f,
                "replication stopped at a gap: the home has evicted everything before seq \
                 {window_base} and keeps no persisted log, so this copy cannot continue its \
                 chain"
            ),
        }
    }
}

/// What one page did.
#[derive(Debug, Default)]
struct Page {
    /// Entries the home served.
    served: usize,
    /// Entries appended.
    appended: u64,
}

/// What a seed shares with the platform serving it, so `/api/view` can
/// say where replication stands without holding the replicator.
#[derive(Debug, Default)]
pub struct Shared {
    pub(crate) status: Mutex<Status>,
    /// The statements this seed has signed, oldest first, at most
    /// [`WITNESS_HISTORY`].
    pub(crate) statements: Mutex<std::collections::VecDeque<SignedStatement>>,
}

impl Shared {
    /// `GET /api/witness` on a seed.
    pub(crate) fn witness_json(&self, home: &Home, key: &ActorKey) -> serde_json::Value {
        let statements = self.statements.lock().expect("replica statements lock");
        let status = self.status.lock().expect("replica status lock");
        serde_json::json!({
            "format_version": 1,
            "witness": key.actor_id().to_hex(),
            "public_key_hex": crate::platform::hex_encode(&key.public_key_bytes()),
            "home": home.url,
            "home_node_id": home.node_id.to_hex(),
            "gap": status.gap,
            // A halt is the event a witness exists to surface, so it is
            // here beside the statement it froze, not only in `/api/view`.
            "halted": status.halted.as_ref().map(|(seq, reason)| serde_json::json!({
                "seq": seq,
                "reason": reason,
            })),
            "latest": statements.back().map(SignedStatement::to_json),
            "history": statements.iter().map(SignedStatement::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Where a seed keeps the statements it signed: beside its log, one JSON
/// object per line.
fn witness_path(root: &Path) -> PathBuf {
    root.join(".choir").join("witness.jsonl")
}

/// The last [`WITNESS_HISTORY`] statements in `root`'s file, oldest first.
/// A line that does not read is skipped: the file is this seed's own
/// record, and the log is the history that matters.
fn load_statements(root: &Path) -> std::collections::VecDeque<SignedStatement> {
    let text = std::fs::read_to_string(witness_path(root)).unwrap_or_default();
    let mut out: std::collections::VecDeque<SignedStatement> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter_map(|value: serde_json::Value| SignedStatement::from_json(&value))
        .collect();
    while out.len() > WITNESS_HISTORY {
        out.pop_front();
    }
    out
}

/// A running seed: the home it copies, the credential it copies with, and
/// the platform it copies into.
pub struct Replica {
    home: Home,
    credential: Credential,
    root: PathBuf,
    platform: Arc<Platform>,
    /// Every key the home's signer table listed at the last round.
    registry: Mutex<Registry>,
    shared: Arc<Shared>,
}

impl Replica {
    /// A seed of `home`, writing into `platform` and fetching git into
    /// the bare repositories under `root`.
    ///
    /// `platform` should have been made a seed of the same home with
    /// [`Platform::as_seed_of`]; [`contact`] is how both learn it. If it
    /// was not, it is made one here, since a platform being replicated
    /// into is a seed whether or not anybody said so.
    #[must_use]
    pub fn new(root: PathBuf, home: Home, credential: Credential, platform: Arc<Platform>) -> Self {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status {
                head_seq: platform.view_seq().checked_sub(1),
                ..Status::default()
            }),
            statements: Mutex::new(load_statements(&root)),
        });
        platform.attach_replica(home.clone(), shared.clone());
        Self {
            home,
            credential,
            root,
            platform,
            registry: Mutex::new(Registry::new()),
            shared,
        }
    }

    /// The home this seed copies.
    #[must_use]
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// Where replication stands.
    #[must_use]
    pub fn status(&self) -> Status {
        self.shared
            .status
            .lock()
            .expect("replica status lock")
            .clone()
    }

    /// Re-reads the home's signer table into this seed's registry, and
    /// checks the home still signs with the pinned key.
    ///
    /// # Errors
    ///
    /// [`ReplicaError::Unreachable`] when the table cannot be read, and
    /// [`ReplicaError::Halted`] when the home's key is not the pinned one.
    pub fn fetch_signers(&self) -> Result<usize, ReplicaError> {
        let signers =
            fetch_signers(&self.home.url, &self.credential).map_err(ReplicaError::Unreachable)?;
        if signers.node_id != self.home.node_id {
            return Err(ReplicaError::Halted {
                seq: self.platform.view_seq(),
                reason: format!(
                    "the home now signs as {}, and this seed pinned {}",
                    signers.node_id.to_hex(),
                    self.home.node_id.to_hex()
                ),
            });
        }
        let count = signers.keys.len();
        *self.registry.lock().expect("replica registry lock") = signers.registry();
        Ok(count)
    }

    /// Revocation positions for verifying `entries`: every one this seed
    /// has already folded, and every `RevokeKey` inside the page itself.
    ///
    /// The second half matters because a page is a window. An entry
    /// signed after its key's revocation *in the same page* is exactly the
    /// case SYNC.md's third check exists to fail, and the view has not
    /// folded that revocation yet when the page is checked.
    fn revocations(&self, entries: &[serde_json::Value]) -> sync::Revocations {
        let mut revoked: sync::Revocations = self.platform.with_view(|view| {
            view.bindings
                .iter()
                .filter_map(|(key, binding)| binding.revoked.as_ref().map(|r| (key.clone(), r.at)))
                .collect()
        });
        for entry in entries.iter().filter_map(sync::rebuild) {
            if let Ok(ViewOp {
                kind: OpKind::RevokeKey { key, .. },
                ..
            }) = ViewOp::from_payload(&entry.payload)
            {
                revoked.entry(key.to_hex()).or_insert(entry.seq);
            }
        }
        revoked
    }

    /// Fetches the page at `from`, verifies it by SYNC.md's three checks,
    /// and replicates every entry before the first one that fails.
    fn fetch_page(&self, from: u64) -> Result<Page, ReplicaError> {
        let (status, body) = get(
            &self.home.url,
            &self.credential,
            &format!("/api/log?from={from}"),
        )
        .map_err(ReplicaError::Unreachable)?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
            ReplicaError::Unreachable(format!(
                "GET /api/log answered {status} with a body that is not JSON"
            ))
        })?;
        if status == 409 && value["code"] == "log_evicted" {
            return Err(ReplicaError::Gap {
                window_base: value["window_base"].as_u64().unwrap_or_default(),
            });
        }
        if status != 200 {
            return Err(ReplicaError::Unreachable(format!(
                "GET /api/log answered {status}: {}",
                value["error"].as_str().unwrap_or_default()
            )));
        }
        let entries = value["entries"].as_array().cloned().unwrap_or_default();
        let Some(last) = entries.last() else {
            return Ok(Page::default());
        };
        let last_seq = last["seq"].as_u64();
        // Recorded before anything is checked: how far the home has shown
        // this seed is a fact about the home whether or not the page
        // verifies, and after a halt it is how far behind the copy is.
        {
            let mut status = self.shared.status.lock().expect("replica status lock");
            status.home_head_seq = status.home_head_seq.max(last_seq);
        }

        let registry = self.registry.lock().expect("replica registry lock");
        let revoked = self.revocations(&entries);
        let report = sync::page(&entries, &registry, &revoked);
        // Everything before the first failure passed all three checks.
        let keep = match report.first_failure {
            Some(seq) => entries
                .iter()
                .position(|entry| entry["seq"].as_u64() == Some(seq))
                .unwrap_or(0),
            None => entries.len(),
        };
        drop(registry);

        let mut batch = Vec::with_capacity(keep);
        // Which entries are attestations, so a statement is signed once
        // one of them has actually been folded.
        let mut attests = Vec::with_capacity(keep);
        for entry in &entries[..keep] {
            // Both passed the checks just made, which rebuilt the one and
            // compared the other, so neither can be absent here.
            let (Some(rebuilt), Some(hash)) = (
                sync::rebuild(entry),
                entry["hash"].as_str().and_then(ContentHash::from_hex),
            ) else {
                return Err(ReplicaError::Halted {
                    seq: entry["seq"].as_u64().unwrap_or(from),
                    reason: "an entry that verified could not be rebuilt".to_string(),
                });
            };
            attests.push(matches!(
                ViewOp::from_payload(&rebuilt.payload),
                Ok(ViewOp {
                    kind: OpKind::RecordRefSnapshot { .. },
                    ..
                })
            ));
            batch.push(Sequenced {
                entry: rebuilt,
                hash,
            });
        }
        let (appended, stopped) = if batch.is_empty() {
            (0, None)
        } else {
            match self.platform.replicate(batch) {
                Ok(done) => (done.appended, None),
                Err(stopped) => (stopped.appended, Some(stopped)),
            }
        };
        // Counted over exactly what was appended, so a halt part-way
        // through a page neither loses nor invents an unverified entry.
        let taken = usize::try_from(appended)
            .unwrap_or(usize::MAX)
            .min(entries.len());
        if attests[..taken.min(attests.len())].contains(&true) {
            self.witness();
        }
        let unverified = if taken == entries.len() {
            report.unverified
        } else {
            let registry = self.registry.lock().expect("replica registry lock");
            sync::page(&entries[..taken], &registry, &revoked).unverified
        };
        self.shared
            .status
            .lock()
            .expect("replica status lock")
            .unverified_entries += unverified as u64;
        if let Some(stopped) = stopped {
            return Err(ReplicaError::Halted {
                seq: stopped.seq,
                reason: stopped.reason,
            });
        }
        if let Some(seq) = report.first_failure {
            return Err(ReplicaError::Halted {
                seq,
                // The verifier says which seq in every sentence; the halt
                // already does, so it is said once.
                reason: report.failures.first().map_or_else(
                    || "the page failed verification".to_string(),
                    |first| {
                        first
                            .strip_prefix(&format!("seq {seq}: "))
                            .unwrap_or(first)
                            .to_string()
                    },
                ),
            });
        }
        Ok(Page {
            served: entries.len(),
            appended,
        })
    }

    /// Signs a statement over the latest attestation this seed has
    /// folded, keeps it, and records it beside the log.
    ///
    /// Never after a gap: a copy with a hole in it has not seen the chain
    /// it would be vouching for.
    fn witness(&self) {
        if self.shared.status.lock().expect("replica status lock").gap {
            return;
        }
        let Some((snapshot, at_seq)) = self.platform.with_view(|view| {
            view.latest_snapshot
                .as_ref()
                .map(|latest| (latest.id().to_hex(), latest.at_seq))
        }) else {
            return;
        };
        let key = self.platform.node_key();
        let signed = SignedStatement::sign(
            key,
            WitnessStatement {
                format_version: 1,
                witness: key.actor_id().to_hex(),
                home_node_id: self.home.node_id.to_hex(),
                snapshot,
                at_seq,
                seen_at_seq: self.platform.view_seq().saturating_sub(1),
            },
        );
        let line = format!("{}\n", signed.to_json());
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(witness_path(&self.root))
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(e) = written {
            // Served either way; the file is how a restart remembers.
            self.shared
                .status
                .lock()
                .expect("replica status lock")
                .last_error = Some(format!("recording a witness statement: {e}"));
        }
        let mut statements = self
            .shared
            .statements
            .lock()
            .expect("replica statements lock");
        statements.push_back(signed);
        while statements.len() > WITNESS_HISTORY {
            statements.pop_front();
        }
    }

    /// The statements this seed has signed, oldest first.
    #[must_use]
    pub fn statements(&self) -> Vec<SignedStatement> {
        self.shared
            .statements
            .lock()
            .expect("replica statements lock")
            .iter()
            .cloned()
            .collect()
    }

    /// Fetches `repo`'s objects and refs from the home into the bare
    /// repository here, then checks every ref the view names for it
    /// against what the repository holds.
    ///
    /// # Errors
    ///
    /// The name is not a repository path, or git could not create or
    /// fetch into the repository.
    pub fn fetch_objects(&self, repo: &str) -> Result<crate::portable::RefCheck, String> {
        if !repo.ends_with(".git") {
            return Err(format!("`{repo}` is not a repository path"));
        }
        let bare = crate::repo_path_in(&self.root, repo).map_err(|e| format!("{repo}: {e}"))?;
        if !bare.join("HEAD").is_file() {
            crate::create_repo_in(&self.root, repo).map_err(|e| format!("creating {repo}: {e}"))?;
        }
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "credential.helper=",
                "fetch",
                "--quiet",
                "--prune",
                "--no-tags",
            ])
            .arg(format!("{}/{repo}", self.home.url))
            .arg("+refs/*:refs/*")
            .current_dir(&bare)
            .env("GIT_TERMINAL_PROMPT", "0")
            // The credential as configuration in the environment, where
            // `ps` does not show it, rather than on the argv or in the URL.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: {}", self.credential.basic()),
            )
            .output()
            .map_err(|e| format!("spawn git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git fetch {repo}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let held = crate::platform::read_git_refs(&bare)
            .ok_or_else(|| format!("{repo}: the fetched repository's refs cannot be read"))?;
        let wanted = self
            .platform
            .with_view(crate::portable::view_refs)
            .by_repo
            .remove(repo)
            .unwrap_or_default();
        Ok(crate::portable::check_refs(&wanted, &held))
    }

    /// One round: the signer table, every log page the home has past this
    /// seed's head, then the git objects for every repository the view
    /// names. The daemon runs this every [`INTERVAL_SECS`]; tests call it
    /// directly.
    ///
    /// # Errors
    ///
    /// See [`ReplicaError`]. A halt is recorded in [`Replica::status`] and
    /// every later round returns it without contacting the home.
    pub fn replicate_once(&self) -> Result<Progress, ReplicaError> {
        let outcome = self.round();
        let mut status = self.shared.status.lock().expect("replica status lock");
        status.head_seq = self.platform.view_seq().checked_sub(1);
        match &outcome {
            Ok(_) => status.last_error = None,
            Err(ReplicaError::Unreachable(why)) => status.last_error = Some(why.clone()),
            Err(ReplicaError::Halted { seq, reason }) => {
                status.halted = Some((*seq, reason.clone()));
            }
            Err(ReplicaError::Gap { window_base }) => {
                status.gap = true;
                status.halted = Some((
                    self.platform.view_seq(),
                    format!(
                        "the home's log now starts at seq {window_base}; everything before it \
                         is evicted and this copy cannot continue the chain"
                    ),
                ));
            }
        }
        outcome.map(|appended| Progress {
            appended,
            status: status.clone(),
        })
    }

    fn round(&self) -> Result<u64, ReplicaError> {
        {
            let status = self.shared.status.lock().expect("replica status lock");
            if let Some((seq, reason)) = &status.halted {
                return Err(ReplicaError::Halted {
                    seq: *seq,
                    reason: reason.clone(),
                });
            }
        }
        self.fetch_signers()?;
        let mut appended = 0;
        for _ in 0..MAX_PAGES_PER_ROUND {
            let page = self.fetch_page(self.platform.view_seq())?;
            appended += page.appended;
            if page.served == 0 {
                break;
            }
        }
        let refs = self.platform.with_view(crate::portable::view_refs);
        let (mut verified, mut pending) = (0, refs.unbacked.len());
        let mut failure = None;
        for (repo, wanted) in &refs.by_repo {
            match self.fetch_objects(repo) {
                Ok(check) => {
                    verified += check.matched;
                    pending += check.disagreements.len();
                }
                Err(why) => {
                    pending += wanted.len();
                    failure.get_or_insert(why);
                }
            }
        }
        {
            let mut status = self.shared.status.lock().expect("replica status lock");
            status.refs_verified = verified;
            status.refs_pending = pending;
        }
        match failure {
            Some(why) => Err(ReplicaError::Unreachable(why)),
            None => Ok(appended),
        }
    }
}

/// Runs `replica` forever, a round every [`INTERVAL_SECS`], on the calling
/// thread.
///
/// A halt is said once and replication stops there while the node keeps
/// serving what it verified, unless `strict`, in which case the process
/// exits nonzero so the operator's supervisor sees it. A transient failure
/// is said when it changes, not every round.
pub fn run(replica: &Replica, strict: bool) -> ! {
    let mut said: Option<String> = None;
    loop {
        // The same exit a serving node takes (see `Node::serve_forever`),
        // here because an archival seed has no accept loop to take it.
        if replica.platform.durability_failed() {
            eprintln!(
                "choir: the op log is no longer durable, so this seed has stopped \
                 replicating; exiting so supervision restarts it"
            );
            std::process::exit(75);
        }
        match replica.replicate_once() {
            Ok(progress) => {
                if progress.appended > 0 {
                    eprintln!(
                        "choir: seed took {} entries from {} (head {}, refs {} verified, {} \
                         pending)",
                        progress.appended,
                        replica.home.url,
                        progress
                            .status
                            .head_seq
                            .map_or_else(|| "none".to_string(), |seq| seq.to_string()),
                        progress.status.refs_verified,
                        progress.status.refs_pending,
                    );
                }
                said = None;
            }
            Err(error) => {
                let text = error.to_string();
                if said.as_deref() != Some(text.as_str()) {
                    eprintln!("choir: {text}");
                    said = Some(text);
                }
                if error.is_halt() && strict {
                    std::process::exit(1);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(INTERVAL_SECS));
    }
}

#[cfg(test)]
mod tests {
    use super::{Relation, SignedStatement, SnapshotChain, WitnessStatement};

    fn statement() -> WitnessStatement {
        WitnessStatement {
            format_version: 1,
            witness: "1e-aa".into(),
            home_node_id: "1e-bb".into(),
            snapshot: "1e-cc".into(),
            at_seq: 4,
            seen_at_seq: 9,
        }
    }

    /// The signed bytes are pinned: a reader in another language rebuilds
    /// exactly these from SYNC.md, so a reordering is a format change.
    #[test]
    fn a_statements_signed_bytes_are_its_fields_in_order() {
        assert_eq!(
            String::from_utf8(statement().canonical_bytes()).unwrap(),
            r#"{"format_version":1,"witness":"1e-aa","home_node_id":"1e-bb","snapshot":"1e-cc","at_seq":4,"seen_at_seq":9}"#
        );
        let back = WitnessStatement::from_json(&statement().to_json()).unwrap();
        assert_eq!(back, statement());
    }

    /// A statement names its witness by key hash, so it verifies only
    /// under the key it carries, and one naming anybody else does not.
    #[test]
    fn a_statement_verifies_only_as_the_witness_it_names() {
        let key = choir_identity::ActorKey::generate();
        let own = WitnessStatement {
            witness: key.actor_id().to_hex(),
            ..statement()
        };
        let signed = SignedStatement::sign(&key, own);
        signed.verify().unwrap();
        let round = SignedStatement::from_json(&signed.to_json()).unwrap();
        assert_eq!(round, signed);
        let claimed = SignedStatement::sign(&key, statement());
        assert!(claimed.verify().unwrap_err().contains("names witness"));
    }

    /// The fork rule over a chain a, b, c, with x branching off a.
    #[test]
    fn two_attestations_agree_by_ancestry_and_fork_otherwise() {
        let mut chain = SnapshotChain::default();
        chain.insert("a".into(), None);
        chain.insert("b".into(), Some("a".into()));
        chain.insert("c".into(), Some("b".into()));
        chain.insert("x".into(), Some("a".into()));
        assert_eq!(chain.relate("c", "c"), Relation::Same);
        assert_eq!(chain.relate("a", "c"), Relation::Behind(2));
        assert_eq!(chain.relate("c", "b"), Relation::Ahead(1));
        assert_eq!(chain.relate("x", "c"), Relation::Fork);
        assert_eq!(chain.relate("nowhere", "c"), Relation::Fork);
    }
}
