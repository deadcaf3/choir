//! One runner seam: the part of an orchestrator adapter that is not about
//! the orchestrator.
//!
//! Two adapters exist, for Claude Code's worktree hooks and for a
//! Symphony workspace backend. Written independently, in shell, they
//! converged on the same five steps: derive a stable identity from
//! whatever the orchestrator calls its unit of work, resolve an exact
//! base revision, drive the lifecycle, *verify the binding that came
//! back is the one that was asked for*, and map Choir's typed rejections
//! onto a retry decision.
//!
//! Only the first and last of those are orchestrator-shaped, and only
//! barely. The rest is the lifecycle contract, and it was duplicated: 557
//! lines of shell holding two hand-rolled copies of the identity
//! derivation and the binding check, which are exactly the parts where a
//! mistake is silent. A workspace name that collides sends two agents
//! into one directory. A binding check that passes vacuously accepts a
//! workspace bound to somebody else's change.
//!
//! So the shared half lives here, in one tested place, and an adapter is
//! left with what genuinely differs: its wire format, and its namespace.
//!
//! # What is deliberately not here
//!
//! No orchestrator's field names, and no agent protocol's concepts. The
//! design note is explicit that one agent protocol must not become
//! Choir's change model, so this module speaks only in the four
//! identifiers the identifier contract already names: workspace, change,
//! revision and operation.

use choir_hash::ContentHash;

/// Wire-format version of the runner request and result.
pub const PROTOCOL_VERSION: u64 = 1;

/// Longest accepted external identifier, in bytes.
///
/// A tracker id or a branch name is short. Something arriving here at
/// kilobyte scale is a caller error or an attempt to push the derived
/// name somewhere it does not fit, and both are better refused at the
/// boundary than truncated into a collision.
const MAX_IDENTIFIER: usize = 512;

/// Longest accepted workspace key, which becomes part of a directory
/// name and so is bounded well under any filesystem's component limit.
const MAX_WORKSPACE_KEY: usize = 160;

/// How much of the workspace key survives into the directory name.
const KEY_PREFIX: usize = 80;

/// How many hex characters of the fingerprint disambiguate the name.
///
/// 16 hex characters is 64 bits. The fingerprint already guarantees
/// uniqueness through `change_id`; this suffix only has to stop two
/// human-chosen keys from colliding in the filesystem, and a birthday
/// collision at 64 bits needs about four billion live workspaces on one
/// repository.
const FINGERPRINT_IN_NAME: usize = 16;

/// A refused request: a stable code, whether retrying can help, and a
/// message safe to hand back to the orchestrator.
///
/// `retryable` is the field an orchestrator actually branches on, so it
/// is computed rather than guessed. Getting it wrong in the safe-looking
/// direction is what hurts: a terminal failure marked retryable becomes
/// a scheduler spinning on a request that can never succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Stable machine-readable code.
    pub code: String,
    /// Whether an identical retry could succeed later.
    pub retryable: bool,
    /// Human-readable detail.
    pub message: String,
}

impl Failure {
    /// A terminal failure: retrying the identical request cannot help.
    #[must_use]
    pub fn terminal(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            retryable: false,
            message: message.into(),
        }
    }

    /// A transient failure: the same request may succeed later.
    #[must_use]
    pub fn transient(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            retryable: true,
            message: message.into(),
        }
    }

    /// The wire form an adapter forwards to its orchestrator.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "error": {
                "code": self.code,
                "retryable": self.retryable,
                "message": self.message,
            }
        })
    }
}

/// Whether a Choir rejection code can succeed on an identical retry.
///
/// The listed codes are decisions about the request itself: a binding
/// that conflicts, a signature that does not verify, a channel the key
/// does not own. None of them change because time passed, so retrying is
/// a scheduler burning attempts on a refusal that is already final.
///
/// Everything else, including an unrecognised code, is treated as
/// transient. That asymmetry is deliberate. Calling a transient failure
/// terminal strands work that would have succeeded, while calling a
/// terminal one transient costs retries that fail fast and loudly. A
/// code this build has never heard of is more likely a newer node than a
/// new class of permanent refusal.
#[must_use]
pub fn is_retryable(code: &str) -> bool {
    !matches!(
        code,
        "workspace_state"
            | "change_state"
            | "stale_head"
            | "malformed_request"
            | "malformed_op"
            | "unknown_key"
            | "channel_not_owned"
            | "identity_state"
            | "bad_signature"
            | "node_only"
            | "duplicate_submission"
            | "foreign_scope"
    )
}

/// Whether one path component is safe to place in a workspace path.
///
/// Leading dot excluded, so a derived name can never produce a hidden
/// entry or either dot directory.
#[must_use]
pub fn safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Splits `owner/repo`, refusing any other shape.
///
/// # Errors
///
/// Returns a [`Failure`] when the name is not exactly two safe segments.
pub fn split_repo(repo: &str) -> Result<(&str, &str), Failure> {
    let mut segments = repo.split('/');
    let (Some(owner), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(Failure::terminal(
            "invalid_config",
            "repo must be owner/repo",
        ));
    };
    if !safe_segment(owner) || !safe_segment(name) {
        return Err(Failure::terminal(
            "invalid_config",
            "repo contains an unsafe path segment",
        ));
    }
    Ok((owner, name))
}

