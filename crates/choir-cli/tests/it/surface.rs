//! Every generated description of the agent surface must match the table
//! it came from. Three hand-maintained descriptions of one surface drift;
//! this is the thing that notices.

use choir_cli::surface;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn assert_local_links_resolve(root: &std::path::Path, rel: &str) {
    let path = root.join(rel);
    let doc = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    let mut rest = doc.as_str();
    while let Some(open) = rest.find("](") {
        rest = &rest[open + 2..];
        let close = rest
            .find(')')
            .unwrap_or_else(|| panic!("{rel} has an unterminated Markdown link"));
        let raw = &rest[..close];
        rest = &rest[close + 1..];
        let target = raw
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(['<', '>'])
            .split('#')
            .next()
            .unwrap_or_default();
        if target.is_empty()
            || target.starts_with('/')
            || target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
        {
            continue;
        }
        assert!(
            path.parent()
                .expect("document parent")
                .join(target)
                .exists(),
            "{rel} links to missing local target `{raw}`"
        );
    }
}

#[test]
fn every_generated_artifact_is_current() {
    let root = repo_root();
    let artifacts = surface::artifacts(&root).expect("render artifacts");
    assert!(
        artifacts.len() >= 6,
        "expected every artifact, got {}",
        artifacts.len()
    );
    let mut stale = Vec::new();
    for (path, expected) in artifacts {
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        if actual != expected {
            stale.push(
                path.strip_prefix(&root)
                    .expect("generated artifact stays under the repository")
                    .display()
                    .to_string(),
            );
        }
    }
    assert!(
        stale.is_empty(),
        "stale generated artifacts: {stale:?}\n\
         run: cargo run -p choir-cli --example gen-surface"
    );
}

/// Each claim that used to be asserted against the README, checked
/// against the document it moved to.
///
/// The assertions did not get weaker when the README was split; they got
/// addressed. A string checked against "some file in the repository"
/// would pass while the sentence sat on a page no reader of that topic
/// opens, which is the failure a documentation split actually has.
#[test]
fn each_claim_stayed_with_its_topic() {
    let root = repo_root();
    for (rel, required) in [
        // The primary-path statement belongs beside the surface it is
        // about, not beside the install instructions.
        (
            "docs/using/cli.md",
            "The signed-operation API is the primary agent path",
        ),
        ("docs/using/cli.md", "git push` remains the compatibility"),
        (
            "docs/using/cli.md",
            "choir-mcp http://127.0.0.1:8417 --auth-file",
        ),
        // Heading level, not just presence: the generated block sits
        // under a `##` on this page and under nothing in the README, so
        // a renderer change that re-flattened it would go unnoticed.
        ("docs/using/cli.md", "### The `choir` CLI"),
        (
            "docs/operating/running-a-node.md",
            "configured invocations must supply both `<repo-root>` and `<port>`",
        ),
        (
            "docs/operating/running-a-node.md",
            "trusted keys, channel bindings, push-certificate signers",
        ),
        (
            "docs/operating/running-a-node.md",
            "--review-retention <count>",
        ),
        ("docs/using/workflow.md", "total_authoritative_view"),
    ] {
        let doc =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        assert!(doc.contains(required), "{rel} omits `{required}`");
    }
}

/// What the README itself still owes a first-time reader.
#[test]
fn readme_keeps_getting_started_and_the_complete_gate() {
    let root = repo_root();
    let readme = std::fs::read_to_string(root.join("README.md")).expect("README.md");

    let gate = readme
        .split_once("Full release gate:\n\n```bash\n")
        .and_then(|(_, rest)| rest.split_once("\n```").map(|(gate, _)| gate))
        .expect("README.md has a Full release gate shell block");
    assert!(
        gate.lines().any(|line| line == "./gate"),
        "README.md must point at the fail-closed gate rather than duplicate a partial command list"
    );

    // The README's job after the split is to hand the reader off. A
    // README that stops naming the index is one that has quietly become
    // the documentation again.
    assert!(
        readme.contains("docs/README.md"),
        "README.md must point at the documentation index"
    );

    // Every page under docs/ is reachable from the README or from the
    // index, and the index is reachable from the README. Reachability is
    // the property; a page nobody links is a page nobody reads.
    let index = std::fs::read_to_string(root.join("docs/README.md")).expect("docs/README.md");
    let mut unlinked = Vec::new();
    for entry in walk_docs(&root.join("docs")) {
        let rel = entry
            .strip_prefix(&root)
            .expect("doc stays under the repository")
            .to_string_lossy()
            .replace('\\', "/");
        // Both of these are navigation rather than pages: `README.md`
        // is the index itself, and `SUMMARY.md` is the book's table of
        // contents. Requiring the index to link to the table of
        // contents that links to the index is a cycle, not a check.
        if rel == "docs/README.md" || rel == "docs/SUMMARY.md" {
            continue;
        }
        // The index links relative to itself, the README relative to the
        // root; accept either spelling of the same page.
        let from_index = rel.trim_start_matches("docs/");
        if !index.contains(from_index) && !readme.contains(&rel) {
            unlinked.push(rel);
        }
    }
    assert!(
        unlinked.is_empty(),
        "documentation pages nothing links to: {unlinked:?}"
    );
}

