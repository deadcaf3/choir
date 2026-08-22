//! Account and credential self-service (D36).
//!
//! Adding a user used to mean the operator appending a line to the
//! `--auth-file` by hand and restarting the daemon, then appending
//! another to the `--acl-file`, then a third to an `authorized_keys`.
//! This module is the same three facts issued through the node instead:
//! one record per account holding the BLAKE3 of its token, the grants it
//! was issued with, and its registered SSH keys.
//!
//! Three properties carry the design.
//!
//! **Issuance is invite-only.** There is no registration endpoint. An
//! account exists because a holder of `@node write` minted a single-use,
//! expiring invite naming it, and the invited person redeemed that
//! invite. The invite *is a credential*: it is presented as ordinary
//! basic auth, so the node's 401 wall, its constant-time compare and
//! D33's per-user buckets all apply to redemption without a line of new
//! code, and an anonymous request still reaches nothing. An unauthenticated
//! redeem route would have been the one unmetered, unattributed way in.
//!
//! **Issued grants are enforced by the D29 table, not beside it.** The
//! store renders its grants as ACL lines and hands them to
//! [`Acl::parse`], and [`Accounts::acl`] returns the result. The node
//! merges that with the file table, so the git chokepoint,
//! [`crate::acl::api_denial`], [`crate::acl::filter_response`], D33's
//! `@node` exemption and the `choir-ssh` shim all grade an issued
//! credential without any of them knowing this module exists. The store
//! can never grant [`crate::acl::Scope::Node`]: node-wide authority stays operator-
//! authored in the ACL file, so self-service cannot mint itself an
//! auditor or a rate-limit exemption.
//!
//! **This is not in the op log, and that is the decision.** D35 measured
//! what a new `OpKind` variant costs: the fold has no tolerant arm, so
//! every reader older than the variant stops materializing the log at the
//! first op it does not know, totally rather than partially. Paying that
//! for credentials would also buy the wrong property — the log is
//! append-only and replayed in full by any `@node auditor`, and a token
//! hash placed there could never be forgotten, while revocation's whole
//! contract is that it forgets. So the store is a node-owned file,
//! rewritten in place, and revocation is deletion.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use choir_identity::ActorKey;
use choir_oplog::ContentHash;

use crate::acl::Acl;

/// Version of the on-disk store. Every persisted struct carries one
/// (invariant 1); new fields are additive and old files still load.
pub const FORMAT_VERSION: u64 = 1;

/// How long an unredeemed invite stays usable when the caller names no
/// lifetime: one day, which is longer than handing someone a secret takes
/// and shorter than forgetting about it does.
pub const DEFAULT_INVITE_SECS: u64 = 86_400;

/// Longest lifetime an invite may be given. An invite is a bearer secret
/// for an account that does not exist yet, so "expires eventually" is not
/// the same promise as "expires".
pub const MAX_INVITE_SECS: u64 = 30 * 86_400;

/// Prefix every invite id carries, and a spelling [`validate_username`]
/// refuses, so an invite can never name the same principal an account
/// does.
pub const INVITE_PREFIX: &str = "invite-";

/// Most WebAuthn credentials one account may enrol (D39). A person has a
/// laptop, a phone and a hardware key; a hundred is not a person with
/// many devices, it is a store being filled by something automated.
pub const MAX_PASSKEYS: usize = 8;

/// Longest credential id accepted. The spec allows up to 1023 raw bytes;
/// this is that ceiling in base64url, so nothing legitimate is refused
/// and an unbounded string is.
pub const MAX_CREDENTIAL_ID_CHARS: usize = 1364;

/// Longest passkey label. Long enough to say "work laptop, touch id",
/// short enough that a roster stays a roster.
pub const MAX_LABEL_CHARS: usize = 64;

/// Who a set of presented credentials turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// An operator credential from `--auth-file`, or a redeemed account
    /// from the store. Both are ordinary users of the node, and the name
    /// is what every downstream check keys on.
    Account(String),
    /// An unredeemed invite, identified by its id. It may reach exactly
    /// one route — its own redemption — and is refused everywhere else,
    /// which is enforced by the caller rather than here.
    Invite(String),
}

impl Principal {
    /// The name to attribute a request to. For an invite this is the
    /// invite id, so a redemption attempt is attributable in the D33
    /// request log without naming the account it would create.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Account(user) | Self::Invite(user) => user,
        }
    }

    /// Whether this principal is an unredeemed invite.
    #[must_use]
    pub fn is_invite(&self) -> bool {
        matches!(self, Self::Invite(_))
    }
}

/// One issued account.
#[derive(Debug, Clone)]
struct Account {
    /// BLAKE3 of the token, never the token: the store is read by the
    /// process that serves requests, and a stolen store should not be a
    /// stolen credential.
    token_hash: String,
    /// Grants in the ACL file's own two-column spelling, e.g.
    /// `owner/repo read`.
    grants: Vec<String>,
    /// `ssh-ed25519 <base64>` pairs, comment deliberately dropped — see
    /// [`validate_ssh_key`].
    ssh_keys: Vec<String>,
    /// Enrolled WebAuthn credentials (D39). Held inside the account
    /// rather than in a table beside it, so revocation — which deletes
    /// the account — cannot forget to forget them.
    passkeys: Vec<Passkey>,
    /// What to call this account's holder in anything a person reads
    /// (D46). `None` on an account whose *name* is already the human
    /// name — every account issued before D46, and any issued since with
    /// an explicit `user`.
    ///
    /// This is the deletable half of an identity and the only half. The
    /// account's key in [`State::accounts`] is what
    /// [`crate::quota::channel_for`] turns into a channel, which
    /// `choir_oplog::signing_hash` covers, which the log keeps forever —
    /// so a name used as the key can never be withdrawn, while a name
    /// held here is one row to drop.
    ///
    /// Nothing authorizes against it and nothing may start: the moment a
    /// display name decides anything, deleting it changes what the node
    /// permits, and it stops being safe to delete.
    display_name: Option<String>,
    /// The channel the redemption bound an ed25519 actor key to, and
    /// that key's public hex. `None` on every account redeemed without
    /// one, which is every account issued before invite binding existed.
    ///
    /// Recorded here so the roster can answer "which key did this
    /// account arrive with" after the fact. It is **not** what admits
    /// the key: the trusted-keys file is, exactly as for a key an
    /// operator pasted by hand, so revoking stays one line to delete and
    /// this row cannot contradict what the node actually trusts.
    actor_key: Option<(String, String)>,
    /// Unix seconds at redemption.
    created_at: u64,
}

/// One enrolled WebAuthn credential (D39).
///
/// Both byte strings are public: a credential id is an opaque handle the
/// browser hands back, and the key is a public key. Nothing here is a
/// secret, which is why this record — unlike [`Account::token_hash`] —
/// stores values rather than hashes of them. A verifier needs the key
/// itself, so hashing it would make it useless.
#[derive(Debug, Clone)]
struct Passkey {
    /// The credential id as the browser reports it, base64url. Used to
    /// pick which enrolled key an assertion claims to come from.
    credential_id: String,
    /// The credential public key, base64url of SubjectPublicKeyInfo DER
    /// — exactly what `getPublicKey()` returns. D39 scoped a CBOR reader
    /// out, so nothing here parses an attestation object.
    public_key: String,
    /// What the holder called it, so a roster of three keys is legible.
    label: String,
    /// Unix seconds at enrolment.
    created_at: u64,
}

