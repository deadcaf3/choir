//! The generated Python client, run against a real node (D17).
//!
//! This is the only artifact in the tree that tests the *description*
//! rather than the code: the generator is handed `/api/schema`'s document
//! and never the Rust table it was rendered from, so a method that comes
//! out wrong means the schema was insufficient. Nothing else asks that
//! question — the shell library wraps the binary, and every other client
//! here is the binary.
//!
//! Standard library only, on purpose. A third party who has to
//! `pip install` something before the client runs has a prerequisite,
//! and the absence of prerequisites is what is being tested.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

/// The generated client, importable from a scratch directory.
fn client_dir(tag: &str) -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let work = std::env::temp_dir().join(format!("choir-python-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::copy(
        root.join("templates/python/choir.py"),
        work.join("choir.py"),
    )
    .expect("the generated client exists");
    work
}

/// Runs `program` with the client importable, returning stdout.
fn python(work: &std::path::Path, program: &str) -> (bool, String, String) {
    let script = work.join("run.py");
    std::fs::write(&script, program).expect("script");
    let out = std::process::Command::new("python3")
        .arg(&script)
        .current_dir(work)
        .output()
        .expect("python3 runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The whole claim: a client generated from the node's own description
/// talks to the node, with nothing installed and no choir source read.
#[test]
fn the_generated_client_reads_a_real_node_with_only_the_standard_library() {
    let work = client_dir("reads");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&key.public_key_bytes())
        .expect("valid key");
    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    let port = node.port();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // Seed the log so a cursor has somewhere to point. Signed by the
    // binary, read by the generated client: the two artifacts meeting is
    // the shape an agent actually uses.
    let key_file = work.join("agent.key");
    std::fs::write(&key_file, key.secret_bytes()).expect("key file");
    let ops = work.join("ops.jsonl");
    let lines: Vec<String> = ["alpha", "beta", "gamma"]
        .iter()
        .map(|name| {
            serde_json::json!({
                "format_version": 1,
                "kind": { "SetRef": {
                    "name": name,
                    "commit": { "codec": 30, "digest": choir_oplog::ContentHash::blake3(name.as_bytes()).digest },
                    "prev": null,
                }},
            })
            .to_string()
        })
        .collect();
    std::fs::write(&ops, lines.join("\n")).expect("ops file");
    let seeded = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args([
            "batch",
            &api,
            key_file.to_str().expect("utf-8"),
            "py-agent",
            ops.to_str().expect("utf-8"),
        ])
        .output()
        .expect("choir runs");
    assert!(seeded.status.success(), "seeding failed: {seeded:?}");

    // Three shapes the schema describes differently: a no-argument GET,
    // a GET whose one argument is a query parameter, and the capability
    // helper that reads the live half of the document.
    let program = format!(
        r#"
import json, sys
from choir import Choir
node = Choir("{api}")
view = node.choir_view()
page = node.choir_log(**{{"from": 0}})
cursored = node.choir_log(**{{"from": 2}})
caps = node.capabilities()
json.dump({{
    "has_log": "log" in view,
    "entries": isinstance(page.get("entries"), list),
    "first_seq": page["entries"][0]["seq"],
    "cursored_first_seq": cursored["entries"][0]["seq"],
    "capability_keys": sorted(caps),
    "api_version": node.choir_schema()["api_version"],
}}, sys.stdout)
"#
    );
    let (ok, stdout, stderr) = python(&work, &program);
    assert!(ok, "the generated client failed: {stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("client emitted JSON");
    assert_eq!(result["has_log"], true, "{result}");
    assert_eq!(result["entries"], true, "{result}");
    // The cursor has to actually reach the transport. Sending `from` as
    // a request body instead of a query parameter leaves the node
    // answering from 0, which looks identical unless the log has
    // entries and the test asks where the page starts — the mutation
    // that stayed green until this assertion existed.
    assert_eq!(result["first_seq"], 0, "{result}");
    assert_eq!(
        result["cursored_first_seq"], 2,
        "the cursor did not reach the node, so `from` is not being sent as a query parameter: {result}"
    );
    assert_eq!(result["api_version"], 1, "{result}");
    assert_eq!(
        result["capability_keys"],
        serde_json::json!(["accounts", "acl", "platform"]),
        "the live capability half did not come through: {result}"
    );

    // The client imports nothing outside the standard library. A
    // generated client that needs a package is not the artifact this
    // test claims it is.
    //
    // Asked of Python's own parser rather than by scanning lines: the
    // first version of this check matched `from choir import Choir`
    // inside the module docstring — example text, not an import — and
    // failed on the artifact being correct. A grep cannot tell a
    // statement from a string that looks like one.
    let (ok, imports, stderr) = python(
        &work,
        "import ast, json, sys\n\
         tree = ast.parse(open('choir.py').read())\n\
         mods = set()\n\
         for node in ast.walk(tree):\n\
         \x20   if isinstance(node, ast.Import):\n\
         \x20       mods.update(a.name.split('.')[0] for a in node.names)\n\
         \x20   elif isinstance(node, ast.ImportFrom):\n\
         \x20       mods.add((node.module or '').split('.')[0])\n\
         json.dump(sorted(mods), sys.stdout)\n",
    );
    assert!(ok, "could not parse the client: {stderr}");
    let imports: Vec<String> = serde_json::from_str(&imports).expect("module list");
    for module in &imports {
        assert!(
            matches!(module.as_str(), "base64" | "json" | "urllib" | "os"),
            "the client imports a non-standard module: {module} (all: {imports:?})"
        );
    }
    assert!(
        !imports.is_empty(),
        "the import scan found nothing, so it proved nothing"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A refusal reaches the caller whole, with the node's own repair note.
///
/// choir's refusals are structured precisely so a client does not have
/// to guess what to do next, and a generated client that flattened them
/// into a status code would throw that away at the last step.
#[test]
fn a_refusal_arrives_with_the_nodes_own_words() {
    let work = client_dir("refusal");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    let port = node.port();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());

    // An unsigned submission: refused, with a code and a next step.
    let program = format!(
        r#"
import json, sys
from choir import Choir, ChoirError
node = Choir("http://127.0.0.1:{port}")
try:
    node.choir_submit(channel="a", payload_hex="00", key_id="1e-x", signature_hex="00")
    json.dump({{"refused": False}}, sys.stdout)
except ChoirError as error:
    json.dump({{
        "refused": True,
        "status": error.status,
        "code": error.body.get("code") if isinstance(error.body, dict) else None,
        "has_next": bool(error.body.get("next")) if isinstance(error.body, dict) else False,
    }}, sys.stdout)
"#
    );
    let (ok, stdout, stderr) = python(&work, &program);
    assert!(ok, "the client crashed instead of raising: {stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("client emitted JSON");
    assert_eq!(
        result["refused"], true,
        "a bad signature was accepted: {result}"
    );
    assert!(
        result["status"].as_u64().is_some_and(|s| s >= 400),
        "{result}"
    );
    assert!(
        result["code"].as_str().is_some(),
        "the refusal lost its code on the way through the client: {result}"
    );
    assert_eq!(
        result["has_next"], true,
        "the refusal lost the node's repair note: {result}"
    );

    std::fs::remove_dir_all(&work).ok();
}
