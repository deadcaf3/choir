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
//! The third test asks where an attacker gets the bytes, and the answer
//! is `GET /api/log`: the sync contract serves `author_key` and
//! `author_sig_hex` so any client can verify what it replays, which is
//! also everything `POST /api/submit` asks for. No wiretap, no key.
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
fn cas_state_is_the_whole_replay_bound_so_aba_and_a_second_node_admit_the_bytes() {
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

    // Replayed now, the captured op does not re-apply — the workspace is
    // at c2 and the op expects c1 — and the CAS refusal is answered as
    // the retry it looks like: same seq, `already_applied`. Note what
    // that dedup is keyed on, because the next step turns on it: the
    // signing hash of these exact bytes, consulted *only* on the CAS
    // error path.
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

    // ABA. The captured bytes admit again, because a CAS `prev` asks
    // "is the state what I expect" and not "have I run before" — and the
    // retry index cannot stand in for the missing nonce, since it is
    // reached only when CAS fails. Once CAS passes, the same signing
    // hash that was answered `already_applied` a moment ago appends a
    // second, distinct entry instead. The
    // resulting entry is signed by ana and attributed to ana's channel,
    // at a sequence ana never submitted, moving her workspace at a
    // moment she did not choose. Nothing in what she signed could have
    // prevented it: a signature over (channel, payload) is by
    // construction position-independent and log-independent.
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(
        code, 200,
        "replay after ABA is currently admitted; if this now fails, the signed \
         tuple or the CAS rule changed and invariant 4 needs rewriting: {resp}"
    );
    assert_ne!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 3, "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["workspaces"]["ana/w"], at("c2"), "{view}");

    // A second node that trusts the same key is the other half of the
    // same gap: the signed tuple names no log, no node and no genesis,
    // so ana's op is equally valid everywhere her key is trusted. The
    // captured create replays onto a node ana never spoke to.
    let (other_api, other) = serving_node(&work.join("repos-2"), &[&ana]);
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &grafted_body("ana", &create, &create_sig),
        &format!("{other_api}/submit"),
    ]);
    assert_eq!(
        code, 200,
        "an op signed for one node currently admits on any node that trusts \
         the key; binding a log id into the signed tuple is what would change \
         this: {resp}"
    );
    assert_eq!(resp["seq"], 0, "{resp}");
    let (_, other_view) = curl(&[&format!("{other_api}/view")]);
    assert_eq!(other_view["workspaces"]["ana/w"], at("c1"), "{other_view}");

    other.unblock();
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
fn the_public_log_is_a_complete_graft_kit_for_a_node_signed_ref_move() {
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
    // point. The attack never needs a trusted key.
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
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

    // Replay. Admitted, because nothing the node signed said "once", and
    // nothing said "here".
    let (code, resp) = curl(&["-X", "POST", "-d", &stolen, &format!("{api}/submit")]);
    assert_eq!(
        code, 200,
        "a log-sourced replay of a node-signed ref move is currently admitted: {resp}"
    );
    assert_ne!(resp["already_applied"], true, "{resp}");

    // And the damage is not just an extra log entry. The view — what the
    // landing gate, `approval_weight` and every reader consult — now says
    // main is at c2, while git's own ref, which no replay can touch,
    // still says c1. The two halves of "one history" disagree, and the
    // op that split them carries the daemon's signature.
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(
        view["refs"][ref_name],
        serde_json::json!(format!("11-{c2}")),
        "{view}"
    );
    let remote = String::from_utf8(git(&clone, &["ls-remote", "origin", "refs/heads/main"]).stdout)
        .unwrap();
    assert!(
        remote.starts_with(&c1),
        "git still holds the reverted ref, so view and git have diverged: {remote}"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn the_landing_gate_does_not_stop_the_replay_and_the_divergence_wedges_the_ref() {
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

    // Replay. The approval for (main, c2) is still in the view, still
    // approved, so the gate says yes to the same landing a second time —
    // for an attacker who reviewed nothing, pushed nothing and holds no
    // key. Review is not the missing control here; a nonce is.
    let (code, resp) = post(&stolen);
    assert_eq!(
        code, 200,
        "the landing gate re-authorises a replayed landing from a standing approval: {resp}"
    );
    assert_eq!(refs()[ref_name], serde_json::json!(format!("11-{c2}")));

    // Second consequence, and the practical one: the ref is now wedged.
    // The next honest push CASes against git's old oid (c1) while the
    // view holds c2, so a fully reviewed, fully approved landing is
    // refused with no way for the pusher to fix it from their side.
    let c3 = commit("three\n", "third");
    approve("land-c3", &c3);
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        !out.status.success(),
        "an approved push onto a diverged ref should be refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote = String::from_utf8(git(&clone, &["ls-remote", "origin", "refs/heads/main"]).stdout)
        .unwrap();
    assert!(remote.starts_with(&c1), "git is stuck at the revert: {remote}");
    assert_eq!(
        refs()[ref_name],
        serde_json::json!(format!("11-{c2}")),
        "and the view is stuck at the replay"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
