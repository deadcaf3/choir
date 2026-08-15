//! `Submit`: landing a review with the reason it was allowed (D43).
//!
//! The gate runs at apply time against an ACL and a protected-ref list
//! that are not in the log, so a plain `SetRef` records that a merge
//! happened and nothing about what permitted it. `Submit` is the same ref
//! move with the gate's own answer attached.
//!
//! The answer is never a client's to assert. Every test here builds the
//! op the way a real client has to: submit, read the authorization the
//! node's gate produced out of the rejection, sign that, submit again.
//! That is one implementation of the rule rather than two, which is the
//! property the record's whole value rests on.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{Authorization, Basis, OpKind, Verdict, ViewOp};

use crate::support::curl;
use crate::support::submit_body_legacy as submit_body;

const REF: &str = "agents/demo.git:refs/heads/main";
const OPEN: &str = "agents/demo.git:refs/heads/topic";

/// A gated node whose ACL and log bindings a test can build up as it
/// goes. Holds the node key, because `BindKey` is node-only and the
/// approver ids in an authorization come from nowhere else.
struct Gated {
    work: std::path::PathBuf,
    acl: std::path::PathBuf,
    api: String,
    node: std::sync::Arc<Node>,
    node_key: ActorKey,
}

fn start(tag: &str, pool: &str, bound: &[(&ActorKey, &str)]) -> (Gated, ActorKey) {
    let work = std::env::temp_dir().join(format!("choir-land-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    let keys_file = work.join("keys");
    let acl = work.join("acl");
    std::fs::write(&pool_file, pool).unwrap();
    // `OPEN` is deliberately absent: one ref the gate does not look at,
    // so "a record for a decision nothing made" has somewhere to be
    // attempted.
    std::fs::write(&refs_file, format!("{REF}\n")).unwrap();
    std::fs::write(&acl, "nobody agents/demo read\n").unwrap();

    let author = ActorKey::generate();
    let node_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    registry.register(&node_key.public_key_bytes()).unwrap();
    let mut names = String::new();
    for (key, name) in bound {
        registry.register(&key.public_key_bytes()).unwrap();
        names.push_str(&format!("{name} {}\n", hex_encode(&key.public_key_bytes())));
    }
    std::fs::write(&keys_file, names).unwrap();

    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
        Some(keys_file),
    )
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
        Gated {
            work,
            acl,
            api: format!("http://127.0.0.1:{port}/api"),
            node,
            node_key,
        },
        author,
    )
}