/// Marker for the derivation rules themselves, mixed into every
/// fingerprint.
///
/// A change to how identity is derived must produce different ids by
/// construction rather than by luck, so that an adapter holding durable
/// state from an older build sees a mismatch it can refuse. Silently
/// deriving a *different* id for the same work is how one unit of work
/// forks into two changes and two workspaces.
const SCHEME_TAG: &str = "choir-runner-1";

/// How a change's identity is derived, and what that choice costs.
///
/// Not a preference. Each orchestrator's own contract forces one of
/// these, and picking the other silently breaks a lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// The workspace name *is* the identity: the change id is derived
    /// from it, so any caller holding the path can recompute the binding
    /// with no stored state.
    ///
    /// Required when the orchestrator hands back only a directory on
    /// teardown, which is exactly Claude Code's `WorktreeRemove`. The
    /// cost is that the binding does not survive a rename, and that two
    /// attempts at one unit of work are two unrelated changes unless the
    /// caller's naming already says otherwise.
    FromName,
    /// The identity is fingerprinted from stable external identity, and
    /// the workspace name is a derived label.
    ///
    /// Required when competing workers must converge on one change for
    /// one unit of work, which is what a scheduler retry needs. The cost
    /// is that the full fingerprint does not fit in the directory name,
    /// so recovering the binding from a path alone is impossible and the
    /// adapter must keep durable state.
    FromExternal,
}

impl Scheme {
    /// The stable spelling recorded in adapter state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FromName => "from-name",
            Self::FromExternal => "from-external",
        }
    }
}

/// The four identifiers one writing attempt is bound to.
///
/// Built together and never separately, because their whole value is
/// that they agree: the workspace a change is bound to, the change a
/// checkpoint advances, and the idempotency key that makes a lost create
/// response converge rather than fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Directory name.
    pub workspace_name: String,
    /// `owner/repo/name`, the workspace's view identity.
    pub workspace_id: String,
    /// Stable logical change id, unique per external unit of work.
    pub change_id: String,
    /// Owner-scoped create retry identity.
    pub idempotency_key: String,
    /// Full hex fingerprint, under [`Scheme::FromExternal`] only. Empty
    /// under [`Scheme::FromName`], where the name carries the identity.
    pub fingerprint: String,
    /// Which rule produced this binding. Recorded so an adapter reading
    /// durable state written by another scheme refuses it rather than
    /// deriving a second change for work that already has one.
    pub scheme: Scheme,
}

/// Checks a namespace is usable in both a path and an identifier.
fn checked_namespace(namespace: &str) -> Result<(), Failure> {
    if safe_segment(namespace) {
        Ok(())
    } else {
        Err(Failure::terminal(
            "invalid_request",
            "namespace must be a safe path segment",
        ))
    }
}

impl Identity {
    /// Derives the binding from the workspace name, under
    /// [`Scheme::FromName`].
    ///
    /// The inverse is what makes this scheme worth having: given only
    /// `repo` and a directory name, the change and idempotency key come
    /// back exactly, so teardown needs no stored state. Claude Code's
    /// `WorktreeRemove` is handed a path and nothing else, and this is
    /// the property that lets it archive the right change.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the repo or the name is not a safe
    /// path component.
    pub fn from_name(namespace: &str, repo: &str, workspace_name: &str) -> Result<Self, Failure> {
        split_repo(repo)?;
        checked_namespace(namespace)?;
        if !safe_segment(workspace_name) || workspace_name.len() > MAX_WORKSPACE_KEY {
            return Err(Failure::terminal(
                "invalid_request",
                "workspace name is unsafe or too long",
            ));
        }
        Ok(Self {
            workspace_id: format!("{repo}/{workspace_name}"),
            change_id: format!("{namespace}:{repo}:{workspace_name}"),
            idempotency_key: format!("{namespace}-create:{repo}:{workspace_name}"),
            workspace_name: workspace_name.to_string(),
            fingerprint: String::new(),
            scheme: Scheme::FromName,
        })
    }

