//! Guided walkthrough of the whole stack: run `cargo run -p choir-demo`.
//!
//! Three "agents" get keys, race signed ops through the single-writer
//! sequencer (one gets rejected, one gets a first-class conflict and
//! keeps working), the op log is time-traveled, and a real `git` client
//! pushes through the choir-node daemon at the end.

use std::collections::BTreeMap;

use choir_identity::{ActorKey, Registry};
use choir_merge::{MergeOutcome, Pipeline};
use choir_oplog::{MemLog, OpEntry};
use choir_sequencer::{Sequencer, SubmitPolicy, Submission};
use choir_store::{get_blob, put_blob, ChunkerParams, MemStore};
use choir_view::{Commit, OpKind, TreeEntry, View, ViewOp};

/// The production admission shape: verify the author's signature, then
/// CAS the op against a view cached on the writer thread.
struct ChoirPolicy {
    registry: Registry,
    view: View,
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        self.registry
            .verify_submission(&sub.channel, &sub.payload, sig)
            .map_err(|e| format!("bad signature: {e:?}"))?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| format!("bad op: {e:?}"))?;
        let mut trial = self.view.clone();
        trial.apply(&op).map_err(|e| format!("stale head: {e:?}"))
    }

    fn accepted(&mut self, entry: &OpEntry) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked");
        self.view.apply(&op).expect("checked");
    }
}

fn short(h: &choir_hash::ContentHash) -> String {
    h.to_hex()[..11].to_string()
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}

