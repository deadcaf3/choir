//! Repository ownership as a landing gate (D42): on a protected ref of a
//! repository somebody holds `own` over, one owner's assent lands it and
//! nothing else does. A repository nobody owns keeps the two-operator
//! rule exactly as it was, which is what makes this additive.
//!
//! Assent has two forms and the gate does not care which: performing the
//! landing, or having approved a review naming that `(ref, commit)`.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::curl;
use crate::support::submit_body_legacy as submit_body;

/// A node with a reviewer pool, one protected ref, and an ACL the test
/// can rewrite between submissions — the file is read per landing, so a
/// grant takes effect without a restart and a test can flip the rule
/// without standing up a second node.
struct Owned {
    work: std::path::PathBuf,
    acl: std::path::PathBuf,
    api: String,
    node: std::sync::Arc<Node>,
}

const REF: &str = "agents/demo.git:refs/heads/main";

fn start(tag: &str, pool: &str, keys: Option<(&ActorKey, &str)>) -> (Owned, ActorKey) {
    let work = std::env::temp_dir().join(format!("choir-own-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    let acl = work.join("acl");
    std::fs::write(&pool_file, pool).unwrap();
    std::fs::write(&refs_file, format!("{REF}\n")).unwrap();
    // Parseable and granting nobody `own`, so every test starts on the
    // pre-D42 rule and has to opt itself in.
    std::fs::write(&acl, "nobody agents/demo read\n").unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = match keys {
        Some((bound, name)) => {
            registry.register(&bound.public_key_bytes()).unwrap();
            let keys_file = work.join("keys");
            std::fs::write(
                &keys_file,
                format!("{name} {}\n", hex_encode(&bound.public_key_bytes())),
            )
            .unwrap();
            Platform::start_reloading(
                registry,
                Box::new(MemLog::new()),
                ActorKey::generate(),
                Some(keys_file),
            )
        }
        None => Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()),
    }
    .unwrap()
    .with_reviewer_pool(pool_file)
    .with_protected_refs(refs_file)
    .with_acl_file(acl.clone())
    .with_required_review();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(platform);
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (
        Owned {
            work,
            acl,
            api: format!("http://127.0.0.1:{port}/api"),
            node,
        },
        author,
    )
}

impl Owned {
    fn submit(&self, key: &ActorKey, channel: &str, op: &ViewOp) -> (u16, serde_json::Value) {
        curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(key, channel, op),
            &format!("{}/submit", self.api),
        ])
    }

    fn head(&self) -> serde_json::Value {
        curl(&[&format!("{}/view", self.api)]).1["refs"][REF].clone()
    }

    fn own(&self, user: &str) {
        std::fs::write(&self.acl, format!("{user} agents/demo own\n")).unwrap();
    }

    fn done(self) {
        self.node.unblock();
        std::fs::remove_dir_all(&self.work).ok();
    }
}

fn oid(c: char) -> ContentHash {
    ContentHash::from_git_oid(&c.to_string().repeat(40)).unwrap()
}

/// The rule switch, proved by changing one file and nothing else.
///
/// The same one-approval review is refused while nobody owns the
/// repository and accepted once somebody does. It also pins the two
/// properties that make an owner's approval *sufficient*: it lands at
/// approval weight 1, and it lands while a drawn reviewer has never
/// answered — so the check cannot be routed through
/// `ReviewState::approved`, which waits for the whole list.
#[test]
fn one_owner_approval_lands_what_two_strangers_could_not() {
    let (node, author) = start("switch", "owner-a\nstranger-b\n", None);

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r1".into(),
        target: oid('2'),
        reviewers: Vec::new(),
        target_ref: Some(REF.into()),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &request);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
    assert!(drawn.contains(&"owner-a".to_string()), "{resp}");
    assert_eq!(drawn.len(), 2, "the draw must seat both operators: {resp}");

    // Exactly one of the two answers. The review is therefore incomplete
    // and carries weight 1.
    let verdict = ViewOp::new(OpKind::PostVerdict {
        id: "r1".into(),
        reviewer: "owner-a".into(),
        verdict: Verdict::Approve,
        note: "mine to land".into(),
    });
    assert_eq!(node.submit(&author, "owner-a", &verdict).0, 200);
    let (_, view) = curl(&[&format!("{}/view", node.api)]);
    assert_eq!(view["reviews"]["r1"]["approved"], false, "{view}");
    assert_eq!(view["reviews"]["r1"]["approval_weight"], 1, "{view}");

    let advance = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });

    // Nobody owns this repository yet, so the pre-D42 rule applies. It
    // scores this review at **zero**, not one: `approval_weight_for`
    // filters on `approved()`, and an incomplete review is not approved,
    // so the one verdict that exists contributes nothing at all. That is
    // the gap the ownership path has to step over, and the reason it
    // cannot be written in terms of `approved()`.
    let (code, resp) = node.submit(&author, "writer/agent", &advance);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "review_required", "{resp}");
    assert_eq!(resp["actual"], "approval weight 0", "{resp}");
    assert_eq!(node.head(), format!("11-{}", "1".repeat(40)).as_str());

    // One line in the ACL, and the same submission is authorized — by the
    // approval that was already there, from the reviewer who was already
    // drawn. Nothing about the review changed.
    node.own("owner-a");
    let (code, resp) = node.submit(&author, "writer/agent", &advance);
    assert_eq!(code, 200, "an owner's approval must be sufficient: {resp}");
    assert_eq!(node.head(), format!("11-{}", "2".repeat(40)).as_str());

    node.done();
}

