//! Rejections that name the repair.
//!
//! An agent that is told "stale head" learns that something failed. An
//! agent told *what* was expected, *what* was found, and *which* of a
//! small set of actions is admissible can act without a human. Two 2026
//! studies (arXiv:2607.14167, arXiv:2606.05037) measure a large gain in
//! agent task success from errors that name the repair, and both isolate
//! it to naming **admissible alternatives** — not to verbosity, and not
//! to JSON rather than prose. The effect was null on at least one small
//! model, so the magnitude is directional; the mechanism is what this
//! implements.
//!
//! # Why this is not on the sequencer seam
//!
//! `SubmitPolicy::check` still returns `Result<(), String>`. Reason
//! codes and next actions are properties of the *API surface*: they say
//! what an HTTP client should do next. Pushing them into L2 admission
//! would make the sequencer carry transport shape, and `SubmitPolicy` is
//! implemented by things with no HTTP surface at all. So the policy
//! renders a [`Rejection`] into its reason string, and the HTTP boundary
//! decodes it — [`Rejection::decode`] falls back to a plain message for
//! any reason that did not come from here, so nothing is lost when a
//! rejection originates elsewhere.
//!
//! Shape follows RFC 9457 (`application/problem+json`) loosely: `type`
//! becomes the stable `code`, `detail` becomes `error`. It is not served
//! as `application/problem+json` because every existing client and test
//! reads `error` out of an ordinary JSON body, and changing the media
//! type would break them for no gain an agent can use.

/// Stable, machine-readable rejection reasons.
///
/// A code is a contract: clients branch on it, so renaming one is a
/// breaking change and adding one is not. Every variant is listed in
/// `ERRORS.md` with the action a client should take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The signature names a key id this node has no record of. Says
    /// nothing about the signature itself, which is not checked once the
    /// key is missing.
    UnknownKey,
    /// The payload did not decode as a `ViewOp`.
    MalformedOp,
    /// The request body was missing fields or badly encoded.
    MalformedRequest,
    /// A verdict claimed a reviewer other than the signed channel.
    ReviewerMismatch,
    /// The signing key is bound to a different channel name.
    ChannelNotOwned,
    /// Only the node's own key may author this operation.
    NodeOnly,
    /// The node assigns reviewers; a self-named list was refused.
    AssignmentRequired,
    /// The target ref is protected and needs a node-drawn reviewer list.
    ProtectedRef,
    /// The target ref lacks the required independent approval weight.
    ReviewRequired,
    /// The target ref is protected and cannot be deleted.
    RefUndeletable,
    /// Compare-and-swap failed: the state moved under the submission.
    StaleHead,
    /// A review-op precondition failed (duplicate id, unknown review,
    /// already assigned, archived).
    ReviewState,
    /// A provenance record was missing a subject or kind.
    ProvenanceState,
    /// A stable change was unknown, duplicated, archived, or mismatched.
    ChangeState,
    /// A workspace lifecycle request conflicted with its durable binding.
    WorkspaceState,
    /// A key-binding precondition failed (a key already bound to another
    /// operator, a revoked or unbound key, a channel naming a different
    /// operator).
    IdentityState,
    /// The operator's protected-ref list could not be read, so the gate
    /// failed closed.
    PolicyUnavailable,
    /// Requested log entries are older than anything this node can serve.
    LogEvicted,
    /// These exact signed bytes have already been admitted.
    DuplicateSubmission,
    /// This node requires a signed scope and the op carried none.
    ScopeRequired,
    /// The op was signed for a different node's log.
    ForeignScope,
    /// The scoped head is no longer recent enough to admit.
    StaleScope,
    /// The signature does not verify under the key it names, which this
    /// node does trust. Distinct from [`Code::UnknownKey`] because the
    /// repairs are opposites: that one widens the trusted set, this one
    /// must not.
    BadSignature,
    /// Anything that did not originate as a structured rejection.
    Unclassified,
}