/// Every page the book's table of contents names is a page that exists,
/// and every page that exists is in the table of contents.
///
/// mdBook is configured with `create-missing = false`, so the first half
/// is also caught by the gate's book stage — but only on a machine that
/// has mdbook installed, and the gate announces a skip when it does not.
/// This half runs everywhere.
///
/// The second half is the one mdBook cannot check at all: a page added
/// under `docs/` and never listed builds fine and is simply absent from
/// the book, reachable only by typing its URL.
#[test]
fn the_book_lists_every_page_and_only_real_ones() {
    let root = repo_root();
    let summary = std::fs::read_to_string(root.join("docs/SUMMARY.md")).expect("docs/SUMMARY.md");

    let mut missing = Vec::new();
    let mut listed = std::collections::HashSet::new();
    let mut rest = summary.as_str();
    while let Some(open) = rest.find("](") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find(')') else { break };
        let target = rest[..close].split('#').next().unwrap_or_default();
        rest = &rest[close + 1..];
        if target.is_empty() || target.starts_with("http") {
            continue;
        }
        if !root.join("docs").join(target).is_file() {
            missing.push(target.to_string());
        }
        listed.insert(format!("docs/{target}"));
    }
    assert!(
        missing.is_empty(),
        "SUMMARY.md names pages that do not exist: {missing:?}"
    );

    let mut unlisted = Vec::new();
    for path in walk_docs(&root.join("docs")) {
        let rel = path
            .strip_prefix(&root)
            .expect("doc stays under the repository")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "docs/SUMMARY.md" || rel == "docs/README.md" {
            continue;
        }
        if !listed.contains(&rel) {
            unlisted.push(rel);
        }
    }
    assert!(
        unlisted.is_empty(),
        "pages under docs/ that the book never shows: {unlisted:?}\n\
         add them to docs/SUMMARY.md"
    );
}

/// The book's palette is the daemon's palette, cut at the right place.
///
/// [`choir_node::ui_tokens_css`] slices `ui.css` at its reset marker and
/// widens two selectors for mdBook's theme classes. Each of those steps
/// fails loudly inside the function, but only when it is *called* — and
/// the thing that calls it is artifact generation, which a reader can
/// forget to run. These are the properties the book depends on.
#[test]
fn the_books_tokens_are_the_daemons_tokens() {
    let css = choir_node::ui_tokens_css();

    // Tokens, and nothing after them: the component styles below the
    // reset are the half that must not be shared.
    assert!(
        css.contains("--accent-ink"),
        "the token block lost its palette"
    );
    assert!(
        css.contains("--measure"),
        "the token block lost the docs prose measure the book's width uses"
    );
    assert!(
        !css.contains("global reset"),
        "the cut let the reset through, so component styles are in the book"
    );
    assert!(
        !css.contains("main>section"),
        "component styles reached the book's stylesheet"
    );

    // The widening, without which mdBook's picker changes nothing.
    for theme in ["coal", "navy", "ayu"] {
        assert!(
            css.contains(&format!(":root.{theme}:not([data-theme=\"light\"])")),
            "mdBook's `{theme}` theme is not mapped to the dark palette"
        );
    }
    for theme in ["light", "rust"] {
        assert!(
            css.contains(&format!(":root.{theme}:not([data-theme=\"dark\"])")),
            "mdBook's `{theme}` theme is not mapped to the light palette"
        );
    }
}