    /// Derives the binding by fingerprinting external identity, under
    /// [`Scheme::FromExternal`].
    ///
    /// `namespace` separates orchestrators, so two of them driving the
    /// same repository cannot collide on a name or, worse, converge onto
    /// one another's change. `external_id` and `generation` are whatever
    /// the orchestrator means by "this unit of work" and "this attempt
    /// at it": a tracker issue and a retry counter.
    ///
    /// Two workers racing the same `(external_id, generation)` derive
    /// the same change and the same idempotency key, which is what makes
    /// one of them reuse the other's workspace instead of forking the
    /// work in two. That convergence is the whole reason to pay for
    /// durable state.
    ///
    /// The fingerprint covers the scheme tag, `repo`, `external_id` and
    /// `generation`, each length-prefixed, so no choice of separators
    /// inside a field can make two different tuples hash alike.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when any input is empty, over-long, or
    /// unsafe as a path component.
    pub fn from_external(
        namespace: &str,
        repo: &str,
        workspace_key: &str,
        external_id: &str,
        generation: &str,
    ) -> Result<Self, Failure> {
        split_repo(repo)?;
        checked_namespace(namespace)?;
        if !safe_segment(workspace_key) || workspace_key.len() > MAX_WORKSPACE_KEY {
            return Err(Failure::terminal(
                "invalid_request",
                "workspace_key is unsafe or too long",
            ));
        }
        for (field, value) in [("external_id", external_id), ("generation", generation)] {
            if value.is_empty() || value.len() > MAX_IDENTIFIER {
                return Err(Failure::terminal(
                    "invalid_request",
                    format!("{field} must be non-empty and at most {MAX_IDENTIFIER} bytes"),
                ));
            }
        }

        // Length-prefixed so a separator inside any field cannot forge a
        // different tuple with the same bytes.
        let mut material = Vec::new();
        for field in [SCHEME_TAG, repo, external_id, generation] {
            material.extend_from_slice(field.len().to_string().as_bytes());
            material.push(b':');
            material.extend_from_slice(field.as_bytes());
            material.push(b'\n');
        }
        let hashed = ContentHash::blake3(&material).to_hex();
        // `to_hex` is `<codec>-<digest>`; the ids want the digest alone.
        let digest = hashed
            .split_once('-')
            .map_or(hashed.as_str(), |(_, rest)| rest);

        let prefix: String = workspace_key.chars().take(KEY_PREFIX).collect();
        let short: String = digest.chars().take(FINGERPRINT_IN_NAME).collect();
        let workspace_name = format!("{namespace}-{prefix}-{short}");

        Ok(Self {
            workspace_id: format!("{repo}/{workspace_name}"),
            workspace_name,
            change_id: format!("{namespace}:{repo}:{digest}"),
            idempotency_key: format!("{namespace}-create:{repo}:{digest}"),
            fingerprint: digest.to_string(),
            scheme: Scheme::FromExternal,
        })
    }
}

/// Extracts the exact Git object id `base_ref` currently points at.
///
/// The view reports a revision as `<codec>-<digest>`; codecs `11` and
/// `12` are git SHA-1 and SHA-256. A revision under any other codec is
/// refused rather than coerced, because a workspace must start from an
/// object git can actually check out.
///
/// # Errors
///
/// Returns a [`Failure`] when the ref is absent, carries a non-Git
/// revision, or names an object id that is not hex of a git width.
pub fn base_from_view(view: &serde_json::Value, base_ref: &str) -> Result<String, Failure> {
    let Some(revision) = view
        .get("refs")
        .and_then(|refs| refs.get(base_ref))
        .and_then(serde_json::Value::as_str)
    else {
        return Err(Failure::terminal(
            "base_ref_missing",
            format!("{base_ref} is absent from the Choir view"),
        ));
    };
    let Some(oid) = revision
        .strip_prefix("11-")
        .or_else(|| revision.strip_prefix("12-"))
    else {
        return Err(Failure::terminal(
            "base_ref_invalid",
            format!("{base_ref} names a non-Git revision"),
        ));
    };
    let git_width = oid.len() == 40 || oid.len() == 64;
    if !git_width || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Failure::terminal(
            "base_ref_invalid",
            format!("{base_ref} names an invalid Git object id"),
        ));
    }
    Ok(oid.to_string())
}

/// The base revision the node says the change is actually bound to.
///
/// Creation is idempotent, so a retry that resolved a `base_ref` which
/// has since moved reuses the existing change at its original base.
/// Reporting the requested revision there would describe a workspace
/// that does not exist: the orchestrator would record one base while the
/// change is bound to another, and every later comparison against it
/// would be wrong. The node's answer is authoritative; `requested` is
/// only a fallback for a response that carries none.
#[must_use]
pub fn bound_base(response: &serde_json::Value, requested: &str) -> String {
    let reported = response
        .get("base_revision")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| {
            value
                .strip_prefix("11-")
                .or_else(|| value.strip_prefix("12-"))
        })
        .filter(|oid| oid.len() == 40 || oid.len() == 64)
        .filter(|oid| oid.chars().all(|c| c.is_ascii_hexdigit()));
    reported.map_or_else(|| requested.to_string(), str::to_string)
}

/// Checks the node returned the binding that was asked for.
///
/// This is the step whose absence is silent. Creation is idempotent by
/// design, so a request that reuses an existing workspace answers 200
/// with a receipt; without comparing identities, a receipt for somebody
/// else's change reads exactly like success, and the adapter hands an
/// agent a directory bound to a change it may not checkpoint.
///
/// # Errors
///
/// Returns a [`Failure`] when either identity is missing from the
/// response or differs from the expected one.
pub fn verify_binding(response: &serde_json::Value, expected: &Identity) -> Result<(), Failure> {
    let field = |key: &str| -> Result<String, Failure> {
        response
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                Failure::transient(
                    "invalid_response",
                    format!("Choir returned no {key} to check the binding against"),
                )
            })
    };
    let (workspace, change) = (field("workspace")?, field("change_id")?);
    if workspace != expected.workspace_id || change != expected.change_id {
        return Err(Failure::terminal(
            "binding_mismatch",
            format!(
                "Choir returned workspace {workspace} change {change}, \
                 expected workspace {} change {}",
                expected.workspace_id, expected.change_id
            ),
        ));
    }
    Ok(())
}

