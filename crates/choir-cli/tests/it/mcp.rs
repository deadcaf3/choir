//! Wire-level MCP compatibility plus one live-node tool path. The adapter
//! is useful only if measured legacy clients can discover it and calls
//! still cross the real HTTP/auth boundary rather than a second platform
//! implementation.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

fn run_mcp(args: &[&str], messages: &[Value]) -> (std::process::ExitStatus, Vec<Value>) {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_choir-mcp"))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("choir-mcp runs");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        for message in messages {
            serde_json::to_writer(&mut stdin, message).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON response per line"))
        .collect();
    if !output.status.success() {
        panic!(
            "choir-mcp failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    (output.status, responses)
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {
            "name": "test-client",
            "version": "1.0.0"
        }
    })
}

fn capture_json_requests(
    count: usize,
) -> (
    String,
    std::sync::mpsc::Receiver<Value>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for _ in 0..count {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut content_length = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
            }
            let mut body = vec![0; content_length.expect("curl sends Content-Length")];
            reader.read_exact(&mut body).unwrap();
            sent.send(serde_json::from_slice(&body).unwrap()).unwrap();
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .unwrap();
        }
    });
    (format!("http://{address}"), received, server)
}

#[test]
fn submission_calls_carry_both_channel_spellings_for_rolling_upgrades() {
    let (api, bodies, server) = capture_json_requests(2);
    let client = choir_cli::mcp::HttpClient::new(&api, None, None).unwrap();
    let signed = json!({
        "channel": "operator/agent",
        "payload_hex": "00",
        "key_id": "key",
        "signature_hex": "00"
    });

    let single = choir_cli::surface::mcp_endpoint("choir_submit").unwrap();
    assert_eq!(client.request(single, &signed).unwrap().0, 200);
    let batch = choir_cli::surface::mcp_endpoint("choir_submit_batch").unwrap();
    assert_eq!(
        client
            .request(batch, &json!({ "ops": [signed] }))
            .unwrap()
            .0,
        200
    );

    for body in [
        bodies.recv().unwrap(),
        bodies.recv().unwrap()["ops"][0].clone(),
    ] {
        assert_eq!(body["channel"], "operator/agent", "{body}");
        assert_eq!(body["workspace"], body["channel"], "{body}");
    }
    server.join().unwrap();
}

