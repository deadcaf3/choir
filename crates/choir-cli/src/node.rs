//! `choir node` — the operator half of the command line.
//!
//! Everything an *agent* does to a node was already here; everything an
//! *operator* does to one lived in `choirctl`, a zsh script with a
//! machine's habits baked into it. This module is where that half comes
//! back, in the language the rest of the binary is written in.
//!
//! # Why this is not a wrapper
//!
//! `choirctl status` answers its question by spawning about
//! twenty-eight processes: `curl` twice, `launchctl`, `ps`, `git`,
//! `date`, `awk` three times, `head` five times, and `python3` twice to
//! parse JSON that a shell cannot. Each is a fork, an exec, a dynamic
//! link and an interpreter start, and two of them boot Python to read
//! four numbers out of a document this binary already deserializes.
//!
//! Here the same report is two HTTP requests and a fold over the
//! response. `curl` stays — the workspace has no HTTP client crate, on
//! purpose, and that is one process per request rather than one per
//! *field*. Nothing else forks.
//!
//! # Examples
//!
//! ```
//! use choir_cli::node::Health;
//!
//! // A node that answers 503 on `/healthz` is reporting its own
//! // durability failure, and that is never softened into a warning.
//! assert!(!Health::Unhealthy.is_ok());
//! assert_eq!(Health::Unhealthy.exit_code(), 1);
//! ```

use crate::style::Style;
use serde_json::Value;

/// What `/healthz` said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The node answered 200: its durable append path is working.
    Healthy,
    /// The node answered 503. It is up, and it is telling us its own
    /// durability check has failed — the state that exits the daemon 75
    /// for a supervisor.
    Unhealthy,
    /// The node is up but would not say, because this credential may
    /// not ask.
    Undisclosed,
    /// Nothing answered.
    Unreachable,
}

impl Health {
    /// Whether this is a node an operator can stop worrying about.
    #[must_use]
    pub fn is_ok(self) -> bool {
        matches!(self, Health::Healthy | Health::Undisclosed)
    }

    /// The process exit code this health implies.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        i32::from(!self.is_ok())
    }

    fn paint(self, style: Style) -> String {
        match self {
            Health::Healthy => style.green("healthy"),
            Health::Unhealthy => style.red("UNHEALTHY — durable append is failing"),
            Health::Undisclosed => style.dim("not disclosed to this credential"),
            Health::Unreachable => style.red("did not answer"),
        }
    }
}

/// Microseconds as milliseconds, to one decimal.
///
/// The node reports microseconds because that is the resolution it
/// measures at; the gate is stated in milliseconds and so is every
/// conversation about it. Converting at the edge rather than in the
/// node keeps the wire honest and the report readable.
fn ms(us: u64) -> String {
    format!("{:.1} ms", us as f64 / 1000.0)
}

/// The `build` line: which commit is actually serving.
///
/// This is the question "did my rebuild reach the running process"
/// actually asks, and it is not answerable from the checkout — a
/// supervisor can restart from a cached job definition and keep running
/// the old binary while every file on disk says otherwise.
fn build_line(view: &Value, style: Style) -> String {
    let build = &view["build"];
    let Some(commit) = build["commit"].as_str() else {
        return style.dim("not disclosed");
    };
    if commit == "unknown" || commit.is_empty() {
        return format!(
            "{} — rebuild to make this checkable",
            style.red("UNSTAMPED")
        );
    }
    let short: String = commit.chars().take(12).collect();
    let source = build["source"].as_str().unwrap_or("?");
    // `dirty` is only a claim when the stamp came from git; a stamp from
    // anywhere else cannot have looked at a working tree, and printing
    // "clean" on its word would be inventing an assurance.
    let tree = match (build["dirty_trusted"].as_bool(), build["dirty"].as_bool()) {
        (Some(true), Some(true)) => style.red("dirty"),
        (Some(true), Some(false)) => "clean".to_string(),
        _ => style.dim("tree unknown"),
    };
    format!("{short}  {source}, {tree}")
}

