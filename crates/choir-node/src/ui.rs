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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    inner: Mutex<(u64, HashMap<String, Arc<String>>)>,
}

impl UiCache {
    /// A cache holding nothing, which is the state after every restart.
    pub(crate) fn new() -> Self {
        UiCache {
            inner: Mutex::new((0, HashMap::new())),
        }
    }

    /// The page for view sequence `seq` as `reader` may see it,
    /// rendering it only if the copy on hand describes some other
    /// sequence or was built for a reader who sees something else.
    ///
    /// `build_json` is called only on a miss. It is a closure rather
    /// than a value so a hit never pays for the JSON it would not use.
    pub(crate) fn page<F>(&self, seq: u64, reader: &str, build_json: F) -> Arc<String>
    where
        F: FnOnce() -> String,
    {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (cached_seq, pages) = &mut *slot;
        if *cached_seq != seq {
            pages.clear();
            *cached_seq = seq;
        }
        if let Some(page) = pages.get(reader) {
            return Arc::clone(page);
        }
        let page = Arc::new(render(&build_json(), seq));
        pages.insert(reader.to_string(), Arc::clone(&page));
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
pub(crate) fn etag(seq: u64, reader: &str) -> String {
    if reader.is_empty() {
        return format!("W/\"{seq}\"");
    }
    format!("W/\"{seq}-{}\"", short_digest(reader))
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
fn esc(s: &str) -> String {
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
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("—")
}

/// Shortens a content hash for display while keeping its codec prefix,
/// which is the part that says what kind of hash it is.
fn short(id: &str) -> String {
    match id.split_once('-') {
        Some((codec, digest)) if digest.len() > 12 => format!("{codec}-{}", &digest[..12]),
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
fn render(json: &str, seq: u64) -> String {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let mut h = String::with_capacity(16 * 1024);

    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir</title>");
    h.push_str(STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");

    header(&mut h, &v, seq);
    refs_section(&mut h, &v);
    reviews_section(&mut h, &v);
    attestation_section(&mut h, &v);
    workspaces_section(&mut h, &v);
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
        h.push_str("<p class=\"empty\">No refs yet. A push or a signed <code>SetRef</code> creates one.</p></section>");
        return;
    }
    let mut current = "";
    h.push_str("<table><tbody>");
    for (full, target) in refs {
        let (repo, name) = split_ref(full);
        if repo != current {
            h.push_str("<tr class=\"group\"><th colspan=\"2\">");
            h.push_str(&esc(repo));
            h.push_str("</th></tr>");
            current = repo;
        }
        h.push_str("<tr><td>");
        h.push_str(&esc(name));
        h.push_str("</td><td class=\"mono\">");
        h.push_str(&esc(&short(target.as_str().unwrap_or("—"))));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table></section>");
}

/// Reviews, incomplete ones first: the queue is the part a human is
/// here to act on, and a completed review is history.
fn reviews_section(h: &mut String, v: &serde_json::Value) {
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
        h.push_str("<p class=\"empty\">No reviews recorded.</p></section>");
        return;
    }

    // The queue a human is here to act on, in full.
    if open.is_empty() {
        h.push_str("<p class=\"empty\">Nothing awaiting a verdict.</p>");
    } else {
        review_table(h, &open);
    }

    // Everything settled, folded away. `<details>` because the browser
    // already implements disclosure: no script, no state to get wrong,
    // and the page stays short as review history grows without bound.
    if !done.is_empty() {
        h.push_str("<details><summary>");
        h.push_str(&done.len().to_string());
        h.push_str(" completed</summary>");
        review_table(h, &done);
        h.push_str("</details>");
    }
    h.push_str("</section>");
}

/// One table of reviews, newest-looking first is not attempted: the
/// view keys reviews by id, and inventing an order the log does not
/// carry would be a display that lies about sequence.
fn review_table(h: &mut String, rows: &[(&String, &serde_json::Value)]) {
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
        h.push_str(&esc(id));
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
        verdicts(h, r);
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
}

/// One reviewer's answer per line, with the note when there is one.
fn verdicts(h: &mut String, r: &serde_json::Value) {
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
        h.push_str(&esc(name));
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
            h.push_str("<table><tbody><tr><td>at seq</td><td class=\"mono\">");
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
            let prev = snap.get("prev_snapshot").and_then(serde_json::Value::as_str);
            h.push_str(&esc(&prev.map(short).unwrap_or_else(|| "genesis".to_string())));
            h.push_str("</td></tr></tbody></table>");
            h.push_str("<p class=\"note\">Compare <code>id</code> at the same <code>at_seq</code> with another reader to check you were shown the same ref state. It is the node's own signature, so this is a comparison primitive, not proof of non-equivocation.</p>");
        }
        _ => h.push_str(
            "<p class=\"empty\">No attestation yet. One is emitted after the next ref update.</p>",
        ),
    }
    h.push_str("</section>");
}

/// Workspaces and their heads.
fn workspaces_section(h: &mut String, v: &serde_json::Value) {
    let ws = match v.get("workspaces").and_then(serde_json::Value::as_object) {
        Some(m) => m,
        None => return,
    };
    h.push_str("<section><h2>Workspaces <span class=\"count\">");
    h.push_str(&ws.len().to_string());
    h.push_str("</span></h2>");
    if ws.is_empty() {
        h.push_str("<p class=\"empty\">None provisioned.</p></section>");
        return;
    }
    h.push_str("<table><tbody>");
    for (name, head) in ws {
        h.push_str("<tr><td>");
        h.push_str(&esc(name));
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
    h.push_str("<section><h2>Health</h2><table><tbody>");

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

    for (label, key) in [
        ("concentration (D24 T3)", "concentration"),
        ("newcomer harm (D24 T4)", "newcomer_harm"),
        ("new-actor reviews (D24 T2)", "new_actor_review_outcomes"),
    ] {
        if let Some(m) = v.get(key).filter(|m| !m.is_null()) {
            let status = s(m, "tripwire_status");
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
    h.push_str("</tbody></table></section>");
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


/// The whole stylesheet, inline.
///
/// Inline because an external file is a second request before first
/// paint, and this page's entire premise is one request. The cache
/// above means it is rendered rarely, so the duplication across
/// responses costs less than the round trip would.
///
/// The sheet is the design system's tokens, vendored
/// verbatim, plus a choir-specific component block that authors no
/// raw values. `ui.css` carries the provenance note and the re-vendor
/// rule; dark is the canonical theme and light follows the reader's
/// system setting, both without a line of JavaScript.
const STYLE: &str = concat!("<style>", include_str!("ui.css"), "</style>");


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
        let page = render(&json, 7);
        assert!(!page.contains("<img src=x"), "raw markup reached the page");
        assert!(page.contains("&lt;img src=x onerror=alert(1)&gt;"));
    }

    /// Malformed and empty payloads render a page rather than panic:
    /// the browser surface must never be the thing that takes the node
    /// down, and a node with nothing in it is a normal first boot.
    #[test]
    fn unparseable_or_empty_state_still_renders() {
        for json in ["not json at all", "null", "{}", r#"{"refs":42}"#] {
            let page = render(json, 0);
            assert!(page.starts_with("<!doctype html>"), "no page for {json}");
            assert!(page.contains("</html>"), "truncated page for {json}");
        }
    }

    /// Nothing is fetched from anywhere. This is what makes the page
    /// paint in one round trip, so it is asserted rather than assumed.
    #[test]
    fn the_page_references_no_external_resource() {
        let page = render(r#"{"refs":{}}"#, 1);
        for probe in ["http://", "https://", "//cdn", "<script", "@import"] {
            assert!(!page.contains(probe), "page reaches out via {probe}");
        }
    }

    /// The design system's one hard rule is that no colour, size,
    /// radius, shadow or duration is authored outside its tokens. The
    /// vendored token block is exempt by definition; everything after
    /// the marker is ours and must reference `var(--…)` instead. A
    /// hand-picked hex here is how a design system quietly dies.
    #[test]
    fn the_component_sheet_authors_no_raw_values() {
        let sheet = include_str!("ui.css");
        // The last occurrence: the provenance note names the marker
        // too, and splitting on the first one would check the vendored
        // tokens against a rule that exists to exempt them.
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

    /// The vendored half must stay recognisably the design system's,
    /// so a future edit that "tidies" it is caught rather than merged.
    #[test]
    fn the_token_block_is_present_and_theme_complete() {
        let sheet = include_str!("ui.css");
        for token in ["--ground", "--accent-ink", "--ok", "--warn", "--focus-ring"] {
            assert!(sheet.contains(token), "vendored tokens lost {token}");
        }
        assert!(
            sheet.contains("prefers-color-scheme:light"),
            "light theme lost"
        );
        assert!(
            sheet.contains(r#":root[data-theme="light"]"#),
            "manual theme override lost"
        );
        assert!(
            sheet.contains("prefers-reduced-motion"),
            "reduced-motion handling lost"
        );
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
        let page = render(&json, 344);

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

    /// A hit must not call the builder: that is the entire performance
    /// claim, and a closure that panics is the only way to prove the
    /// call did not happen.
    #[test]
    fn a_cache_hit_does_not_rebuild_the_page() {
        let cache = UiCache::new();
        let first = cache.page(3, "", || r#"{"refs":{}}"#.to_string());
        let second = cache.page(3, "", || panic!("rebuilt an unchanged page"));
        assert!(Arc::ptr_eq(&first, &second), "same seq served a new page");
    }

    /// ...and a changed sequence must rebuild, or the page would show
    /// state that has already moved on.
    #[test]
    fn a_new_sequence_rebuilds_and_shows_the_new_state() {
        let cache = UiCache::new();
        let before =
            cache.page(1, "", || r#"{"refs":{"o/r.git:refs/heads/main":"11-aaa"}}"#.to_string());
        let after =
            cache.page(2, "", || r#"{"refs":{"o/r.git:refs/heads/main":"11-bbb"}}"#.to_string());
        assert!(before.contains("11-aaa"));
        assert!(after.contains("11-bbb"));
        assert_ne!(etag(1, ""), etag(2, ""));
    }

    /// Two readers at one sequence are two pages, and each is reused.
    /// Serving one reader's page to the other is exactly the leak phase
    /// B exists to close, so a hit must match on the reader as well.
    #[test]
    fn readers_seeing_different_things_get_different_pages() {
        let cache = UiCache::new();
        let alice = cache.page(4, "alice", || r#"{"refs":{"o/a.git:refs/heads/m":"11-a"}}"#.to_string());
        let bob = cache.page(4, "bob", || r#"{"refs":{"o/b.git:refs/heads/m":"11-b"}}"#.to_string());
        assert!(alice.contains("o/a.git") && !alice.contains("o/b.git"));
        assert!(bob.contains("o/b.git") && !bob.contains("o/a.git"));
        let again = cache.page(4, "alice", || panic!("rebuilt a cached reader's page"));
        assert!(Arc::ptr_eq(&alice, &again), "the reader's own page was dropped");
        assert_ne!(etag(4, "alice"), etag(4, "bob"), "one ETag for two pages");
    }

    /// A new sequence must drop every reader's page, not only the one
    /// asking: that is what bounds the map to the readers active between
    /// two ops rather than every reader since the process started.
    #[test]
    fn advancing_the_sequence_clears_every_readers_page() {
        let cache = UiCache::new();
        let stale = cache.page(5, "alice", || r#"{"refs":{}}"#.to_string());
        let _ = cache.page(6, "bob", || r#"{"refs":{}}"#.to_string());
        let fresh = cache.page(6, "alice", || r#"{"refs":{}}"#.to_string());
        assert!(!Arc::ptr_eq(&stale, &fresh), "a page from an older sequence survived");
    }

    /// An ACL-less node's tags must stay what they were, and no tag may
    /// carry a username to whatever logs it.
    #[test]
    fn the_etag_hides_the_reader_and_is_unchanged_without_an_acl() {
        assert_eq!(etag(7, ""), "W/\"7\"");
        let tagged = etag(7, "alice\u{1f}*=r");
        assert!(!tagged.contains("alice"), "the ETag carried the username: {tagged}");
        assert_ne!(tagged, etag(7, "bob\u{1f}*=r"));
    }
}