/// The necessary half, which is the security-relevant direction: once a
/// repository is owned, the approval-weight rule is not an alternative
/// route to the same place. Two independent operators, both approving,
/// with a complete and `approved` review, still do not reach it.
#[test]
fn an_owned_ref_refuses_every_approval_but_an_owner_s() {
    let (node, author) = start("necessary", "one\ntwo\n", None);
    // The owner is deliberately not in the reviewer pool: whoever the
    // draw seats, it cannot seat them.
    node.own("absent-owner");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r1".into(),
        target: oid('2'),
        reviewers: Vec::new(),
        target_ref: Some(REF.into()),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &request);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
    for who in &drawn {
        let verdict = ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: who.clone(),
            verdict: Verdict::Approve,
            note: String::new(),
        });
        assert_eq!(node.submit(&author, who, &verdict).0, 200);
    }
    let (_, view) = curl(&[&format!("{}/view", node.api)]);
    assert_eq!(view["reviews"]["r1"]["approved"], true, "{view}");
    assert_eq!(view["reviews"]["r1"]["approval_weight"], 2, "{view}");

    let advance = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &advance);
    assert_eq!(
        code, 400,
        "a fully approved review must not reach an owned ref: {resp}"
    );
    assert_eq!(resp["code"], "review_required", "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("no owner has assented"),
        "the refusal must name the reason, not report a weight: {resp}"
    );
    assert_eq!(node.head(), format!("11-{}", "1".repeat(40)).as_str());

    node.done();
}

/// Assent by performing the landing, and the identity rule that makes it
/// safe. An owner submitting under their own bound key lands with no
/// review in existence at all. An unbound key submitting on the very same
/// channel does not, because an unbound key is unconstrained in what it
/// calls itself — so honouring the channel there would let any trusted
/// key name itself an owner.
#[test]
fn an_owner_lands_alone_and_an_unbound_key_cannot_borrow_the_name() {
    let boss = ActorKey::generate();
    let (node, unbound) = start("acting", "one\ntwo\n", Some((&boss, "boss")));
    node.own("boss");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&unbound, "writer/agent", &create).0, 200);

    let advance = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });

    // Same channel, same payload, different key. The unbound one is
    // refused for want of a resolvable identity, not for want of a grant.
    let (code, resp) = node.submit(&unbound, "boss", &advance);
    assert_eq!(code, 400, "an unbound key must not act as `boss`: {resp}");
    assert_eq!(resp["code"], "review_required", "{resp}");
    assert_eq!(
        resp["actual"], "a landing by an identity the node cannot resolve to an ACL user",
        "{resp}"
    );
    assert_eq!(node.head(), format!("11-{}", "1".repeat(40)).as_str());

    // The owner's own key lands it, with no review, no reviewers and no
    // approvals anywhere in the log.
    let (code, resp) = node.submit(&boss, "boss", &advance);
    assert_eq!(code, 200, "an owner must land alone: {resp}");
    assert_eq!(node.head(), format!("11-{}", "2".repeat(40)).as_str());
    let (_, view) = curl(&[&format!("{}/view", node.api)]);
    assert_eq!(
        view["reviews"],
        serde_json::json!({}),
        "the landing must not have needed a review to exist: {view}"
    );

    node.done();
}

/// Fails closed. The ownership rule reads the operator's file on every
/// gated landing, and losing that file must refuse rather than conclude
/// there are no owners — "no owners" is precisely the branch that falls
/// back to the weaker rule, so a missing file would silently demote the
/// gate to the policy it was configured to replace.
#[test]
fn a_missing_acl_refuses_the_landing_instead_of_forgetting_the_owners() {
    let boss = ActorKey::generate();
    let (node, other) = start("failclosed", "one\ntwo\n", Some((&boss, "boss")));
    node.own("boss");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&other, "writer/agent", &create).0, 200);

    std::fs::remove_file(&node.acl).unwrap();
    let advance = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });
    let (code, resp) = node.submit(&boss, "boss", &advance);
    assert_eq!(code, 400, "a missing ACL must refuse: {resp}");
    assert_eq!(resp["code"], "policy_unavailable", "{resp}");
    assert_eq!(node.head(), format!("11-{}", "1".repeat(40)).as_str());

    // A malformed one is refused for the same reason: a table understood
    // in part is not a table.
    std::fs::write(&node.acl, "boss agents/demo sideways\n").unwrap();
    let (code, resp) = node.submit(&boss, "boss", &advance);
    assert_eq!(code, 400, "an unparseable ACL must refuse: {resp}");
    assert_eq!(resp["code"], "policy_unavailable", "{resp}");

    // Restored, and the owner lands: the refusal was about the file, not
    // about them.
    node.own("boss");
    assert_eq!(node.submit(&boss, "boss", &advance).0, 200);
    assert_eq!(node.head(), format!("11-{}", "2".repeat(40)).as_str());

    node.done();
}
