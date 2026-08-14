//! Client-side triage and next-action derivation over `/api/view`
//! (internal/oak.md items 2 and 3, after Oak's `branch triage` and
//! `agent state`).
//!
//! Both functions are pure folds of the view JSON the node already
//! serves. Deriving client-side rather than adding endpoints keeps the
//! ACL story unchanged: the node filters `/api/view` per credential
//! (D29), so a triage computed from the filtered document can only see
//! what its caller may see. The cost is honesty bookkeeping: a ref
//! missing from the response is *either* not created yet or not granted
//! — indistinguishable by design — so rows carry `landed: null` rather
//! than a guess when the destination ref is not visible.
//!
//! Output is bounded (internal/oak.md item 4): every list is capped at
//! [`LIST_CAP`] rows ranked most-actionable-first, and every truncation
//! is marked in-band with an `omitted` count, never silent.

use std::collections::BTreeMap;

/// Cap on every list in a derived document. Twenty rows of the most
/// actionable material bounds the token cost of a poll; the full detail
/// is always one `choir view` away.
pub const LIST_CAP: usize = 20;

/// Buckets a review can land in, most actionable first. The discriminant
/// order is the ranking used when a capped list must choose rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReviewBucket {
    /// An approval was retroactively slashed; the same `(ref, commit)`
    /// needs a fresh review before it can authorize a landing again.
    ReReviewRequired,
    /// Complete with at least one standing `RequestChanges`.
    ChangesRequested,
    /// Approved, but the destination ref has moved past the reviewed
    /// commit; landing would CAS-fail, and a fresh review at the new
    /// head is the path forward.
    ApprovedTargetMoved,
    /// Approved and the destination ref has not landed it yet.
    ApprovedAwaitingLanding,
    /// Assigned reviewers have not all answered.
    AwaitingVerdicts,
    /// Opened unassigned; the node's draw has not filled the list yet.
    Unassigned,
    /// Approved but bound to no destination ref; nothing can land it.
    ApprovedUnbound,
    /// The destination ref points at the reviewed commit.
    Landed,
    /// Settled and its bulk dropped.
    Archived,
}

impl ReviewBucket {
    fn name(self) -> &'static str {
        match self {
            Self::ReReviewRequired => "re-review-required",
            Self::ChangesRequested => "changes-requested",
            Self::ApprovedTargetMoved => "approved-target-moved",
            Self::ApprovedAwaitingLanding => "approved-awaiting-landing",
            Self::AwaitingVerdicts => "awaiting-verdicts",
            Self::Unassigned => "unassigned",
            Self::ApprovedUnbound => "approved-unbound",
            Self::Landed => "landed",
            Self::Archived => "archived",
        }
    }
}

/// Facts about one review, read straight off its `/api/view` row.
struct ReviewFacts<'a> {
    target: Option<&'a str>,
    target_ref: Option<&'a str>,
    archived: bool,
    approved: bool,
    complete: bool,
    re_review_required: bool,
    reviewers: Vec<&'a str>,
    answered: Vec<&'a str>,
    changes_requested_by: Vec<&'a str>,
    /// `Some(true/false)` when the destination ref is visible in the
    /// response; `None` when it is absent — not created yet, or not
    /// granted (D29 makes those indistinguishable on purpose).
    landed: Option<bool>,
}

