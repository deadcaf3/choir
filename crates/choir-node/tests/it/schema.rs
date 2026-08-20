//! The machine-readable API description (D17), served.
//!
//! The Code-Mode bet is that an agent writes code against a typed API
//! rather than making many tool calls. That code has to come from
//! somewhere, and the thing it comes from must not be able to disagree
//! with the node it talks to. Two halves, checked here for the property
//! each one carries: the static half is generated from the same table as
//! `--help`, the README, `llms.txt` and the MCP tools, so it cannot drift
//! from them; the capabilities half is read off the live node, so it
//! cannot drift from the deployment.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

use crate::support::curl;

/// A node with everything off, and the same node with everything on, so
/// the capability object is observed changing rather than asserted once.
fn schema(tag: &str, enable: bool) -> serde_json::Value {
    let work = std::env::temp_dir().join(format!("choir-node-schema-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    if enable {
        std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");
        node.watch_acl_file(work.join("acl")).expect("acl loads");
        node.enable_accounts(work.join("accounts.json"), None, None)
            .expect("accounts enable");
        node.enable_platform(
            Platform::start(
                Registry::new(),
                Box::new(MemLog::new()),
                ActorKey::generate(),
            )
            .expect("platform starts"),
        );
    }
    std::thread::spawn(move || node.serve_forever());
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        &format!("http://127.0.0.1:{port}/api/schema"),
    ]);
    assert_eq!(status, 200, "{body}");
    std::fs::remove_dir_all(&work).ok();
    body
}

/// The static half: a version, every endpoint, and the aliases a client
/// would otherwise build on without knowing they are aliases.
#[test]
fn the_schema_describes_the_surface_it_is_generated_from() {
    let doc = schema("static", true);

    assert_eq!(doc["api_version"], 1, "the version must be in the document");

    // Every endpoint the surface table knows, by path, and each one says
    // whether an agent is meant to call it at all — the internal hook
    // endpoints are in the list and marked, rather than hidden, because
    // a client that meets one should know what it is.
    let endpoints = doc["endpoints"].as_array().expect("endpoints");
    assert!(endpoints.len() >= 15, "only {} endpoints", endpoints.len());
    // Paths a generator can use: no query template embedded in them.
    // `/api/log?from=N` is how the docs spell it and is not something a
    // client can build a URL from without parsing it back apart.
    for e in endpoints {
        assert!(
            !e["path"].as_str().expect("a path").contains('?'),
            "a path carries a query template a generator would have to parse: {e}"
        );
    }
    let log = endpoints
        .iter()
        .find(|e| e["path"] == "/api/log")
        .expect("the log endpoint");
    assert_eq!(
        log["query_parameters"][0], "from",
        "the cursor is not named: {log}"
    );
    assert_eq!(
        log["documented_as"], "/api/log?from=N",
        "the docs spelling is lost"
    );

    for path in ["/api/submit", "/api/submit-batch", "/api/view", "/api/log"] {
        let found = endpoints
            .iter()
            .find(|e| e["path"] == path)
            .unwrap_or_else(|| panic!("the schema omits {path}: {doc}"));
        assert!(found["purpose"].as_str().is_some_and(|p| !p.is_empty()));
        assert_eq!(
            found["agent_facing"], true,
            "{path} is not offered to agents"
        );
        assert!(
            found["name"].as_str().is_some(),
            "{path} has no stable name"
        );
        assert!(
            found["input_schema"]["type"] == "object",
            "{path} has no argument schema: {found}"
        );
    }
    // A hook endpoint the node exposes for its own use is present and
    // marked, not omitted.
    assert!(
        endpoints
            .iter()
            .any(|e| e["agent_facing"] == false && e["name"].is_null()),
        "every endpoint claims to be agent-facing, so the flag says nothing"
    );

    // The deprecation policy this row's fallback depends on, stated
    // rather than left to prose: `workspace` is a v1 alias for `channel`.
    let deprecated = doc["deprecations"].as_array().expect("deprecations");
    let alias = deprecated
        .iter()
        .find(|d| d["name"] == "workspace")
        .expect("the `workspace` alias must be declared");
    assert_eq!(alias["replaced_by"], "channel");
    assert!(alias["note"].as_str().is_some_and(|n| n.contains("alias")));

    // The CLI half, so an agent that can run the binary does not need a
    // second document to learn its arguments.
    let commands = doc["commands"].as_array().expect("commands");
    assert!(commands.iter().any(|c| c["name"] == "review"));
    assert!(commands
        .iter()
        .all(|c| c["args"].is_string() && c["summary"].is_string()));
}

/// The live half: what *this* node will accept, which no committed file
/// can carry honestly.
#[test]
fn the_capabilities_describe_the_node_rather_than_the_build() {
    let bare = schema("bare", false);
    let full = schema("full", true);

    for (name, off, on) in [
        (
            "accounts",
            &bare["capabilities"]["accounts"],
            &full["capabilities"]["accounts"],
        ),
        (
            "acl",
            &bare["capabilities"]["acl"],
            &full["capabilities"]["acl"],
        ),
        (
            "platform",
            &bare["capabilities"]["platform"],
            &full["capabilities"]["platform"],
        ),
    ] {
        assert_eq!(
            off,
            &serde_json::Value::Bool(false),
            "{name} claimed on a bare node"
        );
        assert_eq!(
            on,
            &serde_json::Value::Bool(true),
            "{name} claimed off on a full node"
        );
    }

    // The static half is identical across the two nodes: the same build
    // describes the same API, and only the deployment differs.
    for key in ["api_version", "endpoints", "commands", "deprecations"] {
        assert_eq!(
            bare[key], full[key],
            "`{key}` differs between two nodes of one build"
        );
    }

    // The live object *replaces* whatever the generated file carried
    // rather than filling gaps in it, so a capability committed by
    // mistake can never be served. Written after a mutation that baked
    // `"accounts": true` into the generated schema and changed nothing
    // observable: the code was safe, but only because the overwrite is
    // unconditional, and nothing said so. A merge that filled instead of
    // replaced would leak the baked key, and this is the assertion that
    // notices.
    for doc in [&bare, &full] {
        let keys: Vec<&str> = doc["capabilities"]
            .as_object()
            .expect("a capabilities object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["accounts", "acl", "platform"],
            "the served capabilities are not exactly what the node computed: {doc}"
        );
    }
}