/// GitHub's heading-slug rule, which is what a `#fragment` in these
/// files is written against.
///
/// Lowercase, drop every character that is not alphanumeric, a hyphen or
/// an underscore, and turn spaces into hyphens. Runs of hyphens are
/// *kept*, not collapsed: `(`--journal`)` slugs to `---journal`, and a
/// checker that collapsed them would call a working link broken.
fn slug(heading: &str) -> String {
    let mut out = String::new();
    for ch in heading.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ' ' {
            out.push('-');
        }
    }
    out
}

/// The slug of every heading in one document, skipping code fences.
fn heading_slugs(doc: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut inside = false;
    for line in doc.lines() {
        if line.starts_with("```") {
            inside = !inside;
            continue;
        }
        if inside || !line.starts_with('#') {
            continue;
        }
        let text = line.trim_start_matches('#').trim();
        if !text.is_empty() {
            out.insert(slug(text));
        }
    }
    out
}

/// Every `#fragment` link points at a heading that exists.
///
/// [`assert_local_links_resolve`] drops the fragment before checking, so
/// it cannot see this, and a same-page anchor has no file part for it to
/// check at all. Splitting the README into pages broke exactly two links
/// this way: the D36 credential section and the D31 SSH section referred
/// to each other as same-page anchors, and after the split each anchor
/// named a heading that had moved to the other file. Both still rendered
/// as links, and both went nowhere.
#[test]
fn every_doc_anchor_names_a_real_heading() {
    let root = repo_root();
    let mut docs: Vec<std::path::PathBuf> = walk_docs(&root.join("docs"));
    docs.push(root.join("README.md"));

    let mut broken = Vec::new();
    for path in &docs {
        let doc = std::fs::read_to_string(path).expect("read doc");
        let rel = path
            .strip_prefix(&root)
            .expect("doc stays under the repository")
            .display()
            .to_string();
        let mut rest = doc.as_str();
        while let Some(open) = rest.find("](") {
            rest = &rest[open + 2..];
            let Some(close) = rest.find(')') else { break };
            let raw = &rest[..close];
            rest = &rest[close + 1..];
            let Some((target, fragment)) = raw.split_once('#') else {
                continue;
            };
            if fragment.is_empty()
                || target.starts_with("http://")
                || target.starts_with("https://")
            {
                continue;
            }
            // An empty target is this same document.
            let owner = if target.is_empty() {
                path.clone()
            } else {
                path.parent().expect("document parent").join(target)
            };
            let Ok(owner_doc) = std::fs::read_to_string(&owner) else {
                // A missing file is assert_local_links_resolve's finding,
                // not this one; reporting it twice helps nobody.
                continue;
            };
            if !heading_slugs(&owner_doc).contains(fragment) {
                broken.push(format!("{rel} -> {raw}"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "links whose `#fragment` names no heading in the file it points at: {broken:?}"
    );
}

/// Every code fence under `docs/` names its language.
///
/// These pages are included into rustdoc with `#![doc = include_str!]`,
/// and rustdoc reads an *untagged* fence as Rust. Five of them arrived
/// that way when this material moved out of the README, where nothing
/// compiled it, and `cargo doc` refused the workspace.
///
/// That refusal is not the case this test covers. A fence full of ACL
/// columns fails to parse and is caught; a fence whose contents happen
/// to parse as Rust becomes a **silent doctest**, compiled and run by
/// the gate, and the first thing anyone learns about it is a failure
/// somewhere that has nothing to do with the change that caused it.
#[test]
fn every_doc_code_fence_names_its_language() {
    let root = repo_root();
    let mut untagged = Vec::new();
    for path in walk_docs(&root.join("docs")) {
        let doc = std::fs::read_to_string(&path).expect("read doc");
        let rel = path
            .strip_prefix(&root)
            .expect("doc stays under the repository")
            .display()
            .to_string();
        let mut inside = false;
        for (n, line) in doc.lines().enumerate() {
            if !line.starts_with("```") {
                continue;
            }
            if inside {
                inside = false;
            } else {
                inside = true;
                if line.trim_end().len() == 3 {
                    untagged.push(format!("{rel}:{}", n + 1));
                }
            }
        }
    }
    assert!(
        untagged.is_empty(),
        "untagged code fences rustdoc will read as Rust: {untagged:?}\n\
         tag each one (```text, ```bash, ```json), or ```rust if it is meant to compile"
    );
}

/// Every `.md` under `docs/`, recursively.
fn walk_docs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_docs(&path));
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// The cheat-sheet names commands that exist.
///
/// [`surface::DAY_ONE`] is a list of names rather than a field on each
/// command, so nothing in the type system stops a rename from turning an
/// entry into a row that renders as nothing at all.
#[test]
fn every_day_one_command_exists() {
    for name in surface::DAY_ONE {
        assert!(
            surface::COMMANDS.iter().any(|c| &c.name == name),
            "DAY_ONE names `{name}`, which is not a command"
        );
    }
    let sheet = surface::readme_cheatsheet();
    for name in surface::DAY_ONE {
        assert!(
            sheet.contains(&format!("`choir {name}`")),
            "the cheat-sheet dropped `{name}`"
        );
    }
}

#[test]
fn local_only_files_stay_ignored() {
    let root = repo_root();
    // `internal/` is private in full, with no allowlist. The four files
    // below were once excepted back into the public set, which is exactly
    // how they came to be tracked; naming them here means re-adding any
    // such exception fails rather than being noticed after publication.
    for local in [
        "internal/STATUS.md",
        "internal/design.md",
        "internal/integration-workflows.md",
        "internal/measurements.md",
        "internal/plan.md",
        "internal/PHASE0.md",
        ".codex/config.toml",
    ] {
        let ignored = std::process::Command::new("git")
            .args(["check-ignore", "--no-index", local])
            .current_dir(&root)
            .output()
            .expect("git check-ignore runs");
        assert!(
            ignored.status.success(),
            "local file became publishable: {local}"
        );
    }

    // Nothing under `internal/` may be tracked, whatever the ignore rules
    // say: a file added before a rule exists stays tracked forever.
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "internal/"])
        .current_dir(&root)
        .output()
        .expect("git ls-files runs");
    assert!(
        tracked.stdout.is_empty(),
        "private files are tracked: {}",
        String::from_utf8_lossy(&tracked.stdout)
    );

    for public in ["agents.md", "DECISIONS.md", "LICENSE-MIT", "LICENSE-APACHE"] {
        let check = std::process::Command::new("git")
            .args(["check-ignore", "--no-index", public])
            .current_dir(&root)
            .output()
            .expect("git check-ignore runs");
        assert!(!check.status.success(), "public file is ignored: {public}");
    }
    assert!(
        root.join("agents.md").is_file(),
        "generated agents.md is missing"
    );
    assert!(
        !root.join("AGENT_GUIDE.md").exists(),
        "the transport brief requires the root artifact to remain agents.md"
    );
}

#[test]
fn local_markdown_links_resolve() {
    let root = repo_root();
    for rel in [
        "README.md",
        "SYNC.md",
        "DECISIONS.md",
        "ERRORS.md",
        "crates/choir-bridge/PERMISSIONS.md",
        "scripts/flip/RUNBOOK.md",
        "templates/README.md",
    ] {
        assert_local_links_resolve(&root, rel);
    }
    // Enumerated rather than listed, so a page added under `docs/` is
    // covered the moment it exists. The list above cannot do that: those
    // files live in five different directories and there is no rule that
    // finds them.
    for path in walk_docs(&root.join("docs")) {
        let rel = path
            .strip_prefix(&root)
            .expect("doc stays under the repository")
            .to_string_lossy()
            .replace('\\', "/");
        assert_local_links_resolve(&root, &rel);
    }
}

#[test]
fn operator_and_template_guidance_matches_the_shipped_paths() {
    let root = repo_root();
    let runbook =
        std::fs::read_to_string(root.join("scripts/flip/RUNBOOK.md")).expect("flip runbook");
    for required in [
        "current branch by name",
        "target/release/choir --auth-file",
        "push-certificate signers also hot-reload",
        "[sync contract](../../SYNC.md)",
    ] {
        assert!(runbook.contains(required), "runbook omits `{required}`");
    }

    let templates =
        std::fs::read_to_string(root.join("templates/README.md")).expect("template guide");
    for required in [
        "Optional MCP adapter",
        "Claude Code isolated workspaces",
        "WorktreeCreate",
        "settings.worktree.example.json",
        "--auth-file <path> --auth-user <name>",
        "/api/submit-batch",
    ] {
        assert!(
            templates.contains(required),
            "template guide omits `{required}`"
        );
    }
    let env_template =
        std::fs::read_to_string(root.join("templates/choir.env.sh")).expect("environment template");
    assert!(
        !env_template.contains("CHOIR_AUTH_FILE"),
        "CLI and MCP auth must stay explicit flags plus a named file"
    );
}

#[test]
fn the_binarys_help_is_the_tables_help() {
    // A CLI whose help disagrees with the README is the drift this exists
    // to stop, so check the shipped binary rather than the function.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .output()
        .expect("choir runs");
    let printed = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        printed,
        surface::usage(),
        "binary help drifted from the table"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "bare invocation is a usage error"
    );
}