fn main() {
    let mut store = MemStore::new();

    section("1. Identity (L8): one ed25519 key per agent");
    let alice = ActorKey::generate();
    let bob = ActorKey::generate();
    let mallory = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    registry.register(&bob.public_key_bytes()).unwrap();
    println!("  alice   -> actor id {}  (registered)", short(&alice.actor_id()));
    println!("  bob     -> actor id {}  (registered)", short(&bob.actor_id()));
    println!("  mallory -> actor id {}  (NOT registered)", short(&mallory.actor_id()));

    section("2. Content-addressed commits (L0/L1): BLAKE3 + FastCDC");
    let base_src = "fn main() {\n    println!(\"v1\");\n}\n";
    let blob = put_blob(&mut store, base_src.as_bytes(), ChunkerParams::default()).unwrap();
    let mut tree = BTreeMap::new();
    tree.insert("main.rs".to_string(), TreeEntry::File { blob });
    let c_base = Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![],
        tree,
        author: short(&alice.actor_id()),
        message: "base".into(),
    }
    .put(&mut store)
    .unwrap();
    println!("  base commit {} (tree: main.rs)", short(&c_base));

    section("3. Signed ops through the single-writer sequencer (L2)");
    let sequencer = Sequencer::spawn_with_policy(
        Box::new(MemLog::new()),
        Box::new(ChoirPolicy {
            registry,
            view: View::default(),
        }),
    );
    let handle = sequencer.handle();

    let op = ViewOp::new(OpKind::SetRef {
        name: "main".into(),
        commit: c_base.clone(),
        prev: None,
    })
    .to_payload();
    let sig = alice.sign_submission("alice", &op);
    let acc = handle.try_submit("alice", op, Some(sig)).unwrap();
    println!("  alice sets main -> {}   ACCEPTED seq={} ({:?})", short(&c_base), acc.seq, acc.decision_latency);

    let op = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "bob".into(),
        commit: c_base.clone(),
        prev: None,
    })
    .to_payload();
    let sig = bob.sign_submission("bob", &op);
    let acc = handle.try_submit("bob", op, Some(sig)).unwrap();
    println!("  bob checks out base          ACCEPTED seq={}", acc.seq);

    let op = ViewOp::new(OpKind::SetRef {
        name: "main".into(),
        commit: c_base.clone(),
        prev: None,
    })
    .to_payload();
    let sig = mallory.sign_submission("mallory", &op);
    let err = handle.try_submit("mallory", op, Some(sig)).unwrap_err();
    println!("  mallory (unregistered key)   REJECTED: {err}");

    let op = ViewOp::new(OpKind::SetRef {
        name: "main".into(),
        commit: c_base.clone(),
        prev: None, // stale: claims main doesn't exist yet
    })
    .to_payload();
    let sig = bob.sign_submission("bob", &op);
    let err = handle.try_submit("bob", op, Some(sig)).unwrap_err();
    println!("  bob with a stale CAS         REJECTED: {}", &err[..err.len().min(80)]);

    section("4. Merge pipeline (L4): disjoint edits fold, real conflicts stay first-class");
    let left = "fn main() {\n    println!(\"v1\");\n}\n// alice: added docs\n";
    let right = "// bob: added header\nfn main() {\n    println!(\"v1\");\n}\n";
    let r = Pipeline::default_v1().merge(base_src, left, right);
    match &r.outcome {
        MergeOutcome::Resolved(text) => {
            println!("  disjoint edits -> RESOLVED by `{}` strategy:", r.strategy);
            for l in text.lines() {
                println!("    | {l}");
            }
        }
        _ => unreachable!(),
    }

    let l2 = "fn main() {\n    println!(\"alice wins\");\n}\n";
    let r2 = "fn main() {\n    println!(\"bob wins\");\n}\n";
    let r = Pipeline::default_v1().merge(base_src, l2, r2);
    let annotated = match &r.outcome {
        MergeOutcome::Conflict { annotated } => {
            println!("  same-line edits -> CONFLICT (no silent pick). Stored as a commit:");
            annotated.clone()
        }
        _ => unreachable!(),
    };

    // The conflict becomes a *valid* commit; bob keeps working on top.
    let b = put_blob(&mut store, base_src.as_bytes(), ChunkerParams::default()).unwrap();
    let l = put_blob(&mut store, l2.as_bytes(), ChunkerParams::default()).unwrap();
    let rr = put_blob(&mut store, r2.as_bytes(), ChunkerParams::default()).unwrap();
    let mut tree = BTreeMap::new();
    tree.insert(
        "main.rs".to_string(),
        TreeEntry::Conflict {
            base: Some(b),
            left: l,
            right: rr,
        },
    );
    let c_conflict = Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![c_base.clone()],
        tree,
        author: "merge".into(),
        message: "conflicted merge (first-class)".into(),
    }
    .put(&mut store)
    .unwrap();
    let op = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "bob".into(),
        commit: c_conflict.clone(),
        prev: Some(c_base),
    })
    .to_payload();
    let sig = bob.sign_submission("bob", &op);
    let acc = handle.try_submit("bob", op, Some(sig)).unwrap();
    println!("  bob's head -> conflicted commit {} ACCEPTED seq={} (work continues!)", short(&c_conflict), acc.seq);
    for l in annotated.lines().take(7) {
        println!("    | {l}");
    }

    section("5. Time travel (L1): the op log replays to any point");
    let log = sequencer.shutdown();
    let now = View::materialize(log.as_ref()).unwrap();
    let before = View::at(log.as_ref(), 2).unwrap();
    println!("  view NOW     : bob @ {} (conflicted)", short(now.workspaces.get("bob").unwrap()));
    println!("  view at op 2 : bob @ {} (before the merge -- undo is just prefix replay)", short(before.workspaces.get("bob").unwrap()));
    println!("  log entries  : {} total, every one signed + hash-chained", log.len());

    section("6. Round-trip a blob out of the store (verified on read)");
    let head = Commit::get(&store, now.workspaces.get("bob").unwrap()).unwrap();
    if let TreeEntry::Conflict { left, .. } = &head.tree["main.rs"] {
        let bytes = get_blob(&store, left).unwrap();
        println!("  left side of bob's conflict, straight from the chunk store:");
        for l in String::from_utf8_lossy(&bytes).lines() {
            println!("    | {l}");
        }
    }

    section("7. The daemon (L3): a real `git` push through choir-node");
    let work = std::env::temp_dir().join(format!("choir-demo-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let node = choir_node::Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    node.create_repo("demo/hello.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/demo/hello.git");
    println!("  daemon up at {url}");
    let clone_dir = work.join("clone");
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "demo")
            .env("GIT_AUTHOR_EMAIL", "demo@choir")
            .env("GIT_COMMITTER_NAME", "demo")
            .env("GIT_COMMITTER_EMAIL", "demo@choir")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&work, &["clone", "-q", &url, clone_dir.to_str().unwrap()]);
    std::fs::write(clone_dir.join("README.md"), "# pushed through choir-node\n").unwrap();
    git(&clone_dir, &["add", "."]);
    git(&clone_dir, &["commit", "-q", "-m", "hello from the demo"]);
    git(&clone_dir, &["push", "-q", "origin", "HEAD:main"]);
    println!("  cloned, committed, pushed. Fresh clone sees:");
    let verify_dir = work.join("verify");
    git(&work, &["clone", "-q", &url, verify_dir.to_str().unwrap()]);
    let log_out = git(&verify_dir, &["log", "--oneline"]);
    for l in log_out.lines() {
        println!("    | {l}");
    }
    node.unblock();
    std::fs::remove_dir_all(&work).ok();

    println!("\nAll seven layers exercised. This is the stack the plan calls L0-L8.");
}
