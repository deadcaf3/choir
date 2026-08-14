//! Bounded aggregate responses: every list capped, paged, and every
//! omission marked in band.
//!
//! `/api/view` served six maps that grow with the log — `workspaces`,
//! `changes`, `refs`, `reviews`, `provenance`, `bindings` — and nothing
//! capped any of them. The node already *measured* them
//! (`view_growth.serialized_bytes`) and served them whole anyway, which
//! is the exact shape of a budget that exists as a metric and not as a
//! rule. An agent's context is the scarce resource here; a response that
//! grows without limit spends it on the operator's behalf.
//!
//! The contract, adopted wholesale: every list capped, every
//! truncation counted in band rather than silently, and the rest always
//! one named request away. [`Page::from_query`] reads `?limit=` and
//! `?offset=` and [`apply`] enforces them.
//!
//! # Where this runs, and why it matters
//!
//! **After ACL narrowing, never before.** `<section>_omitted` is a count
//! of rows, and a count of rows the caller may not read tells them those
//! rows exist — the disclosure D29 spends a whole module preventing. Cap
//! first and a reader granted one repository learns how many others the
//! node holds. So [`apply`] is the last step in
//! [`crate::handle_api`], after [`crate::acl::filter_response`], and
//! every count it emits describes only what that caller was already
//! allowed to see.
//!
//! # What is deliberately not bounded
//!
//! - **`view_growth`.** It measures the view, not the response. Capping
//!   the measurement to the page would make the growth tripwire
//!   unfalsifiable: the served document would look constant forever
//!   while the thing it describes grew without limit. A budget that
//!   silences its own alarm is worse than no budget.
//! - **The browser page.** `ui.rs` renders for a person and already has
//!   its own truncation rules; item 4 is about output an agent parses.
//!   Bounding it twice, by two different rules, would make the HTML and
//!   the JSON disagree for no reader's benefit.
//! - **`/api/log`.** Already paged, by `from` and `LOG_PAGE`, and its
//!   page is a chain segment rather than a slice of a map. SYNC.md
//!   depends on that shape.

/// Rows per section when the caller names no limit.
///
/// Chosen so the contract is real without being a migration: a node with
/// fewer than this many refs, reviews or workspaces per section is served
/// exactly what it was served before, and everything above it is now
/// reachable by paging rather than by a response that grows forever.
pub(crate) const DEFAULT_LIMIT: usize = 200;

/// The largest page any caller may ask for.
///
/// There is no escape hatch back to an unbounded response, on purpose.
/// `limit=0` and `limit=100000` both clamp here, so the response is
/// bounded by construction rather than by the caller's good manners, and
/// the way to read a large view is to page it.
pub(crate) const MAX_LIMIT: usize = 1000;

/// The map-shaped sections these two endpoints serve.
///
/// Listed rather than discovered: a section this build does not know
/// about is served whole, which is the same fail-safe direction
/// [`crate::acl::filter_response`] takes for disclosure, but pointed the
/// other way — an unknown section keeps working and is merely unbounded,
/// rather than silently truncated by a rule nobody wrote for it.
const SECTIONS: [&str; 7] = [
    "workspaces",
    "changes",
    "refs",
    "reviews",
    "provenance",
    "bindings",
    // `/api/reviews`.
    "pending",
];

/// One caller's requested window into every section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Page {
    /// Rows served per section, clamped to `1..=MAX_LIMIT`.
    pub limit: usize,
    /// Rows skipped per section, in key order.
    pub offset: usize,
}

impl Page {
    /// Reads `?limit=` and `?offset=` out of a request URL.
    ///
    /// An unparseable value takes the default rather than refusing. The
    /// alternative — a 400 on a malformed query — turns a client that
    /// guessed a parameter name into a client that cannot read the view
    /// at all, and the served page already says what window it is.
    pub fn from_query(url: &str) -> Self {
        let field = |name: &str| -> Option<usize> {
            let query = url.split_once('?')?.1;
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value.parse().ok())
        };
        Page {
            limit: field("limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            offset: field("offset").unwrap_or(0),
        }
    }
}