#[test]
fn every_command_appears_in_the_agent_facing_docs() {
    // A command that exists but is documented nowhere is invisible to an
    // agent; one documented but missing is a broken instruction.
    let agents = surface::agents_md();
    let llms = surface::llms_txt();
    for c in surface::COMMANDS {
        assert!(llms.contains(c.name), "llms.txt omits `{}`", c.name);
        if c.agent_facing {
            assert!(agents.contains(c.name), "agents.md omits `{}`", c.name);
        }
    }
    for e in surface::ENDPOINTS {
        assert!(agents.contains(e.path), "agents.md omits `{}`", e.path);
        assert!(llms.contains(e.path), "llms.txt omits `{}`", e.path);
    }
}

#[test]
fn mcp_tools_cover_public_operations_once_in_table_order() {
    // Order is a protocol property here: clients inject this array into
    // prompts, so a reorder loses prompt-cache hits even when the set is
    // identical. Derive the expected order from the endpoint table, not
    // from a separately-maintained name list.
    let tools = surface::mcp_tools();
    let actual: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    let expected: Vec<&str> = surface::ENDPOINTS
        .iter()
        .filter_map(|endpoint| endpoint.mcp.as_ref().map(|tool| tool.name))
        .collect();
    assert_eq!(
        actual, expected,
        "MCP tool order drifted from endpoint order"
    );
    assert_eq!(
        tools,
        surface::mcp_tools(),
        "MCP rendering is not deterministic"
    );

    let unique: std::collections::BTreeSet<_> = actual.iter().copied().collect();
    assert_eq!(unique.len(), actual.len(), "duplicate MCP tool name");
    // Nine since D17 added `/api/schema`: "what will this node accept"
    // is a question an agent asks before branching, and routing it
    // through the tool client is what keeps a credential off a `curl`
    // command line in the generated shell library. The count is
    // hardcoded so that adding a tool is a decision somebody writes
    // down, which is exactly what it forced here.
    //
    // Ten since `/api/search`, which is the first tool that is not a
    // platform operation at all: it reads git and needs no sequencer.
    // It is here rather than left to the browser because the reader
    // this platform is built for cannot open one, and a code host its
    // primary reader cannot search is a code host with a hole in it.
    assert_eq!(
        actual.len(),
        10,
        "only the ten public read and platform operations are tools"
    );
    for (tool, endpoint) in tools
        .iter()
        .zip(surface::ENDPOINTS.iter().filter(|e| e.mcp.is_some()))
    {
        assert_eq!(tool["description"], endpoint.purpose);
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(
            surface::mcp_endpoint(tool["name"].as_str().expect("name")).is_some(),
            "listed tool has no HTTP endpoint"
        );
    }

    // New clients say `channel`; the frozen v1 transport spelling remains
    // discoverable so existing MCP callers keep validating.
    let submit = tools
        .iter()
        .find(|tool| tool["name"] == "choir_submit")
        .expect("submit tool");
    assert!(submit["inputSchema"]["properties"]["channel"].is_object());
    assert!(submit["inputSchema"]["properties"]["workspace"].is_object());
    assert_eq!(
        submit["inputSchema"]["anyOf"].as_array().map(Vec::len),
        Some(2)
    );
    let batch = tools
        .iter()
        .find(|tool| tool["name"] == "choir_submit_batch")
        .expect("batch tool");
    let batch_item = &batch["inputSchema"]["properties"]["ops"]["items"];
    assert!(batch_item["properties"]["channel"].is_object());
    assert!(batch_item["properties"]["workspace"].is_object());
    assert_eq!(batch_item["anyOf"].as_array().map(Vec::len), Some(2));
    let workspace = tools
        .iter()
        .find(|tool| tool["name"] == "choir_workspace")
        .expect("workspace tool");
    for field in ["base", "owner", "change", "idempotency_key"] {
        assert!(
            workspace["inputSchema"]["properties"][field].is_object(),
            "workspace schema omits {field}"
        );
    }

    // Discovery documents are already served directly; the hook is
    // privileged and internal. None belongs in model-controlled tools.
    for path in ["/llms.txt", "/sync.md", "/api/git-update"] {
        let endpoint = surface::ENDPOINTS
            .iter()
            .find(|endpoint| endpoint.path == path)
            .expect("documented endpoint");
        assert!(endpoint.mcp.is_none(), "{path} must not be an MCP tool");
    }
}