/// One minted, not yet redeemed invite.
#[derive(Debug, Clone)]
struct Invite {
    /// BLAKE3 of the secret half.
    secret_hash: String,
    /// Account this invite creates when redeemed. Already the minted
    /// handle when the issuer sent a `display_name` (D46) — the handle
    /// is decided at issue rather than at redemption, so the invite and
    /// the account it becomes name the same principal.
    user: String,
    /// Readable name the redeemed account carries (D46), when the issuer
    /// gave one. Travels on the invite because the person redeeming it
    /// does not choose it — the issuer did.
    display_name: Option<String>,
    /// Grants that account will be issued.
    grants: Vec<String>,
    /// Unix seconds after which it stops working.
    expires_at: u64,
    /// Who minted it. Kept so the operator can audit issuance without a
    /// second log.
    issued_by: String,
    /// Unix seconds at minting.
    issued_at: u64,
}

/// What an invite promises its holder, read by the join page (D57).
///
/// A copy rather than a borrow: the store's lock is released before the
/// page is rendered, and rendering must not be able to hold it.
///
/// Deliberately not the whole stored invite: `secret_hash` has no business
/// leaving the store, and `issued_at` says nothing a holder needs.
pub struct InviteSummary {
    /// The account name redemption will create. Chosen by the issuer at
    /// minting, never by the holder.
    pub user: String,
    /// Readable name the account will carry (D46), when the issuer set one.
    pub display_name: Option<String>,
    /// The grants that account will be issued, as `<repo> <level>`.
    pub grants: Vec<String>,
    /// Unix seconds after which the invite stops working.
    pub expires_at: u64,
    /// Who minted it, so the holder can see whether they know that name.
    pub issued_by: String,
}

/// The whole store, as held in memory.
#[derive(Debug, Default, Clone)]
struct State {
    accounts: BTreeMap<String, Account>,
    invites: BTreeMap<String, Invite>,
    /// Names that have held an account and been revoked, and are
    /// therefore never issued again.
    ///
    /// **Forget the secret, remember the name** — and the second half
    /// costs nothing, because the name was never forgettable.
    ///
    /// Revocation deletes the token hash, the grants and the keys: a
    /// credential must be forgettable, which is the whole case for
    /// keeping credentials out of the append-only log. A name is not a
    /// credential. `OpEntry::channel` carries `git/<name>` on every op
    /// its holder ever authored, and `choir_oplog::signing_hash` covers
    /// that channel, so the name sits *inside the author's signature* in
    /// a hash chain — unrewritable without invalidating the signature
    /// that makes the entry admissible. This list therefore adds no new
    /// permanent record. It indexes one the log already keeps forever,
    /// so that reissuing a name cannot hand a second person the first
    /// person's signed attribution: their workspace tally (D37), their
    /// provenance, their reviews.
    ///
    /// **Deliberately over-refuses.** The precise rule is "refuse a name
    /// that has authored at least one op", since a name that was issued
    /// and never used has no attribution to inherit. Answering that
    /// needs a scan of log entries or a maintained index — `View::apply`
    /// takes a `ViewOp` and never sees the channel, so no fold can
    /// answer it — which is new derived persisted state to protect a
    /// rare case whose workaround is picking another name. Refusing
    /// every revoked name is the cheaper side to err on, and it is a
    /// choice rather than an oversight.
    retired: BTreeSet<String>,
}

/// Where a generated `authorized_keys` goes and what the forced command
/// in it should say (D31's file, written by the node instead of by hand).
#[derive(Debug, Clone)]
pub struct SshKeysOut {
    /// File to write. `sshd` is pointed at it once with
    /// `AuthorizedKeysFile`; it is generated, so a hand edit is lost at
    /// the next mutation.
    pub path: PathBuf,
    /// The `choir-ssh` binary the forced command runs.
    pub shim: PathBuf,
    /// Repository root the shim serves, its `--root`.
    pub root: PathBuf,
    /// The `--handoff` file the shim reads the daemon's address and
    /// loopback secret from. Without it the shim refuses pushes rather
    /// than running them unsequenced, so it is not optional here.
    pub handoff: PathBuf,
}

/// The credential store: accounts, invites, and the two files it owns.
#[derive(Debug)]
pub struct Accounts {
    path: PathBuf,
    keys_out: Option<SshKeysOut>,
    /// Trusted-keys file a redemption may append one bound actor key to,
    /// when the operator asked for that with `--invite-binds-keys`.
    ///
    /// `None` is the default and the pre-existing behaviour: redemption
    /// mints a token and nothing else, and an actor key still reaches
    /// the node by an operator pasting a line. The flag exists because
    /// that paste is the second out-of-band human step in admission, and
    /// whether to automate it is the operator's call rather than ours.
    actor_keys: Option<PathBuf>,
    /// Names the store may never issue, because something else already
    /// answers to them: every `--auth-file` user, plus the `anon`
    /// placeholder an unauthenticated request is attributed to.
    reserved: BTreeSet<String>,
    state: RwLock<State>,
    /// Bumped on every mutation. The node caches the merged ACL against
    /// it, so authentication does not rebuild a table per request.
    generation: AtomicU64,
}

impl Accounts {
    /// Opens the store at `path`, creating an empty one if absent, and
    /// writes the generated `authorized_keys` if one is configured.
    ///
    /// `reserved` is the set of names that already exist elsewhere —
    /// the `--auth-file` users — and can therefore never be issued.
    ///
    /// # Errors
    ///
    /// Returns a message when the file cannot be read, does not parse, or
    /// cannot be written back. Fatal by design: a store that half-loaded
    /// would silently drop somebody's credential, and the failure would
    /// look like a revocation nobody performed.
    pub fn open(
        path: PathBuf,
        keys_out: Option<SshKeysOut>,
        reserved: BTreeSet<String>,
    ) -> Result<Self, String> {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => parse_state(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let store = Self {
            path,
            keys_out,
            actor_keys: None,
            reserved,
            state: RwLock::new(state),
            generation: AtomicU64::new(0),
        };
        // Written on every start for the reason D31's handoff is: the
        // file is derived state, and a store restored from backup beside
        // a stale `authorized_keys` would serve keys nobody holds.
        let state = store.state.read().expect("accounts read lock");
        store.write_authorized_keys(&state)?;
        drop(state);
        Ok(store)
    }

    /// Lets a redemption bind one actor key by appending it to the
    /// operator's trusted-keys file at `path` (D51).
    ///
    /// Opt-in, and off unless the operator passed `--invite-binds-keys`.
    /// What it removes is the *clerical* half of admission: the operator
    /// still decides who is admitted, by issuing the invite, and the
    /// invite still carries the grants. What it stops requiring is a
    /// second out-of-band round trip in which a newcomer pastes a hex
    /// string to a human who pastes it into a file.
    ///
    /// The key lands in the same file an operator would have edited, so
    /// nothing downstream learns a new source of trust: the existing
    /// mtime reload picks it up, `allowed_signers` is regenerated from
    /// it, and revoking is still deleting one line.
    #[must_use]
    pub fn binding_actor_keys_into(mut self, path: PathBuf) -> Self {
        self.actor_keys = Some(path);
        self
    }

    /// Appends one `<channel> <hex>` line to the trusted-keys file.
    ///
    /// Read-modify-write rather than an open-in-append-mode: the file is
    /// 0600 operator-authored config, and this must not widen its mode,
    /// truncate it, or leave it half-written if the process dies. It
    /// also must not append a line the file already carries, which is
    /// what makes a replayed redemption a no-op instead of a duplicate
    /// the parser then refuses as two keys for one name.
    fn append_actor_key(&self, channel: &str, key_hex: &str) -> Result<(), String> {
        let Some(path) = &self.actor_keys else {
            return Ok(());
        };
        let existing = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let line = format!("{channel} {key_hex}");
        if existing
            .lines()
            .any(|row| row.split('#').next().unwrap_or("").trim() == line)
        {
            return Ok(());
        }
        // A name already spoken for by another key is refused rather
        // than appended: `parse_keys_file` rejects a file binding one
        // name twice, so appending would not grant this key anything —
        // it would break every key in the file at the next reload.
        if existing.lines().any(|row| {
            row.split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next()
                .is_some_and(|name| name == channel)
        }) {
            return Err(format!(
                "the trusted-keys file already binds `{channel}` to a different key"
            ));
        }
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&line);
        next.push('\n');
        choir_fs::write_atomic_private(path, &next).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Where the store is persisted. The `choir-ssh` shim is pointed at
    /// it through the D31 handoff file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many accounts have been issued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .read()
            .expect("accounts read lock")
            .accounts
            .len()
    }