/// The `lag` line: is the sequencer meeting the Phase-0 gate on real
/// traffic, rather than in the test suite.
///
/// Both percentiles, never one. `decision` is the gate as written and
/// `durable` is what a submitter actually waits out; reporting only the
/// first is how a node with a slow disk looks fast.
///
/// The section is `Disclosure::NodeWide`, so a per-repo credential is
/// handed a view with no `sequencer_lag` at all. That is reported as
/// withheld and never as an idle node — the two look identical in the
/// JSON and mean opposite things to an operator.
fn lag_line(view: &Value, style: Style) -> String {
    let lag = &view["sequencer_lag"];
    if lag.is_null() {
        return style.dim("not disclosed to this credential");
    }
    let Some(ops) = lag["observed_ops"].as_u64() else {
        return style.dim("not reported by this node");
    };
    if ops == 0 {
        return style.dim("no ops measured since start");
    }
    let gate = lag["gate_us"].as_u64().unwrap_or(0);
    let decision = lag["decision"]["p99_us"].as_u64().unwrap_or(0);
    let durable = lag["durable"]["p99_us"].as_u64().unwrap_or(0);
    let breaches = lag["durable"]["breaches"].as_u64().unwrap_or(0)
        + lag["decision"]["breaches"].as_u64().unwrap_or(0);
    let mut line = format!(
        "{ops} ops · decision p99 {} · durable p99 {} · gate {}",
        ms(decision),
        ms(durable),
        ms(gate)
    );
    if breaches > 0 {
        line.push_str(&format!(
            " · {}",
            style.red(&format!("{breaches} BREACHES"))
        ));
    }
    // A lag log that cannot be written is a measurement this node is
    // silently not keeping. Worth a line: the absence of breaches in a
    // log nobody could write is not evidence of none.
    if lag["log_write_failures"].as_u64().unwrap_or(0) > 0 {
        let why = lag["log_error"].as_str().unwrap_or("unknown");
        line.push_str(&format!(
            " · {}",
            style.red(&format!("lag log unwritable: {why}"))
        ));
    }
    line
}

/// How much this node is holding, as far as this credential may see.
///
/// Qualified rather than absolute on purpose: `refs`, `reviews` and
/// `workspaces` are all `Disclosure::PerRepo`, so these are counts of
/// the visible subset. Printing them as totals would make a narrowly
/// scoped credential look at an almost-empty node.
fn holds_line(view: &Value) -> String {
    let count = |name: &str| -> String {
        match &view[name] {
            Value::Object(map) => map.len().to_string(),
            Value::Array(rows) => rows.len().to_string(),
            _ => "—".to_string(),
        }
    };
    format!(
        "{} refs · {} reviews · {} workspaces  (visible to this credential)",
        count("refs"),
        count("reviews"),
        count("workspaces")
    )
}

/// Shortens a `ContentHash` hex without destroying what it says.
///
/// The wire form is `<codec byte>-<digest>` (invariant 2: a hash always
/// carries its codec, so a hash-function change stays additive and a git
/// oid never pretends to be BLAKE3). Truncating the whole string eats
/// into the digest by however many characters the prefix took, which
/// prints a different number of real bytes for a BLAKE3 hash than for a
/// git oid. Keep the prefix whole and shorten only the digest.
fn short_hash(hex: &str) -> String {
    match hex.split_once('-') {
        Some((codec, digest)) => {
            format!("{codec}-{}", digest.chars().take(12).collect::<String>())
        }
        None => hex.chars().take(12).collect(),
    }
}

/// The sequencer's own position.
fn seq_line(view: &Value, style: Style) -> String {
    let log = &view["log"];
    let next = log["next_seq"].as_u64();
    let head = log["head"].as_str().map(short_hash);
    match (next, head) {
        (Some(next), Some(head)) => format!("seq {next} · head {head}"),
        (Some(next), None) => format!("seq {next} · {}", style.dim("empty log")),
        _ => style.dim("not disclosed"),
    }
}