#[test]
fn splicing_refuses_a_file_without_markers_rather_than_appending() {
    // Appending would produce two generated regions and a file that
    // regenerates differently every run.
    assert!(surface::splice("no markers here", "x").is_err());
    assert!(surface::splice(&format!("{} only start", surface::GEN_START), "x").is_err());
    let reversed = format!("{} then {}", surface::GEN_END, surface::GEN_START);
    assert!(surface::splice(&reversed, "x").is_err());

    let doc = format!(
        "before\n{}\nold\n{}\nafter",
        surface::GEN_START,
        surface::GEN_END
    );
    let spliced = surface::splice(&doc, "new").expect("splice");
    assert!(spliced.contains("before") && spliced.contains("after"));
    assert!(spliced.contains("new") && !spliced.contains("old"));
    // Idempotent: regenerating an already-generated file changes nothing.
    assert_eq!(surface::splice(&spliced, "new").expect("splice"), spliced);
}

#[test]
fn every_rejection_code_the_node_can_emit_is_documented() {
    // A client branching on a code that is not in ERRORS.md fails in the
    // least debuggable way available, so the table and the enum must not
    // be able to disagree.
    let doc = std::fs::read_to_string(repo_root().join("ERRORS.md")).expect("ERRORS.md");
    for code in choir_node::reject::Code::all() {
        assert!(
            doc.contains(&format!("`{}`", code.as_str())),
            "ERRORS.md omits `{}`",
            code.as_str()
        );
        assert!(!code.action().is_empty(), "{code:?} has no action");
        assert!(!code.meaning().is_empty(), "{code:?} has no meaning");
    }
    // And every code the node emits appears in the node's own source, so
    // a documented-but-dead code shows up as a missing constructor. The
    // search covers every `.rs` in the crate rather than a hand-listed
    // pair of modules: a code constructed in a module the list forgot is
    // live code that reads as dead, and the failure then accuses the
    // wrong thing.
    let src = node_sources();
    for code in choir_node::reject::Code::all() {
        let name = format!("{code:?}");
        assert!(
            src.contains(&format!("Code::{name}")),
            "`{name}` is documented but never constructed"
        );
    }
}