/// Turns a Choir error response into a [`Failure`], preserving its code.
///
/// A body that does not parse as a typed rejection still has to produce
/// something an orchestrator can branch on, so it becomes a transient
/// `choir_unavailable` rather than being dropped.
#[must_use]
pub fn failure_from_response(body: &str, context: &str) -> Failure {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(serde_json::Value::as_str);
    let detail = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("detail")
                .or_else(|| value.get("error"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string);
    match code {
        Some(code) => Failure {
            code: code.to_string(),
            retryable: is_retryable(code),
            message: detail.unwrap_or_else(|| format!("Choir did not complete {context}")),
        },
        None => Failure::transient(
            "choir_unavailable",
            detail.unwrap_or_else(|| format!("Choir did not complete {context}")),
        ),
    }
}

/// One lifecycle step an orchestrator asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Bind a workspace and change at an exact base, idempotently.
    Ensure,
    /// Publish an immutable revision of the bound change.
    Checkpoint,
    /// Owner-authorized recoverable archive of the bound workspace.
    Archive,
}

impl Operation {
    fn parse(value: &str) -> Result<Self, Failure> {
        match value {
            "ensure" => Ok(Self::Ensure),
            "checkpoint" => Ok(Self::Checkpoint),
            "archive" => Ok(Self::Archive),
            other => Err(Failure::terminal(
                "unsupported_operation",
                format!("operation must be ensure, checkpoint or archive, not {other}"),
            )),
        }
    }
}

/// Install-time settings: the node, the repository, and the owner
/// identity that signs. Supplied by the operator, not by the
/// orchestrator, so a request cannot redirect a workspace at another
/// repository or sign with another key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Node API base URL.
    pub api: String,
    /// Repository as `owner/repo`.
    pub repo: String,
    /// Owner channel that signs lifecycle authorizations.
    pub owner: String,
    /// Absolute path to the owner's 32-byte key file.
    pub key_file: String,
    /// Identifier namespace separating this orchestrator from others.
    pub namespace: String,
    /// View ref an `ensure` resolves its base revision from.
    pub base_ref: Option<String>,
    /// Basic-auth credentials file, absolute when present.
    pub auth_file: Option<String>,
    /// Basic-auth username; needs `auth_file`.
    pub auth_user: Option<String>,
}

fn string_field(object: &serde_json::Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

impl Config {
    /// Parses and validates operator configuration.
    ///
    /// Every path is required to be absolute, because the adapter is
    /// invoked by a scheduler whose working directory is its own
    /// business, and a relative key path would resolve somewhere nobody
    /// chose.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] naming the first field that is missing or
    /// unusable.
    pub fn parse(value: &serde_json::Value) -> Result<Self, Failure> {
        let missing = |field: &str| {
            Failure::terminal("invalid_config", format!("config needs string {field}"))
        };
        let api = string_field(value, "api").ok_or_else(|| missing("api"))?;
        let repo = string_field(value, "repo").ok_or_else(|| missing("repo"))?;
        let owner = string_field(value, "owner").ok_or_else(|| missing("owner"))?;
        let key_file = string_field(value, "key_file").ok_or_else(|| missing("key_file"))?;
        let namespace = string_field(value, "namespace").ok_or_else(|| missing("namespace"))?;

        if !(api.starts_with("http://") || api.starts_with("https://")) {
            return Err(Failure::terminal(
                "invalid_config",
                "config api must use http:// or https://",
            ));
        }
        split_repo(&repo)?;
        checked_namespace(&namespace)?;
        if !key_file.starts_with('/') {
            return Err(Failure::terminal(
                "invalid_config",
                "config key_file must be an absolute path",
            ));
        }
        let auth_file = string_field(value, "auth_file");
        let auth_user = string_field(value, "auth_user");
        if auth_file
            .as_ref()
            .is_some_and(|path| !path.starts_with('/'))
        {
            return Err(Failure::terminal(
                "invalid_config",
                "config auth_file must be an absolute path",
            ));
        }
        if auth_user.is_some() && auth_file.is_none() {
            return Err(Failure::terminal(
                "invalid_config",
                "config auth_user needs auth_file",
            ));
        }
        let base_ref = string_field(value, "base_ref");
        // A base ref naming another repository would provision this
        // workspace from a history it has nothing to do with.
        if let Some(base_ref) = &base_ref {
            if !base_ref.starts_with(&format!("{repo}.git:refs/")) {
                return Err(Failure::terminal(
                    "invalid_config",
                    "config base_ref must name this repository as owner/repo.git:refs/...",
                ));
            }
        }
        Ok(Self {
            api,
            repo,
            owner,
            key_file,
            namespace,
            base_ref,
            auth_file,
            auth_user,
        })
    }
}

/// One request from an orchestrator adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Which lifecycle step.
    pub operation: Operation,
    /// The binding this request is about.
    pub identity: Identity,
    /// Workspace directory, required by checkpoint.
    pub workspace_path: Option<String>,
    /// Exact base revision, when the caller pins one itself instead of
    /// resolving `base_ref`.
    pub base: Option<String>,
}