    /// Whether no account has been issued yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Counter bumped by every mutation, for callers caching anything
    /// derived from the store.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The grants this store holds, as an [`Acl`] the node merges with
    /// the file table.
    ///
    /// Built by rendering ACL lines and parsing them, rather than by
    /// constructing grants directly, so the two paths cannot drift into
    /// meaning different things by the same words.
    #[must_use]
    pub fn acl(&self) -> Acl {
        let state = self.state.read().expect("accounts read lock");
        Acl::parse(&acl_text(&state)).unwrap_or_default()
    }

    /// Identifies a presented `user:secret` pair, in constant time
    /// against the stored hash, or `None` when it matches nothing.
    ///
    /// An expired invite matches nothing, so expiry needs no sweep to
    /// take effect.
    #[must_use]
    pub fn authenticate(&self, user: &str, secret: &str) -> Option<Principal> {
        let presented = hash(secret);
        let state = self.state.read().expect("accounts read lock");
        if let Some(account) = state.accounts.get(user) {
            if ct_eq(account.token_hash.as_bytes(), presented.as_bytes()) {
                return Some(Principal::Account(user.to_string()));
            }
            return None;
        }
        let invite = state.invites.get(user)?;
        if invite.expires_at <= now_secs() {
            return None;
        }
        ct_eq(invite.secret_hash.as_bytes(), presented.as_bytes())
            .then(|| Principal::Invite(user.to_string()))
    }