#[test]
fn measured_legacy_versions_initialize_and_list_tools() {
    for version in ["2025-06-18", "2025-11-25"] {
        let messages = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": { "name": "measured-client", "version": "1.0.0" }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        ];
        let (_, responses) = run_mcp(&["http://127.0.0.1:1"], &messages);
        assert_eq!(responses.len(), 2, "notification must not get a response");
        assert_eq!(responses[0]["result"]["protocolVersion"], version);
        assert_eq!(
            responses[0]["result"]["capabilities"],
            json!({ "tools": {} })
        );
        assert_eq!(responses[0]["result"]["resultType"], "complete");
        assert_eq!(
            responses[0]["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "choir-mcp"
        );
        assert_eq!(responses[1]["result"]["resultType"], "complete");
        assert_eq!(responses[1]["result"]["cacheScope"], "public");
        assert!(responses[1]["result"]["ttlMs"].as_u64().unwrap() > 0);
    }
}

#[test]
fn modern_discovery_is_stateless_and_tool_order_is_cache_stable() {
    let messages = [
        json!({
            "jsonrpc": "2.0",
            "id": "discover",
            "method": "server/discover",
            "params": { "_meta": modern_meta() }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": "first",
            "method": "tools/list",
            "params": { "_meta": modern_meta() }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": "tools/list",
            "params": { "_meta": modern_meta() }
        }),
    ];
    let (_, responses) = run_mcp(&["http://127.0.0.1:1"], &messages);
    assert_eq!(responses.len(), 3);
    assert_eq!(
        responses[0]["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-11-25", "2025-06-18"])
    );
    assert_eq!(responses[0]["result"]["resultType"], "complete");
    assert_eq!(responses[0]["result"]["cacheScope"], "public");
    for response in &responses {
        assert_eq!(
            response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "choir-mcp"
        );
    }
    assert_eq!(responses[1]["result"], responses[2]["result"]);
    let names: Vec<&str> = responses[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "choir_submit",
            "choir_submit_batch",
            "choir_view",
            "choir_appeal",
            "choir_log",
            "choir_workspace",
            "choir_reviews"
        ]
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn tool_calls_cross_real_http_auth_and_preserve_node_results() {
    let work = std::env::temp_dir().join(format!("choir-mcp-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let key = ActorKey::from_secret_bytes(&[37; 32]);
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let mut auth = AuthTable::new();
    auth.insert("mcp-agent".to_string(), "placeholder-token".to_string());
    let auth_file = work.join("auth");
    std::fs::write(&auth_file, "mcp-agent:placeholder-token\n").unwrap();

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(auth)).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    let server = {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever())
    };

    let head = choir_hash::ContentHash::from_git_oid(&"1".repeat(40)).unwrap();
    let op = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "mcp-workspace".to_string(),
        commit: head.clone(),
        prev: None,
    });
    let payload = op.to_payload();
    let signature = key.sign_submission("mcp-agent", &payload);
    let submit = json!({
        "workspace": "mcp-agent",
        "payload_hex": hex(&payload),
        "key_id": signature.key_id,
        "signature_hex": hex(&signature.signature)
    });
    let messages = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_submit",
                "arguments": submit
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_view",
                "arguments": {}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_log",
                "arguments": { "from": 0 }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_submit_batch",
                "arguments": { "ops": [] }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_reviews",
                "arguments": { "reviewer": "operator/reviewer" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_workspace",
                "arguments": {
                    "repo": "missing/repository",
                    "name": "mcp-workspace"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "_meta": modern_meta(),
                "name": "choir_appeal",
                "arguments": { "attempt_id": 0 }
            }
        }),
    ];
    let api = format!("http://127.0.0.1:{port}");
    let auth_path = auth_file.to_str().unwrap();
    let (_, responses) = run_mcp(
        &[&api, "--auth-file", auth_path, "--auth-user", "mcp-agent"],
        &messages,
    );

    assert_eq!(responses.len(), 7);
    for response in &responses[..5] {
        assert_eq!(response["result"]["resultType"], "complete");
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["structuredContent"]["status"], 200);
    }
    assert_eq!(
        responses[0]["result"]["structuredContent"]["body"]["seq"],
        0
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["body"]["workspaces"]["mcp-workspace"],
        head.to_hex()
    );
    assert_eq!(
        responses[2]["result"]["structuredContent"]["body"]["entries"][0]["seq"],
        0
    );
    assert_eq!(
        responses[3]["result"]["structuredContent"]["body"],
        json!({ "accepted": 0, "rejected": 0, "results": [] })
    );
    assert_eq!(
        responses[4]["result"]["structuredContent"]["body"],
        json!({ "pending": {} })
    );
    assert_eq!(responses[5]["result"]["resultType"], "complete");
    assert_eq!(responses[5]["result"]["isError"], true);
    assert_eq!(responses[5]["result"]["structuredContent"]["status"], 404);
    assert_eq!(
        responses[5]["result"]["structuredContent"]["body"],
        json!({ "error": "no such repo" })
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            responses[5]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        responses[5]["result"]["structuredContent"]["body"]
    );
    assert_eq!(responses[6]["result"]["isError"], true);
    assert_eq!(responses[6]["result"]["structuredContent"]["status"], 503);
    assert_eq!(
        responses[6]["result"]["structuredContent"]["body"]["code"],
        "policy_unavailable"
    );

    node.unblock();
    server.join().unwrap();
    std::fs::remove_dir_all(&work).ok();
}