impl Code {
    /// The stable wire string. Written out rather than derived from the
    /// variant name so renaming the Rust identifier cannot silently
    /// change the contract.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown_key",
            Self::MalformedOp => "malformed_op",
            Self::MalformedRequest => "malformed_request",
            Self::ReviewerMismatch => "reviewer_mismatch",
            Self::ChannelNotOwned => "channel_not_owned",
            Self::NodeOnly => "node_only",
            Self::AssignmentRequired => "assignment_required",
            Self::ProtectedRef => "protected_ref",
            Self::ReviewRequired => "review_required",
            Self::RefUndeletable => "ref_undeletable",
            Self::StaleHead => "stale_head",
            Self::ReviewState => "review_state",
            Self::ProvenanceState => "provenance_state",
            Self::ChangeState => "change_state",
            Self::WorkspaceState => "workspace_state",
            Self::IdentityState => "identity_state",
            Self::PolicyUnavailable => "policy_unavailable",
            Self::LogEvicted => "log_evicted",
            Self::DuplicateSubmission => "duplicate_submission",
            Self::ScopeRequired => "scope_required",
            Self::ForeignScope => "foreign_scope",
            Self::StaleScope => "stale_scope",
            Self::BadSignature => "bad_signature",
            Self::Unclassified => "unclassified",
        }
    }

    /// Every code, for the docs generator and the coverage test.
    #[must_use]
    pub fn all() -> &'static [Code] {
        &[
            Self::UnknownKey,
            Self::MalformedOp,
            Self::MalformedRequest,
            Self::ReviewerMismatch,
            Self::ChannelNotOwned,
            Self::NodeOnly,
            Self::AssignmentRequired,
            Self::ProtectedRef,
            Self::ReviewRequired,
            Self::RefUndeletable,
            Self::StaleHead,
            Self::ReviewState,
            Self::ProvenanceState,
            Self::ChangeState,
            Self::WorkspaceState,
            Self::IdentityState,
            Self::PolicyUnavailable,
            Self::LogEvicted,
            Self::DuplicateSubmission,
            Self::ScopeRequired,
            Self::ForeignScope,
            Self::StaleScope,
            Self::BadSignature,
            Self::Unclassified,
        ]
    }
}

/// A refusal, with enough for a client to decide what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Stable machine-readable reason.
    pub code: String,
    /// Human-readable detail. Kept named `error` because that is what
    /// every existing client and test already reads.
    pub error: String,
    /// What the check required, when the check compared two states.
    pub expected: Option<String>,
    /// What it found instead.
    pub actual: Option<String>,
    /// The admissible next action, in the imperative. **This is the
    /// field the research isolates the gain to**, so it is required
    /// rather than optional: a rejection that cannot name a next action
    /// is a rejection whose author has not finished thinking.
    pub next: String,
}