impl<'a> ReviewFacts<'a> {
    fn read(
        row: &'a serde_json::Value,
        refs: Option<&'a serde_json::Map<String, serde_json::Value>>,
    ) -> Self {
        let target = row["target"].as_str();
        let target_ref = row["target_ref"].as_str();
        let reviewers: Vec<&str> = row["reviewers"]
            .as_array()
            .map(|list| list.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        let verdicts = row["verdicts"].as_object();
        let answered: Vec<&str> = verdicts
            .map(|map| map.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let changes_requested_by: Vec<&str> = verdicts
            .map(|map| {
                map.iter()
                    .filter(|(_, v)| v["verdict"].as_str() == Some("RequestChanges"))
                    .map(|(who, _)| who.as_str())
                    .collect()
            })
            .unwrap_or_default();
        let landed = match (target_ref, target) {
            (Some(name), Some(commit)) => refs
                .and_then(|map| map.get(name))
                .map(|current| current.as_str() == Some(commit)),
            _ => None,
        };
        Self {
            target,
            target_ref,
            archived: row["archived"].as_bool().unwrap_or(false),
            approved: row["approved"].as_bool().unwrap_or(false),
            complete: row["complete"].as_bool().unwrap_or(false),
            re_review_required: row["re_review_required"].as_bool().unwrap_or(false),
            reviewers,
            answered,
            changes_requested_by,
            landed,
        }
    }

    fn missing(&self) -> Vec<&'a str> {
        self.reviewers
            .iter()
            .filter(|r| !self.answered.contains(r))
            .copied()
            .collect()
    }

    fn bucket(&self) -> ReviewBucket {
        if self.re_review_required {
            return ReviewBucket::ReReviewRequired;
        }
        if self.landed == Some(true) {
            return ReviewBucket::Landed;
        }
        if self.archived {
            return ReviewBucket::Archived;
        }
        if self.approved {
            return match (self.target_ref, self.landed) {
                (None, _) => ReviewBucket::ApprovedUnbound,
                (Some(_), Some(false)) => ReviewBucket::ApprovedTargetMoved,
                // Ref not visible: first landing still pending, or not
                // granted. Awaiting-landing is the honest larger class.
                (Some(_), None) => ReviewBucket::ApprovedAwaitingLanding,
                (Some(_), Some(true)) => unreachable!("handled above"),
            };
        }
        if self.reviewers.is_empty() {
            return ReviewBucket::Unassigned;
        }
        if !self.complete {
            return ReviewBucket::AwaitingVerdicts;
        }
        ReviewBucket::ChangesRequested
    }

    fn row_json(&self, bucket: ReviewBucket) -> serde_json::Value {
        serde_json::json!({
            "bucket": bucket.name(),
            "target": self.target,
            "target_ref": self.target_ref,
            "landed": self.landed,
            "missing_verdicts": capped_list(&self.missing()),
            "changes_requested_by": capped_list(&self.changes_requested_by),
        })
    }
}

/// The first [`LIST_CAP`] items plus an in-band `omitted` count.
fn capped_list(items: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "items": items.iter().take(LIST_CAP).collect::<Vec<_>>(),
        "omitted": items.len().saturating_sub(LIST_CAP),
    })
}

/// Reviews indexed by the hex commit they target.
fn reviews_by_target(
    reviews: Option<&serde_json::Map<String, serde_json::Value>>,
) -> BTreeMap<&str, Vec<&str>> {
    let mut by_target: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, row) in reviews.into_iter().flatten() {
        if let Some(target) = row["target"].as_str() {
            by_target.entry(target).or_default().push(id);
        }
    }
    by_target
}

/// One change's bucket: `archived`, `unstarted` (no revision beyond its
/// base), `unreviewed` (a revision no review targets), or `in-review`.
fn change_row(
    row: &serde_json::Value,
    by_target: &BTreeMap<&str, Vec<&str>>,
) -> (&'static str, Vec<String>) {
    if row["active_workspace"].is_null() {
        return ("archived", Vec::new());
    }
    let revision = row["revision_id"].as_str().unwrap_or_default();
    if row["base_revision"].as_str() == Some(revision) {
        return ("unstarted", Vec::new());
    }
    match by_target.get(revision) {
        Some(ids) => (
            "in-review",
            ids.iter().take(LIST_CAP).map(ToString::to_string).collect(),
        ),
        None => ("unreviewed", Vec::new()),
    }
}

/// Derives the triage document from a `/api/view` response: every review
/// and change classified into a bucket, ranked most-actionable-first,
/// capped and counted.
#[must_use]
pub fn triage(view: &serde_json::Value) -> serde_json::Value {
    let refs = view["refs"].as_object();
    let reviews = view["reviews"].as_object();
    let mut rows: Vec<(ReviewBucket, &str, serde_json::Value)> = reviews
        .into_iter()
        .flatten()
        .map(|(id, row)| {
            let facts = ReviewFacts::read(row, refs);
            let bucket = facts.bucket();
            (bucket, id.as_str(), facts.row_json(bucket))
        })
        .collect();
    rows.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));

    let mut bucket_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (bucket, _, _) in &rows {
        *bucket_counts.entry(bucket.name()).or_insert(0) += 1;
    }
    let review_total = rows.len();
    let review_rows: serde_json::Map<String, serde_json::Value> = rows
        .into_iter()
        .take(LIST_CAP)
        .map(|(_, id, row)| (id.to_string(), row))
        .collect();

    let by_target = reviews_by_target(reviews);
    let changes = view["changes"].as_object();
    let mut change_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut change_rows = serde_json::Map::new();
    let change_total = changes.map_or(0, serde_json::Map::len);
    for (id, row) in changes.into_iter().flatten() {
        let (bucket, review_ids) = change_row(row, &by_target);
        *change_counts.entry(bucket).or_insert(0) += 1;
        if change_rows.len() < LIST_CAP {
            change_rows.insert(
                id.clone(),
                serde_json::json!({ "bucket": bucket, "reviews": review_ids }),
            );
        }
    }

    serde_json::json!({
        "format_version": 1,
        "review_buckets": bucket_counts,
        "reviews": review_rows,
        "reviews_omitted": review_total.saturating_sub(LIST_CAP),
        "change_buckets": change_counts,
        "changes": change_rows,
        "changes_omitted": change_total.saturating_sub(LIST_CAP),
        // What the classification could not see: with no refs section in
        // the (possibly ACL-filtered) response, no landing is decidable.
        "refs_visible": refs.is_some(),
    })
}

