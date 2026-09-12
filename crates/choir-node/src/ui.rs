//! The browser surface: one server-rendered page, pre-built and cached.
//!
//! # Why HTML and not a client app
//!
//! The node already holds the whole materialized view in memory and
//! already knows how to describe it: `GET /api/view` returns exactly
//! that description as JSON. A single-page app would boot a runtime,
//! ask for that JSON, and only then paint — three serial steps whose
//! best case is slower than sending bytes that are already correct.
//! So this module renders the *same JSON the API serves* into HTML.
//!
//! Rendering from the API's own payload rather than from [`View`]
//! directly is a deliberate anti-drift choice: there is one source of
//! truth for what the node knows, and a field that appears in the API
//! can be shown here without a second traversal getting written, and
//! getting written differently. The cost is one `serde_json` round
//! trip per *render*, which the cache below makes rare.
//!
//! [`View`]: choir_view::View
//!
//! # Why it does not lag
//!
//! Three properties, in the order they matter:
//!
//! 1. **The page is rendered on state change, not per request.**
//!    [`UiCache`] keys a finished `String` by the view's `next_seq`
//!    and, under a D29 ACL, by which reader asked. Between two ops,
//!    every request from readers who see the same thing serves the
//!    same prebuilt buffer, and the work per request is a clone of an
//!    `Arc` plus a socket write. Without an ACL there is one reader
//!    key, so the map holds exactly the single slot it used to be.
//! 2. **Unchanged state costs zero bytes.** The cache key is also the
//!    `ETag`, so a browser that already has the page gets `304` with
//!    an empty body. A poll loop on an idle node transfers nothing.
//! 3. **Nothing external is fetched.** No CDN, no web font, no
//!    analytics, no framework. One request paints the page, which is
//!    also why it works over a slow link and inside a private network
//!    that cannot reach the internet at all.
//!
//! The lock discipline matters as much as the caching: the view mutex
//! is held to read a `u64` and, on a miss, to build the JSON. It is
//! never held across a socket write, so a slow or malicious client
//! cannot stall the sequencer by reading the page slowly.
//!
//! # Robustness
//!
//! Every value that reaches the page goes through [`esc`], including
//! ref names, review ids, verdict notes and operator names — all of
//! which are attacker-influenced (anyone who can push a branch names
//! a ref). The renderer treats missing and malformed JSON as normal:
//! there are no `unwrap`s on shape, every lookup falls back to a
//! visible placeholder, and an unexpected payload yields a page that
//! says less rather than a panic that serves nothing.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Handle to readable name, for the accounts a store can still name.
///
/// The D46 rendering half. Channels in the log are opaque handles, so
/// every surface a person reads resolves through one of these or shows
/// the handle.
pub(crate) type Roster = BTreeMap<String, String>;

/// A channel with every segment the roster can name replaced by the
/// name (D46).
///
/// Channels are `/`-joined and the principal is one segment of them —
/// `git/<handle>` for a push, `<handle>/reviewer` for a review seat — so
/// resolving per segment covers both shapes and any later one without
/// this function knowing which is which. The segments around it carry
/// meaning (`git/` is a provenance label only the node may apply) and
/// are kept.
///
/// **A segment the roster cannot name is returned unchanged, and that is
/// the point.** It is the expected state for three situations a reader
/// must not be able to tell apart: an account issued before D46, one
/// issued with an explicit `user`, and one whose name has been deleted.
/// Rendering "unknown" or "deleted" for the third would undo the
/// deletion by announcing it.
pub(crate) fn person(channel: &str, roster: &Roster) -> String {
    if roster.is_empty() {
        return channel.to_string();
    }
    channel
        .split('/')
        .map(|segment| match roster.get(segment) {
            Some(name) => name.as_str(),
            None => segment,
        })
        .collect::<Vec<&str>>()
        .join("/")
}

/// The rendered pages for one view sequence, one per distinct reader.
///
/// The sequence doubles as the `ETag`, so "is the cache valid" and "is
/// the client's copy valid" are the same question asked twice. Under a
/// D29 ACL two readers are shown different pages, so the `ETag` carries
/// the reader key too and the map holds an entry per key.
///
/// Advancing the sequence clears the map rather than growing it: every
/// page it held describes a state the node has left, so the bound on
/// what this can hold is the number of distinct readers between two ops.
pub(crate) struct UiCache {
    inner: Mutex<(PageState, Pages)>,
}

/// What the pages on hand were built from: the view sequence, and the
/// accounts-store generation the names on them were resolved at. Either
/// moving invalidates every page, which is why they travel as one value
/// rather than as two fields that could be compared separately.
type PageState = (u64, u64);

/// The finished pages for one [`PageState`], keyed by reader.
type Pages = HashMap<String, Arc<String>>;

impl UiCache {
    /// A cache holding nothing, which is the state after every restart.
    pub(crate) fn new() -> Self {
        UiCache {
            inner: Mutex::new(((0, 0), HashMap::new())),
        }
    }

    /// The page for view sequence `seq` as `reader` may see it,
    /// rendering it only if the copy on hand describes some other
    /// sequence, was built for a reader who sees something else, or
    /// resolved names against an accounts store that has since changed.
    ///
    /// `build_json` is called only on a miss. It is a closure rather
    /// than a value so a hit never pays for the JSON it would not use.
    ///
    /// **`generation` is not an optimization.** Names on this page are
    /// resolved through the accounts store (D46), which changes without
    /// the view sequence moving — revoking an account deletes a name and
    /// appends no op. Keyed on `seq` alone, a cached page would keep
    /// showing a name after the row carrying it was deleted, which is
    /// the one thing the deletion was for. Pass
    /// [`crate::accounts::Accounts::generation`], or `0` on a node with
    /// no store, where the roster is empty and nothing resolves anyway.
    pub(crate) fn page<F>(
        &self,
        seq: u64,
        generation: u64,
        reader: &str,
        roster: &Roster,
        chrome: crate::browse::Chrome<'_>,
        build_json: F,
    ) -> Arc<String>
    where
        F: FnOnce() -> String,
    {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (cached_at, pages) = &mut *slot;
        if *cached_at != (seq, generation) {
            pages.clear();
            *cached_at = (seq, generation);
        }
        // The palette is part of what a cached page *is*, so it is part
        // of the key. Keyed on the reader alone, the first visitor's
        // choice would be served to the next one — the page is byte-for
        // -byte different, and the difference is the whole document's
        // colour.
        let key = format!("{reader}\u{1f}{}", chrome.theme.unwrap_or("auto"));
        if let Some(page) = pages.get(&key) {
            return Arc::clone(page);
        }
        let page = Arc::new(render(&build_json(), seq, roster, chrome));
        pages.insert(key, Arc::clone(&page));
        page
    }
}

/// The `ETag` for a view sequence as one reader sees it.
///
/// Weak (`W/`) because two renders of the same sequence are equivalent
/// for a reader's purposes without being guaranteed byte-identical.
///
/// The reader key is hashed into the tag rather than appended, so an
/// `ETag` never carries a username back to whatever logs it, and it is
/// omitted entirely when no ACL is configured — that node serves one
/// page to everybody, and its tags stay the bare sequence numbers they
/// have always been.
///
/// `generation` joins it for the reason [`UiCache::page`] gives: a name
/// deleted from the accounts store changes the page without changing the
/// sequence, and a browser holding the previous copy has to be told.
///
/// So does the build stamp, for the reason [`crate::browse`]'s own tag
/// gives at length: this page is *this daemon's rendering* of a
/// sequence, so a daemon that renders it differently is serving a
/// different page even though the sequence has not moved. Without the
/// stamp, a node whose log is quiet — which is most nodes most of the
/// time — answers `304` to every reader who visited before the upgrade,
/// and a change to this surface ships to nobody. That was not
/// hypothetical: it is how a restyled page kept rendering as the old
/// one until a cache-bypassing reload, which is not a thing a reader
/// knows to do.
///
/// The tags are therefore no longer bare sequence numbers on an
/// ACL-less node. What survives from that shape is the part that was
/// load-bearing: the reader key is still hashed rather than appended,
/// so an `ETag` never carries a username back to whatever logs it.
pub(crate) fn etag(seq: u64, generation: u64, reader: &str, theme: Option<&str>) -> String {
    let state = if generation == 0 {
        seq.to_string()
    } else {
        format!("{seq}.{generation}")
    };
    // The palette joins the build for the same reason the build joined
    // the sequence: it changes the document, so a tag that ignores it
    // hands a reader who just switched their own cached page back.
    let build = short_digest(&format!(
        "{}\u{1f}{}",
        crate::BUILD_COMMIT,
        theme.unwrap_or("auto")
    ));
    if reader.is_empty() {
        return format!("W/\"{state}-{build}\"");
    }
    format!("W/\"{state}-{build}-{}\"", short_digest(reader))
}

/// A short, stable, non-reversing tag for a reader key.
///
/// FNV-1a rather than anything from `choir-hash`: this decides cache
/// identity for one process's lifetime, never signs or addresses
/// anything, and a collision costs a reader a page rebuilt too often —
/// never a page built for somebody else, because the map is keyed on the
/// full string and only the `ETag` is abbreviated.
fn short_digest(reader: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in reader.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// HTML-escapes text for element and attribute context alike.
///
/// Quotes are escaped too, so one function is safe in both places and
/// no call site has to remember which kind of context it is in.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Reads a string field, or a placeholder when it is absent or is not
/// a string. Absent data is a normal state here, not an error.
fn s<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("—")
}

/// Shortens a content hash for display while keeping its codec prefix,
/// which is the part that says what kind of hash it is.
///
/// Counted and cut in `char`s, not bytes. A digest reaching here is hex
/// in every payload the API builds, but this function is handed whatever
/// the JSON held, and byte-slicing a multi-byte character in half panics
/// — which on this path means the browser surface taking the node down
/// over a display detail. The module's promise is a page that says less,
/// never a panic that serves nothing.
fn short(id: &str) -> String {
    match id.split_once('-') {
        Some((codec, digest)) if digest.chars().count() > 12 => {
            let head: String = digest.chars().take(12).collect();
            format!("{codec}-{head}")
        }
        // A bare digest — a git commit id, which carries no codec
        // prefix. Found by rendering a real view rather than a fixture:
        // invented data all had prefixes, so this branch did not exist
        // and the node's build commit printed at full length.
        None if id.len() > 12 && id.chars().all(|c| c.is_ascii_hexdigit()) => id[..12].to_string(),
        _ => id.to_string(),
    }
}

/// Splits `owner/repo.git:refs/heads/main` into repository and ref.
fn split_ref(full: &str) -> (&str, &str) {
    full.split_once(':').unwrap_or(("—", full))
}