impl Gated {
    fn submit(&self, key: &ActorKey, channel: &str, op: &ViewOp) -> (u16, serde_json::Value) {
        curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(key, channel, op),
            &format!("{}/submit", self.api),
        ])
    }

    fn head(&self, name: &str) -> serde_json::Value {
        curl(&[&format!("{}/view", self.api)]).1["refs"][name].clone()
    }

    fn own(&self, user: &str) {
        std::fs::write(&self.acl, format!("{user} agents/demo own\n")).unwrap();
    }

    /// Puts `key` in the log under `channel`, which is the only place an
    /// authorization's approver ids can come from.
    fn bind(&self, key: &ActorKey, channel: &str) {
        let op = ViewOp::new(OpKind::BindKey {
            operator: channel.to_string(),
            key: key.actor_id(),
            channel: Some(channel.to_string()),
        });
        let (code, resp) = self.submit(&self.node_key, "node/bind", &op);
        assert_eq!(code, 200, "binding {channel}: {resp}");
    }

    /// Opens a review of `commit` landing on `name`, and returns the
    /// reviewers the node drew.
    fn review(&self, author: &ActorKey, id: &str, name: &str, commit: &ContentHash) -> Vec<String> {
        let op = ViewOp::new(OpKind::RequestReview {
            id: id.to_string(),
            target: commit.clone(),
            reviewers: Vec::new(),
            target_ref: Some(name.to_string()),
        });
        let (code, resp) = self.submit(author, "writer/agent", &op);
        assert_eq!(code, 200, "{resp}");
        serde_json::from_value(resp["reviewers"].clone()).unwrap()
    }

    fn approve(&self, key: &ActorKey, reviewer: &str, id: &str) {
        let op = ViewOp::new(OpKind::PostVerdict {
            id: id.to_string(),
            reviewer: reviewer.to_string(),
            verdict: Verdict::Approve,
            note: String::new(),
        });
        let (code, resp) = self.submit(key, reviewer, &op);
        assert_eq!(code, 200, "{reviewer} approving {id}: {resp}");
    }

    /// The client protocol, in one call: submit a landing with a
    /// deliberately wrong authorization, and hand back what the gate said
    /// it should have been. A client cannot know this in advance without
    /// a second copy of the gate, which is the thing that must not exist.
    fn ask_authorization(
        &self,
        key: &ActorKey,
        channel: &str,
        review: &str,
        name: &str,
        commit: &ContentHash,
        prev: Option<ContentHash>,
    ) -> (serde_json::Value, Authorization) {
        let bogus = Authorization::new(
            Basis::OwnerLanded {
                owner: "not-a-real-owner".into(),
            },
            Vec::new(),
        );
        let op = ViewOp::new(OpKind::Submit {
            review: review.to_string(),
            name: name.to_string(),
            commit: commit.clone(),
            prev,
            authorization: bogus,
        });
        let (code, resp) = self.submit(key, channel, &op);
        assert_eq!(code, 400, "a claimed authorization must never land: {resp}");
        let expected = resp["expected"].as_str().unwrap_or_default().to_string();
        let parsed = serde_json::from_str(&expected)
            .unwrap_or_else(|e| panic!("`expected` must be the gate's own record: {expected} {e}"));
        (resp, parsed)
    }

    #[allow(clippy::too_many_arguments)]
    fn land(
        &self,
        key: &ActorKey,
        channel: &str,
        review: &str,
        name: &str,
        commit: &ContentHash,
        prev: Option<ContentHash>,
        authorization: Authorization,
    ) -> (u16, serde_json::Value) {
        let op = ViewOp::new(OpKind::Submit {
            review: review.to_string(),
            name: name.to_string(),
            commit: commit.clone(),
            prev,
            authorization,
        });
        self.submit(key, channel, &op)
    }

    fn done(self) {
        self.node.unblock();
        std::fs::remove_dir_all(&self.work).ok();
    }
}

fn oid(c: char) -> ContentHash {
    ContentHash::from_git_oid(&c.to_string().repeat(40)).unwrap()
}

fn hex(c: char) -> String {
    format!("11-{}", c.to_string().repeat(40))
}

/// The whole point of the op, on the rule that motivated it.
///
/// A non-owner lands a commit because an owner approved it, and the
/// entry records *which* rule allowed it and *who* the approval belonged
/// to — as an actor id, resolved from the log's own binding rather than
/// from the mutable keys file. Under D42 this landing is authorized with
/// one approval on an incomplete review, so an approver list alone could
/// not distinguish it from an owner landing alone with none.
#[test]
fn an_owner_s_approval_lands_and_the_entry_records_whose_it_was() {
    let owner = ActorKey::generate();
    let (node, author) = start("approved", "owner-a\nstranger-b\n", &[(&owner, "owner-a")]);
    node.bind(&owner, "owner-a");
    node.own("owner-a");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let drawn = node.review(&author, "r1", REF, &oid('2'));
    assert!(drawn.contains(&"owner-a".to_string()), "{drawn:?}");
    node.approve(&owner, "owner-a", "r1");

    // The review is incomplete and its weight is 1, so nothing about the
    // weight rule could have admitted this.
    let (_, view) = curl(&[&format!("{}/view", node.api)]);
    assert_eq!(view["reviews"]["r1"]["approved"], false, "{view}");

    let (rejected, authorization) = node.ask_authorization(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
    );
    assert_eq!(rejected["code"], "review_required", "{rejected}");
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("approved by owner owner-a"),
        "the refusal must name the rule it would have applied: {rejected}"
    );
    assert_eq!(
        authorization.basis,
        Basis::OwnerApproved {
            owner: "owner-a".into()
        }
    );
    assert_eq!(
        authorization.approvers,
        vec![owner.actor_id()],
        "the approver must be an actor id the log itself binds, not a name"
    );

    let (code, resp) = node.land(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
        authorization,
    );
    assert_eq!(code, 200, "{resp}");
    assert_eq!(node.head(REF), hex('2').as_str());

    node.done();
}