/// One recommended action. `mutates` and `needs_network` are independent
/// axes (an inspection needs the network but changes nothing), and the
/// discriminant order ranks actions by urgency: answering unblocks other
/// people, landing rots, revising and opening reviews unblock only you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActionKind {
    /// A review assigned to you is waiting on your verdict.
    AnswerReview,
    /// Your approved review is unlanded; submit the landing.
    Land,
    /// Your approved review's destination moved; open a fresh review.
    ReReview,
    /// A reviewer requested changes; revise and checkpoint again.
    Revise,
    /// Your checkpointed revision has no review; open one.
    RequestReview,
    /// Your change has published nothing beyond its base; checkpoint.
    Checkpoint,
}

impl ActionKind {
    fn describe(self) -> (&'static str, bool, bool) {
        match self {
            Self::AnswerReview => ("answer-review", true, true),
            Self::Land => ("land", true, true),
            Self::ReReview => ("re-review", true, true),
            Self::Revise => ("revise", true, false),
            Self::RequestReview => ("request-review", true, true),
            Self::Checkpoint => ("checkpoint", true, true),
        }
    }
}

fn action_json(
    kind: ActionKind,
    subject: (&str, &str),
    command: String,
    why: String,
) -> serde_json::Value {
    let (name, mutates, needs_network) = kind.describe();
    let (subject_kind, subject_id) = subject;
    serde_json::json!({
        "action": name,
        subject_kind: subject_id,
        "command": command,
        "mutates": mutates,
        "needs_network": needs_network,
        "why": why,
    })
}