impl Rejection {
    /// A rejection with no state comparison.
    #[must_use]
    pub fn new(code: Code, error: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            error: error.into(),
            expected: None,
            actual: None,
            next: next.into(),
        }
    }

    /// Adds the two states a failed comparison was between.
    #[must_use]
    pub fn with_states(
        mut self,
        expected: Option<String>,
        actual: Option<String>,
    ) -> Self {
        self.expected = expected;
        self.actual = actual;
        self
    }

    /// This rejection as a JSON value.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "code": self.code,
            "error": self.error,
            "next": self.next,
        });
        // Absent rather than null: a client checking `expected` should
        // find nothing when no comparison happened, not a null to
        // special-case.
        if let Some(e) = &self.expected {
            v["expected"] = serde_json::json!(e);
        }
        if let Some(a) = &self.actual {
            v["actual"] = serde_json::json!(a);
        }
        v
    }

    /// Renders for the `Result<(), String>` seam.
    #[must_use]
    pub fn encode(&self) -> String {
        self.to_json().to_string()
    }

    /// Recovers a rejection from a reason string.
    ///
    /// A reason that did not come from [`Rejection::encode`] becomes an
    /// [`Code::Unclassified`] rejection carrying the original text, so a
    /// caller always gets the same shape and no message is ever dropped
    /// on the floor.
    #[must_use]
    pub fn decode(reason: &str) -> Self {
        let unclassified = || Self {
            code: Code::Unclassified.as_str().to_string(),
            error: reason.to_string(),
            expected: None,
            actual: None,
            next: "read the message; this path does not yet name a repair".to_string(),
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(reason) else {
            return unclassified();
        };
        let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
        // All three required fields or nothing: a half-decoded rejection
        // would report a code it cannot back up.
        match (field("code"), field("error"), field("next")) {
            (Some(code), Some(error), Some(next)) => Self {
                code: code.to_string(),
                error: error.to_string(),
                expected: field("expected").map(String::from),
                actual: field("actual").map(String::from),
                next: next.to_string(),
            },
            _ => unclassified(),
        }
    }

    /// The HTTP body: the rejection as JSON.
    #[must_use]
    pub fn body(&self) -> String {
        self.encode()
    }
}

/// Maps a `ViewError` onto a rejection, unpacking the states it already
/// carries rather than stringifying them into prose.
#[must_use]
pub fn from_view_error(e: &choir_view::ViewError) -> Rejection {
    use choir_view::ViewError;
    match e {
        ViewError::StaleHead {
            target,
            expected,
            actual,
        } => Rejection::new(
            Code::StaleHead,
            format!("compare-and-swap failed on {target}"),
            "re-read GET /api/view for the current value, rebase your intent on it, and resubmit \
             with the new prev",
        )
        .with_states(
            expected.as_ref().map(choir_oplog::ContentHash::to_hex),
            actual.as_ref().map(choir_oplog::ContentHash::to_hex),
        ),
        ViewError::Review(msg) => Rejection::new(
            Code::ReviewState,
            msg.clone(),
            "read GET /api/view `reviews` for this id's current state; a review that is already \
             assigned, complete, or archived does not accept the op you sent",
        ),
        ViewError::Provenance(msg) => Rejection::new(
            Code::ProvenanceState,
            msg.clone(),
            "resubmit with a non-empty subject and kind",
        ),
        ViewError::Change(msg) => Rejection::new(
            Code::ChangeState,
            msg.clone(),
            "read GET /api/view `changes` for the current owner, workspace and revision; use a \
             new change id or checkpoint from the reported revision",
        ),
        ViewError::Identity(msg) => Rejection::new(
            Code::IdentityState,
            msg.clone(),
            "not a retry: a key belongs to one operator for its lifetime and a revoked key is \
             never rebindable, so bind a fresh key instead",
        ),
        ViewError::Decode(msg) => Rejection::new(
            Code::MalformedOp,
            format!("decode failed: {msg}"),
            "serialize the op with the same ViewOp version the node runs; see GET /llms.txt",
        ),
        other => Rejection::new(
            Code::Unclassified,
            format!("{other:?}"),
            "retry once; if it persists the node has a problem the client cannot fix",
        ),
    }
}

