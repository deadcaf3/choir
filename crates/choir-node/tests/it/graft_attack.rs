//! Graft attacks against invariant 4: the author signs `(channel,
//! payload)` and *nothing else*. This module attacks that boundary from
//! the outside — it never re-signs, it takes the exact `key_id` and
//! `signature_hex` bytes of a submission the author really made and
//! tries to transplant them somewhere the author never sent them.
//!
//! Two halves, and both are the point:
//!
//! - What the signature does cover is proved by rejection: moving the
//!   bytes to another channel, or under another payload, fails the
//!   check.
//! - What it does not cover is proved by acceptance. A signature carries
//!   no position, no log identity and no nonce, so the *only* thing
//!   standing between a captured op and a replay is the CAS `prev`
//!   inside the payload — which is a statement about state, not about
//!   history. Return the state and the op admits again (ABA); offer the
//!   bytes to a second node that trusts the same key and they admit
//!   there too. Both are recorded here as current behaviour, deliberately,
//!   so that a future change to what is signed shows up as a test that
//!   starts failing rather than as a design discussion nobody has.
//!
//! Where the bytes come from is `GET /api/log`: the sync contract serves
//! `author_key` and `author_sig_hex` so a follower can verify what it
//! replicates, and those are everything `POST /api/submit` asks for. What
//! that costs an attacker depends on the deployment, and the honest
//! statement is narrower than "no key". A node started without
//! `--auth-file` requires no credentials at all, so the capture is a bare
//! GET. A node started with one answers 401 to every endpoint, so the
//! capture needs a transport token — but that table is `user:token` per
//! *person*, not per signing key, so any token holder can lift and replay
//! any actor's ops. The escalation is from one transport credential to
//! every key the node has ever trusted, which is precisely the per-actor
//! attribution the signatures exist to provide.
//!
//! `key_names.rs` is the neighbouring module and a different attack: it
//! re-signs honestly on a channel the key does not own. Here the
//! signature is always genuine and always the author's.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog, Witness};
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::curl;

/// A `/api/submit` body assembled from parts, so channel, payload and
/// signature can be recombined the way an attacker recombines them.
/// `support::submit_body` cannot express this: it signs what it sends.
fn grafted_body(channel: &str, payload: &[u8], sig: &Witness) -> String {
    serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

/// Move `workspace` to `commit`, expecting it to currently be at `prev`.
fn head(workspace: &str, commit: &str, prev: Option<&str>) -> Vec<u8> {
    ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: workspace.into(),
        commit: ContentHash::blake3(commit.as_bytes()),
        prev: prev.map(|p| ContentHash::blake3(p.as_bytes())),
    })
    .to_payload()
}

/// A `<codec>-<digest>` content hash, as `/api/view` serves one.
fn parse_hash(hex: &str) -> ContentHash {
    let (codec, digest) = hex.split_once('-').expect("codec-prefixed hash");
    ContentHash {
        codec: u8::from_str_radix(codec, 16).expect("hex codec"),
        digest: (0..digest.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digest[i..i + 2], 16).expect("hex digest"))
            .collect(),
    }
}