/// Derives one bounded next-actions document for `channel` from a
/// `/api/view` response fetched from `api`: what you owe others, what
/// your own changes need, and what you are waiting on, ranked. Command
/// strings carry `<key-file>` (and other placeholders) literally where
/// only the caller knows the value.
#[must_use]
pub fn next_actions(view: &serde_json::Value, api: &str, channel: &str) -> serde_json::Value {
    let refs = view["refs"].as_object();
    let reviews = view["reviews"].as_object();
    let mut actions: Vec<(ActionKind, &str, serde_json::Value)> = Vec::new();
    let mut waiting: Vec<serde_json::Value> = Vec::new();

    for (id, row) in reviews.into_iter().flatten() {
        let facts = ReviewFacts::read(row, refs);
        if facts.archived {
            continue;
        }
        if facts.reviewers.contains(&channel) && !facts.answered.contains(&channel) {
            actions.push((
                ActionKind::AnswerReview,
                id,
                action_json(
                    ActionKind::AnswerReview,
                    ("review", id),
                    format!("choir verdict {api} <key-file> {channel} {id} approve|request-changes [note]"),
                    "assigned to you and unanswered; the review cannot complete without you".into(),
                ),
            ));
        }
    }

    let by_target = reviews_by_target(reviews);
    for (change_id, row) in view["changes"].as_object().into_iter().flatten() {
        if row["owner"].as_str() != Some(channel) || row["active_workspace"].is_null() {
            continue;
        }
        let revision = row["revision_id"].as_str().unwrap_or_default();
        let workspace = row["workspace_id"].as_str().unwrap_or_default();
        if row["base_revision"].as_str() == Some(revision) {
            actions.push((
                ActionKind::Checkpoint,
                change_id,
                action_json(
                    ActionKind::Checkpoint,
                    ("change", change_id),
                    format!("choir checkpoint {api} <key-file> {channel} {change_id} {workspace} <git-oid>"),
                    "no revision published beyond the base; commit and push, then checkpoint".into(),
                ),
            ));
            continue;
        }
        let Some(review_ids) = by_target.get(revision) else {
            actions.push((
                ActionKind::RequestReview,
                change_id,
                action_json(
                    ActionKind::RequestReview,
                    ("change", change_id),
                    format!("choir review {api} <key-file> {channel} <review-id> {revision} [--ref <repo:ref>]"),
                    "latest revision has no review; name no reviewers and the node draws them".into(),
                ),
            ));
            continue;
        };
        for id in review_ids {
            let facts = ReviewFacts::read(&reviews.expect("indexed from reviews")[*id], refs);
            match facts.bucket() {
                ReviewBucket::ChangesRequested | ReviewBucket::ReReviewRequired => {
                    actions.push((
                        ActionKind::Revise,
                        change_id,
                        action_json(
                            ActionKind::Revise,
                            ("change", change_id),
                            format!("choir checkpoint {api} <key-file> {channel} {change_id} {workspace} <git-oid>"),
                            format!(
                                "review {id}: changes requested by {:?}; revise, checkpoint, request a fresh review",
                                facts.changes_requested_by
                            ),
                        ),
                    ));
                }
                ReviewBucket::ApprovedAwaitingLanding => {
                    let target_ref = facts.target_ref.unwrap_or("<repo:ref>");
                    actions.push((
                        ActionKind::Land,
                        change_id,
                        action_json(
                            ActionKind::Land,
                            ("review", id),
                            format!(
                                "choir submit {api} <key-file> {channel} '{{\"format_version\":1,\"kind\":{{\"SetRef\":{{\"name\":\"{target_ref}\",\"commit\":\"{revision}\",\"prev\":<current-or-null>}}}}}}'"
                            ),
                            "approved and unlanded; a protected ref needs this exact (ref, commit) and CAS-fails if the ref moved".into(),
                        ),
                    ));
                }
                ReviewBucket::ApprovedTargetMoved => {
                    actions.push((
                        ActionKind::ReReview,
                        change_id,
                        action_json(
                            ActionKind::ReReview,
                            ("review", id),
                            format!("choir review {api} <key-file> {channel} <review-id> {revision} --ref {}", facts.target_ref.unwrap_or_default()),
                            "approved, but the destination ref moved past the reviewed commit; open a fresh review".into(),
                        ),
                    ));
                }
                ReviewBucket::AwaitingVerdicts => {
                    waiting.push(serde_json::json!({
                        "review": id,
                        "why": format!("awaiting verdicts from {:?}", facts.missing()),
                    }));
                }
                ReviewBucket::Unassigned => {
                    waiting.push(serde_json::json!({
                        "review": id,
                        "why": "unassigned; the node's reviewer draw has not filled the list",
                    }));
                }
                _ => {}
            }
        }
    }

    actions.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let action_total = actions.len();
    let waiting_total = waiting.len();
    serde_json::json!({
        "format_version": 1,
        "channel": channel,
        // Echoed so a signer can bind its next op without a second read.
        "log": view["log"],
        "actions": actions.into_iter().take(LIST_CAP).map(|(_, _, a)| a).collect::<Vec<_>>(),
        "actions_omitted": action_total.saturating_sub(LIST_CAP),
        "waiting": waiting.iter().take(LIST_CAP).collect::<Vec<_>>(),
        "waiting_omitted": waiting_total.saturating_sub(LIST_CAP),
        "refs_visible": refs.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view with one review in each interesting configuration, plus
    /// refs and changes exercising the landed/moved distinction.
    fn sample_view() -> serde_json::Value {
        serde_json::json!({
            "log": { "node": "01-aa", "head": null, "scope_required": false },
            "refs": { "demo.git:refs/heads/main": "01-c1" },
            "reviews": {
                "r-landed": { "target": "01-c1", "target_ref": "demo.git:refs/heads/main",
                    "reviewers": ["ops/ana"], "verdicts": { "ops/ana": { "verdict": "Approve", "note": "" } },
                    "slashes": {}, "complete": true, "approved": true, "approval_weight": 1,
                    "re_review_required": false, "archived": false, "comments": [] },
                "r-moved": { "target": "01-c0", "target_ref": "demo.git:refs/heads/main",
                    "reviewers": ["ops/ana"], "verdicts": { "ops/ana": { "verdict": "Approve", "note": "" } },
                    "slashes": {}, "complete": true, "approved": true, "approval_weight": 1,
                    "re_review_required": false, "archived": false, "comments": [] },
                "r-pending": { "target": "01-c2", "target_ref": null,
                    "reviewers": ["ops/ana", "ops/bob"],
                    "verdicts": { "ops/ana": { "verdict": "Approve", "note": "" } },
                    "slashes": {}, "complete": false, "approved": false, "approval_weight": 1,
                    "re_review_required": false, "archived": false, "comments": [] },
                "r-rejected": { "target": "01-c3", "target_ref": null,
                    "reviewers": ["ops/bob"],
                    "verdicts": { "ops/bob": { "verdict": "RequestChanges", "note": "no" } },
                    "slashes": {}, "complete": true, "approved": false, "approval_weight": 0,
                    "re_review_required": false, "archived": false, "comments": [] },
                "r-unassigned": { "target": "01-c4", "target_ref": null,
                    "reviewers": [], "verdicts": {}, "slashes": {}, "complete": false,
                    "approved": false, "approval_weight": 0,
                    "re_review_required": false, "archived": false, "comments": [] },
            },
            "changes": {
                "ch-idle": { "owner": "dev/kim", "workspace_id": "ws-1",
                    "active_workspace": "ws-1", "base_revision": "01-b0", "revision_id": "01-b0" },
                "ch-reviewed": { "owner": "dev/kim", "workspace_id": "ws-2",
                    "active_workspace": "ws-2", "base_revision": "01-b0", "revision_id": "01-c3" },
            },
        })
    }

    #[test]
    fn buckets_cover_the_sample() {
        let doc = triage(&sample_view());
        let bucket = |id: &str| doc["reviews"][id]["bucket"].as_str().unwrap().to_string();
        assert_eq!(bucket("r-landed"), "landed");
        assert_eq!(bucket("r-moved"), "approved-target-moved");
        assert_eq!(bucket("r-pending"), "awaiting-verdicts");
        assert_eq!(bucket("r-rejected"), "changes-requested");
        assert_eq!(bucket("r-unassigned"), "unassigned");
        assert_eq!(doc["review_buckets"]["landed"], 1);
        assert_eq!(doc["reviews_omitted"], 0);
        assert_eq!(doc["changes"]["ch-idle"]["bucket"], "unstarted");
        assert_eq!(doc["changes"]["ch-reviewed"]["bucket"], "in-review");
        assert_eq!(doc["refs_visible"], true);
    }

    #[test]
    fn missing_refs_section_yields_null_landed_not_a_guess() {
        let mut view = sample_view();
        view.as_object_mut().unwrap().remove("refs");
        let doc = triage(&view);
        assert_eq!(doc["refs_visible"], false);
        assert!(doc["reviews"]["r-landed"]["landed"].is_null());
        assert_eq!(
            doc["reviews"]["r-landed"]["bucket"],
            "approved-awaiting-landing"
        );
    }

    #[test]
    fn reviewer_owes_a_verdict() {
        let doc = next_actions(&sample_view(), "http://n", "ops/bob");
        let kinds: Vec<&str> = doc["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["action"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["answer-review"]);
        assert!(doc["actions"][0]["command"]
            .as_str()
            .unwrap()
            .contains("choir verdict http://n <key-file> ops/bob r-pending"));
    }

    #[test]
    fn owner_sees_change_work_ranked() {
        let doc = next_actions(&sample_view(), "http://n", "dev/kim");
        let kinds: Vec<&str> = doc["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["action"].as_str().unwrap())
            .collect();
        // Revise (r-rejected targets ch-reviewed's revision) outranks the
        // fresh checkpoint on the idle change.
        assert_eq!(kinds, ["revise", "checkpoint"]);
        assert_eq!(doc["actions_omitted"], 0);
    }

    #[test]
    fn boundaries_hold_on_an_empty_view() {
        let empty = serde_json::json!({});
        let doc = triage(&empty);
        assert_eq!(doc["review_buckets"], serde_json::json!({}));
        assert_eq!(doc["refs_visible"], false);
        let actions = next_actions(&empty, "http://n", "nobody");
        assert_eq!(actions["actions"], serde_json::json!([]));
        assert_eq!(actions["waiting"], serde_json::json!([]));
    }
}