impl Code {
    /// One-line meaning, for the `ERRORS.md` table.
    #[must_use]
    pub fn meaning(self) -> &'static str {
        match self {
            Self::UnknownKey => "The signature names a key id this node has no record of",
            Self::MalformedOp => "The payload did not decode as a `ViewOp`",
            Self::MalformedRequest => "The request body was missing fields or badly encoded",
            Self::ReviewerMismatch => "A verdict claimed a reviewer other than the signed channel",
            Self::ChannelNotOwned => "The signing key is bound to a different channel name",
            Self::NodeOnly => "Only the node's own key may author this operation",
            Self::AssignmentRequired => "This node assigns reviewers; a self-named list was refused",
            Self::ProtectedRef => "The target ref is protected and needs a node-drawn reviewer list",
            Self::ReviewRequired => "The target ref lacks the required independent approval weight",
            Self::RefUndeletable => "The target ref is protected and cannot be deleted",
            Self::StaleHead => "Compare-and-swap failed: the state moved under the submission",
            Self::ReviewState => "A review-op precondition failed (duplicate id, unknown review, already assigned, archived)",
            Self::ProvenanceState => "A provenance record was missing a subject or kind",
            Self::ChangeState => "A stable change was unknown, duplicated, archived, or mismatched",
            Self::WorkspaceState => "A workspace lifecycle request conflicted with its durable binding",
            Self::IdentityState => "A key-binding precondition failed (key already bound to another operator, revoked or unbound key, channel naming a different operator)",
            Self::PolicyUnavailable => "The operator's protected-ref list could not be read, so the gate failed closed",
            Self::LogEvicted => "Requested log entries are older than anything this node can serve",
            Self::DuplicateSubmission => "These exact signed bytes already landed; a signature is admissible once",
            Self::ScopeRequired => "This node admits only ops signed for its own log and a recent head, and this op carried no scope",
            Self::ForeignScope => "The op was signed for another node's log",
            Self::StaleScope => "The head the op was signed against is no longer in the node's recent window",
            Self::BadSignature => "The signature does not verify over these bytes, under a key this node does trust",
            Self::Unclassified => "A rejection that did not originate as a structured one",
        }
    }

    /// What a client should do on receipt. One row of `ERRORS.md`.
    #[must_use]
    pub fn action(self) -> &'static str {
        match self {
            Self::UnknownKey => "Ask the operator to register your public key.                 `choir key <file> <you>` prints the line; it takes effect on the next request.",
            Self::MalformedOp => "Serialize a `ViewOp` and sign its bytes. `choir submit` does                 this correctly; `GET /llms.txt` lists the operations.",
            Self::MalformedRequest => "Send a JSON object with the fields the endpoint wants.                 `GET /llms.txt` lists them.",
            Self::ReviewerMismatch => "Resubmit on your own channel. `choir verdict` signs on                 the reviewer name by construction, so use it rather than hand-rolling.",
            Self::ChannelNotOwned => "Submit on the channel your key is bound to — it is in                 `expected`. Or ask the operator to bind a key to the channel you want.",
            Self::NodeOnly => "Nothing to retry: this operation is the node's to author. For                 reviewer assignment, request a review with an empty reviewer list.",
            Self::AssignmentRequired => "Resubmit with an empty reviewer list. The node draws                 reviewers and returns their names in the response.",
            Self::ProtectedRef => "Resubmit with an empty reviewer list. On a protected ref only                 a node-drawn list is accepted.",
            Self::ReviewRequired => "Open a review naming this ref and commit                 (`choir review ... --ref <repo:ref>`), obtain approvals from two distinct                 operators, then push again.",
            Self::RefUndeletable => "Do not delete this ref, or ask the operator to remove it                 from the protected-ref list.",
            Self::StaleHead => "Re-read `GET /api/view`, rebase your intent on the value in                 `actual`, and resubmit with that as `prev`. If you are retrying a submission                 whose response you lost, check for `already_applied` first — a completed retry                 answers 200, not this.",
            Self::ReviewState => "Read `reviews` in `GET /api/view` for this id. A review that                 is already assigned, complete, or archived does not accept the op you sent.",
            Self::ProvenanceState => "Resubmit with a non-empty subject and kind.",
            Self::ChangeState => "Read `changes` in `GET /api/view`, then use its owner, workspace and revision or choose a new change id.",
            Self::WorkspaceState => "Read `changes` and `workspaces` in `GET /api/view`; retry only with the exact existing binding, or choose a new workspace name.",
            Self::IdentityState => "Read `bindings` in `GET /api/view` for this key. Not a retry:                 a key belongs to one operator for the life of the key, and a revoked key is                 never rebindable. Bind a fresh key instead. `error` names which of the two                 applies.",
            Self::PolicyUnavailable => "Operator problem, not a client one: the gate fails                 closed rather than guessing. Retry once the file is restored.",
            Self::LogEvicted => "Resync from the sequence in `window_base`; entries before it                 are gone from this node.",
            Self::DuplicateSubmission => "If you are retrying, this is your op: read `seq`.                 A submission that already landed answers 200 with `already_applied`, and                 only reaches you as a rejection if the window moved underneath the retry.                 If you meant a second, distinct change, sign a new op — two otherwise                 byte-identical ops are told apart by their scope.",
            Self::ScopeRequired => "Read `log.node` and `log.head` from `GET /api/view`,                 put them in the op's `scope`, and sign that. `choir submit` does this                 automatically. An unscoped op cannot be admitted here because nothing in it                 says which log it was meant for or that it has not run before.",
            Self::ForeignScope => "Nothing to retry against this node: the op names another                 node's id in `expected`. Sign a scope naming this node, whose id is in                 `actual` and in `log.node` of `GET /api/view`.",
            Self::StaleScope => "Re-read `log.head` from `GET /api/view` and sign a fresh op                 against it. A signature is only admissible while the head it names is still                 in the node's window, which is what stops a captured op from being replayed                 later.",
            Self::BadSignature => "Re-sign the exact bytes you are submitting: a signature covers                 one `(channel, payload)` pair and does not carry to another. Registering a key                 does not help here, the key this names is already trusted. If you did not send                 this, a signature of yours was replayed onto bytes you never signed, and the                 operator wants to know.",
            Self::Unclassified => "Read `error`. This path does not name a repair yet — that is                 a gap, and worth reporting.",
        }
    }
}