    /// Mints an invite for the account described by `body`, as `issuer`.
    ///
    /// Returns the API's `(status, json)`. The secret half is in the
    /// response and nowhere else: only its hash is stored, so an invite
    /// that is lost is reissued rather than recovered.
    #[must_use]
    pub fn invite(&self, issuer: &str, body: &serde_json::Value) -> (u16, String) {
        // D46: which field names the account decides whether the log
        // will carry a person's name forever. `user` is the pre-D46
        // spelling and still means "this exact string is the principal",
        // so it is what an operator credential or a bot wants.
        // `display_name` mints an opaque handle and keeps the readable
        // name in the store, where revoking can delete it.
        let (user, display_name) = match (
            body.get("user").and_then(serde_json::Value::as_str),
            body.get("display_name").and_then(serde_json::Value::as_str),
        ) {
            (Some(_), Some(_)) => {
                return bad_request(
                    "send `user` or `display_name`, not both: they are two different \
                     decisions about what the log records forever",
                )
            }
            (Some(user), None) => (user.to_string(), None),
            (None, Some(name)) => {
                if let Err(e) = validate_display_name(name) {
                    return bad_request(&e);
                }
                let state = self.state.read().expect("accounts read lock");
                let taken = |candidate: &str| {
                    state.accounts.contains_key(candidate)
                        || state.retired.contains(candidate)
                        || self.reserved.contains(candidate)
                };
                let Some(handle) = mint_handle(&taken) else {
                    drop(state);
                    return server_error("could not mint a free account handle");
                };
                drop(state);
                (handle, Some(name.to_string()))
            }
            (None, None) => return bad_request("`user` or `display_name` is required"),
        };
        let user = user.as_str();
        if let Err(e) = validate_username(user) {
            return bad_request(&e);
        }
        if self.reserved.contains(user) {
            return bad_request(&format!(
                "`{user}` is already an operator credential; the auth file owns that name"
            ));
        }
        let grants = match body.get("grants") {
            Some(serde_json::Value::Array(items)) => {
                let mut grants = Vec::new();
                for item in items {
                    let Some(text) = item.as_str() else {
                        return bad_request("each grant must be a string, `<repo|*> <read|write>`");
                    };
                    match validate_grant(user, text) {
                        Ok(grant) => grants.push(grant),
                        Err(e) => return bad_request(&e),
                    }
                }
                grants
            }
            // An account with no grants is a credential that authenticates
            // and reaches nothing, which is a confusing thing to hand
            // somebody. Refused rather than issued.
            _ => return bad_request("`grants` is required and must be a non-empty array"),
        };
        if grants.is_empty() {
            return bad_request("`grants` is required and must be a non-empty array");
        }
        let ttl = match body.get("expires_in_secs") {
            None => DEFAULT_INVITE_SECS,
            Some(value) => match value.as_u64() {
                Some(secs) if secs > 0 && secs <= MAX_INVITE_SECS => secs,
                _ => {
                    return bad_request(&format!(
                        "`expires_in_secs` must be between 1 and {MAX_INVITE_SECS}"
                    ))
                }
            },
        };

        let mut state = self.state.write().expect("accounts write lock");
        if state.accounts.contains_key(user) {
            return conflict(&format!("`{user}` already has an account; revoke it first"));
        }
        // Names are never reused. See `State::retired`: the log has this
        // string frozen into every channel the old holder wrote under,
        // and issuing it again transfers their attribution to somebody
        // else with nothing able to tell them apart afterwards.
        if state.retired.contains(user) {
            return conflict(&format!(
                "`{user}` was revoked and is never reused: the op log still attributes that \
                 name's history to whoever held it. Choose another name."
            ));
        }
        state
            .invites
            .retain(|_, invite| invite.expires_at > now_secs());
        if state.invites.values().any(|invite| invite.user == user) {
            return conflict(&format!(
                "`{user}` already has an invite outstanding; revoke it first"
            ));
        }
        let id = format!("{INVITE_PREFIX}{}", mint_secret());
        let secret = mint_secret();
        let expires_at = now_secs().saturating_add(ttl);
        state.invites.insert(
            id.clone(),
            Invite {
                secret_hash: hash(&secret),
                user: user.to_string(),
                display_name: display_name.clone(),
                grants: grants.clone(),
                expires_at,
                issued_by: issuer.to_string(),
                issued_at: now_secs(),
            },
        );
        if let Err(e) = self.commit(&state) {
            return server_error(&e);
        }
        drop(state);
        (
            200,
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                // The principal, which is the minted handle when the
                // issuer sent a `display_name` (D46). Returned because
                // this is the only moment the issuer learns it, and they
                // need it to write an ACL line — the store is the only
                // other place the pairing exists, and deleting the
                // account is meant to destroy it.
                "user": user,
                "display_name": display_name,
                "invite_id": id,
                // The two halves as one basic-auth pair, because that is
                // how it is used: `curl -u <invite>` and nothing else.
                "invite": format!("{id}:{secret}"),
                "grants": grants,
                "expires_at": expires_at,
                "note": "single use; shown once. The holder redeems it at POST /api/accounts/redeem.",
            })
            .to_string(),
        )
    }

    /// Redeems `invite_id`, creating its account and minting its token.
    ///
    /// The token is in the response and nowhere else. An optional
    /// `ssh_key` registers a key at the same time, which is what makes
    /// the SSH transport self-service rather than a second errand.
    #[must_use]
    pub fn redeem(&self, invite_id: &str, body: &serde_json::Value) -> (u16, String) {
        let actor_key = match body.get("actor_key") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(hex)) => {
                if self.actor_keys.is_none() {
                    return bad_request(
                        "this node does not bind actor keys at redemption; \
                         ask the operator to register your key, or to start the \
                         daemon with --invite-binds-keys",
                    );
                }
                match validate_actor_key(hex) {
                    Ok(hex) => Some(hex),
                    Err(e) => return bad_request(&e),
                }
            }
            Some(_) => return bad_request("`actor_key` must be a string"),
        };
        let ssh_key = match body.get("ssh_key") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(line)) => match validate_ssh_key(line) {
                Ok(key) => Some(key),
                Err(e) => return bad_request(&e),
            },
            Some(_) => return bad_request("`ssh_key` must be a string"),
        };
        let mut state = self.state.write().expect("accounts write lock");
        let Some(invite) = state.invites.get(invite_id).cloned() else {
            return (404, error_json("no such invite"));
        };
        if invite.expires_at <= now_secs() {
            state.invites.remove(invite_id);
            let _ = self.commit(&state);
            return (403, error_json("this invite has expired"));
        }
        if state.accounts.contains_key(&invite.user) {
            return conflict("that account already exists");
        }
        // The auth file can have gained that name between minting and
        // redemption. Issuing anyway would create an account nothing can
        // ever use, because authentication consults the operator's file
        // first — a credential that authenticates as somebody else.
        if self.reserved.contains(&invite.user) {
            return conflict("that name became an operator credential; ask for a new invite");
        }
        // Defence in depth, and measured to be exactly that: no sequence
        // of API calls can reach this line with a retired name, because
        // `revoke` drops every invite naming the user in the same locked
        // write that retires it, and `invite` refuses a retired name.
        // Deleting this check leaves the whole suite green — proved by
        // mutation on 2026-08-14 rather than assumed — so it is not a
        // guard any test can be said to hold. What it does cover is the
        // store edited by hand while the node is stopped, which D36 keeps
        // as the emergency revocation path, and a future `revoke` that
        // stops dropping invites. It is kept for the second reason more
        // than the first: this is the line that would notice.
        if state.retired.contains(&invite.user) {
            return conflict("that name has been revoked and is never reused; ask for another");
        }
        // The bound channel always carries the account name as its
        // operator prefix. That is not decoration: the reviewer draw
        // refuses to draw a reviewer sharing the author's operator
        // prefix, so a channel a newcomer could name freely would let
        // them place themselves outside their own operator and be drawn
        // onto a colleague's review -- or name a prefix belonging to
        // somebody else entirely.
        let channel = match actor_key.as_ref() {
            None => None,
            Some(_) => match body.get("channel") {
                None | Some(serde_json::Value::Null) => Some(format!("{}/agent", invite.user)),
                Some(serde_json::Value::String(channel)) => {
                    match channel.split_once('/') {
                        Some((prefix, suffix))
                            if prefix == invite.user
                                && !suffix.is_empty()
                                && suffix.len() <= 32
                                && suffix
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
                        {
                            Some(channel.clone())
                        }
                        _ => {
                            return bad_request(&format!(
                                "`channel` must be `{}/<name>`, where <name> is letters, digits, `-` or `_`",
                                invite.user
                            ))
                        }
                    }
                }
                Some(_) => return bad_request("`channel` must be a string"),
            },
        };
        // Before the account is inserted and the invite consumed: an
        // append that fails must leave the invite redeemable, or a
        // newcomer whose key collided with a name is left holding a
        // spent invite and no account.
        if let (Some(channel), Some(key_hex)) = (channel.as_ref(), actor_key.as_ref()) {
            if let Err(e) = self.append_actor_key(channel, key_hex) {
                return conflict(&e);
            }
        }
        let token = mint_secret();
        state.accounts.insert(
            invite.user.clone(),
            Account {
                token_hash: hash(&token),
                display_name: invite.display_name.clone(),
                grants: invite.grants.clone(),
                ssh_keys: ssh_key.into_iter().collect(),
                actor_key: channel.clone().zip(actor_key.clone()),
                // Enrolment is a later, separately authenticated act:
                // redemption proves you hold the invite, not that you
                // hold an authenticator.
                passkeys: Vec::new(),
                created_at: now_secs(),
            },
        );
        // Single use: consumed whether or not anything below fails, so a
        // replayed redemption cannot mint a second token.
        state.invites.remove(invite_id);
        if let Err(e) = self.commit(&state) {
            return server_error(&e);
        }
        // Moved rather than cloned: the record is already this function's
        // own copy, and the store keeps its own.
        let (user, grants) = (invite.user, invite.grants);
        drop(state);
        (
            200,
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                "user": user,
                "token": token,
                "grants": grants,
                "channel": channel,
                "actor_key_bound": actor_key.is_some(),
                "note": "shown once; the node stores only its hash. Use it as the password in basic auth.",
            })
            .to_string(),
        )
    }

    /// Removes an account and any invite outstanding for the same name.
    ///
    /// Deletion rather than a tombstone: the token stops authenticating
    /// on the next request, the grants leave the merged table, and the
    /// key leaves the generated `authorized_keys`. That is the property
    /// an append-only log could not have provided.
    #[must_use]
    pub fn revoke(&self, body: &serde_json::Value) -> (u16, String) {
        let Some(user) = body.get("user").and_then(serde_json::Value::as_str) else {
            return bad_request("`user` is required");
        };
        let mut state = self.state.write().expect("accounts write lock");
        let had_account = state.accounts.remove(user).is_some();
        let before = state.invites.len();
        state.invites.retain(|_, invite| invite.user != user);
        let invites_dropped = before - state.invites.len();
        if !had_account && invites_dropped == 0 {
            return (404, error_json("no such account"));
        }
        // Retired only when an account really existed. A name whose
        // invite was cancelled before redemption never wrote anything to
        // the log, so there is no history to protect and burning the name
        // over a mistyped invite would be a worse answer than reusing it.
        if had_account {
            state.retired.insert(user.to_string());
        }
        if let Err(e) = self.commit(&state) {
            return server_error(&e);
        }
        drop(state);
        (
            200,
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                "user": user,
                "account_revoked": had_account,
                "invites_revoked": invites_dropped,
            })
            .to_string(),
        )
    }

    /// Enrols a WebAuthn credential on `user`'s own account (D39).
    ///
    /// The caller is the account: this never takes a `user` from the
    /// body, so holding a credential is the whole authorization story
    /// and there is no way to spell "enrol a key on someone else".
    ///
    /// **What this does not check, said plainly rather than implied by
    /// silence: possession.** D39 scoped out a CBOR reader, so nothing
    /// here parses or verifies an attestation object; the node takes the
    /// public key the authenticated caller sends. Enrolling a key you do
    /// not hold gains you nothing — you still cannot sign with it — but
    /// a *stolen token* can enrol an attacker's own authenticator and
    /// keep it. Two things bound that: the roster lists every enrolled
    /// credential, so it is visible rather than silent, and revocation
    /// deletes the account and its keys with it.
    ///
    /// # Errors
    ///
    /// 400 for a missing or malformed field, a public key that is not a
    /// P-256 SubjectPublicKeyInfo, or a credential id already enrolled
    /// on this account; 404 when the caller has no account record, which
    /// is the case for an `--auth-file` operator.
    #[must_use]
    pub fn enroll_passkey(&self, user: &str, body: &serde_json::Value) -> (u16, String) {
        let field = |name: &str| {
            body.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let (Some(credential_id), Some(public_key)) = (field("credential_id"), field("public_key"))
        else {
            return bad_request("`credential_id` and `public_key` are required");
        };
        let label = field("label").unwrap_or("passkey");
        if let Err(e) = validate_passkey_id(credential_id) {
            return bad_request(&e);
        }
        if let Err(e) = validate_label(label) {
            return bad_request(&e);
        }
        if let Err(e) = validate_p256_spki(public_key) {
            return bad_request(&e);
        }

        let mut state = self.state.write().expect("accounts write lock");
        let Some(account) = state.accounts.get_mut(user) else {
            return (
                404,
                error_json(
                    "no account record for this credential; passkeys are enrolled on issued \
                     accounts, and an operator credential from the auth file is not one",
                ),
            );
        };
        if account
            .passkeys
            .iter()
            .any(|k| k.credential_id == credential_id)
        {
            return conflict("that credential is already enrolled");
        }
        if account.passkeys.len() >= MAX_PASSKEYS {
            return conflict("this account already holds the maximum number of passkeys");
        }
        account.passkeys.push(Passkey {
            credential_id: credential_id.to_string(),
            public_key: public_key.to_string(),
            label: label.to_string(),
            created_at: now_secs(),
        });
        let enrolled = account.passkeys.len();
        if let Err(e) = self.commit(&state) {
            return server_error(&e);
        }
        drop(state);
        (
            200,
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                "user": user,
                "credential_id": credential_id,
                "label": label,
                "enrolled": enrolled,
            })
            .to_string(),
        )
    }

    /// Removes one of `user`'s own enrolled credentials (D39).
    ///
    /// The mirror of enrolment, and it exists for the same reason
    /// revocation does: a credential that cannot be withdrawn is not a
    /// credential, it is a permanent fact. A lost authenticator has to be
    /// removable by the person who still holds the token.
    ///
    /// # Errors
    ///
    /// 400 without `credential_id`, and 404 when this account has no such
    /// credential enrolled.
    #[must_use]
    pub fn remove_passkey(&self, user: &str, body: &serde_json::Value) -> (u16, String) {
        let Some(credential_id) = body
            .get("credential_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return bad_request("`credential_id` is required");
        };
        let mut state = self.state.write().expect("accounts write lock");
        let Some(account) = state.accounts.get_mut(user) else {
            return (404, error_json("no account record for this credential"));
        };
        let before = account.passkeys.len();
        account
            .passkeys
            .retain(|k| k.credential_id != credential_id);
        if account.passkeys.len() == before {
            return (404, error_json("no such credential on this account"));
        }
        let remaining = account.passkeys.len();
        if let Err(e) = self.commit(&state) {
            return server_error(&e);
        }
        drop(state);
        (
            200,
            serde_json::json!({
                "format_version": FORMAT_VERSION,
                "user": user,
                "credential_id": credential_id,
                "remaining": remaining,
            })
            .to_string(),
        )
    }

    /// The SubjectPublicKeyInfo DER of one enrolled credential, ready for
    /// [`choir_identity::verify_webauthn_assertion`] (D39).
    ///
    /// Keyed by `(user, credential_id)` rather than by credential id
    /// alone: an assertion names a credential, but the request already
    /// names a principal, and looking the key up under that principal is
    /// what stops one account's assertion from being spent as another's.
    #[must_use]
    pub fn passkey_spki(&self, user: &str, credential_id: &str) -> Option<Vec<u8>> {
        let state = self.state.read().expect("accounts read lock");
        let encoded = state
            .accounts
            .get(user)?
            .passkeys
            .iter()
            .find(|k| k.credential_id == credential_id)?
            .public_key
            .clone();
        drop(state);
        base64url_decode(&encoded)
    }

    /// One account's own enrolled credentials, for the page that account
    /// manages them on (D39).
    ///
    /// Separate from [`Accounts::list_json`] because the questions differ:
    /// the roster is the operator's view of everyone and needs a node-wide
    /// read, while this is a person looking at their own keys and needs
    /// only their own credential. Returning an empty list for an unknown
    /// name is deliberate — an `--auth-file` operator has no account
    /// record, and that is "nothing enrolled", not an error.
    #[must_use]
    pub fn passkeys_json(&self, user: &str) -> Vec<serde_json::Value> {
        let state = self.state.read().expect("accounts read lock");
        state
            .accounts
            .get(user)
            .map(|account| render_passkeys(&account.passkeys))
            .unwrap_or_default()
    }

    /// Whether this name has an account record at all, which is what
    /// decides between "you have no passkeys yet" and "this credential
    /// cannot hold one".
    #[must_use]
    pub fn has_account(&self, user: &str) -> bool {
        self.state
            .read()
            .expect("accounts read lock")
            .accounts
            .contains_key(user)
    }

    /// What an invite promises, for the page that shows a holder what
    /// they are about to accept (D57).
    ///
    /// **Only ever call this for an invite whose secret has already
    /// authenticated.** Nothing here checks possession, so a caller that
    /// reaches it with an id alone has built an oracle: `Some` versus
    /// `None` would tell an anonymous stranger which invite ids exist and
    /// which accounts are pending on this node. The one caller is the join
    /// page, which gets the id from a successful
    /// [`Accounts::authenticate`] and never from the request.
    ///
    /// Expiry is re-checked rather than assumed, so this cannot be the
    /// place a stale invite is presented as a live one.
    #[must_use]
    pub fn invite_summary(&self, invite_id: &str) -> Option<InviteSummary> {
        let state = self.state.read().expect("accounts read lock");
        let invite = state.invites.get(invite_id)?;
        if invite.expires_at <= now_secs() {
            return None;
        }
        Some(InviteSummary {
            user: invite.user.clone(),
            display_name: invite.display_name.clone(),
            grants: invite.grants.clone(),
            expires_at: invite.expires_at,
            issued_by: invite.issued_by.clone(),
        })
    }

    /// What to call `user` in something a person reads (D46).
    ///
    /// `None` means "call it by its own name", which is the answer in
    /// three different situations a caller must not try to tell apart:
    /// an account issued before D46, one issued with an explicit `user`,
    /// and one whose display name has been deleted. **The third is the
    /// point of the field**, so a caller that renders "unknown" or
    /// "deleted" for it undoes the deletion by announcing it. Render the
    /// handle and say nothing.
    #[must_use]
    pub fn display_name(&self, user: &str) -> Option<String> {
        self.state
            .read()
            .expect("accounts read lock")
            .accounts
            .get(user)
            .and_then(|account| account.display_name.clone())
    }

    /// Every handle the store can name, as `(handle, display name)`.
    ///
    /// The rendering half of the D46 ACL decision: grants are written
    /// against handles, and `choir acl render` regenerates the trailing
    /// comments from this. Accounts with no display name are omitted
    /// rather than listed as themselves — a comment repeating the handle
    /// is noise, and the file already says it.
    #[must_use]
    pub fn roster(&self) -> BTreeMap<String, String> {
        self.state
            .read()
            .expect("accounts read lock")
            .accounts
            .iter()
            .filter_map(|(user, account)| {
                account
                    .display_name
                    .clone()
                    .map(|name| (user.clone(), name))
            })
            .collect()
    }

    /// Everything the store holds except the secrets: who has an account,
    /// what they were granted, which public keys are registered, and
    /// which invites are outstanding.
    #[must_use]
    pub fn list_json(&self) -> serde_json::Value {
        let state = self.state.read().expect("accounts read lock");
        let now = now_secs();
        let accounts: Vec<serde_json::Value> = state
            .accounts
            .iter()
            .map(|(user, account)| {
                serde_json::json!({
                    "user": user,
                    // Always present, `null` when the account has none
                    // (D46). The store file omits the key to stay
                    // readable; this is read by programs, and a key that
                    // appears and disappears is one every client has to
                    // handle twice.
                    "display_name": account.display_name,
                    "grants": account.grants,
                    "ssh_keys": account.ssh_keys,
                    "passkeys": render_passkeys(&account.passkeys),
                    "created_at": account.created_at,
                })
            })
            .collect();
        let invites: Vec<serde_json::Value> = state
            .invites
            .iter()
            .filter(|(_, invite)| invite.expires_at > now)
            .map(|(id, invite)| {
                serde_json::json!({
                    "invite_id": id,
                    "user": invite.user,
                    "display_name": invite.display_name,
                    "grants": invite.grants,
                    "issued_by": invite.issued_by,
                    "issued_at": invite.issued_at,
                    "expires_at": invite.expires_at,
                })
            })
            .collect();
        serde_json::json!({
            "format_version": FORMAT_VERSION,
            "accounts": accounts,
            "invites": invites,
            // Served so an operator can see why a name is refused before
            // they go looking for an override.
            "retired": state.retired,
        })
    }

    /// Persists the state and regenerates the derived key file, then
    /// bumps the generation counter.
    fn commit(&self, state: &State) -> Result<(), String> {
        let text = render_state(state);
        write_private(&self.path, text.as_bytes())?;
        self.write_authorized_keys(state)?;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Rewrites the generated `authorized_keys`, when one is configured.
    ///
    /// Every field interpolated into a line is validated before it is
    /// stored — the username by [`validate_username`], the key by
    /// [`validate_ssh_key`], which also drops the client's comment — so
    /// no value here can end a line early and start another with a
    /// forced command of its own.
    fn write_authorized_keys(&self, state: &State) -> Result<(), String> {
        let Some(out) = &self.keys_out else {
            return Ok(());
        };
        let mut text = String::from(
            "# Generated by choir-node (D36). Edits are lost at the next account change;\n\
             # point sshd at a second file rather than editing this one.\n",
        );
        for (user, account) in &state.accounts {
            for key in &account.ssh_keys {
                text.push_str(&format!(
                    "command=\"{} --root {} --user {user} --handoff {}\",restrict {key} {user}\n",
                    out.shim.display(),
                    out.root.display(),
                    out.handoff.display(),
                ));
            }
        }
        write_private(&out.path, text.as_bytes())
    }
}