/// Caps every known section of `body` to `page`, marking what it dropped.
///
/// Applies to the two aggregate reads and nothing else, tested by the
/// same prefix [`crate::acl::filter_response`] uses — they narrow and
/// bound the same responses, and a rule that drifted from that one would
/// bound a document nobody narrowed. Returns `body` unchanged if it is
/// not a JSON object, which is how every refusal passes through: a
/// `Rejection` is small and already bounded, and rewriting one would only
/// risk mangling the error a caller needs to read.
///
/// Key order is [`serde_json::Map`]'s order, which is sorted because
/// `preserve_order` is off. That is load-bearing: paging is only stable
/// if two requests agree on what row 200 is, so enabling that feature
/// anywhere in the workspace would silently make pages overlap and skip.
/// [`paging_is_stable_across_requests`](self) is the test that notices.
pub(crate) fn apply(url: &str, body: &str) -> String {
    if !(url.starts_with("/api/view") || url.starts_with("/api/reviews")) {
        return body.to_string();
    }
    let page = Page::from_query(url);
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(object) = value.as_object_mut() else {
        return body.to_string();
    };

    let mut truncated = false;
    let mut marks: Vec<(String, usize)> = Vec::new();
    for section in SECTIONS {
        let Some(rows) = object.get_mut(section).and_then(serde_json::Value::as_object_mut) else {
            continue;
        };
        let total = rows.len();
        if page.offset == 0 && total <= page.limit {
            // Complete, and said so: an explicit zero is what lets a
            // client tell "nothing omitted" from "this build does not
            // mark omissions", which is the whole point of marking in
            // band rather than out of it.
            marks.push((format!("{section}_omitted"), 0));
            continue;
        }
        let kept: serde_json::Map<String, serde_json::Value> = std::mem::take(rows)
            .into_iter()
            .skip(page.offset)
            .take(page.limit)
            .collect();
        let served = kept.len();
        *rows = kept;
        if total > page.offset + served {
            truncated = true;
        }
        marks.push((format!("{section}_omitted"), total - served));
    }
    for (key, count) in marks {
        object.insert(key, serde_json::json!(count));
    }

    let path = url.split_once('?').map_or(url, |(path, _)| path);
    object.insert(
        "paging".to_string(),
        serde_json::json!({
            "format_version": 1,
            "limit": page.limit,
            "offset": page.offset,
            // The named request that fetches the rest, spelled out
            // rather than described, so a client pages by following a
            // string instead of reimplementing this arithmetic. Null
            // when every section is complete: an absent next page and a
            // next page that happens to be empty are different answers.
            "next": truncated.then(|| {
                format!("{path}?limit={}&offset={}", page.limit, page.offset + page.limit)
            }),
        }),
    );
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::{apply, Page, DEFAULT_LIMIT, MAX_LIMIT};

    /// A body with `count` rows in one section, keyed so that sort order
    /// and insertion order differ — otherwise a test of ordered paging
    /// passes on an unordered map by luck.
    fn body(count: usize) -> String {
        let rows: serde_json::Map<String, serde_json::Value> = (0..count)
            .map(|i| (format!("r{:04}", count - 1 - i), serde_json::json!(i)))
            .collect();
        serde_json::json!({ "refs": rows, "build": "x" }).to_string()
    }

    #[test]
    fn a_short_section_is_served_whole_and_marked_complete() {
        let out: serde_json::Value = serde_json::from_str(&apply("/api/view", &body(3))).unwrap();
        assert_eq!(out["refs"].as_object().unwrap().len(), 3);
        assert_eq!(out["refs_omitted"], 0);
        assert!(out["paging"]["next"].is_null(), "nothing was dropped");
    }

    /// The default is a cap, not a suggestion: a caller who passes
    /// nothing still gets a bounded document.
    #[test]
    fn the_default_bounds_a_caller_who_asked_for_nothing() {
        let out: serde_json::Value =
            serde_json::from_str(&apply("/api/view", &body(DEFAULT_LIMIT + 40))).unwrap();
        assert_eq!(out["refs"].as_object().unwrap().len(), DEFAULT_LIMIT);
        assert_eq!(out["refs_omitted"], 40);
        assert_eq!(
            out["paging"]["next"],
            serde_json::json!(format!("/api/view?limit={DEFAULT_LIMIT}&offset={DEFAULT_LIMIT}"))
        );
    }

    /// Paging must partition the section: every row exactly once, no
    /// overlap and no hole. A page with a hole in it is the failure
    /// `/api/log` refuses outright, and the same standard applies here.
    #[test]
    fn paging_is_stable_across_requests() {
        let source = body(25);
        let mut seen: Vec<String> = Vec::new();
        for offset in [0, 10, 20] {
            let out: serde_json::Value =
                serde_json::from_str(&apply(&format!("/api/view?limit=10&offset={offset}"), &source))
                    .unwrap();
            seen.extend(out["refs"].as_object().unwrap().keys().cloned());
        }
        let mut expected: Vec<String> = (0..25).map(|i| format!("r{i:04}")).collect();
        expected.sort();
        assert_eq!(seen, expected, "pages overlapped, skipped, or reordered");
    }

    /// Past the end is an empty page, not a refusal and not a wrap.
    #[test]
    fn an_offset_past_the_end_serves_nothing_and_stops() {
        let out: serde_json::Value =
            serde_json::from_str(&apply("/api/view?limit=10&offset=900", &body(25))).unwrap();
        assert_eq!(out["refs"].as_object().unwrap().len(), 0);
        assert_eq!(out["refs_omitted"], 25);
        assert!(out["paging"]["next"].is_null(), "there is no next page");
    }

    /// No query can ask for an unbounded response.
    #[test]
    fn a_caller_cannot_opt_out_of_the_budget() {
        for query in ["limit=0", "limit=999999", "limit=-1", "limit=banana"] {
            let page = Page::from_query(&format!("/api/view?{query}"));
            assert!(
                (1..=MAX_LIMIT).contains(&page.limit),
                "{query} escaped the clamp as {}",
                page.limit
            );
        }
    }

    /// Refusals are small already and must survive untouched: a client
    /// reading `code` and `next` out of a rejection gets the node's own
    /// words, not this module's rewrite of them.
    #[test]
    fn a_non_object_body_passes_through_unchanged() {
        assert_eq!(apply("/api/view", "not json"), "not json");
        assert_eq!(apply("/api/view", "[1,2]"), "[1,2]");
    }

    /// Only the sections this module names are touched. `build` and
    /// `view_growth` are objects too, and capping them would turn a
    /// measurement into a page of itself.
    #[test]
    fn an_unlisted_object_section_is_left_alone() {
        let source = serde_json::json!({
            "view_growth": { "serialized_bytes": { "refs": 10, "reviews": 20 } },
        })
        .to_string();
        let out: serde_json::Value = serde_json::from_str(&apply("/api/view?limit=1", &source)).unwrap();
        assert_eq!(out["view_growth"]["serialized_bytes"].as_object().unwrap().len(), 2);
        assert!(out.get("view_growth_omitted").is_none());
    }
}