/// Renders the whole page for one view payload.
///
/// `seq` is displayed rather than derived from `json` so the number on
/// the page is the same number in the `ETag`; a reader comparing two
/// nodes is comparing the thing the cache keyed on.
fn render(json: &str, seq: u64, roster: &Roster, chrome: crate::browse::Chrome<'_>) -> String {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let mut h = String::with_capacity(16 * 1024);

    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir</title>");
    h.push_str(STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    // The same fixed bar the browse pages carry. This page is about the
    // node rather than a repository, so its box filters the repository
    // list — which is the only thing a reader on this page could be
    // looking for that is not already on it.
    crate::browse::chrome(&mut h, crate::browse::Bar::index(chrome));

    header(&mut h, &v, seq);
    refs_section(&mut h, &v);
    reviews_section(&mut h, &v, roster);
    attestation_section(&mut h, &v);
    workspaces_section(&mut h, &v, roster);
    health_section(&mut h, &v);

    h.push_str("</main><footer>Served pre-rendered at view seq ");
    h.push_str(&seq.to_string());
    h.push_str(". This page is a read-only projection; every write goes through the signed-op API.</footer>");
    h.push_str("</body></html>");
    h
}

/// Node identity, provenance and the gates that are on.
fn header(h: &mut String, v: &serde_json::Value, seq: u64) {
    let log = v.get("log").cloned().unwrap_or(serde_json::Value::Null);
    let build = v.get("build").cloned().unwrap_or(serde_json::Value::Null);
    let dirty = build
        .get("dirty")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    h.push_str("<header class=\"top\"><h1>choir</h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\">seq ");
    h.push_str(&seq.to_string());
    h.push_str("</span>");
    h.push_str("<span class=\"pill\">build ");
    h.push_str(&esc(&short(s(&build, "commit"))));
    if dirty {
        h.push_str(" <b class=\"tag warn\">dirty</b>");
    }
    h.push_str("</span>");
    if log
        .get("scope_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        h.push_str("<span class=\"pill on\">scope gate</span>");
    }
    h.push_str("<span class=\"pill\">node ");
    h.push_str(&esc(&short(s(&log, "node"))));
    h.push_str("</span>");
    // D80. A seed says whose copy this is, in the header, because a
    // reader who does not know they are on a copy reads a stale page as
    // the truth.
    if let Some(replica) = v.get("replica").filter(|r| r.is_object()) {
        let at = |key: &str| replica.get(key).and_then(serde_json::Value::as_u64);
        let behind = match (at("home_head_seq"), at("head_seq")) {
            (Some(home), Some(here)) => home.saturating_sub(here),
            (Some(home), None) => home + 1,
            (None, _) => 0,
        };
        h.push_str("<span class=\"pill\">seed of ");
        h.push_str(&esc(s(replica, "home")));
        h.push_str(", ");
        h.push_str(&behind.to_string());
        h.push_str(" behind");
        if replica
            .get("halted")
            .is_some_and(serde_json::Value::is_object)
        {
            h.push_str(" <b class=\"tag warn\">halted</b>");
        }
        if replica.get("gap").and_then(serde_json::Value::as_bool) == Some(true) {
            h.push_str(" <b class=\"tag warn\">gap</b>");
        }
        h.push_str("</span>");
    }
    // The way out. `/r/` links back here and this did not link there, so
    // a reader who opened the node's front door could see everything it
    // *knows* and never find the code — which is the thing they came for.
    h.push_str("<span class=\"pill\"><a href=\"/\">repositories</a></span>");
    h.push_str("</div></header><main id=\"main\">");
}