impl Request {
    /// Parses a request against operator `config`.
    ///
    /// The scheme is named by the caller rather than inferred from which
    /// fields are present. Inferring it would make a typo in a field
    /// name silently select the other scheme, and the two derive
    /// different change ids for the same work.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the protocol version, operation, or
    /// the fields the named scheme requires are missing or unusable.
    pub fn parse(value: &serde_json::Value, config: &Config) -> Result<Self, Failure> {
        if value
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            != Some(PROTOCOL_VERSION)
        {
            return Err(Failure::terminal(
                "invalid_request",
                format!("request must be a protocol_version {PROTOCOL_VERSION} object"),
            ));
        }
        let operation = Operation::parse(&string_field(value, "operation").ok_or_else(|| {
            Failure::terminal("invalid_request", "request needs string operation")
        })?)?;

        let needs = |field: &str| {
            Failure::terminal(
                "invalid_request",
                format!("this scheme needs string {field}"),
            )
        };
        let identity = match string_field(value, "scheme").as_deref() {
            Some("from-name") => Identity::from_name(
                &config.namespace,
                &config.repo,
                &string_field(value, "workspace_name").ok_or_else(|| needs("workspace_name"))?,
            )?,
            Some("from-external") => Identity::from_external(
                &config.namespace,
                &config.repo,
                &string_field(value, "workspace_key").ok_or_else(|| needs("workspace_key"))?,
                &string_field(value, "external_id").ok_or_else(|| needs("external_id"))?,
                &string_field(value, "generation").ok_or_else(|| needs("generation"))?,
            )?,
            _ => {
                return Err(Failure::terminal(
                    "invalid_request",
                    "request needs scheme to be from-name or from-external",
                ))
            }
        };

        Ok(Self {
            operation,
            identity,
            workspace_path: string_field(value, "workspace_path"),
            base: string_field(value, "base"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_json() -> serde_json::Value {
        serde_json::json!({
            "api": "http://127.0.0.1:9000",
            "repo": "owner/repo",
            "owner": "operator/agent",
            "key_file": "/keys/owner.key",
            "namespace": "sy",
            "base_ref": "owner/repo.git:refs/heads/main",
        })
    }

    fn config() -> Config {
        Config::parse(&config_json()).expect("config parses")
    }

    fn identity(external: &str, generation: &str) -> Identity {
        Identity::from_external("sy", "owner/repo", "issue-7", external, generation)
            .expect("derives")
    }

    /// The property the whole scheme exists for: teardown is handed a
    /// path and nothing else, so the binding must come back from the
    /// name alone. If this ever stops holding, `WorktreeRemove` archives
    /// the wrong change or none at all.
    #[test]
    fn a_from_name_binding_is_recoverable_from_the_path_alone() {
        let created =
            Identity::from_name("claude-code", "owner/repo", "cc-feature-abc123").expect("derives");
        // Teardown's only inputs: the repo it is configured for, and the
        // final component of the worktree path it was given.
        let recovered =
            Identity::from_name("claude-code", "owner/repo", "cc-feature-abc123").expect("derives");
        assert_eq!(created, recovered);
        assert_eq!(created.scheme, Scheme::FromName);
        assert_eq!(
            created.change_id,
            "claude-code:owner/repo:cc-feature-abc123"
        );
        assert!(
            created.fingerprint.is_empty(),
            "a name-derived binding must not imply a fingerprint it cannot recover"
        );
    }

    /// The converse, and the reason `FromExternal` needs durable state:
    /// the name carries a truncated fingerprint, so the change id is not
    /// recoverable from it. Stated as a test so nobody migrates an
    /// adapter onto the wrong scheme expecting recovery to work.
    #[test]
    fn a_from_external_binding_is_not_recoverable_from_the_name() {
        let derived = identity("ISSUE-7", "1");
        assert!(
            !derived.change_id.contains(&derived.workspace_name),
            "the name would have carried the whole change id"
        );
        let truncated = derived.workspace_name.rsplit('-').next().expect("a suffix");
        assert!(
            truncated.len() < derived.fingerprint.len(),
            "the name carries the full fingerprint, so durable state is not needed \
             and this scheme has no cost to justify it"
        );
    }

    /// Two schemes must not quietly produce the same ids for the same
    /// inputs, or a migration between them would look like a no-op while
    /// changing which change a workspace is bound to.
    #[test]
    fn the_two_schemes_do_not_collide() {
        let named = Identity::from_name("sy", "owner/repo", "sy-issue-7").expect("derives");
        let fingerprinted = identity("sy-issue-7", "1");
        assert_ne!(named.change_id, fingerprinted.change_id);
        assert_ne!(named.scheme, fingerprinted.scheme);
    }

    /// The scheme tag is inside the fingerprint, so changing the
    /// derivation rules cannot leave ids untouched. Pinned because the
    /// failure it prevents is silent: an adapter with durable state from
    /// an older build would otherwise derive a second change for work
    /// that already has one.
    #[test]
    fn the_scheme_tag_participates_in_the_fingerprint() {
        let material_without_tag = {
            let mut material = Vec::new();
            for field in ["owner/repo", "ISSUE-7", "1"] {
                material.extend_from_slice(field.len().to_string().as_bytes());
                material.push(b':');
                material.extend_from_slice(field.as_bytes());
                material.push(b'\n');
            }
            ContentHash::blake3(&material).to_hex()
        };
        assert!(
            !material_without_tag.contains(&identity("ISSUE-7", "1").fingerprint),
            "the scheme tag is not covered, so a derivation change could keep the same ids"
        );
    }

    #[test]
    fn a_binding_is_stable_for_one_unit_of_work_and_attempt() {
        let first = identity("ISSUE-7", "1");
        let second = identity("ISSUE-7", "1");
        assert_eq!(first, second, "derivation is not deterministic");
        assert_eq!(
            first.workspace_id,
            format!("owner/repo/{}", first.workspace_name)
        );
        assert!(first.workspace_name.starts_with("sy-issue-7-"));
        assert!(first.change_id.starts_with("sy:owner/repo:"));
    }

    /// A new attempt at the same work is a different change, and a
    /// different unit of work is a different change. Both directions
    /// matter: the first is what makes a retry a fresh workspace rather
    /// than a collision, the second is the whole point of the id.
    #[test]
    fn a_different_unit_or_attempt_derives_a_different_binding() {
        let base = identity("ISSUE-7", "1");
        for (external, generation, why) in [
            ("ISSUE-8", "1", "a different unit of work"),
            ("ISSUE-7", "2", "a different attempt"),
        ] {
            let other = identity(external, generation);
            assert_ne!(base.change_id, other.change_id, "{why} shared a change id");
            assert_ne!(
                base.workspace_name, other.workspace_name,
                "{why} shared a workspace name"
            );
        }
    }

    /// The namespace is what stops two orchestrators driving one
    /// repository from converging onto each other's change.
    #[test]
    fn the_namespace_separates_orchestrators() {
        let symphony =
            Identity::from_external("sy", "owner/repo", "k", "ISSUE-7", "1").expect("derives");
        let claude =
            Identity::from_external("cc", "owner/repo", "k", "ISSUE-7", "1").expect("derives");
        assert_ne!(symphony.change_id, claude.change_id);
        assert_ne!(symphony.workspace_name, claude.workspace_name);
    }

    /// Length-prefixing the fingerprint material. Without it, moving a
    /// separator between two adjacent fields produces identical bytes
    /// and therefore one change id for two different units of work.
    #[test]
    fn a_separator_cannot_be_moved_between_fingerprint_fields() {
        let left = Identity::from_external("sy", "owner/repo", "k", "a:b", "c").expect("derives");
        let right = Identity::from_external("sy", "owner/repo", "k", "a", "b:c").expect("derives");
        assert_ne!(
            left.fingerprint, right.fingerprint,
            "a moved separator collided two distinct bindings"
        );
    }

    #[test]
    fn unsafe_or_oversized_identity_input_is_refused() {
        let long = "x".repeat(MAX_IDENTIFIER + 1);
        let cases = [
            ("sy", "owner/repo", "../escape", "i", "1", "a traversal key"),
            ("sy", "owner/repo", ".hidden", "i", "1", "a hidden key"),
            ("sy", "owner/repo", "", "i", "1", "an empty key"),
            ("sy", "owner", "k", "i", "1", "a one-segment repo"),
            ("sy", "a/b/c", "k", "i", "1", "a three-segment repo"),
            ("../sy", "owner/repo", "k", "i", "1", "an unsafe namespace"),
            ("sy", "owner/repo", "k", "", "1", "an empty external id"),
            (
                "sy",
                "owner/repo",
                "k",
                &long,
                "1",
                "an over-long external id",
            ),
            (
                "sy",
                "owner/repo",
                "k",
                "i",
                &long,
                "an over-long generation",
            ),
        ];
        for (namespace, repo, key, external, generation, why) in cases {
            assert!(
                Identity::from_external(namespace, repo, key, external, generation).is_err(),
                "accepted {why}"
            );
        }
    }

    #[test]
    fn an_over_long_workspace_key_is_refused_but_a_long_one_is_truncated_in_the_name() {
        let too_long = "k".repeat(MAX_WORKSPACE_KEY + 1);
        assert!(Identity::from_external("sy", "owner/repo", &too_long, "i", "1").is_err());

        let long = "k".repeat(MAX_WORKSPACE_KEY);
        let derived =
            Identity::from_external("sy", "owner/repo", &long, "i", "1").expect("derives");
        // Truncated for the directory name, but the change id still
        // separates two keys sharing that truncated prefix.
        assert!(derived.workspace_name.len() < long.len() + KEY_PREFIX);
        let sibling =
            Identity::from_external("sy", "owner/repo", &long, "i", "2").expect("derives");
        assert_ne!(derived.workspace_name, sibling.workspace_name);
    }

    #[test]
    fn a_base_ref_resolves_only_to_a_real_git_object() {
        let view = serde_json::json!({
            "refs": {
                "owner/repo.git:refs/heads/main": format!("11-{}", "a".repeat(40)),
                "sha256": format!("12-{}", "b".repeat(64)),
                "not-git": format!("1e-{}", "c".repeat(64)),
                "short": "11-abc",
                "not-hex": format!("11-{}", "z".repeat(40)),
            }
        });
        assert_eq!(
            base_from_view(&view, "owner/repo.git:refs/heads/main").expect("resolves"),
            "a".repeat(40)
        );
        assert_eq!(
            base_from_view(&view, "sha256").expect("resolves"),
            "b".repeat(64)
        );
        for (name, why) in [
            ("not-git", "a non-Git codec"),
            ("short", "a truncated oid"),
            ("not-hex", "a non-hex oid"),
            ("absent", "a missing ref"),
        ] {
            let failure = base_from_view(&view, name).expect_err(why);
            assert!(!failure.retryable, "{why} was reported as retryable");
        }
    }

    #[test]
    fn a_binding_check_refuses_a_receipt_for_another_change() {
        let expected = identity("ISSUE-7", "1");
        let good = serde_json::json!({
            "workspace": expected.workspace_id,
            "change_id": expected.change_id,
        });
        assert!(verify_binding(&good, &expected).is_ok());

        let other = identity("ISSUE-8", "1");
        for (response, why) in [
            (
                serde_json::json!({ "workspace": other.workspace_id, "change_id": expected.change_id }),
                "another workspace",
            ),
            (
                serde_json::json!({ "workspace": expected.workspace_id, "change_id": other.change_id }),
                "another change",
            ),
            (
                serde_json::json!({ "change_id": expected.change_id }),
                "no workspace",
            ),
            (
                serde_json::json!({ "workspace": expected.workspace_id }),
                "no change",
            ),
            (
                serde_json::json!({ "workspace": "", "change_id": expected.change_id }),
                "an empty workspace",
            ),
        ] {
            assert!(
                verify_binding(&response, &expected).is_err(),
                "accepted a receipt naming {why}"
            );
        }
    }

    /// The retry decision is the field a scheduler branches on, so the
    /// terminal codes are pinned rather than left to a default.
    #[test]
    fn refusals_that_cannot_change_are_terminal_and_the_rest_are_not() {
        for code in [
            "workspace_state",
            "change_state",
            "stale_head",
            "unknown_key",
            "channel_not_owned",
            "identity_state",
            "bad_signature",
        ] {
            assert!(!is_retryable(code), "{code} would be retried forever");
        }
        for code in ["policy_unavailable", "log_evicted", "unclassified"] {
            assert!(
                is_retryable(code),
                "{code} stranded work that could succeed"
            );
        }
        // An unrecognised code is transient: a newer node is likelier
        // than a new permanent refusal, and failing fast is recoverable
        // while stranding is not.
        assert!(is_retryable("a_code_from_a_newer_node"));
    }

    #[test]
    fn a_choir_rejection_keeps_its_code_and_a_broken_body_still_decides() {
        let typed = failure_from_response(
            r#"{"code":"workspace_state","detail":"binding differs"}"#,
            "creation",
        );
        assert_eq!(typed.code, "workspace_state");
        assert!(!typed.retryable);
        assert_eq!(typed.message, "binding differs");

        let garbage = failure_from_response("<html>502</html>", "creation");
        assert_eq!(garbage.code, "choir_unavailable");
        assert!(garbage.retryable, "an unreadable body stranded the work");
        assert!(garbage.message.contains("creation"));
    }

    /// Operator settings are not negotiable by the caller. Each of
    /// these would let a request reach a repository, a key, or a history
    /// the operator did not choose.
    #[test]
    fn configuration_refuses_what_would_redirect_the_lifecycle() {
        for (field, value, why) in [
            ("api", serde_json::json!("ftp://host"), "a non-HTTP scheme"),
            ("api", serde_json::json!(""), "an empty api"),
            ("repo", serde_json::json!("owner"), "a one-segment repo"),
            ("repo", serde_json::json!("a/b/c"), "a three-segment repo"),
            (
                "repo",
                serde_json::json!("../etc/passwd"),
                "a traversal repo",
            ),
            (
                "key_file",
                serde_json::json!("relative.key"),
                "a relative key path",
            ),
            (
                "namespace",
                serde_json::json!("../sy"),
                "an unsafe namespace",
            ),
            (
                "auth_file",
                serde_json::json!("relative"),
                "a relative auth file",
            ),
            (
                "base_ref",
                serde_json::json!("other/repo.git:refs/heads/main"),
                "a base ref naming another repository",
            ),
        ] {
            let mut raw = config_json();
            raw[field] = value;
            assert!(Config::parse(&raw).is_err(), "config accepted {why}");
        }

        let mut raw = config_json();
        raw["auth_user"] = serde_json::json!("someone");
        assert!(
            Config::parse(&raw).is_err(),
            "config accepted a username with no credentials file"
        );
    }

    /// The scheme is named, never inferred from which fields happen to
    /// be present. Inference would turn a typo into a silent switch
    /// between two rules that derive different change ids for one unit
    /// of work.
    #[test]
    fn the_scheme_is_named_rather_than_guessed_from_the_fields() {
        let config = config();
        let complete = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "operation": "ensure",
            "workspace_key": "issue-7",
            "external_id": "ISSUE-7",
            "generation": "1",
            "workspace_name": "sy-issue-7",
        });
        // Every field for both schemes is present, and it still refuses.
        assert!(Request::parse(&complete, &config).is_err());

        let mut named = complete.clone();
        named["scheme"] = serde_json::json!("from-name");
        let mut external = complete;
        external["scheme"] = serde_json::json!("from-external");
        let named = Request::parse(&named, &config).expect("from-name parses");
        let external = Request::parse(&external, &config).expect("from-external parses");
        assert_eq!(named.identity.scheme, Scheme::FromName);
        assert_eq!(external.identity.scheme, Scheme::FromExternal);
        assert_ne!(
            named.identity.change_id, external.identity.change_id,
            "the two schemes agreed, so naming one would not matter"
        );
    }

    #[test]
    fn a_request_is_refused_without_its_version_operation_or_scheme_fields() {
        let config = config();
        let good = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "operation": "ensure",
            "scheme": "from-name",
            "workspace_name": "sy-issue-7",
        });
        assert!(Request::parse(&good, &config).is_ok());

        for (mutate, why) in [
            (
                serde_json::json!({"protocol_version": 2}),
                "a future protocol version",
            ),
            (
                serde_json::json!({"protocol_version": null}),
                "no protocol version",
            ),
            (
                serde_json::json!({"operation": "delete"}),
                "an unknown operation",
            ),
            (serde_json::json!({"operation": null}), "no operation"),
            (
                serde_json::json!({"scheme": "invented"}),
                "an unknown scheme",
            ),
            (
                serde_json::json!({"workspace_name": null}),
                "no workspace name",
            ),
            (
                serde_json::json!({"workspace_name": "../escape"}),
                "a traversal name",
            ),
        ] {
            let mut raw = good.clone();
            for (key, value) in mutate.as_object().expect("object") {
                raw[key] = value.clone();
            }
            assert!(Request::parse(&raw, &config).is_err(), "accepted {why}");
        }
    }