/// The whole report, as `choir node status` prints it.
///
/// Takes the two responses rather than fetching them, so the rendering
/// is a pure function of what the node said and can be tested without a
/// node.
#[must_use]
pub fn status_report(api: &str, health: Health, view: &Value, style: Style) -> String {
    let rows: Vec<(&str, String)> = vec![
        ("node", api.to_string()),
        ("health", health.paint(style)),
        ("build", build_line(view, style)),
        ("position", seq_line(view, style)),
        ("lag", lag_line(view, style)),
        ("holds", holds_line(view)),
    ];
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (key, value) in rows {
        let key = format!("{key:width$}");
        out.push_str(&format!("  {}  {value}\n", style.dim(&key)));
    }
    out
}

/// `choir node status [<api>]`.
///
/// Two requests: `/healthz`, which performs the node's own durability
/// check, and `/api/view`, which carries everything else. They are
/// separate because they answer different questions and one can be
/// withheld without the other — a credential that may not read the view
/// can still be told the node is alive.
///
/// # Errors
///
/// Returns a description when the node cannot be reached or its view
/// does not parse.
pub fn status(api: &str, auth: Option<&std::path::Path>) -> Result<(Health, Value), String> {
    let client = crate::mcp::HttpClient::new(api, auth, None)?;
    let health = match client.get("/healthz") {
        Ok((200, _)) => Health::Healthy,
        Ok((503, _)) => Health::Unhealthy,
        Ok((401 | 403, _)) => Health::Undisclosed,
        Ok(_) => Health::Unreachable,
        Err(error) => return Err(error),
    };
    let (code, body) = client.get("/api/view")?;
    if !(200..300).contains(&code) {
        return Err(format!("GET /api/view returned {code}: {body}"));
    }
    let view: Value =
        serde_json::from_str(&body).map_err(|error| format!("the view did not parse: {error}"))?;
    Ok((health, view))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style::plain()
    }

    /// A view with everything present, as a node-wide credential sees it.
    fn full_view() -> Value {
        serde_json::json!({
            "log": { "next_seq": 41, "head": "1e-a7c634769ddf0011223344556677" },
            "build": { "commit": "3b95afe0c6d0aaaabbbbccccddddeeeeffff0000",
                       "source": "git", "dirty": false, "dirty_trusted": true },
            "sequencer_lag": {
                "gate_us": 100_000, "observed_ops": 412,
                "decision": { "p50_us": 512, "p99_us": 604, "breaches": 0 },
                "durable":  { "p50_us": 4096, "p99_us": 8400, "breaches": 0 },
                "log_write_failures": 0,
            },
            "refs": { "a": 1, "b": 2 },
            "reviews": {},
            "workspaces": { "w": 1 },
        })
    }

    /// The trap this command exists to avoid.
    ///
    /// `sequencer_lag` is `Disclosure::NodeWide`, so a per-repo
    /// credential is handed a view with the section absent. "Absent"
    /// and "idle" are the same JSON and opposite facts, and a status
    /// line that renders the first as the second tells an operator
    /// their sequencer is quiet when they simply may not look.
    #[test]
    fn a_withheld_lag_section_says_withheld_not_idle() {
        let mut view = full_view();
        view.as_object_mut()
            .expect("object")
            .remove("sequencer_lag");
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(out.contains("not disclosed"), "{out}");
        assert!(
            !out.contains("no ops"),
            "withheld must not read as idle:\n{out}"
        );
    }

    /// A node that is genuinely idle says so, and differently.
    #[test]
    fn an_idle_node_is_distinguishable_from_a_withheld_one() {
        let mut view = full_view();
        view["sequencer_lag"]["observed_ops"] = serde_json::json!(0);
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(out.contains("no ops measured since start"), "{out}");
        assert!(!out.contains("not disclosed"), "{out}");
    }

    /// Both percentiles, always.
    ///
    /// `decision` is the Phase-0 gate as written; `durable` is what a
    /// submitter actually waits out. A node with a slow disk looks fast
    /// if only the first is printed.
    #[test]
    fn both_latency_forms_are_reported_with_the_gate() {
        let out = status_report("http://n", Health::Healthy, &full_view(), plain());
        assert!(out.contains("decision p99 0.6 ms"), "{out}");
        assert!(out.contains("durable p99 8.4 ms"), "{out}");
        assert!(
            out.contains("gate 100.0 ms"),
            "the gate is the comparison:\n{out}"
        );
    }

    /// Breaches are never quiet.
    #[test]
    fn gate_breaches_are_named() {
        let mut view = full_view();
        view["sequencer_lag"]["durable"]["breaches"] = serde_json::json!(3);
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(out.contains("3 BREACHES"), "{out}");
    }

    /// A lag log that cannot be written is a measurement not being kept,
    /// and the absence of breaches in it is not evidence of none.
    #[test]
    fn an_unwritable_lag_log_is_reported() {
        let mut view = full_view();
        view["sequencer_lag"]["log_write_failures"] = serde_json::json!(2);
        view["sequencer_lag"]["log_error"] = serde_json::json!("permission denied");
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(
            out.contains("lag log unwritable: permission denied"),
            "{out}"
        );
    }

    /// An unstamped binary is called out rather than printed as a commit
    /// called "unknown".
    #[test]
    fn an_unstamped_build_says_so() {
        let mut view = full_view();
        view["build"]["commit"] = serde_json::json!("unknown");
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(out.contains("UNSTAMPED"), "{out}");
    }

    /// `dirty` is only a claim when the stamp came from git.
    ///
    /// A stamp from anywhere else never looked at a working tree, so
    /// printing "clean" on its word invents an assurance nobody made.
    #[test]
    fn a_dirty_flag_from_an_untrusted_stamp_is_not_believed() {
        let mut view = full_view();
        view["build"]["source"] = serde_json::json!("env");
        view["build"]["dirty_trusted"] = serde_json::json!(false);
        let out = status_report("http://n", Health::Healthy, &view, plain());
        assert!(out.contains("tree unknown"), "{out}");
        assert!(!out.contains("clean"), "{out}");
    }

    /// Shortening a hash keeps its codec, because the codec is what says
    /// which hash function produced the digest (invariant 2).
    #[test]
    fn shortening_a_hash_keeps_its_codec() {
        assert_eq!(short_hash("1e-a7c634769ddf0011223344"), "1e-a7c634769ddf");
        // A git oid carries a different codec and must stay
        // distinguishable from a BLAKE3 digest at a glance.
        assert_eq!(short_hash("70-0123456789abcdef0123"), "70-0123456789ab");
        assert_eq!(short_hash("nodash"), "nodash");
    }

    /// Counts are stated as what this credential can see.
    ///
    /// `refs`, `reviews` and `workspaces` are `Disclosure::PerRepo`, so
    /// an unqualified total would make a narrowly scoped credential look
    /// at an almost-empty node.
    #[test]
    fn counts_are_qualified_by_what_the_credential_sees() {
        let out = status_report("http://n", Health::Healthy, &full_view(), plain());
        assert!(out.contains("2 refs"), "{out}");
        assert!(out.contains("visible to this credential"), "{out}");
    }

    /// 503 on `/healthz` is the node reporting its own durability
    /// failure — the state that exits the daemon 75 for a supervisor.
    /// It is the one answer this command must not soften.
    #[test]
    fn an_unhealthy_node_fails_the_command() {
        assert_eq!(Health::Unhealthy.exit_code(), 1);
        assert_eq!(Health::Unreachable.exit_code(), 1);
        assert_eq!(Health::Healthy.exit_code(), 0);
        // Undisclosed is a node that is up and a credential that may not
        // ask. Not knowing is not the same as being broken.
        assert_eq!(Health::Undisclosed.exit_code(), 0);
        let out = status_report("http://n", Health::Unhealthy, &full_view(), plain());
        assert!(out.contains("durable append is failing"), "{out}");
    }
}