/// An owner landing their own review records `OwnerLanded`, with an
/// empty approver list that means "no approval was required" rather than
/// "this predates the field".
///
/// It also pins the precedence: this owner *also* approved the review, so
/// both answers are available, and the record says the one the gate
/// actually rested on.
#[test]
fn an_owner_landing_alone_is_recorded_as_having_landed_not_approved() {
    let boss = ActorKey::generate();
    let (node, author) = start("landed", "boss\nother\n", &[(&boss, "boss")]);
    node.bind(&boss, "boss");
    node.own("boss");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let drawn = node.review(&author, "r1", REF, &oid('2'));
    assert!(drawn.contains(&"boss".to_string()), "{drawn:?}");
    node.approve(&boss, "boss", "r1");

    let (_, authorization) =
        node.ask_authorization(&boss, "boss", "r1", REF, &oid('2'), Some(oid('1')));
    assert_eq!(
        authorization.basis,
        Basis::OwnerLanded {
            owner: "boss".into()
        },
        "performing the landing is the assent the gate rested on"
    );
    assert!(
        authorization.approvers.is_empty(),
        "an owner-landed record states that no approval was required: {authorization:?}"
    );

    let (code, resp) = node.land(
        &boss,
        "boss",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
        authorization,
    );
    assert_eq!(code, 200, "{resp}");
    assert_eq!(node.head(REF), hex('2').as_str());

    node.done();
}

/// The unowned repository keeps the weight rule, and the record pins the
/// threshold that was in force — the half of "the rule is not pinned"
/// that could be closed. The approvers are the ones the weight actually
/// counted, one per operator, not everyone who clicked approve.
#[test]
fn the_weight_basis_records_the_threshold_and_the_approvals_it_counted() {
    let one = ActorKey::generate();
    let two = ActorKey::generate();
    let (node, author) = start("weight", "one\ntwo\n", &[(&one, "one"), (&two, "two")]);
    node.bind(&one, "one");
    node.bind(&two, "two");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let drawn = node.review(&author, "r1", REF, &oid('2'));
    assert_eq!(drawn.len(), 2, "{drawn:?}");
    node.approve(&one, "one", "r1");
    node.approve(&two, "two", "r1");

    let (_, authorization) = node.ask_authorization(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
    );
    assert_eq!(
        authorization.basis,
        Basis::ApprovalWeight {
            required: 2,
            met: 2
        },
        "the threshold in force belongs in the entry, not in config history"
    );
    let mut got = authorization.approvers.clone();
    let mut want = vec![one.actor_id(), two.actor_id()];
    got.sort_by_key(ContentHash::to_hex);
    want.sort_by_key(ContentHash::to_hex);
    assert_eq!(got, want, "both counted approvals must be named");

    let (code, resp) = node.land(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
        authorization,
    );
    assert_eq!(code, 200, "{resp}");
    assert_eq!(node.head(REF), hex('2').as_str());

    node.done();
}

/// An approval the log cannot name refuses the landing rather than
/// recording it with a gap — and, in the same breath, does not disturb
/// the plain `SetRef` gate, which admits the identical landing.
///
/// That pairing is the finding: a record requirement that silently
/// tightened the push gate would be a policy change wearing the shape of
/// an audit change.
#[test]
fn an_approval_with_no_binding_refuses_the_record_but_not_the_push() {
    let one = ActorKey::generate();
    let two = ActorKey::generate();
    let (node, author) = start("unbound", "one\ntwo\n", &[(&one, "one"), (&two, "two")]);
    // Only one of the two approvers is bound in the log.
    node.bind(&one, "one");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let drawn = node.review(&author, "r1", REF, &oid('2'));
    assert_eq!(drawn.len(), 2, "{drawn:?}");
    node.approve(&one, "one", "r1");
    node.approve(&two, "two", "r1");

    let op = ViewOp::new(OpKind::Submit {
        review: "r1".into(),
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
        authorization: Authorization::new(
            Basis::ApprovalWeight {
                required: 2,
                met: 2,
            },
            Vec::new(),
        ),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &op);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("cannot name its approvers"),
        "the refusal must be about naming the approver, not about the weight: {resp}"
    );
    assert!(
        resp["next"].as_str().unwrap().contains("bind"),
        "the repair is binding the key: {resp}"
    );
    assert_eq!(node.head(REF), hex('1').as_str());

    // The same landing, as an ordinary ref move, is still admitted: this
    // phase added a record, not a rule.
    let advance = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &advance);
    assert_eq!(code, 200, "the push gate must be unchanged: {resp}");
    assert_eq!(node.head(REF), hex('2').as_str());

    node.done();
}