    /// A from-external request carries its own required fields, and the
    /// binding it produces is the one the identity rules give.
    #[test]
    fn a_from_external_request_binds_what_its_fields_name() {
        let config = config();
        let raw = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "operation": "checkpoint",
            "scheme": "from-external",
            "workspace_key": "issue-7",
            "external_id": "ISSUE-7",
            "generation": "1",
            "workspace_path": "/work/sy-issue-7",
        });
        let request = Request::parse(&raw, &config).expect("parses");
        assert_eq!(request.operation, Operation::Checkpoint);
        assert_eq!(request.workspace_path.as_deref(), Some("/work/sy-issue-7"));
        assert_eq!(
            request.identity,
            Identity::from_external("sy", "owner/repo", "issue-7", "ISSUE-7", "1")
                .expect("derives")
        );

        for missing in ["workspace_key", "external_id", "generation"] {
            let mut raw = raw.clone();
            raw[missing] = serde_json::Value::Null;
            assert!(
                Request::parse(&raw, &config).is_err(),
                "accepted a from-external request with no {missing}"
            );
        }
    }

    /// The reported base has to describe the workspace that exists, not
    /// the one this call asked for. They differ exactly when a retry
    /// resolves a ref that moved, which is the case an adapter is least
    /// able to notice on its own.
    #[test]
    fn a_reused_change_reports_the_base_it_is_bound_to_not_the_one_requested() {
        let requested = "a".repeat(40);
        let bound = "b".repeat(40);
        let reused = serde_json::json!({
            "reused": true,
            "base_revision": format!("11-{bound}"),
        });
        assert_eq!(bound_base(&reused, &requested), bound);

        // A response that reports nothing usable must not invent one.
        for absent in [
            serde_json::json!({}),
            serde_json::json!({ "base_revision": "13-not-a-git-object" }),
            serde_json::json!({ "base_revision": format!("11-{}", "z".repeat(40)) }),
            serde_json::json!({ "base_revision": "11-abc" }),
        ] {
            assert_eq!(
                bound_base(&absent, &requested),
                requested,
                "an unusable base_revision was trusted: {absent}"
            );
        }
    }

    #[test]
    fn a_failure_renders_the_wire_shape_an_adapter_forwards() {
        let json = Failure::terminal("binding_mismatch", "no").to_json();
        assert_eq!(json["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(json["error"]["code"], "binding_mismatch");
        assert_eq!(json["error"]["retryable"], false);
    }
}
