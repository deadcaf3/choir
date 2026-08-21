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
//! D24's Sybil resistance wants key age, vouches, scoped grants and
//! bonds. Two of the four are persisted now — key age as
//! [`KeyBinding::bound_at`], and the vouch graph as [`View::vouches`]
//! (D65) — and this reads both. Scoped grants and bonds still do not
//! exist, and are still named rather than implied.
//!
//! There is deliberately no score. Two inputs are not more scoreable
//! than one: any weighting of age against vouches is a claim about how
//! much an endorsement is worth, which nothing here has measured, and a
//! number would read as that measurement while being a guess. The
//! reader gets the inputs.
//!
//! **Vouches are operator-scoped, and the profile says so.** They are
//! edges between operator identities, so a page about the channel
//! `ops/agent` shows what was vouched to `ops` — which is the truth, and
//! is why the operator is named in the section rather than left for a
//! reader to infer from a name that does not match the heading.
//!
//! [`KeyBinding::bound_at`]: choir_view::KeyBinding::bound_at
//! [`View::vouches`]: choir_view::View::vouches
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
//!     // Subject -> voucher, the direction the fold stores. `bob`
//!     // vouches for `alice`, and `alice` does not vouch back.
//!     "vouches": { "alice": { "bob": { "at": 55, "note": "shipped the parser" } } },
//! });
//! let profile = choir_node::profile::of(&view, "alice");
//! assert_eq!(profile["channel"], "alice");
//! assert_eq!(profile["changes"]["owned"], 1);
//! // 100 ops have been sequenced, 40 of them before this key existed.
//! assert_eq!(profile["keys"][0]["ops_since_binding"], 60);
//! assert_eq!(profile["vouches"]["operator"], "alice");
//! assert_eq!(profile["vouches"]["received"][0]["voucher"], "bob");
//! assert_eq!(profile["vouches"]["received"][0]["reciprocal"], false);
//! assert_eq!(profile["vouches"]["given"], 0);
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

    // D65. Operator-scoped, so an agent channel reads its operator's
    // graph: `ops/agent` and `ops` are one identity here, and a page
    // that showed nothing for the agent would be hiding the record
    // rather than reporting it.
    let operator = channel.split('/').next().unwrap_or(channel);
    let mut received: Vec<serde_json::Value> = Vec::new();
    if let Some(from) = view["vouches"][operator].as_object() {
        for (voucher, edge) in from {
            received.push(serde_json::json!({
                "voucher": voucher,
                "at": edge["at"].as_u64().unwrap_or_default(),
                "note": edge["note"],
                // The cheapest Sybil tell there is, and a fact rather
                // than a judgement: a ring of identities vouching for
                // each other is the shape a farm makes, and it looks
                // identical to a real team until you can see which
                // edges point both ways.
                "reciprocal": view["vouches"][voucher.as_str()][operator].is_object(),
            }));
        }
    }
    received.sort_by_key(|edge| edge["at"].as_u64().unwrap_or_default());
    let given = view["vouches"].as_object().map_or(0, |subjects| {
        subjects
            .values()
            .filter(|from| from[operator].is_object())
            .count()
    });

    // `received` earns a place in this disjunction because a binding
    // carrying no channel never reaches `keys`, so an operator can be
    // vouched for here and hold nothing above.
    let known = !keys.is_empty()
        || owned > 0
        || assigned > 0
        || reported > 0
        || commented > 0
        || !received.is_empty();

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
        // Always an object, never null, and `operator` is always
        // present: an empty `received` says "nobody vouches for this
        // operator on the records you may read", which is a different
        // sentence from "this node cannot record vouches" and the two
        // must not share a rendering.
        "vouches": {
            "operator": operator,
            "received": received,
            "given": given,
        },
    })
}