/// The grants in a store, as an [`Acl`] file body.
///
/// Exposed for the `choir-ssh` shim, which enforces the same grants from
/// a separate process and must not be able to disagree with the daemon
/// about what they mean.
///
/// # Errors
///
/// Returns a message when the store cannot be read or does not parse. A
/// missing file yields an empty table rather than an error: an ACL that
/// grants nothing is the fail-closed answer, and refusing to start would
/// take the SSH transport down with a file the daemon may simply not
/// have written yet.
pub fn grants_acl(path: &Path) -> Result<Acl, String> {
    let state = match std::fs::read_to_string(path) {
        Ok(text) => parse_state(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    Acl::parse(&acl_text(&state))
}

/// The ACL file body a store's grants amount to.
fn acl_text(state: &State) -> String {
    let mut text = String::new();
    for (user, account) in &state.accounts {
        for grant in &account.grants {
            text.push_str(&format!("{user} {grant}\n"));
        }
    }
    text
}

/// Checks one grant column pair against the real ACL parser and returns
/// it in canonical spelling.
///
/// # Errors
///
/// Returns a message when the pair does not parse, or when it names
/// [`crate::acl::Scope::Node`]: node-wide authority is what D33's rate-limit
/// exemption and D29's whole-node reads key on, so it stays operator-
/// authored in the ACL file and is never self-service.
pub fn validate_grant(user: &str, grant: &str) -> Result<String, String> {
    let mut columns = grant.split_whitespace();
    let (Some(target), Some(level), deadline, None) = (
        columns.next(),
        columns.next(),
        columns.next(),
        columns.next(),
    ) else {
        return Err(format!(
            "`{grant}` is not a grant; write `<repo|*> <read|write>`, optionally followed by \
             `until=<unix seconds>`"
        ));
    };
    if target == "@node" || target.starts_with('@') {
        return Err(
            "`@node` cannot be issued: node-wide authority stays in the ACL file (D36)".to_string(),
        );
    }
    let tail = deadline.map(|d| format!(" {d}")).unwrap_or_default();
    let table = Acl::parse(&format!("{user} {target} {level}{tail}\n"))?;
    // Belt and braces against a future spelling of the node scope that
    // the check above does not recognize: ask the parsed table rather
    // than the text. Asked of the file's own words rather than of a
    // dated table, so a grant issued with a deadline in the past cannot
    // pass this check by being expired rather than by being allowed.
    if table.grants_node(user) {
        return Err(
            "`@node` cannot be issued: node-wide authority stays in the ACL file (D36)".to_string(),
        );
    }
    Ok(format!("{target} {level}{tail}"))
}

/// Checks a readable display name (D46).
///
/// Looser than [`validate_username`] on purpose: this string is never a
/// principal, never a path segment, never an ACL subject and never a
/// channel, so the reasons a username is restricted do not apply to it.
/// What it must not do is break the files and pages that render it, so
/// control characters and newlines are refused and the length is capped.
///
/// The one restriction that is not about rendering: a name may not be
/// spelled like a handle. A page shows an unresolvable handle as itself,
/// so a display name of `HANDLE_CHARS` hex characters renders exactly
/// as somebody else's deleted account does, and the person it points at
/// cannot correct the record because their name is the thing that was
/// deleted. Refused at the door because the alternative is a rendering
/// rule that has to know which of two identical strings it is holding.
///
/// # Errors
///
/// Returns a message when it is empty, too long, carries a control
/// character, or is spelled like an account handle.
pub fn validate_display_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_LABEL_CHARS {
        return Err(format!(
            "a display name must be 1 to {MAX_LABEL_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("a display name must not contain control characters".to_string());
    }
    if looks_like_a_handle(name) {
        return Err(format!(
            "a display name must not be spelled like an account handle \
             ({HANDLE_CHARS} hexadecimal characters)"
        ));
    }
    Ok(())
}

/// Whether a string is spelled the way [`mint_handle`] spells one.
///
/// Case-insensitive, because a reader comparing a name against a handle
/// is not comparing bytes, and `7F3AC2AB19CD` impersonates
/// `7f3ac2ab19cd` on every surface a person actually reads.
fn looks_like_a_handle(name: &str) -> bool {
    name.len() == HANDLE_CHARS && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Checks a name the node will interpolate into an `authorized_keys`
/// forced command and key on for every authorization decision.
///
/// # Errors
///
/// Returns a message when the name is empty, too long, spelled with
/// anything but ASCII letters, digits, `-`, `_` and `.`, reserved for the
/// unauthenticated placeholder, or spelled like an invite id.
pub fn validate_username(user: &str) -> Result<(), String> {
    if user.is_empty() || user.len() > 32 {
        return Err("a username must be 1 to 32 characters".to_string());
    }
    if !user
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err("a username may hold only ASCII letters, digits, `-`, `_` and `.`".to_string());
    }
    if user == "anon" {
        return Err("`anon` is the name an unauthenticated request already has".to_string());
    }
    if user.starts_with(INVITE_PREFIX) {
        return Err(format!("a username may not start with `{INVITE_PREFIX}`"));
    }
    Ok(())
}

/// Checks an ed25519 actor public key and returns it lowercased.
///
/// Normalised rather than accepted as sent, because this string becomes
/// a line in the trusted-keys file and the duplicate check that keeps a
/// replayed redemption idempotent is a string comparison. Two spellings
/// of one key would append it twice, and a file binding one name to two
/// keys is refused wholesale at the next reload -- taking every other
/// key in it down with it.
///
/// # Errors
///
/// Returns a message when the value is not 64 hex characters.
pub fn validate_actor_key(hex: &str) -> Result<String, String> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("an actor key is 64 hex characters (an ed25519 public key)".to_string());
    }
    Ok(hex.to_ascii_lowercase())
}

/// Checks an OpenSSH public key line and returns it without its comment.
///
/// The comment is dropped rather than validated: it is the one field a
/// client controls freely, the generated `authorized_keys` is a
/// line-oriented file where a forced command precedes the key, and a
/// value that cannot appear cannot be escaped wrongly. The node writes
/// the account name there instead.
///
/// # Errors
///
/// Returns a message when the line is not a single `ssh-ed25519 <base64>`
/// pair whose blob really is a 32-byte ed25519 key.
pub fn validate_ssh_key(line: &str) -> Result<String, String> {
    if line.contains('\n') || line.contains('\r') {
        return Err("an ssh key must be one line".to_string());
    }
    let mut columns = line.split_whitespace();
    let (Some(algorithm), Some(blob)) = (columns.next(), columns.next()) else {
        return Err("an ssh key is `ssh-ed25519 <base64>`".to_string());
    };
    if algorithm != "ssh-ed25519" {
        return Err(format!(
            "`{algorithm}` keys are not registered here; this node uses ssh-ed25519"
        ));
    }
    let Some(bytes) = crate::base64_decode(blob) else {
        return Err("the key blob is not base64".to_string());
    };
    // OpenSSH wire form: length-prefixed algorithm name, then the
    // length-prefixed 32-byte key. Anything else is not the key it says
    // it is, whatever it decodes to.
    let expected: Vec<u8> = [0, 0, 0, 11]
        .iter()
        .copied()
        .chain(b"ssh-ed25519".iter().copied())
        .chain([0, 0, 0, 32])
        .collect();
    if bytes.len() != 51 || !bytes.starts_with(&expected) {
        return Err("that is not an ssh-ed25519 public key".to_string());
    }
    Ok(format!("{algorithm} {blob}"))
}

/// A fresh 256-bit secret in hex, from the same generator every other key
/// on this node comes from. No `rand` dependency is added for it: an
/// actor key is already an OS-random keypair, and its actor id is the
/// BLAKE3 of the public half.
fn mint_secret() -> String {
    ActorKey::generate().actor_id().to_hex()
}

/// How many hex characters an account handle carries (D46).
///
/// Twelve is 48 bits: long enough that [`mint_handle`]'s retry loop is
/// theatre rather than a real code path, short enough to read in an ACL
/// line and a review page. The value is not load-bearing — the
/// collision check is — so it can be raised later without a migration.
const HANDLE_CHARS: usize = 12;

/// A fresh account handle: opaque, and from the same OS-random source
/// every other secret here comes from (D46).
///
/// **Deliberately derived from nothing.** A handle computed from the
/// person — a hash of their name or email — would be stable, which
/// sounds like a feature and is the whole vulnerability: anyone holding
/// a guess at the input can confirm it against the log forever, and the
/// log is the thing we cannot take back. Random costs a collision check
/// and buys the property.
///
/// `taken` must answer for live accounts, retired handles **and**
/// operator credentials from the auth file. Returns `None` if it cannot
/// find a free handle, which the caller must treat as a refusal rather
/// than fall back to a name.
fn mint_handle(taken: &dyn Fn(&str) -> bool) -> Option<String> {
    for _ in 0..8 {
        let minted = mint_secret();
        // `mint_secret` returns a self-describing hash, `<codec>-<hex>`
        // (invariant 2). The codec byte is the same on every one of
        // them, so it carries no entropy and only costs width here.
        let handle: String = minted
            .rsplit('-')
            .next()?
            .chars()
            .take(HANDLE_CHARS)
            .collect();
        if handle.len() == HANDLE_CHARS && !taken(&handle) {
            return Some(handle);
        }
    }
    None
}

/// BLAKE3 of a secret, hex, with its codec byte — the same envelope
/// every other hash on this node carries (invariant 2).
fn hash(secret: &str) -> String {
    ContentHash::blake3(secret.as_bytes()).to_hex()
}

/// Compares without an early exit, so timing does not leak how much of a
/// presented secret matched.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as usize;
    }
    diff == 0
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
///
/// The one clock authorization reads. An invite's expiry (D36) and a
/// grant's deadline (D66) are the same kind of statement about the same
/// timeline, so they are answered by the same function rather than by
/// two that could drift.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Writes `bytes` to `path` at 0600 through a temporary file and a
/// rename, so a reader never sees a half-written store and a crash
/// leaves the previous one intact.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let name = path
        .file_name()
        .ok_or_else(|| format!("{}: not a file path", path.display()))?;
    let temp = path.with_file_name(format!("{}.tmp", name.to_string_lossy()));
    std::fs::write(&temp, bytes).map_err(|e| format!("{}: {e}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", temp.display()))?;
    }
    std::fs::rename(&temp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The store as JSON.
fn render_state(state: &State) -> String {
    let accounts: Vec<serde_json::Value> = state
        .accounts
        .iter()
        .map(|(user, account)| {
            let mut record = serde_json::json!({
                "user": user,
                "token_hash": account.token_hash,
                "grants": account.grants,
                "ssh_keys": account.ssh_keys,
                "passkeys": account.passkeys.iter().map(|k| serde_json::json!({
                    "credential_id": k.credential_id,
                    "public_key": k.public_key,
                    "label": k.label,
                    "created_at": k.created_at,
                })).collect::<Vec<_>>(),
                "created_at": account.created_at,
            });
            // Emitted only when set, so an account issued before D46
            // renders exactly the record it always did. The store is a
            // node-owned file rather than hashed bytes, so this is
            // tidiness rather than an invariant -- but a `null` on every
            // legacy row is noise an operator has to learn to ignore,
            // and things operators learn to ignore stop being read.
            if let Some((channel, hex)) = account.actor_key.as_ref() {
                record["actor_key_channel"] = serde_json::json!(channel);
                record["actor_key"] = serde_json::json!(hex);
            }
            if let Some(name) = account.display_name.as_deref() {
                record
                    .as_object_mut()
                    .expect("account record is an object")
                    .insert("display_name".into(), serde_json::json!(name));
            }
            record
        })
        .collect();
    let invites: Vec<serde_json::Value> = state
        .invites
        .iter()
        .map(|(id, invite)| {
            let mut record = serde_json::json!({
                "invite_id": id,
                "secret_hash": invite.secret_hash,
                "user": invite.user,
                "grants": invite.grants,
                "expires_at": invite.expires_at,
                "issued_by": invite.issued_by,
                "issued_at": invite.issued_at,
            });
            if let Some(name) = invite.display_name.as_deref() {
                record
                    .as_object_mut()
                    .expect("invite record is an object")
                    .insert("display_name".into(), serde_json::json!(name));
            }
            record
        })
        .collect();
    format!(
        "{}\n",
        serde_json::json!({
            "format_version": FORMAT_VERSION,
            "accounts": accounts,
            "invites": invites,
            "retired": state.retired,
        })
    )
}

/// Parses the store, refusing anything it does not fully understand.
fn parse_state(text: &str) -> Result<State, String> {
    if text.trim().is_empty() {
        return Ok(State::default());
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(FORMAT_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "format_version {other} is newer than this binary understands ({FORMAT_VERSION}); \
                 refusing to load rather than drop the fields it does not know"
            ))
        }
        None => return Err("missing format_version".to_string()),
    }
    let strings = |value: Option<&serde_json::Value>| -> Vec<String> {
        value
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut state = State {
        retired: strings(value.get("retired")).into_iter().collect(),
        ..State::default()
    };
    for entry in value
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let (Some(user), Some(token_hash)) = (
            entry.get("user").and_then(serde_json::Value::as_str),
            entry.get("token_hash").and_then(serde_json::Value::as_str),
        ) else {
            return Err("an account record is missing `user` or `token_hash`".to_string());
        };
        state.accounts.insert(
            user.to_string(),
            Account {
                token_hash: token_hash.to_string(),
                // Absent on every account issued before D46, and that
                // decodes as `None` rather than as the username: the
                // point of the field is that it can be deleted, and a
                // value invented at load could not be.
                display_name: entry
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string),
                grants: strings(entry.get("grants")),
                ssh_keys: strings(entry.get("ssh_keys")),
                passkeys: parse_passkeys(entry.get("passkeys")),
                // Both halves or neither: a channel with no key names
                // nothing, and a key with no channel says the account
                // arrived with a binding while refusing to say to what.
                actor_key: entry
                    .get("actor_key_channel")
                    .and_then(serde_json::Value::as_str)
                    .zip(entry.get("actor_key").and_then(serde_json::Value::as_str))
                    .map(|(channel, hex)| (channel.to_string(), hex.to_string())),
                created_at: entry
                    .get("created_at")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        );
    }
    for entry in value
        .get("invites")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let (Some(id), Some(secret_hash), Some(user)) = (
            entry.get("invite_id").and_then(serde_json::Value::as_str),
            entry.get("secret_hash").and_then(serde_json::Value::as_str),
            entry.get("user").and_then(serde_json::Value::as_str),
        ) else {
            return Err(
                "an invite record is missing `invite_id`, `secret_hash` or `user`".to_string(),
            );
        };
        state.invites.insert(
            id.to_string(),
            Invite {
                secret_hash: secret_hash.to_string(),
                user: user.to_string(),
                display_name: entry
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string),
                grants: strings(entry.get("grants")),
                expires_at: entry
                    .get("expires_at")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                issued_by: entry
                    .get("issued_by")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                issued_at: entry
                    .get("issued_at")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            },
        );
    }
    Ok(state)
}