/// Every Rust source file in `choir-node`, concatenated.
fn node_sources() -> String {
    let mut src = String::new();
    let mut dirs = vec![repo_root().join("crates/choir-node/src")];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                src.push_str(
                    &std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {path:?}: {e}")),
                );
            }
        }
    }
    assert!(!src.is_empty(), "no node sources found");
    src
}

/// The generated shell library has to be a *shell* file, which is not
/// something the staleness test can notice: a byte-perfect artifact that
/// `sh` refuses is still stale in the only way that matters to whoever
/// sources it.
///
/// Written after the first generated copy spliced in cleanly, compared
/// equal, and failed `sh -n` at the marker — the markers were HTML
/// comments, which are a syntax error in shell rather than a comment.
#[test]
fn the_generated_shell_library_parses_as_shell() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("templates/shell/choir.sh");
    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("sh runs");
    assert!(
        out.status.success(),
        "the shell library does not parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Every agent-facing command has a function, and the name is the
    // command with hyphens replaced — a hyphen is not portable in a
    // shell function name.
    let text = std::fs::read_to_string(&path).expect("readable");
    for command in surface::COMMANDS.iter().filter(|c| c.agent_facing) {
        let function = format!("choir_{}()", command.name.replace('-', "_"));
        assert!(
            text.contains(&function),
            "no wrapper for `{}`: expected `{function}`",
            command.name
        );
    }

    // The hand-written flows live outside the generated region and must
    // survive regeneration, which is the whole point of the split.
    for flow in [
        "choir_submit_all()",
        "choir_verify_log()",
        "choir_capabilities()",
    ] {
        assert!(text.contains(flow), "a hand-written flow was lost: {flow}");
    }

    // And no credential is ever interpolated into a command line. The
    // first draft of this file did exactly that — `cat`-ing the auth
    // file into a `curl -u` argument, where `ps` shows it to every
    // process on the machine.
    assert!(
        !text.contains("$(cat \"$CHOIR_AUTH_FILE\")") && !text.contains("-u $"),
        "a credential reaches a command line: {text}"
    );
}
