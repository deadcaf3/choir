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
            path.parent().expect("document parent").join(target).exists(),
            "{rel} links to missing local target `{raw}`"
        );
    }
}

#[test]
fn every_generated_artifact_is_current() {
    let root = repo_root();
    let artifacts = surface::artifacts(&root).expect("render artifacts");
    assert!(artifacts.len() >= 6, "expected every artifact, got {}", artifacts.len());
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

#[test]
fn readme_keeps_the_primary_path_and_complete_gate() {
    let readme = std::fs::read_to_string(repo_root().join("README.md")).expect("README.md");
    for required in [
        "The signed-operation API is the primary agent path",
        "git push` remains the compatibility",
        "choir-mcp http://127.0.0.1:8417 --auth-file",
        "configured invocations must supply both `<repo-root>` and `<port>`",
        "trusted keys, channel bindings, push-certificate signers",
        "--review-retention <count>",
        "total_authoritative_view",
        "#### The `choir` CLI",
    ] {
        assert!(readme.contains(required), "README.md omits `{required}`");
    }
    let gate = readme
        .split_once("Full gate:\n\n```bash\n")
        .and_then(|(_, rest)| rest.split_once("\n```").map(|(gate, _)| gate))
        .expect("README.md has a Full gate shell block");
    for command in [
        "cargo test --workspace",
        "cargo clippy --workspace --all-targets",
        "cargo run -p choir-spike --release",
    ] {
        assert!(
            gate.lines().any(|line| line == command),
            "README.md Full gate omits `{command}`"
        );
    }
    for path in [
        "internal/design.md",
        "internal/integration-workflows.md",
        "internal/measurements.md",
    ] {
        assert!(
            repo_root().join(path).is_file(),
            "README target is missing: {path}"
        );
    }
}

#[test]
fn local_only_files_stay_ignored() {
    let root = repo_root();
    for local in ["internal/STATUS.md", ".codex/config.toml"] {
        let ignored = std::process::Command::new("git")
            .args(["check-ignore", "--no-index", local])
            .current_dir(&root)
            .output()
            .expect("git check-ignore runs");
        assert!(ignored.status.success(), "local file became publishable: {local}");
    }

    for public in [
        "agents.md",
        "internal/design.md",
        "internal/integration-workflows.md",
        "internal/measurements.md",
    ] {
        let check = std::process::Command::new("git")
            .args(["check-ignore", "--no-index", public])
            .current_dir(&root)
            .output()
            .expect("git check-ignore runs");
        assert!(
            !check.status.success(),
            "tracked design doc is ignored: {public}"
        );
    }
    assert!(root.join("agents.md").is_file(), "generated agents.md is missing");
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
        "crates/choir-bridge/PERMISSIONS.md",
        "scripts/flip/RUNBOOK.md",
        "templates/README.md",
    ] {
        assert_local_links_resolve(&root, rel);
    }
}

#[test]
fn operator_and_template_guidance_matches_the_shipped_paths() {
    let root = repo_root();
    let runbook = std::fs::read_to_string(root.join("scripts/flip/RUNBOOK.md"))
        .expect("flip runbook");
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
        assert!(templates.contains(required), "template guide omits `{required}`");
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
    assert_eq!(printed, surface::usage(), "binary help drifted from the table");
    assert_eq!(out.status.code(), Some(2), "bare invocation is a usage error");
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
    assert_eq!(
        actual.len(),
        8,
        "only the eight public platform operations are tools"
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

    let doc = format!("before\n{}\nold\n{}\nafter", surface::GEN_START, surface::GEN_END);
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
                src.push_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}")));
            }
        }
    }
    assert!(!src.is_empty(), "no node sources found");
    src
}
