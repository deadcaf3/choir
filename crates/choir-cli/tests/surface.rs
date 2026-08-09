//! Every generated description of the agent surface must match the table
//! it came from. Three hand-maintained descriptions of one surface drift;
//! this is the thing that notices.

use choir_cli::surface;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
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
            stale.push(path.display().to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "stale generated artifacts: {stale:?}\n\
         run: cargo run -p choir-cli --example gen-surface"
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
        6,
        "only the six public platform operations are tools"
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
    // And every code the node emits appears in the platform source, so a
    // documented-but-dead code shows up as a missing constructor.
    let src = std::fs::read_to_string(
        repo_root().join("crates/choir-node/src/platform.rs"),
    )
    .expect("platform.rs");
    let reject_src = std::fs::read_to_string(
        repo_root().join("crates/choir-node/src/reject.rs"),
    )
    .expect("reject.rs");
    for code in choir_node::reject::Code::all() {
        let name = format!("{code:?}");
        assert!(
            src.contains(&format!("Code::{name}")) || reject_src.contains(&format!("Code::{name}")),
            "`{name}` is documented but never constructed"
        );
    }
}