/// What the node says a client should sign a scope against: its own id
/// and the current head (`None` on an empty log).
fn log_scope(api: &str) -> (ContentHash, Option<ContentHash>) {
    let (code, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    (
        parse_hash(view["log"]["node"].as_str().expect("log.node")),
        view["log"]["head"].as_str().map(parse_hash),
    )
}

/// A serving node that trusts exactly `trusted`. Returns its API base
/// and the node handle to `unblock()` at the end.
fn serving_node(root: &std::path::Path, trusted: &[&ActorKey]) -> (String, std::sync::Arc<Node>) {
    let mut registry = Registry::new();
    for key in trusted {
        registry.register(&key.public_key_bytes()).unwrap();
    }
    let mut node = Node::bind(root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (format!("http://127.0.0.1:{port}/api"), node)
}

#[test]
fn a_genuine_signature_does_not_transplant_across_channel_or_payload() {
    let work = std::env::temp_dir().join(format!("choir-graft-scope-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let (api, node) = serving_node(&work.join("repos"), &[&ana]);
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);

    // The one op ana actually authorises: her own channel, one payload.
    let payload = head("ana/w", "c1", None);
    let sig = ana.sign_submission("ana", &payload);
    let (code, resp) = post(&grafted_body("ana", &payload, &sig));
    assert_eq!(code, 200, "{resp}");

    // Graft 1 — same bytes, another channel. The channel is inside the
    // signed tuple, so this is the attack invariant 4 exists to stop:
    // one signed op cannot become an op attributed to another actor's
    // collaboration channel.
    let (code, resp) = post(&grafted_body("bob", &payload, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Graft 2 — same channel and same signature, a payload ana never
    // signed. The workspace she moves and the commit she moves it to are
    // both inside the signed bytes, so neither can be substituted.
    let elsewhere = head("bob/w", "c1", None);
    let (code, resp) = post(&grafted_body("ana", &elsewhere, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    let retarget = head("ana/w", "attacker-commit", None);
    let (code, resp) = post(&grafted_body("ana", &retarget, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Nothing partial landed: the view holds ana's one op and neither
    // rejected graft left a trace.
    let (_, view) = curl(&[&format!("{api}/view")]);
    let workspaces = view["workspaces"].as_object().unwrap();
    assert_eq!(workspaces.len(), 1, "{view}");
    assert_eq!(
        workspaces["ana/w"],
        serde_json::json!(ContentHash::blake3(b"c1").to_hex()),
        "{view}"
    );

    // A byte-identical resubmission is a retry, not a graft, and is
    // answered as the op it already is rather than appended twice.
    let (code, resp) = post(&grafted_body("ana", &payload, &sig));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 0, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn an_aba_replay_is_refused_and_a_scope_is_what_closes_the_second_node() {
    let work = std::env::temp_dir().join(format!("choir-graft-replay-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let mallory = ActorKey::generate();
    let (api, node) = serving_node(&work.join("repos"), &[&ana, &mallory]);
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);
    let at = |name: &str| serde_json::json!(ContentHash::blake3(name.as_bytes()).to_hex());

    // ana creates the workspace, then advances it. Mallory is on the
    // wire and keeps the second submission's exact bytes.
    let create = head("ana/w", "c1", None);
    let create_sig = ana.sign_submission("ana", &create);
    let (code, resp) = post(&grafted_body("ana", &create, &create_sig));
    assert_eq!(code, 200, "{resp}");

    let advance = head("ana/w", "c2", Some("c1"));
    let advance_sig = ana.sign_submission("ana", &advance);
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(code, 200, "{resp}");

    // Replayed while the state has moved on, the captured op is answered
    // as the retry it is indistinguishable from: original seq,
    // `already_applied`, nothing appended.
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 1, "{resp}");

    // Mallory rolls the state back to c1 with an op of his own. This is
    // ordinary authorised work for a trusted key: a revert.
    let revert = head("ana/w", "c1", Some("c2"));
    let revert_sig = mallory.sign_submission("mallory", &revert);
    let (code, resp) = post(&grafted_body("mallory", &revert, &revert_sig));
    assert_eq!(code, 200, "{resp}");

    // The ABA moment: the state the captured op expected is back, so its
    // CAS `prev` matches again and `prev` alone would re-apply it. It does
    // not, because a signature is admissible once — the duplicate check
    // reads the same signing-hash index whether CAS passes or fails, so
    // "have these bytes run before" is asked before "is the state right".
    // The answer is the seq ana really submitted, and the workspace stays
    // where the revert left it.
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 1, "an ABA replay must not become a new entry: {resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["workspaces"]["ana/w"], at("c1"), "{view}");

    // A second node that trusts the same key is the other half, and it is
    // the half a duplicate index cannot see: another node's window holds
    // none of these bytes. An *unscoped* signature still lands there,
    // which is exactly what `--require-scope` refuses and why the flag
    // exists rather than being implied.
    let (other_api, other) = serving_node(&work.join("repos-2"), &[&ana]);
    let other_post = |body: &str| {
        curl(&["-X", "POST", "-d", body, &format!("{other_api}/submit")])
    };
    let (code, resp) = other_post(&grafted_body("ana", &create, &create_sig));
    assert_eq!(
        code, 200,
        "an unscoped signature still admits on any node that trusts the key: {resp}"
    );

    // Scoped, it does not. The scope names the log ana meant, and it is
    // inside the payload she signed, so the relayer cannot rewrite it —
    // and no flag is needed for the refusal: a scope that *is* present is
    // always enforced.
    let (ana_node, ana_head) = log_scope(&api);
    let scoped = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "ana/scoped".into(),
        commit: ContentHash::blake3(b"c9"),
        prev: None,
    })
    .in_scope(ana_node.clone(), ana_head)
    .to_payload();
    let scoped_sig = ana.sign_submission("ana", &scoped);
    let (code, resp) = post(&grafted_body("ana", &scoped, &scoped_sig));
    assert_eq!(code, 200, "{resp}");

    let (code, resp) = other_post(&grafted_body("ana", &scoped, &scoped_sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "foreign_scope", "{resp}");
    assert_eq!(resp["expected"], serde_json::json!(ana_node.to_hex()), "{resp}");
    let (_, other_view) = curl(&[&format!("{other_api}/view")]);
    assert!(
        other_view["workspaces"]["ana/scoped"].is_null(),
        "{other_view}"
    );

    other.unblock();
    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn a_scope_required_node_refuses_unscoped_foreign_and_aged_out_signatures() {
    // The flag, and the three ways a signature can fail to be bound to
    // this log at this moment. Each refusal names what to do next,
    // because "your signature is fine but not here" is not a diagnosis a
    // client can act on by itself.
    let work = std::env::temp_dir().join(format!("choir-graft-scoped-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&ana.public_key_bytes()).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            // Two entries of history, so the third op ages the first head
            // out and a scope naming it stops being admissible.
            .with_log_window_cap(2)
            .with_required_scope(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);
    let scoped = |name: &str, node: ContentHash, head: Option<ContentHash>| {
        ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: name.into(),
            commit: ContentHash::blake3(name.as_bytes()),
            prev: None,
        })
        .in_scope(node, head)
        .to_payload()
    };

    // The node advertises what to sign against, and that it is enforcing.
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["log"]["scope_required"], true, "{view}");
    let (node_id, genesis_head) = log_scope(&api);
    assert!(view["log"]["head"].is_null(), "empty log has no head: {view}");

    // Unscoped: refused, with the node's identity in the response so the
    // client can build the scope it was missing.
    let bare = head("ana/bare", "c1", None);
    let (code, resp) = post(&grafted_body("ana", &bare, &ana.sign_submission("ana", &bare)));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "scope_required", "{resp}");

    // Scoped to another node's log: refused even though the signature is
    // genuine and the key is trusted here.
    let elsewhere = scoped("ana/elsewhere", ContentHash::blake3(b"some other node"), None);
    let (code, resp) = post(&grafted_body(
        "ana",
        &elsewhere,
        &ana.sign_submission("ana", &elsewhere),
    ));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "foreign_scope", "{resp}");
    assert_eq!(resp["actual"], serde_json::json!(node_id.to_hex()), "{resp}");

    // Correctly scoped: lands. `head` is null on an empty log, and a
    // headless scope is what a client signs then — including every op of
    // a batch it signed in one read, since only the first of those meets
    // an empty log.
    let genesis = scoped("ana/one", node_id.clone(), genesis_head);
    let (code, resp) = post(&grafted_body(
        "ana",
        &genesis,
        &ana.sign_submission("ana", &genesis),
    ));
    assert_eq!(code, 200, "{resp}");

    // Capture a head, then let two more ops push it out of the window.
    let (_, aged_head) = log_scope(&api);
    assert!(aged_head.is_some());
    for name in ["ana/two", "ana/three"] {
        let op = scoped(name, node_id.clone(), log_scope(&api).1);
        let (code, resp) = post(&grafted_body("ana", &op, &ana.sign_submission("ana", &op)));
        assert_eq!(code, 200, "{resp}");
    }

    // A signature against that aged-out head is no longer admissible.
    // This is the half that makes the duplicate index enough: bytes old
    // enough to have left the index are also old enough that the head
    // they name has left the window.
    let stale = scoped("ana/stale", node_id, aged_head.clone());
    let (code, resp) = post(&grafted_body("ana", &stale, &ana.sign_submission("ana", &stale)));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "stale_scope", "{resp}");
    assert_eq!(
        resp["expected"],
        serde_json::json!(aged_head.unwrap().to_hex()),
        "{resp}"
    );
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert!(view["workspaces"]["ana/stale"].is_null(), "{view}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// `git`, configured the way every module here configures it: no
/// signing, deterministic identity, never prompting.
fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

#[test]
fn a_log_sourced_replay_of_a_node_signed_ref_move_no_longer_lands() {
    // The two tests above hand the attacker a signature. This one does
    // not: it gives them a URL. `/api/log` publishes `author_key` and
    // `author_sig_hex` on purpose — a follower has to be able to verify
    // what it replicates (SYNC.md) — and those are the same two fields
    // `/api/submit` wants. So the capture channel for a graft is an
    // unauthenticated GET, and the strongest bytes on offer are the
    // node's own: a `git push` reaches the sequencer as a *node-signed*
    // SetRef, which is authority no external key has.
    let work = std::env::temp_dir().join(format!("choir-graft-log-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // An empty registry: nobody outside is trusted at all, which is the
    // point. The attack never needs a trusted key. The node's own key is
    // held here only so the second node further down can trust it — an
    // untrusted key would be refused for the wrong reason.
    let node_key = ActorKey::generate();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::from_secret_bytes(&node_key.secret_bytes()),
        )
        .unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let ref_name = "agents/demo.git:refs/heads/main";

    // Two honest pushes: create `main`, then advance it.
    let clone = work.join("clone");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    let commit = |body: &str, message: &str| {
        std::fs::write(clone.join("f.txt"), body).unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-q", "-m", message]);
        assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
            .status
            .success());
        String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let c1 = commit("one\n", "first");
    let c2 = commit("two\n", "second");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["refs"][ref_name], serde_json::json!(format!("11-{c2}")));

    // The whole capture: one GET, no credentials. Take the entry that
    // moved main to c2 and rebuild the submit body from the served
    // fields alone.
    let (code, page) = curl(&[&format!("{api}/log?from=0")]);
    assert_eq!(code, 200, "{page}");
    let entries = page["entries"].as_array().unwrap();
    let advanced = ContentHash::from_git_oid(&c2).expect("git oid");
    let captured = entries
        .iter()
        .find(|e| {
            let payload =
                choir_node::platform::hex_decode(e["payload_hex"].as_str().unwrap()).unwrap();
            matches!(
                ViewOp::from_payload(&payload).unwrap().kind,
                OpKind::SetRef { ref commit, .. } if *commit == advanced
            )
        })
        .expect("the ref move is in the public log");
    let stolen = serde_json::json!({
        "channel": captured["workspace"],
        "payload_hex": captured["payload_hex"],
        "key_id": captured["author_key"],
        "signature_hex": captured["author_sig_hex"],
    })
    .to_string();
    // It really is the daemon's signature being reused, not a client's.
    assert!(captured["author_key"].is_string(), "{captured}");

    // Someone with push rights reverts main to c1. Ordinary work, and
    // the state the captured op expected is now back.
    assert!(git(&clone, &["push", "-qf", "origin", "HEAD~1:main"])
        .status
        .success());
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["refs"][ref_name], serde_json::json!(format!("11-{c1}")));

    // Replay, with the state the captured op expected restored. Refused
    // as the duplicate it is, and answered with the sequence the real
    // push landed at rather than a new one.
    let (code, resp) = curl(&["-X", "POST", "-d", &stolen, &format!("{api}/submit")]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");

    // Which is the whole point: the view and git still agree. Before the
    // duplicate check, this replay moved the view to c2 while git kept
    // c1 — and the view is what the landing gate, `approval_weight` and
    // every reader consult, so the half that was wrong was the
    // authoritative one.
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(
        view["refs"][ref_name],
        serde_json::json!(format!("11-{c1}")),
        "{view}"
    );
    let remote = String::from_utf8(git(&clone, &["ls-remote", "origin", "refs/heads/main"]).stdout)
        .unwrap();
    assert!(remote.starts_with(&c1), "git holds the reverted ref: {remote}");

    // The other node is closed too, and without the operator turning
    // anything on: the daemon scopes the ops it signs itself, so a
    // git-derived ref move names the log it was pushed to. This second
    // node trusts the first node's key — so the signature verifies here,
    // and the refusal is about the log, not the signer.
    let node_id = log_scope(&api).0;
    let (other_api, other) = serving_node(&work.join("repos-2"), &[&node_key]);
    assert_ne!(
        log_scope(&other_api).0.to_hex(),
        node_id.to_hex(),
        "the second node must be a different log for this to prove anything"
    );
    let (code, resp) = curl(&["-X", "POST", "-d", &stolen, &format!("{other_api}/submit")]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "foreign_scope", "{resp}");
    assert_eq!(resp["expected"], serde_json::json!(node_id.to_hex()), "{resp}");
    let (_, other_view) = curl(&[&format!("{other_api}/view")]);
    assert!(other_view["refs"][ref_name].is_null(), "{other_view}");

    other.unblock();
    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn the_landing_gate_cannot_refuse_the_replay_but_the_duplicate_check_does() {
    // The gate is the thing worth attacking, so: `--protected-refs` plus
    // `--require-review`, the full configuration from `landing.rs`.
    //
    // The gate is not bypassed here, and that is the finding. It asks
    // whether an approved review named this (ref, commit) pair, and an
    // approval never expires — deliberately, so retention cannot become
    // an expiry policy. A replayed landing therefore satisfies the gate
    // by re-using the approval that authorised it the first time. What
    // review cannot express is "and not again, after it was taken back".
    let work = std::env::temp_dir().join(format!("choir-graft-gate-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    let ref_name = "agents/demo.git:refs/heads/main";
    std::fs::write(&pool_file, "ana\nbot\n").unwrap();
    std::fs::write(&refs_file, format!("{ref_name}\n")).unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool_file)
            .with_protected_refs(refs_file)
            .with_required_review(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let refs = || curl(&[&format!("{api}/view")]).1["refs"].clone();
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);

    // Open a review naming (ref, oid) and drive it to approved.
    let approve = |id: &str, oid: &str| {
        let review = ViewOp::new(OpKind::RequestReview {
            id: id.into(),
            target: ContentHash::from_git_oid(oid).expect("git oid"),
            reviewers: Vec::new(),
            target_ref: Some(ref_name.into()),
        });
        let (code, resp) = post(&crate::support::submit_body(&author, "carol", &review));
        assert_eq!(code, 200, "{resp}");
        let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
        for who in &drawn {
            let verdict = ViewOp::new(OpKind::PostVerdict {
                id: id.into(),
                reviewer: who.clone(),
                verdict: Verdict::Approve,
                note: "lgtm".into(),
            });
            let (code, resp) = post(&crate::support::submit_body(&author, who, &verdict));
            assert_eq!(code, 200, "{resp}");
        }
    };

    let clone = work.join("clone");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    let commit = |body: &str, message: &str| {
        std::fs::write(clone.join("f.txt"), body).unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-q", "-m", message]);
        String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };

    // Create main, then land c2 the legitimate way: reviewed, approved,
    // pushed. Call c2 the change someone later decides was a mistake.
    let c1 = commit("one\n", "first");
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let c2 = commit("two\n", "second");
    approve("land-c2", &c2);
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    assert_eq!(refs()[ref_name], serde_json::json!(format!("11-{c2}")));

    // Capture the node-signed landing off the public log.
    let landed = ContentHash::from_git_oid(&c2).expect("git oid");
    let (_, page) = curl(&[&format!("{api}/log?from=0")]);
    let captured = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            let payload =
                choir_node::platform::hex_decode(e["payload_hex"].as_str().unwrap()).unwrap();
            matches!(
                ViewOp::from_payload(&payload).unwrap().kind,
                OpKind::SetRef { ref commit, .. } if *commit == landed
            )
        })
        .expect("the landing is in the public log")
        .clone();
    let stolen = serde_json::json!({
        "channel": captured["workspace"],
        "payload_hex": captured["payload_hex"],
        "key_id": captured["author_key"],
        "signature_hex": captured["author_sig_hex"],
    })
    .to_string();

    // The revert is itself reviewed and approved — the most careful
    // version of taking a change back that this system offers.
    approve("land-revert", &c1);
    let out = git(&clone, &["push", "-qf", "origin", "HEAD~1:main"]);
    assert!(
        out.status.success(),
        "an approved revert should land: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(refs()[ref_name], serde_json::json!(format!("11-{c1}")));

    // Replay. The gate itself cannot refuse this: the approval for
    // (main, c2) is still in the view and still approved, deliberately,
    // because an approval that expired on a timer would make retention an
    // authorization policy. So the gate says yes to the same landing a
    // second time, and what refuses it is the duplicate check — the
    // signature, not the approval, is what has already been spent.
    let (code, resp) = post(&stolen);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(refs()[ref_name], serde_json::json!(format!("11-{c1}")));

    // And so the ref is not wedged. Before the duplicate check, the view
    // held c2 while git held c1, and the next honest push CASed git's oid
    // against a view that disagreed — a fully reviewed, fully approved
    // landing refused with nothing the pusher could fix from their side.
    // It lands.
    let c3 = commit("three\n", "third");
    approve("land-c3", &c3);
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "an approved push must still land after a replay attempt: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote = String::from_utf8(git(&clone, &["ls-remote", "origin", "refs/heads/main"]).stdout)
        .unwrap();
    assert!(remote.starts_with(&c3), "git advanced to the approved commit: {remote}");
    assert_eq!(
        refs()[ref_name],
        serde_json::json!(format!("11-{c3}")),
        "and the view agrees with git"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn behind_an_auth_file_the_capture_needs_a_token_and_a_token_is_enough() {
    // The deployment shape the module doc now distinguishes. With
    // `--auth-file` the whole API is 401 without credentials, so a graft
    // is not remote-anonymous. It is also not much harder: the auth table
    // is one token per person, while a signature is per actor key, so a
    // token holder reads the log and replays anyone's ops. This test is
    // what keeps that claim from drifting back to either extreme.
    let work = std::env::temp_dir().join(format!("choir-graft-auth-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&ana.public_key_bytes()).unwrap();
    let mut table = choir_node::AuthTable::new();
    // Not ana. A different person entirely, with no signing key here.
    table.insert("reader".into(), "sekrit-token".into());

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let creds = "reader:sekrit-token";

    // ana submits one op, with credentials like everyone else.
    let payload = head("ana/w", "c1", None);
    let sig = ana.sign_submission("ana", &payload);
    let (code, resp) = curl(&[
        "-u", creds, "-X", "POST", "-d",
        &grafted_body("ana", &payload, &sig),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // No credentials: the log is not readable, so the capture channel is
    // shut. `curl` gets a plain-text 401 body, not JSON.
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", &format!("{api}/log?from=0")])
        .output()
        .expect("curl runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "401");

    // With the token — held by someone who has no key here at all — the
    // author's signature comes back out of the log, ready to replay.
    let (code, page) = curl(&["-u", creds, &format!("{api}/log?from=0")]);
    assert_eq!(code, 200, "{page}");
    let entry = &page["entries"][0];
    assert!(entry["author_sig_hex"].is_string(), "{page}");
    assert_eq!(
        entry["author_key"],
        serde_json::json!(ana.actor_id().to_hex()),
        "the token holder is reading a signature that is not theirs: {page}"
    );

    // And the replay is refused by the duplicate check rather than by
    // anything about the token. Transport auth is not the control that
    // stops this; it only decides who can try.
    let stolen = serde_json::json!({
        "channel": entry["workspace"],
        "payload_hex": entry["payload_hex"],
        "key_id": entry["author_key"],
        "signature_hex": entry["author_sig_hex"],
    })
    .to_string();
    let (code, resp) = curl(&[
        "-u", creds, "-X", "POST", "-d", &stolen, &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 0, "{resp}");

    // A corrupted signature over those same already-applied bytes is a
    // rejection, not a 200. The duplicate answer is only reachable once
    // the signature has verified — otherwise "already applied" would be
    // the reply to a failed signature check.
    let mut forged = sig.signature;
    forged[0] ^= 0xff;
    let tampered = serde_json::json!({
        "channel": "ana",
        "payload_hex": entry["payload_hex"],
        "key_id": entry["author_key"],
        "signature_hex": choir_node::platform::hex_encode(&forged),
    })
    .to_string();
    let (code, resp) = curl(&[
        "-u", creds, "-X", "POST", "-d", &tampered, &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "a bad signature must not be answered as success: {resp}");
    assert_ne!(resp["already_applied"], true, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