/// Refs, grouped by repository, because that is how a reader thinks
/// about them even though the view keys them flat.
fn refs_section(h: &mut String, v: &serde_json::Value) {
    let refs = match v.get("refs").and_then(serde_json::Value::as_object) {
        Some(m) => m,
        None => return,
    };
    h.push_str("<section><h2>Refs <span class=\"count\">");
    h.push_str(&refs.len().to_string());
    h.push_str("</span></h2>");
    if refs.is_empty() {
        h.push_str("<p class=\"empty\">No refs yet. This node is holding repositories that nobody has pushed to.</p>");
        next_action(
            h,
            "Push a branch — <code>git push &lt;clone-url&gt; HEAD:main</code> — or submit a \
             signed <code>SetRef</code>. Either way it appears here at the next sequence.",
        );
        h.push_str("</section>");
        return;
    }
    // Open reviews, counted against the ref they target, so the status
    // rides the row a reader is already looking at instead of waiting on
    // a section they have to remember to scroll to. Same payload as the
    // reviews section below — a chip here can never disagree with it.
    let mut pending: HashMap<&str, usize> = HashMap::new();
    if let Some(reviews) = v.get("reviews").and_then(serde_json::Value::as_object) {
        for r in reviews.values() {
            let complete = r
                .get("complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let archived = r
                .get("archived")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if complete || archived {
                continue;
            }
            if let Some(target) = r.get("target_ref").and_then(serde_json::Value::as_str) {
                *pending.entry(target).or_default() += 1;
            }
        }
    }
    let mut current = "";
    h.push_str("<table><tbody>");
    for (full, target) in refs {
        let (repo, name) = split_ref(full);
        if repo != current {
            // The view keys refs by the on-disk name, which ends in
            // `.git`; `/r/` answers to the name without it. Showing the
            // key meant the node displayed a name that 404s on its own
            // browse surface, so this shows the one a reader can use and
            // links it. The `.git` name is not lost — it is on the
            // repository page, as the path you actually clone.
            h.push_str("<tr class=\"group\"><th colspan=\"2\">");
            let stem = repo.strip_suffix(".git").unwrap_or(repo);
            h.push_str("<a href=\"/r/");
            h.push_str(&esc(stem));
            h.push_str("\">");
            h.push_str(&esc(stem));
            h.push_str("</a>");
            h.push_str("</th></tr>");
            current = repo;
        }
        h.push_str("<tr><td>");
        h.push_str(&esc(name));
        if let Some(open) = pending.get(full.as_str()) {
            h.push_str(" <b class=\"tag pending\">");
            h.push_str(&open.to_string());
            h.push_str(" pending</b>");
        }
        h.push_str("</td><td class=\"mono\">");
        h.push_str(&esc(&short(target.as_str().unwrap_or("—"))));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table></section>");
}

/// Reviews, incomplete ones first: the queue is the part a human is
/// here to act on, and a completed review is history.
fn reviews_section(h: &mut String, v: &serde_json::Value, roster: &Roster) {
    let reviews = match v.get("reviews").and_then(serde_json::Value::as_object) {
        Some(m) => m,
        None => return,
    };
    let mut open: Vec<(&String, &serde_json::Value)> = Vec::new();
    let mut done: Vec<(&String, &serde_json::Value)> = Vec::new();
    for (id, r) in reviews {
        let complete = r
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if complete {
            done.push((id, r));
        } else {
            open.push((id, r));
        }
    }

    h.push_str("<section><h2>Reviews <span class=\"count\">");
    h.push_str(&open.len().to_string());
    h.push_str(" open / ");
    h.push_str(&reviews.len().to_string());
    h.push_str(" total</span></h2>");

    if reviews.is_empty() {
        h.push_str("<p class=\"empty\">No reviews recorded. Nobody has asked for one yet.</p>");
        next_action(
            h,
            "Request one with <code>choir review &lt;api&gt; &lt;key-file&gt; &lt;channel&gt; \
             &lt;id&gt; &lt;git-oid&gt;</code>. Name no reviewers and this node draws them.",
        );
        h.push_str("</section>");
        return;
    }

    // The queue a human is here to act on, in full.
    if open.is_empty() {
        h.push_str(
            "<p class=\"empty\">Nothing awaiting a verdict. Every review here has been answered.</p>",
        );
    } else {
        review_table(h, &open, roster);
    }

    // Everything settled, folded away. `<details>` because the browser
    // already implements disclosure: no script, no state to get wrong,
    // and the page stays short as review history grows without bound.
    if !done.is_empty() {
        h.push_str("<details><summary>");
        h.push_str(&done.len().to_string());
        h.push_str(" completed</summary>");
        review_table(h, &done, roster);
        h.push_str("</details>");
    }
    h.push_str("</section>");
}

/// One table of reviews, newest-looking first is not attempted: the
/// view keys reviews by id, and inventing an order the log does not
/// carry would be a display that lies about sequence.
fn review_table(h: &mut String, rows: &[(&String, &serde_json::Value)], roster: &Roster) {
    h.push_str("<table><thead><tr><th>Review</th><th>Target</th><th>Weight</th><th>Verdicts</th></tr></thead><tbody>");
    for (id, r) in rows {
        let archived = r
            .get("archived")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let approved = r
            .get("approved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let complete = r
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let weight = r
            .get("approval_weight")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let re_review = r
            .get("re_review_required")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        h.push_str("<tr");
        if !complete {
            h.push_str(" class=\"open\"");
        }
        h.push_str("><td><span class=\"id\">");
        // A review whose target ref names a repository links through to
        // its D34 page, where the diff and the verdict notes live. One
        // that names none has no repository to be shown under, so it
        // stays plain text rather than linking somewhere that would have
        // to guess.
        match r
            .get("target_ref")
            .and_then(serde_json::Value::as_str)
            .and_then(|target| target.split_once(':'))
            .map(|(repo, _)| repo.strip_suffix(".git").unwrap_or(repo))
        {
            Some(repo) => {
                h.push_str("<a href=\"/r/");
                h.push_str(&esc(repo));
                h.push_str("/review/");
                h.push_str(&esc(id));
                h.push_str("\">");
                h.push_str(&esc(id));
                h.push_str("</a>");
            }
            None => h.push_str(&esc(id)),
        }
        h.push_str("</span>");
        if approved {
            h.push_str(" <b class=\"tag ok\">approved</b>");
        } else if complete {
            h.push_str(" <b class=\"tag warn\">changes</b>");
        } else {
            h.push_str(" <b class=\"tag pending\">pending</b>");
        }
        if re_review {
            h.push_str(" <b class=\"tag warn\">re-review</b>");
        }
        if archived {
            h.push_str(" <span class=\"muted\">archived</span>");
        }
        h.push_str("</td><td class=\"mono\">");
        let (_, target_ref) = split_ref(s(r, "target_ref"));
        h.push_str(&esc(target_ref));
        h.push_str("<br><span class=\"muted\">");
        h.push_str(&esc(&short(s(r, "target"))));
        h.push_str("</span></td><td class=\"num\">");
        h.push_str(&weight.to_string());
        h.push_str("</td><td>");
        verdicts(h, r, roster);
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
}

/// One reviewer's answer per line, with the note when there is one.
///
/// `pub(crate)` because the D34 review list folds the same rendering
/// into each row's `<details>`: one function is how "what a verdict
/// looks like" stays one answer across both surfaces.
pub(crate) fn verdicts(h: &mut String, r: &serde_json::Value, roster: &Roster) {
    let assigned = r
        .get("reviewers")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let verdicts = r.get("verdicts").and_then(serde_json::Value::as_object);
    if assigned.is_empty() && verdicts.map(serde_json::Map::is_empty).unwrap_or(true) {
        h.push_str("<span class=\"muted\">unassigned</span>");
        return;
    }
    for who in assigned {
        let name = who.as_str().unwrap_or("—");
        let answer = verdicts.and_then(|m| m.get(name));
        h.push_str("<div class=\"v\">");
        h.push_str(&esc(&person(name, roster)));
        match answer {
            Some(a) => {
                let verdict = s(a, "verdict");
                let class = if verdict == "Approve" { "ok" } else { "warn" };
                h.push_str(" <b class=\"tag ");
                h.push_str(class);
                h.push_str("\">");
                h.push_str(&esc(verdict));
                h.push_str("</b>");
                let note = s(a, "note");
                if !note.is_empty() && note != "—" {
                    h.push_str(" <span class=\"muted\">");
                    h.push_str(&esc(note));
                    h.push_str("</span>");
                }
            }
            None => h.push_str(" <span class=\"muted\">waiting</span>"),
        }
        h.push_str("</div>");
    }
}

/// The D25 ref-state attestation, with its chain pointer.
fn attestation_section(h: &mut String, v: &serde_json::Value) {
    h.push_str("<section><h2>Attestation</h2>");
    match v.get("snapshot") {
        Some(snap) if !snap.is_null() => {
            h.push_str("<table class=\"kv\"><tbody><tr><td>at seq</td><td class=\"mono\">");
            h.push_str(
                &snap
                    .get("at_seq")
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".to_string()),
            );
            h.push_str("</td></tr><tr><td>id</td><td class=\"mono\">");
            h.push_str(&esc(&short(s(snap, "id"))));
            h.push_str("</td></tr><tr><td>chains from</td><td class=\"mono\">");
            let prev = snap
                .get("prev_snapshot")
                .and_then(serde_json::Value::as_str);
            h.push_str(&esc(&prev
                .map(short)
                .unwrap_or_else(|| "genesis".to_string())));
            h.push_str("</td></tr></tbody></table>");
            h.push_str("<p class=\"note\">Compare <code>id</code> at the same <code>at_seq</code> with another reader to check you were shown the same ref state. It is the node's own signature, so this is a comparison primitive, not proof of non-equivocation.</p>");
        }
        _ => {
            h.push_str(
                "<p class=\"empty\">No attestation yet. This node signs one after a ref moves, \
                 and no ref has moved since it started.</p>",
            );
            next_action(
                h,
                "Push anything, or wait for somebody else to. Nothing here needs fixing — an \
                 attestation is a consequence of a ref update, not a thing to switch on.",
            );
        }
    }
    h.push_str("</section>");
}

/// Workspaces and their heads.
fn workspaces_section(h: &mut String, v: &serde_json::Value, roster: &Roster) {
    let ws = match v.get("workspaces").and_then(serde_json::Value::as_object) {
        Some(m) => m,
        None => return,
    };
    h.push_str("<section><h2>Workspaces <span class=\"count\">");
    h.push_str(&ws.len().to_string());
    h.push_str("</span></h2>");
    if ws.is_empty() {
        h.push_str(
            "<p class=\"empty\">None provisioned. A workspace is an agent's copy-on-write \
             checkout, so an idle node has none.</p>",
        );
        next_action(
            h,
            "Provision one with <code>choir workspace &lt;api&gt; &lt;owner/repo&gt; \
             &lt;name&gt;</code>, or leave this empty — nothing else on this page depends on it.",
        );
        h.push_str("</section>");
        return;
    }
    h.push_str("<table><tbody>");
    for (name, head) in ws {
        h.push_str("<tr><td>");
        h.push_str(&esc(&person(name, roster)));
        h.push_str("</td><td class=\"mono\">");
        h.push_str(&esc(&short(head.as_str().unwrap_or("—"))));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table></section>");
}

/// Sequencer health and the measured tripwires, stated as what they
/// are: instrumentation, mostly with thresholds still unset.
///
/// The lag figures are nested under `durable`/`decision` and carried in
/// microseconds; both facts were learned by rendering a real view,
/// after a first version read flat millisecond keys that do not exist
/// and displayed a column of placeholders. `the_health_section_reads_
/// the_shape_the_api_actually_sends` is the guard against that
/// returning.
fn health_section(h: &mut String, v: &serde_json::Value) {
    h.push_str("<section><h2>Health</h2><table class=\"kv\"><tbody>");

    if let Some(lag) = v.get("sequencer_lag").filter(|l| !l.is_null()) {
        let gate = lag.get("gate_us").and_then(serde_json::Value::as_u64);
        for (label, key) in [("durable p99", "durable"), ("decision p99", "decision")] {
            let p99 = lag
                .get(key)
                .and_then(|d| d.get("p99_us"))
                .and_then(serde_json::Value::as_u64);
            let mut text = match p99 {
                Some(us) => millis(us),
                None => "—".to_string(),
            };
            if let (Some(_), Some(gate)) = (p99, gate) {
                text.push_str(" of ");
                text.push_str(&millis(gate));
                text.push_str(" gate");
            }
            row(h, label, &text);
        }
        let breaches = lag
            .get("durable")
            .and_then(|d| d.get("breaches"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if breaches > 0 {
            h.push_str("<tr><td>gate breaches</td><td><b class=\"tag danger\">");
            h.push_str(&breaches.to_string());
            h.push_str("</b></td></tr>");
        }
    }

    if let Some(bytes) = v
        .get("view_growth")
        .and_then(|g| g.get("serialized_bytes"))
        .and_then(|b| b.get("total_authoritative_view"))
        .and_then(serde_json::Value::as_u64)
    {
        row(h, "authoritative view", &format!("{bytes} bytes"));
    }

    let mut indeterminate = false;
    for (label, key) in [
        ("concentration (D24 T3)", "concentration"),
        ("newcomer harm (D24 T4)", "newcomer_harm"),
        ("new-actor reviews (D24 T2)", "new_actor_review_outcomes"),
    ] {
        if let Some(m) = v.get(key).filter(|m| !m.is_null()) {
            let status = s(m, "tripwire_status");
            indeterminate |= status == "indeterminate";
            let class = match status {
                "pass" => "ok",
                "observed" => "warn",
                _ => "muted",
            };
            h.push_str("<tr><td>");
            h.push_str(&esc(label));
            h.push_str("</td><td><b class=\"tag ");
            h.push_str(class);
            h.push_str("\">");
            h.push_str(&esc(status));
            h.push_str("</b></td></tr>");
        }
    }
    h.push_str("</tbody></table>");
    // Three grey badges reading `indeterminate` and nothing saying what
    // that is. A reader assumes it means "not enough data yet", and for
    // one of the three that is true; for the other two the bound itself
    // is unset, so waiting changes nothing. Both readings are wrong
    // without this, and the difference decides whether there is anything
    // for the operator to do.
    if indeterminate {
        h.push_str(
            "<p class=\"note\"><code>indeterminate</code> is neither a pass nor a breach: \
             the node could not complete the evaluation, and each tripwire reports what it \
             was missing as <code>evaluation_complete</code> in <code>/api/view</code>. \
             Concentration completes once every trusted key has a sequenced binding — that \
             one is waiting on this node. The other two are waiting on bounds D24 records \
             as unset pending a first real measurement, so they stay here however long the \
             node runs.</p>",
        );
    }
    h.push_str("</section>");
}

/// Microseconds as milliseconds, one decimal. The API reports the unit
/// the sequencer measures in; a page for humans should not make its
/// reader divide by a thousand to know whether a number is alarming.
fn millis(us: u64) -> String {
    format!("{}.{} ms", us / 1000, (us % 1000) / 100)
}

/// One label/value row.
fn row(h: &mut String, label: &str, value: &str) {
    h.push_str("<tr><td>");
    h.push_str(&esc(label));
    h.push_str("</td><td class=\"mono\">");
    h.push_str(&esc(value));
    h.push_str("</td></tr>");
}

/// The one next action, as the note-level admonition.
///
/// The markup is the `.callout` shape — an `.ico` label
/// column beside the prose — rather than a choir-local invention, for the
/// same reason the tokens are named once rather than re-picked.
///
/// `html` is markup on purpose, because these sentences carry `<code>`
/// around the command a reader is meant to run. **Nothing
/// attacker-influenced may reach it**, which is what [`esc`] exists for.
///
/// That rule is the `&'static str` and not the comment. Request data on
/// this surface arrives as a `String` or borrowed from a buffer with a
/// request lifetime, so it cannot be passed here at all — the compiler
/// refuses it, and a caller with text rather than markup is pushed to
/// [`next_action_text`], which escapes. This was a comment enforcing an
/// invariant, the weakest kind of guard, and no test could fail when a
/// future caller walked around the escaper.
pub(crate) fn next_action(h: &mut String, html: &'static str) {
    next_action_escaped(h, html);
}

/// The same admonition, for a next action assembled from data.
///
/// Escapes, because the only reason to reach for this rather than
/// [`next_action`] is that the sentence is not a literal.
pub(crate) fn next_action_text(h: &mut String, text: &str) {
    next_action_escaped(h, &esc(text));
}

/// The shared body, taking markup that is already safe by construction.
///
/// `pub(crate)` for the sentence that mixes a literal with operator data:
/// the landing page's is part fixed prose, part a link built from the
/// contact line, so neither [`next_action`] (literals only) nor
/// [`next_action_text`] (escapes the markup away) fits.
///
/// **The caller owes the escaping.** Every dynamic fragment reaching this
/// must have gone through [`esc`] first; the name is the reminder, and
/// it is the only thing standing between operator input and the one page
/// that answers anybody.
pub(crate) fn next_action_escaped(h: &mut String, html: &str) {
    h.push_str("<div class=\"callout callout-note\"><span class=\"ico\">next</span><p>");
    h.push_str(html);
    h.push_str("</p></div>");
}

/// A refusal, in the shape [`reject::Rejection`] already settled on.
///
/// That record is what the node tells an agent when it says no: a stable
/// `code`, the human sentence, the two states a failed comparison was
/// between, and a required `next` — the field its doc calls "the one the
/// research isolates the gain to". A person reading HTML needs exactly
/// those four things, so this is that record rather than a second
/// vocabulary invented for the browser. Keeping one shape is also how a
/// reader and the agent they are debugging with can compare notes.
///
/// [`reject::Rejection`]: crate::reject::Rejection
pub(crate) struct Refusal<'a> {
    /// The stable machine-readable reason, shown so a reader can quote it
    /// to an operator without paraphrasing it into something else.
    pub(crate) code: &'a str,
    /// What happened, in one sentence, in words that are about the
    /// reader's situation rather than about the node's internals.
    pub(crate) error: &'a str,
    /// What the node required — omitted when nothing was compared.
    pub(crate) expected: Option<&'a str>,
    /// What it found instead.
    pub(crate) actual: Option<&'a str>,
    /// The single action that changes the situation, in the imperative.
    pub(crate) next: &'a str,
}

/// Renders a refusal as a whole page.
///
/// `nav` is where a reader who is now lost may actually go, as links —
/// never back to the thing that just refused them, which is how a
/// friendly error page becomes a loop. It is a slice rather than a fixed
/// pair because the honest destinations differ: a reader refused a
/// repository should be sent to the list of ones they can read, and a
/// reader refused the node page should not be sent to the node page.
pub(crate) fn refusal(
    headline: &str,
    status: u16,
    r: &Refusal,
    nav: &[(&str, &str)],
    chrome: crate::browse::Chrome<'_>,
) -> String {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: ");
    h.push_str(&esc(headline));
    h.push_str("</title>");
    h.push_str(STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    // Even here. A refusal is where a reader is most lost, and the box
    // is scoped to the repository list, which is already filtered to
    // this reader's grants — so it can restate nothing the refusal
    // itself withheld.
    crate::browse::chrome(&mut h, crate::browse::Bar::index(chrome));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(headline));
    h.push_str("</h1><div class=\"sub\"><span class=\"pill\">");
    h.push_str(&status.to_string());
    h.push_str("</span><span class=\"pill\">");
    h.push_str(&esc(r.code));
    h.push_str("</span>");
    for (href, label) in nav {
        h.push_str("<span class=\"pill\"><a href=\"");
        h.push_str(&esc(href));
        h.push_str("\">");
        h.push_str(&esc(label));
        h.push_str("</a></span>");
    }
    h.push_str("</div></header><main id=\"main\"><section>");
    h.push_str("<p class=\"lede\">");
    h.push_str(&esc(r.error));
    h.push_str("</p>");
    if r.expected.is_some() || r.actual.is_some() {
        h.push_str("<table><tbody>");
        for (label, value) in [("expected", r.expected), ("found", r.actual)] {
            if let Some(value) = value {
                row(&mut h, label, value);
            }
        }
        h.push_str("</tbody></table>");
    }
    next_action_text(&mut h, r.next);
    h.push_str("</section></main><footer>");
    h.push_str("Read-only. Every write goes through the signed-op API.");
    h.push_str("</footer></body></html>");
    h
}

/// The whole stylesheet, inline.
///
/// Inline because an external file is a second request before first
/// paint, and this page's entire premise is one request. The cache
/// above means it is rendered rarely, so the duplication across
/// responses costs less than the round trip would.
///
/// The sheet is a token block plus a component block that authors no raw
/// values, both belonging to this repository; `ui.css` states the rule
/// that binds them. Dark is the canonical theme and light follows the
/// reader's system setting, both without a line of JavaScript.
/// The social-preview card, served at [`CARD_PATH`].
///
/// The only binary this node ships. It exists because a link pasted into
/// a chat window is rendered by that client into a card, and a card with
/// no image is a grey rectangle beside the one thing a newcomer has been
/// asked to trust.
///
/// It is generic on purpose and carries no text beyond the wordmark: the
/// preview is fetched and rendered by a third party's servers and shown
/// to everyone in the channel, so the repository, the inviter and the
/// username stay in the page body where only the holder of the link
/// sees them.
///
/// Drawn from `scripts/card.html` by `scripts/render_card.sh`, in the
/// same palette and the same display face as the front door. A binary
/// with no generator is a file nobody can correct, and this one spent a
/// redesign carrying the palette of the surface before last.
pub(crate) const CARD: &[u8] = include_bytes!("card.png");

/// Where [`CARD`] is served. Named once because the route, the
/// `og:image` tag and the test that proves it is reachable must agree,
/// and three spellings of a path is how one of them goes stale.
pub(crate) const CARD_PATH: &str = "/static/card.png";

/// Where [`ROBOTS`] is served, named once for the same reason
/// [`CARD_PATH`] is.
pub(crate) const ROBOTS_PATH: &str = "/robots.txt";

/// The crawl policy, and the one file on this node written for a robot.
///
/// Everything but the front door is disallowed, which is a statement
/// about *indexing* rather than about access — every one of those paths
/// already answers `401` to a crawler, and a search result quoting a
/// refusal page is the only thing crawling them could produce.
///
/// Two `Allow` lines carve out what D57 publishes on purpose. `/$`
/// anchors to the landing page alone rather than the whole tree.
/// [`CARD_PATH`] is there because the card exists to be fetched by
/// somebody else's server when the address is pasted into a chat window,
/// and a client that honours this file would render the grey rectangle
/// the card was drawn to replace. Longest match wins (RFC 9309), so both
/// beat the `Disallow: /` under them.
///
/// `/join` is named explicitly even though `Disallow: /` already covers
/// it. An invite link that reaches an index is an invite spent by a
/// crawler, and the route already says so per-response with
/// `X-Robots-Tag`; saying it twice costs one line and closes the gap
/// between a crawler that reads headers and one that only reads this.
///
/// Served to anybody, because a policy behind a credential is a policy
/// no crawler ever sees, and its whole content is paths this node
/// already publishes.
pub(crate) const ROBOTS: &str = concat!(
    "User-agent: *\n",
    "Allow: /$\n",
    "Allow: /static/card.png\n",
    "Allow: /static/card/\n",
    "Disallow: /join\n",
    "Disallow: /\n",
);

/// The crawl policy for a node that publishes something, built from the
/// table that decides what is published.
///
/// [`ROBOTS`] above is the policy for a node that publishes nothing, and
/// its reasoning was load-bearing: *"every one of those paths already
/// answers `401` to a crawler, and a search result quoting a refusal
/// page is the only thing crawling them could produce."* D78 made that
/// false. A published repository answers `200` to anybody, and the node
/// was still telling every crawler not to look at the one thing it had
/// been changed to show them.
///
/// So the policy is derived rather than written, for the same reason the
/// front door is (see `front_door_published`): a static `Allow: /r/`
/// would be right here and wrong on the next node, inviting crawlers
/// into a wall of `401`s that the old comment correctly refused to do.
/// Editing the ACL file rewrites this, because the table is watched and
/// this reads the table.
///
/// **Longest match wins** (RFC 9309), which is what lets a specific
/// `Allow` sit under the blanket `Disallow: /` and beat it. Each
/// repository gets two lines: the bare path and the subtree, because
/// `/r/owner/name` and `/r/owner/name/tree/main` are both pages and only
/// the second is a prefix match of the first with a separator.
///
/// `/join` keeps its explicit `Disallow` even though the blanket covers
/// it: an invite link that reaches an index is an invite spent by a
/// crawler, and a later `Allow` added above must not quietly outrank it.
pub(crate) fn robots(repos: &[String], downloads: bool, origin: Option<&str>) -> String {
    if repos.is_empty() && !downloads {
        return ROBOTS.to_string();
    }
    let mut out = String::with_capacity(256);
    out.push_str("User-agent: *\n");
    out.push_str("Allow: /$\n");
    out.push_str("Allow: ");
    out.push_str(CARD_PATH);
    out.push('\n');
    // The per-repository cards. A crawler that honours this file and
    // cannot fetch the image renders the grey rectangle the card exists
    // to replace, so the policy has to allow the picture as well as the
    // page it is on.
    out.push_str("Allow: ");
    out.push_str(crate::card::PREFIX);
    out.push('\n');
    for repo in repos {
        out.push_str("Allow: /r/");
        out.push_str(repo);
        out.push_str("$\n");
        out.push_str("Allow: /r/");
        out.push_str(repo);
        out.push_str("/\n");
    }
    if downloads {
        // The shelf is a page that answers a question somebody types
        // into a search engine -- "how do I install this" -- and it is
        // the one page here whose whole content is an instruction.
        out.push_str("Allow: /download/\n");
    }
    out.push_str("Disallow: /join\n");
    out.push_str("Disallow: /\n");
    // Absolute per RFC 9309, which is why this takes an origin at all.
    // Omitted rather than guessed when the request carried no `Host`, on
    // the same reasoning as every other absolute URL this node mints.
    if let Some(origin) = origin {
        out.push_str("Sitemap: ");
        out.push_str(origin);
        out.push_str(SITEMAP_PATH);
        out.push('\n');
    }
    out
}

/// Where [`sitemap`] is served, named once for the reason
/// [`CARD_PATH`] is.
pub(crate) const SITEMAP_PATH: &str = "/sitemap.xml";

/// Every address on this node worth an index entry, as a sitemap.
///
/// The same table as [`robots`], answering the other half of the
/// question: that one says what a crawler *may* fetch, this says what is
/// there. A node that publishes nothing renders an empty `urlset` rather
/// than a refusal, because the document discloses only what the ACL has
/// already made public and an empty one is the honest answer.
///
/// **Repository roots only, and no deeper.** Every file, tree and commit
/// page is reachable by following links from the root, which is what a
/// crawler does; enumerating them here would be a document that grows
/// with the history and goes stale the moment anybody pushes. The
/// addresses listed are the ones that are stable: the front door, each
/// published repository, and the shelf.
///
/// No `lastmod`. It would have to come from the tip commit's date, which
/// means a `git log` per repository per crawl -- a spawn budget spent on
/// a hint, and the tip is what a crawler learns by fetching the page
/// anyway.
pub(crate) fn sitemap(origin: &str, repos: &[String], downloads: bool) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    let mut url = |path: &str| {
        out.push_str("  <url><loc>");
        out.push_str(&esc(origin));
        out.push_str(&esc(path));
        out.push_str("</loc></url>\n");
    };
    url("/");
    for repo in repos {
        url(&format!("/r/{repo}"));
    }
    if downloads {
        url("/download/");
    }
    out.push_str("</urlset>\n");
    out
}

pub(crate) const STYLE: &str = concat!("<style>", include_str!("ui.css"), "</style>");

/// The URL D39's client half is served from, in one place because the
/// route, the tag and the tests all have to name the same string.
pub(crate) const WEBAUTHN_JS_PATH: &str = "/static/webauthn.js";

/// D39's client half: three passkey ceremonies, no library, no build
/// step, no third party, and nothing off this origin.
///
/// **A file rather than a string constant, and fetched rather than
/// inlined.** The stylesheet above argues for inlining and this argues
/// the other way, because they answer to different headers: a page that
/// runs no script can carry `default-src 'none'` whatever its CSS does,
/// while a page that runs script must either license the exact bytes by
/// digest or license a source. Digests meant recomputing a SHA-256 per
/// script at bind through `openssl`, and a node without `openssl`
/// silently fell back to `'unsafe-inline'` — a weaker policy than
/// intended, arrived at by accident, visible only in a served header.
/// One same-origin source removes the digest, the subprocess and the
/// fallback together.
///
/// What D39's row scoped is intact: no framework, no build step, no
/// third-party host, works offline. The clause that changes is "no
/// fetched script", which becomes "nothing fetched off this origin".
pub(crate) const WEBAUTHN_JS: &str = include_str!("webauthn.js");

/// The element that pulls [`WEBAUTHN_JS`] in, on the two pages that
/// carry a ceremony and on no other.
///
/// `defer` rather than placement: the ceremonies query elements by id,
/// and deferring to after parse is what makes that true wherever the tag
/// sits.
pub(crate) const CEREMONY_SCRIPT: &str = "<script src=\"/static/webauthn.js\" defer></script>";

#[cfg(test)]
mod tests {
    use super::*;

    /// The escaper is the page's only defence against a ref name
    /// chosen by whoever can push a branch, so it is checked directly
    /// rather than only through a rendered page.
    #[test]
    fn escaping_neutralises_markup_in_every_context() {
        assert_eq!(
            esc("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
        assert_eq!(esc("a\"b"), "a&quot;b");
        assert_eq!(esc("a&b"), "a&amp;b");
    }

    /// A hostile ref name reaches the page as text, not as markup.
    #[test]
    fn a_ref_named_like_markup_cannot_inject() {
        let json = serde_json::json!({
            "refs": {"o/r.git:refs/heads/<img src=x onerror=alert(1)>": "11-deadbeefdeadbeef"}
        })
        .to_string();
        let page = render(&json, 7, &Roster::new(), crate::browse::Chrome::default());
        assert!(!page.contains("<img src=x"), "raw markup reached the page");
        assert!(page.contains("&lt;img src=x onerror=alert(1)&gt;"));
    }

    /// The refs table names repositories, and the name it shows has to be
    /// one the reader can use.
    ///
    /// A ref key is `owner/name.git:refs/…`, so the heading used to read
    /// `agents/one.git` — which is a `404` under `/r/`, the node printing
    /// a name its own browse surface rejects. It shows `agents/one` now
    /// and links it, and the `.git` path appears on the repository page
    /// as the thing it is actually for, which is cloning.
    #[test]
    fn every_repository_the_refs_table_names_can_be_opened() {
        let json = serde_json::json!({
            "refs": {
                "agents/one.git:refs/heads/main": "11-deadbeefdeadbeef",
                "agents/one.git:refs/heads/wip": "11-deadbeefdeadbeef",
                "r/project.git:refs/heads/main": "11-deadbeefdeadbeef",
            }
        })
        .to_string();
        let page = render(&json, 7, &Roster::new(), crate::browse::Chrome::default());
        for name in ["agents/one", "r/project"] {
            assert!(
                page.contains(&format!("<a href=\"/r/{name}\">{name}</a>")),
                "the refs table names {name} and gives no way to open it: {page}"
            );
        }
        // The name that 404s under `/r/` must not be what the page shows.
        assert!(
            !page.contains("agents/one.git"),
            "the refs table still prints the clone name, which its own \
             browse surface refuses: {page}"
        );
        // One heading per repository: two would read as two repositories.
        assert_eq!(
            page.matches("<a href=\"/r/agents/one\">").count(),
            1,
            "a repository is headed more than once: {page}"
        );
    }

    /// Status lives where the work is: a ref with an open review says so
    /// on its own row, and a ref with none stays quiet — a chip that is
    /// always there is a chip nobody reads.
    #[test]
    fn a_ref_with_an_open_review_carries_a_pending_chip() {
        let json = serde_json::json!({
            "refs": {
                "o/r.git:refs/heads/main": "11-deadbeefdeadbeef",
                "o/r.git:refs/heads/side": "11-deadbeefdeadbeef",
            },
            "reviews": {
                "rv1": {"target_ref": "o/r.git:refs/heads/main", "complete": false},
                "rv2": {"target_ref": "o/r.git:refs/heads/main", "complete": true,
                        "approved": true},
                "rv3": {"target_ref": "o/r.git:refs/heads/main", "complete": false,
                        "archived": true},
            }
        })
        .to_string();
        let page = render(&json, 1, &Roster::new(), crate::browse::Chrome::default());
        assert!(
            page.contains("refs/heads/main <b class=\"tag pending\">1 pending</b>"),
            "the open review is not on its ref's row: {page}"
        );
        assert!(
            !page.contains("refs/heads/side <b"),
            "a ref nothing targets grew a chip: {page}"
        );
    }

    /// A tripwire nobody can interpret is worse than one that is absent.
    ///
    /// The three D24 rows render `indeterminate` as a grey badge and a
    /// reader takes that to mean "not enough data yet". For concentration
    /// that is true and the operator can act on it; for the other two the
    /// bound itself is unset, so the badge will read the same in a year.
    /// One word covering both is the page misleading whoever trusts it.
    #[test]
    fn a_tripwire_the_node_cannot_evaluate_says_what_that_means() {
        let waiting = render(
            &serde_json::json!({"concentration": {"tripwire_status": "indeterminate"}}).to_string(),
            1,
            &Roster::new(),
            crate::browse::Chrome::default(),
        );
        assert!(
            waiting.contains("indeterminate"),
            "the status vanished: {waiting}"
        );
        assert!(
            waiting.contains("evaluation_complete"),
            "the page shows `indeterminate` and never says what would resolve it: {waiting}"
        );
        assert!(
            waiting.contains("neither a pass nor a breach"),
            "a reader is left to guess whether the node is failing: {waiting}"
        );

        // ...and it is not boilerplate stapled under every health table:
        // a node that completed its evaluation has nothing to explain.
        let settled = render(
            &serde_json::json!({"concentration": {"tripwire_status": "not_observed"}}).to_string(),
            1,
            &Roster::new(),
            crate::browse::Chrome::default(),
        );
        assert!(
            !settled.contains("neither a pass nor a breach"),
            "the note is shown where nothing is indeterminate: {settled}"
        );
    }

    /// Malformed and empty payloads render a page rather than panic:
    /// the browser surface must never be the thing that takes the node
    /// down, and a node with nothing in it is a normal first boot.
    #[test]
    fn unparseable_or_empty_state_still_renders() {
        for json in ["not json at all", "null", "{}", r#"{"refs":42}"#] {
            let page = render(json, 0, &Roster::new(), crate::browse::Chrome::default());
            assert!(page.starts_with("<!doctype html>"), "no page for {json}");
            assert!(page.contains("</html>"), "truncated page for {json}");
        }
    }

    /// Nothing is fetched from anywhere. This is what makes the page
    /// paint in one round trip, so it is asserted rather than assumed.
    ///
    /// **`<script` left this list in D39 and the test got stronger, not
    /// weaker.** The old probe conflated two things: "this page fetches
    /// something" and "this page runs script". D39 reversed the second
    /// in one narrow place, and the row scoped the reversal to a script
    /// with no library, no build step and no third party. So the
    /// property that mattered is now stated directly — every other probe
    /// stands, and any `src` on *this* page is refused by name.
    ///
    /// This page has no script at all and fetches nothing. The two
    /// ceremony pages fetch exactly one thing, [`WEBAUTHN_JS`], from
    /// this origin; `the_client_half_is_one_same_origin_file` below is
    /// the rule for that, and this stays the rule for the read surface.
    /// The dark theme's grain is an SVG in a data URI, and an SVG
    /// document names its namespace by URL. Nothing is fetched from it:
    /// it is an identifier the parser compares, not an address it
    /// visits, so the probes below look past exactly that string and
    /// nothing else.
    fn without_the_svg_namespace(page: &str) -> String {
        page.replace("xmlns='http://www.w3.org/2000/svg'", "")
    }

    #[test]
    fn the_page_references_no_external_resource() {
        let page = render(
            r#"{"refs":{}}"#,
            1,
            &Roster::new(),
            crate::browse::Chrome::default(),
        );
        let page = without_the_svg_namespace(&page);
        for probe in ["http://", "https://", "//cdn", "@import"] {
            assert!(!page.contains(probe), "page reaches out via {probe}");
        }
        assert!(
            !page.contains("<script"),
            "the D28 read surface gained script; D39's reversal was scoped to the review page"
        );
        assert!(
            !page.contains("src="),
            "a fetched resource is a fetched resource whether or not it is script"
        );
    }

    /// D39's scope, restated for the file the ceremonies moved into: one
    /// same-origin resource, no library, no build step, no third party.
    ///
    /// This probes the constant that ships rather than a copy, because
    /// the constant *is* what the node serves — `include_str!` means
    /// there is no second version to drift from.
    #[test]
    fn the_client_half_is_one_same_origin_file() {
        for probe in [
            "http://", "https://", "//cdn", "@import", "import ", "require(",
        ] {
            assert!(
                !WEBAUTHN_JS.contains(probe),
                "the ceremony reaches out via {probe}"
            );
        }
        // Every fetch it makes is a path, never an origin, and it says
        // so twice: same-origin credentials and a leading slash.
        for path in ["'/api/submit'", "'/api/prepare'", "'/api/accounts/passkey'"] {
            assert!(WEBAUTHN_JS.contains(path), "{path} is not where this posts");
        }
        assert!(
            !WEBAUTHN_JS.contains("<script"),
            "a script file carrying markup"
        );
        // Nothing is interpolated into it, which is what lets one
        // response serve every reader and every render.
        assert!(!WEBAUTHN_JS.contains("{}"));
        // The tag and the route must name the same URL; they are two
        // constants precisely so the route can be matched without
        // parsing markup, and two constants can disagree.
        assert!(
            CEREMONY_SCRIPT.contains(WEBAUTHN_JS_PATH),
            "the tag points somewhere the node does not serve: {CEREMONY_SCRIPT}"
        );
        assert!(
            CEREMONY_SCRIPT.contains(" defer"),
            "the ceremonies run before the DOM exists"
        );
    }

    /// The crawl policy says what D57 decided, and the two constants
    /// that name the same picture do not disagree.
    ///
    /// The card is the one that would fail silently: a chat client that
    /// honours this file and finds the card disallowed renders the grey
    /// rectangle the card exists to replace, and nobody sees a refusal
    /// because there is no reader on that request.
    #[test]
    fn the_crawl_policy_publishes_the_front_door_and_nothing_behind_it() {
        assert!(ROBOTS.starts_with("User-agent: *\n"), "{ROBOTS}");
        assert!(
            ROBOTS.contains("\nAllow: /$\n"),
            "the landing page is not indexable: {ROBOTS}"
        );
        assert!(
            ROBOTS.contains(&format!("\nAllow: {CARD_PATH}\n")),
            "the social card is disallowed, so a preview renders nothing: {ROBOTS}"
        );
        assert!(
            ROBOTS.contains("\nDisallow: /join\n"),
            "an invite link may be indexed, which spends it: {ROBOTS}"
        );
        assert!(
            ROBOTS.trim_end().ends_with("Disallow: /"),
            "the catch-all is not last, so a longer rule cannot beat it: {ROBOTS}"
        );
        // It says nothing about this node. A repository name in a
        // sitemap would be readable by anybody who resolves the DNS
        // record, which is the property the landing page is built on.
        assert!(!ROBOTS.contains("Sitemap"), "{ROBOTS}");
        assert!(!ROBOTS.contains("/r/"), "{ROBOTS}");
    }

    /// ES256 only, because that is the one scheme the node can verify.
    /// Offering `alg: -257` would enrol RSA keys that
    /// `verify_webauthn_assertion` refuses at first use, which is the
    /// failure mode enrolment-time validation exists to prevent.
    ///
    /// It moved here with the script it constrains. The rule did not
    /// change; the place it is written did, and one file with three
    /// ceremonies in it is why.
    #[test]
    fn the_ceremony_asks_for_the_only_algorithm_the_node_verifies() {
        assert!(WEBAUTHN_JS.contains("alg: -7"));
        assert!(!WEBAUTHN_JS.contains("-257"), "RSA was offered");
        // `getPublicKey()` rather than parsing the attestation object:
        // D39 scoped a CBOR reader out, and a hand-rolled one is the
        // tripwire on that row.
        assert!(WEBAUTHN_JS.contains("getPublicKey"));
        assert!(!WEBAUTHN_JS.contains("attestationObject"));
        // Scheme 2 is WebAuthn/ES256 on the wire. A submission built
        // with any other number is refused by the node, so a page that
        // sent one would offer a button that never works.
        assert!(WEBAUTHN_JS.contains("scheme: 2"));
    }

    /// Shortening is a display detail, and a display detail must not be
    /// able to stop the node answering. `short` used to cut its digest at
    /// byte 12, which panics whenever byte 12 lands inside a multi-byte
    /// character — and every value on this page comes out of a JSON
    /// document rather than out of a type that guarantees hex.
    #[test]
    fn shortening_a_hash_survives_text_that_is_not_hex() {
        // Byte 12 falls inside the fourth `日` (1 + 3 + 3 + 3 = 10, next
        // boundary 13), which is the exact case that panicked.
        assert_eq!(short("11-a日日日日日"), "11-a日日日日日");
        // Twelve characters kept, whatever they cost in bytes.
        assert_eq!(
            short("11-日日日日日日日日日日日日日日"),
            "11-日日日日日日日日日日日日"
        );
        // The ordinary case is unchanged: a hex digest still shortens.
        assert_eq!(short("11-deadbeefdeadbeef"), "11-deadbeefdead");
        // ...and a whole page built from such a payload still renders.
        let json = serde_json::json!({
            "refs": {"o/r.git:refs/heads/main": "11-a日日日日日"},
            "build": {"commit": "日日日日日日日日日日日日日日"},
        })
        .to_string();
        assert!(
            render(&json, 1, &Roster::new(), crate::browse::Chrome::default()).ends_with("</html>")
        );
    }

    /// A refusal has to carry four things, because that is what the JSON
    /// refusals carry and what a person needs for the same reason: the
    /// code they can quote, the sentence, the two states, and the one
    /// action. `next` is the one this test would be pointless without —
    /// it is the field `reject.rs` calls the one the gain is isolated to.
    #[test]
    fn a_refusal_page_carries_the_code_the_states_and_the_next_action() {
        let page = refusal(
            "No repository here",
            404,
            &Refusal {
                code: "no_such_repository",
                error: "Nothing readable by this credential is at that address.",
                expected: Some("a repository this credential holds a read grant on"),
                actual: None,
                next: "Open the repository list and follow a link from it.",
            },
            &[("/r/", "repositories you can read")],
            crate::browse::Chrome::default(),
        );
        assert!(page.starts_with("<!doctype html>") && page.ends_with("</html>"));
        assert!(page.contains("404"), "the status is not on the page");
        assert!(
            page.contains("no_such_repository"),
            "the code is not on the page"
        );
        assert!(
            page.contains("read grant"),
            "the expected state is not on the page"
        );
        assert!(
            page.contains("Open the repository list"),
            "the next action is not on the page: {page}"
        );
        assert!(
            page.contains("callout-note"),
            "the next action is not marked as the note admonition"
        );
        assert!(
            page.contains("href=\"/r/\""),
            "a refused reader was given nowhere to go"
        );
        // `actual` was `None`, so no empty row may be invented for it.
        assert!(
            !page.contains("<td>found</td>"),
            "an absent state got a row anyway"
        );
    }

    /// A refusal renders values the node did not choose — a revision from
    /// the URL, git's own stderr — so it escapes exactly like the page it
    /// stands in for.
    #[test]
    fn a_refusal_escapes_everything_it_is_handed() {
        let page = refusal(
            "<h1>headline",
            404,
            &Refusal {
                code: "<code>",
                error: "<em>error",
                expected: Some("<b>expected"),
                actual: Some("<img src=x onerror=alert(1)>"),
                next: "<script>alert('x')</script>",
            },
            &[("\" onmouseover=alert(1) x=\"", "<i>label")],
            crate::browse::Chrome::default(),
        );
        for raw in [
            "<h1>headline",
            "<code>",
            "<em>error",
            "<b>expected",
            "<img src=x",
            "<script>alert",
            "<i>label",
        ] {
            assert!(!page.contains(raw), "{raw} reached the page as markup");
        }
        assert!(
            page.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "the value vanished"
        );
        assert!(
            !page.contains("\" onmouseover=alert(1) x=\""),
            "an attribute escaped its quotes"
        );
    }

    /// The same claim `the_page_references_no_external_resource` makes,
    /// for the other document this module serves. Deliberately its own
    /// test with its own list rather than a shared constant: D39 narrows
    /// that probe list when it adds the first script, and a shared list
    /// would narrow this one silently at the same moment. Two lists means
    /// two decisions.
    ///
    /// **The decision, taken rather than deferred:** this list stays
    /// strict, `<script` included, where D39 scoped its reversal to the
    /// review page. A refusal page is a dead end by construction — its
    /// entire content is what happened and the one action that changes
    /// it, and there is no interaction on it to progressively enhance.
    /// So the probe list here is the rule for this surface rather than a
    /// lagging copy of another surface's. If a later change wants script
    /// on a refusal page, this test failing is the point: the question is
    /// worth asking then, and answering it by matching the other list is
    /// how a rule turns into a habit.
    #[test]
    fn a_refusal_page_references_no_external_resource() {
        let page = refusal(
            "No repository here",
            404,
            &Refusal {
                code: "no_such_repository",
                error: "e",
                expected: None,
                actual: None,
                next: "n",
            },
            &[("/r/", "repositories")],
            crate::browse::Chrome::default(),
        );
        let page = without_the_svg_namespace(&page);
        for probe in ["http://", "https://", "//cdn", "<script", "@import"] {
            assert!(!page.contains(probe), "a refusal reaches out via {probe}");
        }
    }

    /// Every token the component block references must be one the
    /// token block defines.
    ///
    /// The raw-value rule above says what a component rule may *not*
    /// write; it says nothing about whether the token it wrote instead
    /// exists. A `var(--fw-600)` that no `:root` defines is not an
    /// error anywhere — the browser drops that one declaration and
    /// renders the rest, so the rule half-applies and the page looks
    /// almost right. Four invented token names passed the raw-value
    /// lint and reached a browser before this test existed.
    #[test]
    fn every_component_token_is_defined_by_the_token_block() {
        let sheet = include_str!("ui.css");
        let (tokens, ours) = sheet
            .rsplit_once("CHOIR COMPONENTS")
            .expect("the component marker names both halves of the sheet");
        let defined: std::collections::BTreeSet<&str> = tokens
            .match_indices("--")
            .filter_map(|(at, _)| {
                let rest = &tokens[at + 2..];
                let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '-')?;
                // A definition is `--name:`; a `var(--name)` reference
                // inside the token block defines nothing.
                (rest.as_bytes().get(end) == Some(&b':')).then(|| &rest[..end])
            })
            .collect();
        let mut undefined: Vec<&str> = ours
            .match_indices("var(--")
            .filter_map(|(at, _)| {
                let rest = &ours[at + 6..];
                let end = rest.find(')')?;
                let name = &rest[..end];
                (!defined.contains(name)).then_some(name)
            })
            .collect();
        undefined.sort_unstable();
        undefined.dedup();
        assert!(
            undefined.is_empty(),
            "the component block references {} token(s) the token block never defines, so \
             each of those declarations is silently dropped: {}",
            undefined.len(),
            undefined.join(", "),
        );
    }

    /// The sheet's one hard rule is that no colour, size, radius,
    /// shadow or duration is authored outside the token block. That
    /// block is exempt by definition — it is where values live;
    /// everything after the marker must reference `var(--…)` instead. A
    /// hand-picked hex here is how a token set quietly dies.
    #[test]
    fn the_component_sheet_authors_no_raw_values() {
        let sheet = include_str!("ui.css");
        // The last occurrence: the header comment names the marker too,
        // and splitting on the first one would check the token block
        // against a rule that exists to exempt it.
        let ours = sheet
            .rsplit_once("CHOIR COMPONENTS")
            .expect("the provenance marker must stay in ui.css")
            .1;
        for (n, line) in ours.lines().enumerate() {
            let code = line.split("/*").next().unwrap_or("");
            assert!(
                !code.contains('#'),
                "component line {n} authors a raw colour: {line}"
            );
            for unit in ["px", "rem", "ms"] {
                // `var(--sp-4)` and friends carry their own units; a
                // literal digit before one is a value authored here.
                if let Some(at) = code.find(unit) {
                    let before = code[..at].chars().next_back().unwrap_or(' ');
                    assert!(
                        !before.is_ascii_digit(),
                        "component line {n} authors a raw {unit} value: {line}"
                    );
                }
            }
        }
    }

    /// A token used for the wrong property is invisible to every other
    /// check here, and it cost the diff its two most important lines.
    ///
    /// `pre.diff .add` and `.del` set `--syn-added` and `--syn-removed`
    /// as `color`. Both carry an alpha — washes, meant as backgrounds
    /// — so on `--sunken`, the darkest surface in the sheet, the added
    /// and removed lines a reader opens a diff *for* rendered at 14%
    /// opacity. `the_component_sheet_authors_no_raw_values` passed the
    /// whole time, correctly: the rule it enforces is "tokens only", and
    /// this was a token.
    ///
    /// So the rule this adds is the narrower true one: a translucent
    /// token is not a foreground colour. Mechanical, and it generalises
    /// past the case that prompted it — `--ok-tint` as `color` would be
    /// exactly as unreadable and exactly as green under every other test.
    ///
    /// An alias is followed rather than missed. `--info-tint:
    /// var(--accent-tint)` carries no alpha of its own and used to
    /// slip through, which this recorded as a named gap on the grounds
    /// that resolving it meant writing a CSS evaluator. It does not: a
    /// declaration that is exactly one `var(--x)` is a rename, and
    /// following renames to their definition is a bounded walk. Anything
    /// harder — a wash inside a `linear-gradient`, a value assembled from
    /// two tokens — is still not followed, and is also not a way to paint
    /// text at 14% opacity by accident.
    #[test]
    fn no_translucent_token_is_used_as_a_foreground_colour() {
        let sheet = include_str!("ui.css");
        // Every token the sheet defines, read out of the token block
        // rather than listed by hand — a hand-written list is a second
        // copy of the palette to keep in step.
        let mut defined: Vec<(&str, &str)> = Vec::new();
        for line in sheet.lines() {
            for decl in line.split(';') {
                let Some((name, value)) = decl.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                if name.starts_with("--") {
                    defined.push((name, value.trim()));
                }
            }
        }
        // A token is a wash if its definition carries an alpha, mixes
        // with `transparent`, or renames one that does. `rgba(` is kept
        // beside the `oklch()` spelling the palette now uses so a token
        // written either way is caught: the sheet converted in one pass, and a rule that
        // silently stopped applying to the old spelling is how the
        // conversion would have taken this guard down with it. The cap
        // is what keeps a cycle from hanging the suite; no chain in
        // this sheet is anywhere near it.
        let resolve = |mut value: &str| -> bool {
            for _ in 0..8 {
                if value.starts_with("rgba(")
                    || (value.starts_with("oklch(") && value.contains('/'))
                    // A mix with `transparent` in it is translucent by
                    // construction, whatever the other operand is.
                    || (value.starts_with("color-mix(") && value.contains("transparent"))
                {
                    return true;
                }
                let Some(alias) = value
                    .strip_prefix("var(")
                    .and_then(|rest| rest.strip_suffix(')'))
                    .filter(|alias| alias.starts_with("--"))
                else {
                    return false;
                };
                let Some((_, next)) = defined.iter().find(|(name, _)| *name == alias) else {
                    return false;
                };
                value = next;
            }
            false
        };
        let washes: Vec<&str> = defined
            .iter()
            .filter(|(_, value)| resolve(value))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            washes.len() > 5,
            "found only {} translucent tokens, so this test is not reading the \
             palette it thinks it is: {washes:?}",
            washes.len()
        );

        let ours = sheet
            .rsplit_once("CHOIR COMPONENTS")
            .expect("the provenance marker must stay in ui.css")
            .1;
        for (n, line) in ours.lines().enumerate() {
            let code = line.split("/*").next().unwrap_or("");
            // Split on `{` as well as `;`: this sheet writes most rules on
            // one line, so the first declaration of a rule sits directly
            // behind its selector and a `;`-only split never sees it. The
            // first version of this test did exactly that and passed
            // against the very defect it was written for.
            for decl in code.split([';', '{']) {
                // Must *start with* `color:`, which is what excludes
                // `background-color`, `border-color` and
                // `border-left-color` — the properties these tokens are
                // actually for.
                let Some(value) = decl.trim().strip_prefix("color:") else {
                    continue;
                };
                for wash in &washes {
                    assert!(
                        !value.contains(wash),
                        "component line {n} paints text with {wash}, which is a \
                         translucent wash: it renders at its own alpha against \
                         whatever is behind it. Use a signal token for the text \
                         and the wash as its background.\n{line}"
                    );
                }
            }
        }
    }

    /// Nothing below the marker pins a box to a fixed width.
    ///
    /// This is the guard the mutation round said had no mechanical form.
    /// It does; the form is just narrower than "a fixed-width gutter
    /// overflows past four digits". `pre.code .ln` was `width:var(--sp-10)`
    /// — a token, so `the_component_sheet_authors_no_raw_values` passed —
    /// and a token is a fixed length, which is exactly the property that
    /// broke: five-digit line numbers ran into their own source.
    ///
    /// `max-width` and `min-width` are deliberately fine. A cap leaves the
    /// box free to be narrower and a floor leaves it free to be wider;
    /// only `width` refuses both, which is what makes it wrong for a box
    /// whose contents are not a fixed size. Percentages and keywords are
    /// fine for the same reason — `table{width:100%}` is relative to
    /// whatever it is in.
    ///
    /// It is also the closest thing here to a narrow-viewport assertion.
    /// Nothing in this crate can render a page at 375 px and measure it,
    /// so what is checkable is the cause rather than the symptom: a page
    /// whose every box is free to shrink has no fixed minimum to overflow
    /// a phone with. That is why there is no media query — not because
    /// narrow viewports went unconsidered.
    #[test]
    fn no_component_rule_pins_a_box_to_a_fixed_width() {
        let sheet = include_str!("ui.css");
        let ours = sheet
            .rsplit_once("CHOIR COMPONENTS")
            .expect("the provenance marker must stay in ui.css")
            .1;
        for (n, line) in ours.lines().enumerate() {
            let code = line.split("/*").next().unwrap_or("");
            for decl in code.split([';', '{']) {
                // `strip_prefix` after `trim` is what distinguishes this
                // from `max-width:` and `min-width:`, both of which
                // contain the same substring and are both allowed.
                let Some(value) = decl.trim().strip_prefix("width:") else {
                    continue;
                };
                assert!(
                    !value.contains("var("),
                    "component line {n} sets `width` to a token, which is a fixed \
                     length: the box can no longer shrink to its viewport or grow \
                     to its contents. Use `min-width` for a floor, `max-width` for \
                     a cap, or a percentage.\n{line}"
                );
            }
        }
    }

    /// The token block must keep the load-bearing names and both
    /// themes, so a future edit that "tidies" it is caught rather than
    /// merged.
    ///
    /// The system-palette assertion is written as
    /// `prefers-color-scheme`, without naming which of the two the
    /// media query carries. It used to say `prefers-color-scheme:light`,
    /// which was not a claim about theme completeness at all: it was a
    /// claim about *which theme is canonical*. The sheet was
    /// dark-canonical with light as the deviation; it is now
    /// light-canonical with dark as the deviation, and both spellings
    /// satisfy the property this test is for. What actually has to hold
    /// is that a system preference is honoured **and** that both
    /// `data-theme` overrides beat it — a reader who has chosen must win
    /// over the machine that guessed.
    ///
    /// **Existing is not beating, and the difference is one selector.**
    /// `:root[data-theme="light"]` and `:root:not([data-theme="light"])`
    /// score the same, (0,2,0): a `:root` pseudo-class and an attribute
    /// are both class-level, and `:not()` contributes only its argument.
    /// Equal specificity is settled by source order, and the media query
    /// is written below the light block — so an *unguarded* query would
    /// win over a reader who explicitly chose light, on every machine
    /// set to dark. Nothing else here would see it: every token is
    /// defined, every component is clean, and the page is simply the
    /// palette that reader said they did not want.
    ///
    /// The guard is what makes it right, so the guard is what is
    /// asserted. This is not the sheet's arrangement today being pinned
    /// for its own sake: the same property could be had by moving the
    /// block instead, and a sheet that does that will fail this and
    /// should be read before it is changed to pass.
    #[test]
    fn the_token_block_is_present_and_theme_complete() {
        let sheet = include_str!("ui.css");
        for token in ["--ground", "--accent-ink", "--ok", "--warn", "--focus-ring"] {
            assert!(sheet.contains(token), "the token block lost {token}");
        }
        assert!(
            sheet.contains("prefers-color-scheme"),
            "the system palette is no longer honoured"
        );
        assert!(
            sheet.contains(r#":root[data-theme="light"]"#),
            "manual light override lost"
        );
        assert!(
            sheet.contains(r#":root[data-theme="dark"]"#),
            "manual dark override lost"
        );
        // The chosen palette must survive the guessed one. Either the
        // query excludes the reader who chose, or the block that serves
        // them is written after it; the sheet does the first.
        let query = sheet
            .find("@media (prefers-color-scheme:dark)")
            .expect("the dark palette is still carried by a media query");
        let light = sheet
            .find(r#":root[data-theme="light"]"#)
            .expect("checked above");
        assert!(
            sheet[query..].contains(r#":root:not([data-theme="light"])"#) || light > query,
            "the dark media query neither excludes an explicit light choice nor is \
             written above it, and the two selectors score the same — so a reader who \
             chose light is served dark on any machine set to dark"
        );
        assert!(
            sheet.contains("prefers-reduced-motion"),
            "reduced-motion handling lost"
        );
    }

    /// Every text colour clears 4.5:1 against every ground it is set
    /// on, in both palettes.
    ///
    /// The redesign that introduced this palette reported its ratios in
    /// a hand-written table computed by a script in a scratch directory,
    /// which is a number nobody can check again and nothing can keep
    /// true. Two of the values in that first table were **below** 4.5:1
    /// — the diff gutter, in both themes — and they were caught by
    /// running the script, not by anything in this suite. A palette is
    /// exactly the kind of thing that gets nudged later for taste.
    ///
    /// The pairs below are the ones that actually occur: the component
    /// block sets `--ink` on `--ground`, `--sunken` and `--raise`, and
    /// so on. A pair that stops occurring is a pair to delete from this
    /// list on purpose, not a reason to lower the threshold.
    ///
    /// Large text may legally sit at 3:1 and this asserts 4.5:1 for all
    /// of it, which is stricter than required and cheaper than encoding
    /// which token is set at which size.
    #[test]
    fn every_text_colour_clears_aa_against_every_ground_it_is_set_on() {
        for (theme, palette) in [
            ("light", palette(":root[data-theme=\"light\"]{")),
            ("dark", palette(":root[data-theme=\"dark\"]{")),
        ] {
            let hex = |name: &str| -> String {
                palette
                    .get(name)
                    .unwrap_or_else(|| panic!("{theme} palette has no {name}"))
                    .clone()
            };
            for ground in ["--ground", "--surface", "--raise", "--sunken", "--head"] {
                for ink in [
                    "--ink",
                    "--strong",
                    "--muted",
                    "--faint",
                    "--accent-ink",
                    "--ok",
                    "--warn",
                    "--danger",
                    "--syn-plain",
                    "--syn-comment",
                    "--syn-keyword",
                    "--syn-type",
                    "--syn-func",
                    "--syn-string",
                    "--syn-number",
                    "--syn-const",
                    "--syn-macro",
                    "--syn-punct",
                    "--syn-gutter",
                ] {
                    let ratio = contrast(&hex(ink), &hex(ground));
                    assert!(
                        ratio >= 4.5,
                        "{theme}: {ink} on {ground} is {ratio:.2}:1, below the 4.5:1 floor"
                    );
                }
            }
            // The one pair that is the other way round: text on a solid
            // accent fill, which is what the skip link and the page's one
            // primary button are.
            let ratio = contrast(&hex("--inverse"), &hex("--accent"));
            assert!(
                ratio >= 4.5,
                "{theme}: --inverse on --accent is {ratio:.2}:1, below the 4.5:1 floor"
            );
        }
    }

    /// The dark palette is written twice and the two copies must agree.
    ///
    /// Plain CSS has no mixins, so the palette a reader gets from
    /// `prefers-color-scheme` and the one they get from choosing `dark`
    /// are two literal blocks. Nothing makes them the same, and a nudge
    /// applied to one is a node whose appearance depends on whether the
    /// reader ever pressed the switch — which reads as a rendering bug
    /// and is nearly impossible to attribute.
    #[test]
    fn the_two_spellings_of_the_dark_palette_are_the_same_palette() {
        let queried = palette(":root:not([data-theme=\"light\"]){");
        let chosen = palette(":root[data-theme=\"dark\"]{");
        assert!(
            queried.len() > 20,
            "the media-query dark block is not being read: {} tokens",
            queried.len()
        );
        assert_eq!(
            queried, chosen,
            "the system-preference dark palette and the chosen one have drifted apart"
        );
    }

    /// Every opaque `--name:oklch(…)` in the block that opens with
    /// `selector`.
    ///
    /// Deliberately opaque-only: a token carrying an alpha — spelled
    /// `oklch(L% C H / a)` — is a wash with no single ratio to assert,
    /// and `no_translucent_token_is_used_as_a_foreground_colour`
    /// already forbids painting text with one. The slash is the whole
    /// test for that, which is why the palette may not spell an opaque
    /// colour with a redundant `/ 1`.
    ///
    /// Comments are stripped before anything is split, and that is not
    /// tidiness. This block is written one declaration per line with the
    /// ratio in a trailing comment, so splitting on `;` first hands the
    /// *next* declaration a leading `/* … */` and `--ink` parses as a
    /// name beginning with a slash. The first draft did exactly that and
    /// read exactly one token out of a palette of forty — and it failed
    /// loudly rather than quietly only because the lookup panics on a
    /// missing name instead of skipping it.
    fn palette(selector: &str) -> std::collections::BTreeMap<String, String> {
        let sheet = include_str!("ui.css");
        let at = sheet
            .find(selector)
            .unwrap_or_else(|| panic!("ui.css lost the `{selector}` block"));
        let body = &sheet[at + selector.len()..];
        let end = body.find('}').expect("a palette block closes");
        let mut code = String::with_capacity(end);
        let mut rest = &body[..end];
        while let Some(open) = rest.find("/*") {
            code.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            match after.find("*/") {
                Some(close) => rest = &after[close + 2..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        code.push_str(rest);
        code.split(';')
            .filter_map(|decl| {
                let (name, value) = decl.split_once(':')?;
                let (name, value) = (name.trim(), value.trim());
                (name.starts_with("--") && value.starts_with("oklch(") && !value.contains('/'))
                    .then(|| (name.to_string(), value.to_string()))
            })
            .collect()
    }

    /// The 8-bit sRGB a browser paints for an opaque `oklch(L% C H)`.
    ///
    /// Björn Ottosson's inverse, then the sRGB transfer function. The
    /// quantisation at the end is not a rounding convenience: WCAG is
    /// defined over the channel values a display is driven with, and
    /// asserting a ratio against unquantised floats would be asserting
    /// something no reader ever sees. It is also what keeps every ratio
    /// in this suite identical to the one the palette asserted when it
    /// was written in hex.
    fn srgb8(colour: &str) -> [u8; 3] {
        let body = colour
            .trim()
            .strip_prefix("oklch(")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{colour} is not an opaque oklch() colour"));
        let mut parts = body.split_whitespace();
        let mut next = |what: &str| -> f64 {
            parts
                .next()
                .unwrap_or_else(|| panic!("{colour} has no {what}"))
                .trim_end_matches('%')
                .parse()
                .unwrap_or_else(|error| panic!("{colour} has an unreadable {what}: {error}"))
        };
        let (lightness, chroma, hue) = (next("lightness") / 100.0, next("chroma"), next("hue"));
        let (a, b) = (
            chroma * hue.to_radians().cos(),
            chroma * hue.to_radians().sin(),
        );
        let l = (lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
        let m = (lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
        let s = (lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
        let linear = [
            4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
            -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
            -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s,
        ];
        linear.map(|c| {
            let encoded = if c <= 0.003_130_8 {
                12.92 * c
            } else {
                1.055 * c.powf(1.0 / 2.4) - 0.055
            };
            (encoded * 255.0).round().clamp(0.0, 255.0) as u8
        })
    }

    /// The WCAG contrast ratio between two opaque `oklch()` colours.
    fn contrast(a: &str, b: &str) -> f64 {
        let luminance = |colour: &str| -> f64 {
            let channel = |raw: u8| {
                let c = raw as f64 / 255.0;
                if c <= 0.040_45 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            let [r, g, b] = srgb8(colour);
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        };
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Every section must read the shape `/api/view` actually sends.
    ///
    /// This exists because the first health section read flat keys in
    /// milliseconds (`durable_p99_ms`) that the API has never emitted;
    /// it renders as a tidy column of em-dashes, which looks like "no
    /// data yet" rather than like a bug. The fixture below is the real
    /// nesting and the real units, and the assertion is that no field
    /// falls back to its placeholder — a check that fails if a key is
    /// renamed on either side.
    #[test]
    fn the_health_section_reads_the_shape_the_api_actually_sends() {
        let json = serde_json::json!({
            "sequencer_lag": {
                "decision": {"p99_us": 551, "p50_us": 512, "breaches": 0},
                "durable": {"p99_us": 8400, "p50_us": 4096, "breaches": 0},
                "gate_us": 100000,
            },
            "view_growth": {"serialized_bytes": {"total_authoritative_view": 30190}},
            "concentration": {"tripwire_status": "observed"},
            "build": {"commit": "dc87f0904ce34ad63d115b4dd386487ee520e998"},
            "snapshot": {"at_seq": 344, "id": "1e-151d4b26d084280e", "prev_snapshot": null},
        })
        .to_string();
        let page = render(&json, 344, &Roster::new(), crate::browse::Chrome::default());

        assert!(page.contains("8.4 ms"), "durable p99 never resolved");
        assert!(page.contains("0.5 ms"), "decision p99 never resolved");
        assert!(page.contains("100.0 ms gate"), "the gate never resolved");
        assert!(page.contains("30190 bytes"), "view growth never resolved");
        assert!(page.contains("observed"), "tripwire status never resolved");
        // A git commit carries no codec prefix; it must still shorten.
        assert!(page.contains("dc87f0904ce3"), "build commit missing");
        assert!(
            !page.contains("dc87f0904ce34ad63d115b4dd386487ee520e998"),
            "the build commit rendered at full length"
        );
        assert!(page.contains("genesis"), "a null prev_snapshot must say so");
    }

    /// A handle the roster names is rendered as the person, in both
    /// channel shapes, and the segments around it survive.
    ///
    /// `git/` is a provenance label only the node may apply (D41), so
    /// dropping it would turn "the node saw this push authenticated"
    /// into "somebody claimed to be this person".
    #[test]
    fn a_named_handle_renders_as_the_person_and_keeps_its_label() {
        let mut roster = Roster::new();
        roster.insert("7f3ac2ab19cd".to_string(), "Alice Ng".to_string());

        assert_eq!(person("7f3ac2ab19cd", &roster), "Alice Ng");
        assert_eq!(person("git/7f3ac2ab19cd", &roster), "git/Alice Ng");
        assert_eq!(
            person("7f3ac2ab19cd/reviewer", &roster),
            "Alice Ng/reviewer"
        );
    }

    /// A handle the roster cannot name is rendered as itself, silently.
    ///
    /// The three situations that produce one — issued before D46,
    /// issued with an explicit `user`, and **deleted** — must be
    /// indistinguishable on the page. A word like "unknown" or
    /// "deleted" beside the third undoes the deletion by announcing
    /// that there was something there to delete.
    #[test]
    fn an_unnamed_handle_renders_as_itself_and_says_nothing_else() {
        let mut roster = Roster::new();
        roster.insert("7f3ac2ab19cd".to_string(), "Alice Ng".to_string());

        for channel in ["91bd04ff2a17", "git/91bd04ff2a17", "91bd04ff2a17/reviewer"] {
            let rendered = person(channel, &roster);
            assert_eq!(rendered, channel, "a handle nobody can name was rewritten");
            for tell in ["unknown", "deleted", "revoked", "former", "?"] {
                assert!(
                    !rendered.contains(tell),
                    "rendering {channel} announced the absence with {tell:?}: {rendered}"
                );
            }
        }
    }

    /// The whole point, asserted on the page rather than on the helper:
    /// a name the store has forgotten is gone from the rendered HTML,
    /// and the handle it was attached to is what a reader sees.
    #[test]
    fn deleting_a_name_removes_it_from_the_page() {
        let json = serde_json::json!({
            "reviews": {
                "r1": {
                    "reviewers": ["7f3ac2ab19cd/reviewer"],
                    "verdicts": {"7f3ac2ab19cd/reviewer": {"verdict": "Approve", "note": ""}},
                }
            }
        })
        .to_string();

        let mut roster = Roster::new();
        roster.insert("7f3ac2ab19cd".to_string(), "Alice Ng".to_string());
        let named = render(&json, 1, &roster, crate::browse::Chrome::default());
        assert!(
            named.contains("Alice Ng"),
            "a named handle rendered as a handle: {named}"
        );

        // Revocation deletes the row the name lived in; the handle
        // survives, because the log kept it and cannot be edited.
        let forgotten = render(&json, 1, &Roster::new(), crate::browse::Chrome::default());
        assert!(
            !forgotten.contains("Alice Ng"),
            "the deleted name is still on the page: {forgotten}"
        );
        assert!(
            forgotten.contains("7f3ac2ab19cd"),
            "the handle vanished with the name, so the verdict names nobody at all: {forgotten}"
        );
    }

    /// A hit must not call the builder: that is the entire performance
    /// claim, and a closure that panics is the only way to prove the
    /// call did not happen.
    #[test]
    fn a_cache_hit_does_not_rebuild_the_page() {
        let cache = UiCache::new();
        let first = cache.page(
            3,
            0,
            "",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{}}"#.to_string(),
        );
        let second = cache.page(
            3,
            0,
            "",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || panic!("rebuilt an unchanged page"),
        );
        assert!(Arc::ptr_eq(&first, &second), "same seq served a new page");
    }

    /// Revoking an account deletes a name and appends no op, so the
    /// sequence does not move. Keyed on the sequence alone the cache
    /// would go on serving the deleted name until the next push — which
    /// on a quiet repository is never. The `ETag` has to move with it,
    /// or a browser holding the previous copy is told nothing changed.
    #[test]
    fn forgetting_a_name_invalidates_the_page_without_a_new_op() {
        let json = serde_json::json!({
            "reviews": {"r1": {"reviewers": ["7f3ac2ab19cd/reviewer"], "verdicts": {}}}
        })
        .to_string();
        let mut roster = Roster::new();
        roster.insert("7f3ac2ab19cd".to_string(), "Alice Ng".to_string());

        let cache = UiCache::new();
        let named = cache.page(3, 9, "", &roster, crate::browse::Chrome::default(), || {
            json.clone()
        });
        assert!(named.contains("Alice Ng"), "{named}");

        // Same sequence, later store: the name is gone from the store
        // and must be gone from the page.
        let forgotten = cache.page(
            3,
            10,
            "",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || json.clone(),
        );
        assert!(
            !forgotten.contains("Alice Ng"),
            "a deleted name survived in the cache at an unchanged sequence: {forgotten}"
        );
        assert_ne!(
            etag(3, 9, "", None),
            etag(3, 10, "", None),
            "the ETag did not move, so a browser keeps the page with the name on it"
        );
    }

    /// ...and a changed sequence must rebuild, or the page would show
    /// state that has already moved on.
    #[test]
    fn a_new_sequence_rebuilds_and_shows_the_new_state() {
        let cache = UiCache::new();
        let before = cache.page(
            1,
            0,
            "",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{"o/r.git:refs/heads/main":"11-aaa"}}"#.to_string(),
        );
        let after = cache.page(
            2,
            0,
            "",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{"o/r.git:refs/heads/main":"11-bbb"}}"#.to_string(),
        );
        assert!(before.contains("11-aaa"));
        assert!(after.contains("11-bbb"));
        assert_ne!(etag(1, 0, "", None), etag(2, 0, "", None));
    }

    /// Two readers at one sequence are two pages, and each is reused.
    /// Serving one reader's page to the other is exactly the leak phase
    /// B exists to close, so a hit must match on the reader as well.
    #[test]
    fn readers_seeing_different_things_get_different_pages() {
        let cache = UiCache::new();
        let alice = cache.page(
            4,
            0,
            "alice",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{"o/a.git:refs/heads/m":"11-a"}}"#.to_string(),
        );
        let bob = cache.page(
            4,
            0,
            "bob",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{"o/b.git:refs/heads/m":"11-b"}}"#.to_string(),
        );
        // On the repository name rather than the `.git` key it is stored
        // under: this test is about one reader never seeing the other's
        // page, and the negative half is stricter for the shorter string.
        assert!(alice.contains("o/a") && !alice.contains("o/b"));
        assert!(bob.contains("o/b") && !bob.contains("o/a"));
        let again = cache.page(
            4,
            0,
            "alice",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || panic!("rebuilt a cached reader's page"),
        );
        assert!(
            Arc::ptr_eq(&alice, &again),
            "the reader's own page was dropped"
        );
        assert_ne!(
            etag(4, 0, "alice", None),
            etag(4, 0, "bob", None),
            "one ETag for two pages"
        );
    }

    /// A new sequence must drop every reader's page, not only the one
    /// asking: that is what bounds the map to the readers active between
    /// two ops rather than every reader since the process started.
    #[test]
    fn advancing_the_sequence_clears_every_readers_page() {
        let cache = UiCache::new();
        let stale = cache.page(
            5,
            0,
            "alice",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{}}"#.to_string(),
        );
        let _ = cache.page(
            6,
            0,
            "bob",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{}}"#.to_string(),
        );
        let fresh = cache.page(
            6,
            0,
            "alice",
            &Roster::new(),
            crate::browse::Chrome::default(),
            || r#"{"refs":{}}"#.to_string(),
        );
        assert!(
            !Arc::ptr_eq(&stale, &fresh),
            "a page from an older sequence survived"
        );
    }

    /// No tag may carry a username to whatever logs it, and a tag must
    /// move when the daemon's rendering does.
    ///
    /// The first half is why the reader key is hashed. The second is
    /// why the build stamp is in there at all: the sequence alone says
    /// "the log has not moved", which is not the same claim as "the
    /// page you hold is the page I would send".
    #[test]
    fn the_etag_hides_the_reader_and_moves_with_the_build() {
        let anonymous = etag(7, 0, "", None);
        assert!(anonymous.starts_with("W/\"7-"), "{anonymous}");
        // The build and the palette are hashed together, so the tag
        // moves when either does and neither is readable off it.
        assert_ne!(
            anonymous,
            etag(7, 0, "", Some("dark")),
            "two palettes of one sequence share a tag, so a reader who \
             switches is handed their old page back"
        );
        assert_ne!(etag(7, 0, "", Some("dark")), etag(7, 0, "", Some("light")));
        let tagged = etag(7, 0, "alice\u{1f}*=r", None);
        assert!(
            !tagged.contains("alice"),
            "the ETag carried the username: {tagged}"
        );
        assert_ne!(tagged, etag(7, 0, "bob\u{1f}*=r", None));
    }
}
