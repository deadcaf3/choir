//! Who an actor is, from what the log already says about them (D63).
//!
//! Nothing here is new state. Every number is counted out of the same
//! `/api/view` body the caller could fetch themselves; the point is that
//! nobody does, because the answer to "should I trust this reviewer" is
//! spread across four sections of a document that is mostly about
//! something else.
//!
//! **It derives from the view the caller may already see, never from the
//! raw one.** That is the whole ACL story: a profile cannot disclose a
//! change, review or check that `/api/view` would have withheld, because
//! it is looking at the withheld copy. A second filter here would be a
//! second thing to keep right, and the first time the two disagreed the
//! profile would be the one that leaked.
//!
//! What is deliberately absent is vouches. D24's Sybil resistance wants
//! key age, vouches, scoped grants and bonds; only the first of those is
//! persisted today, as [`KeyBinding::bound_at`], and this page reports it
//! rather than pretending the rest exist. A profile that showed a
//! trust score computed from one input would be worse than one that
//! shows the input.
//!
//! [`KeyBinding::bound_at`]: choir_view::KeyBinding::bound_at
//!
//! # Examples
//!
//! ```
//! // Shaped the way `/api/view` really answers, which is the whole
//! // reason this example is worth reading: `next_seq` lives inside
//! // `log`, and an example that put it anywhere else would document a
//! // response no node sends.
//! let view = serde_json::json!({
//!     "log": { "next_seq": 100 },
//!     "bindings": {
//!         "b3:aa": { "operator": "ops", "channel": "alice", "bound_at": 40 }
//!     },
//!     "changes": { "c1": { "owner": "alice" } },
//!     "reviews": {},
//!     "checks": {},
//! });
//! let profile = choir_node::profile::of(&view, "alice");
//! assert_eq!(profile["channel"], "alice");
//! assert_eq!(profile["changes"]["owned"], 1);
//! // 100 ops have been sequenced, 40 of them before this key existed.
//! assert_eq!(profile["keys"][0]["ops_since_binding"], 60);
//! ```

/// Counts one actor's standing out of a view body.
///
/// `view` is a `/api/view` response as JSON, already narrowed to what
/// the caller may read. `channel` is the name an actor signs as.
///
/// An actor with no keys, changes, reviews or checks is not an error:
/// the answer is a profile with zeroes and `known: false`, because "I
/// have never heard of them" and "they have done nothing here" are
/// different sentences and a caller deciding whether to trust a
/// reviewer needs to be told which one it got.
#[must_use]
pub fn of(view: &serde_json::Value, channel: &str) -> serde_json::Value {
    // `log.next_seq`, not a top-level `next_seq`. The first version of
    // this read the latter, which `/api/view` does not emit, so every
    // key on every node reported an age of zero -- and the doctest below
    // passed the whole time, because it fed a hand-written document
    // shaped the way the code wished the view were.
    let next_seq = view["log"]["next_seq"].as_u64().unwrap_or_default();

    // Keys, oldest binding first. `bound_at` is assigned once and never
    // moves, so this order is one replay reproduces exactly.
    let mut keys: Vec<serde_json::Value> = Vec::new();
    if let Some(bindings) = view["bindings"].as_object() {
        for (hash, binding) in bindings {
            if binding["channel"].as_str() != Some(channel) {
                continue;
            }
            let bound_at = binding["bound_at"].as_u64().unwrap_or_default();
            keys.push(serde_json::json!({
                "key": hash,
                "operator": binding["operator"],
                "bound_at": bound_at,
                // Ops, not elapsed time. A node that sat idle for a month
                // and a node that took a thousand pushes in an hour are
                // not comparable on a clock, and this number says so by
                // being in the only unit the log actually has.
                "ops_since_binding": next_seq.saturating_sub(bound_at),
                "revoked": binding["revoked"],
            }));
        }
    }
    keys.sort_by_key(|key| key["bound_at"].as_u64().unwrap_or_default());

    let mut owned = 0u64;
    let mut first_verdict: Option<u64> = None;
    if let Some(changes) = view["changes"].as_object() {
        for change in changes.values() {
            if change["owner"].as_str() == Some(channel) {
                owned += 1;
            }
        }
    }

    let (mut assigned, mut approved, mut requested, mut slashed, mut commented) = (0, 0, 0, 0, 0);
    if let Some(reviews) = view["reviews"].as_object() {
        for review in reviews.values() {
            if review["reviewers"]
                .as_array()
                .is_some_and(|who| who.iter().any(|name| name.as_str() == Some(channel)))
            {
                assigned += 1;
            }
            match review["verdicts"][channel]["verdict"].as_str() {
                Some("Approve") => approved += 1,
                Some("RequestChanges") => requested += 1,
                _ => {}
            }
            if review["slashes"][channel].is_string() {
                slashed += 1;
            }
            if let Some(comments) = review["comments"].as_array() {
                commented += comments
                    .iter()
                    .filter(|c| c["author"].as_str() == Some(channel))
                    .count() as u64;
            }
            // The earliest sequence this actor is on record at, across
            // every verdict they gave. Reviews are keyed by id rather
            // than by position, so the minimum has to be searched for.
            if let Some(at) = review["verdicts"][channel]["at"].as_u64() {
                first_verdict = Some(first_verdict.map_or(at, |seen: u64| seen.min(at)));
            }
        }
    }

    let (mut reported, mut failed) = (0u64, 0u64);
    if let Some(checks) = view["checks"].as_object() {
        for check in checks.values() {
            if check["reporter"].as_str() != Some(channel) {
                continue;
            }
            reported += 1;
            if check["status"].as_str() == Some("Failed") {
                failed += 1;
            }
        }
    }

    let known = !keys.is_empty() || owned > 0 || assigned > 0 || reported > 0 || commented > 0;

    serde_json::json!({
        "channel": channel,
        "known": known,
        "keys": keys,
        "changes": { "owned": owned },
        "reviews": {
            "assigned": assigned,
            "approved": approved,
            "changes_requested": requested,
            "slashed": slashed,
            "comments": commented,
            "first_verdict_at": first_verdict,
        },
        "checks": { "reported": reported, "failed": failed },
        // Named rather than omitted. A reader who does not find vouches
        // here should learn that there are none to find, not conclude
        // this actor has none.
        "vouches": serde_json::Value::Null,
    })
}