/// A landing record is refused wherever no rule would examine the
/// landing. Otherwise `Submit` becomes a way to mint an authorization for
/// a decision nothing made — which reads, to every later auditor, exactly
/// like one that was checked.
#[test]
fn a_submit_on_a_ref_no_rule_gates_is_refused() {
    let one = ActorKey::generate();
    let (node, author) = start("ungated", "one\ntwo\n", &[(&one, "one")]);
    node.bind(&one, "one");

    let create = ViewOp::new(OpKind::SetRef {
        name: OPEN.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    let drawn = node.review(&author, "r1", OPEN, &oid('2'));
    assert!(drawn.contains(&"one".to_string()), "{drawn:?}");
    node.approve(&one, "one", "r1");

    let op = ViewOp::new(OpKind::Submit {
        review: "r1".into(),
        name: OPEN.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
        authorization: Authorization::new(
            Basis::ApprovalWeight {
                required: 2,
                met: 2,
            },
            Vec::new(),
        ),
    });
    let (code, resp) = node.submit(&author, "writer/agent", &op);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("not gated"),
        "{resp}"
    );
    assert!(
        resp["next"].as_str().unwrap().contains("SetRef"),
        "the repair is the op that makes no claim: {resp}"
    );
    assert_eq!(node.head(OPEN), hex('1').as_str());

    // And the ungated ref still moves, by the op that asserts nothing.
    let advance = ViewOp::new(OpKind::SetRef {
        name: OPEN.into(),
        commit: oid('2'),
        prev: Some(oid('1')),
    });
    assert_eq!(node.submit(&author, "writer/agent", &advance).0, 200);
    assert_eq!(node.head(OPEN), hex('2').as_str());

    node.done();
}

/// Two landings race one ref. The loser is refused on the compare-and-set
/// inside its own payload, and the rejection names the oid the ref
/// actually holds, so the client can rebase rather than guess.
///
/// This is the buildable half of the brief's submit-versus-train race:
/// no crate depends on `choir-queue`, so the mechanism two submits
/// actually contend on is the CAS, not the train.
#[test]
fn the_loser_of_a_landing_race_is_told_what_the_ref_now_holds() {
    let one = ActorKey::generate();
    let two = ActorKey::generate();
    let (node, author) = start("race", "one\ntwo\n", &[(&one, "one"), (&two, "two")]);
    node.bind(&one, "one");
    node.bind(&two, "two");

    let create = ViewOp::new(OpKind::SetRef {
        name: REF.into(),
        commit: oid('1'),
        prev: None,
    });
    assert_eq!(node.submit(&author, "writer/agent", &create).0, 200);

    // Two reviews of two different commits, both proposing the same ref,
    // both fully approved: two merge buttons a person could press.
    for (id, commit) in [("r1", oid('2')), ("r2", oid('3'))] {
        let drawn = node.review(&author, id, REF, &commit);
        assert_eq!(drawn.len(), 2, "{drawn:?}");
        node.approve(&one, "one", id);
        node.approve(&two, "two", id);
    }

    let (_, first) = node.ask_authorization(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
    );
    let (_, second) = node.ask_authorization(
        &author,
        "writer/agent",
        "r2",
        REF,
        &oid('3'),
        Some(oid('1')),
    );

    let (code, resp) = node.land(
        &author,
        "writer/agent",
        "r1",
        REF,
        &oid('2'),
        Some(oid('1')),
        first,
    );
    assert_eq!(code, 200, "{resp}");

    // The second was authorized against a head that has since moved.
    let (code, resp) = node.land(
        &author,
        "writer/agent",
        "r2",
        REF,
        &oid('3'),
        Some(oid('1')),
        second,
    );
    assert_eq!(code, 400, "a stale landing must not clobber: {resp}");
    assert_eq!(
        resp["actual"],
        serde_json::Value::String(hex('2')),
        "the rejection must name the oid the ref now holds: {resp}"
    );
    assert_eq!(node.head(REF), hex('2').as_str());

    node.done();
}