/// The enrolled credentials of one account, as the roster shows them.
///
/// The public key is included: it is a public key, and an operator
/// auditing what can sign for an account needs to see the thing that
/// signs, not a count of them.
fn render_passkeys(passkeys: &[Passkey]) -> Vec<serde_json::Value> {
    passkeys
        .iter()
        .map(|k| {
            serde_json::json!({
                "credential_id": k.credential_id,
                "public_key": k.public_key,
                "label": k.label,
                "created_at": k.created_at,
            })
        })
        .collect()
}

/// Reads the `passkeys` array of one stored account record.
///
/// A record written before D39 has no such member, which parses to an
/// empty list rather than an error: the field is additive, so an older
/// store still loads (invariant 1). A malformed entry is dropped rather
/// than failing the whole load, because the alternative — refusing to
/// start — would turn one bad credential into a node-wide outage, and a
/// dropped passkey is visible in the roster.
fn parse_passkeys(value: Option<&serde_json::Value>) -> Vec<Passkey> {
    value
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let text = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
            Some(Passkey {
                credential_id: text("credential_id")?.to_string(),
                public_key: text("public_key")?.to_string(),
                label: text("label").unwrap_or("passkey").to_string(),
                created_at: entry
                    .get("created_at")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Base64url, the encoding WebAuthn uses everywhere, decoded by
/// translating into the standard alphabet and reusing the node's one
/// decoder rather than writing a second.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    if input.contains(['+', '/']) {
        // Standard-alphabet input is refused rather than accepted as a
        // courtesy: two spellings of one credential id would let the
        // same key enrol twice and be removed once.
        return None;
    }
    crate::base64_decode(&input.replace('-', "+").replace('_', "/"))
}

/// Refuses a credential id that is not a plausible base64url handle.
fn validate_passkey_id(id: &str) -> Result<(), String> {
    if id.len() > MAX_CREDENTIAL_ID_CHARS {
        return Err(format!(
            "credential id is longer than {MAX_CREDENTIAL_ID_CHARS} characters"
        ));
    }
    if base64url_decode(id).is_none() {
        return Err("credential id is not base64url".to_string());
    }
    Ok(())
}

/// Refuses a label that would make a roster unreadable or smuggle a
/// control character into one.
fn validate_label(label: &str) -> Result<(), String> {
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!("label is longer than {MAX_LABEL_CHARS} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("label contains a control character".to_string());
    }
    Ok(())
}

/// Refuses anything that is not exactly a P-256 SubjectPublicKeyInfo.
///
/// Checked at enrolment rather than at first use, because the two fail in
/// different places: a bad key caught here is a 400 on the request that
/// sent it, while the same key caught later is a signature that will not
/// verify for a reason the holder cannot see. The shape is the fixed
/// 26-byte SPKI prefix followed by a 65-byte uncompressed point, which is
/// what [`choir_identity::p256_point_to_spki`] builds and what `openssl`
/// emits.
fn validate_p256_spki(encoded: &str) -> Result<(), String> {
    let Some(der) = base64url_decode(encoded) else {
        return Err("public key is not base64url".to_string());
    };
    let prefix = choir_identity::P256_SPKI_PREFIX;
    if der.len() != prefix.len() + 65 {
        return Err(format!(
            "public key is {} bytes; a P-256 SubjectPublicKeyInfo is {}",
            der.len(),
            prefix.len() + 65
        ));
    }
    if der[..prefix.len()] != prefix[..] {
        return Err("public key is not a P-256 SubjectPublicKeyInfo".to_string());
    }
    if der[prefix.len()] != 0x04 {
        return Err("public key is not an uncompressed point".to_string());
    }
    Ok(())
}

/// One error body, in the shape every other API error uses.
fn error_json(reason: &str) -> String {
    serde_json::json!({ "error": reason }).to_string()
}

fn bad_request(reason: &str) -> (u16, String) {
    (400, error_json(reason))
}

fn conflict(reason: &str) -> (u16, String) {
    (409, error_json(reason))
}

fn server_error(reason: &str) -> (u16, String) {
    (500, error_json(reason))
}