/// Collapses runs of whitespace to single spaces.
///
/// Rust's `\` string continuation keeps the next line's indentation, so
/// a wrapped literal renders into markdown with the source's leading
/// spaces intact — which markdown shows verbatim and, at four spaces,
/// turns into a code block.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `ERRORS.md`: every code and what to do about it.
///
/// Generated rather than authored, for the same reason the agent surface
/// is: a hand-maintained table of codes drifts from the codes, and a
/// client branching on a code that no longer exists fails in the least
/// debuggable way available.
#[must_use]
pub fn errors_md() -> String {
    // Each paragraph is normalized: a wrapped Rust literal keeps the
    // source's indentation, and four leading spaces in markdown is a code
    // block rather than prose.
    let para = |s: &str| format!("{}\n\n", one_line(s));
    let mut out = String::from("# Rejection codes\n\n");
    out.push_str(&para(
        "Generated from `crates/choir-node/src/reject.rs`. Do not edit; edit the table.",
    ));
    out.push_str(&para(
        "Every rejection body carries `code`, `error` and `next`. `expected` and `actual` are
         present when the check compared two states — a compare-and-swap failure, or a channel
         bound to a name other than the one used.",
    ));
    out.push_str(&para(
        "`code` is the contract: branch on it, not on `error`. Adding a code is not a breaking
         change; renaming one is.",
    ));
    out.push_str("| Code | Meaning | What to do |\n|---|---|---|\n");
    for c in Code::all() {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            c.as_str(),
            one_line(c.meaning()),
            one_line(c.action())
        ));
    }
    out.push_str("\n## Retrying a submission whose response you lost\n\n");
    out.push_str(&para(
        "Resubmit the identical signed bytes. If it already landed, the node answers **200**
         with `already_applied: true` and the original `seq` and `hash`, rather than the
         compare-and-swap failure the two cases would otherwise share. Read those back and
         proceed; do not rebuild the operation.",
    ));
    out.push_str(&para(
        "This is bounded to recent history: the node keeps the index over the same window
         `GET /api/log` serves from. A retry seconds or minutes later is covered; one after the
         window has turned over reads as `stale_head`, which is the safe direction.",
    ));
    out
}
