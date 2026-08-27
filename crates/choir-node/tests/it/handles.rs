//! D46 and D75: what a person is called here, and who decides it.
//!
//! The property every test circles is the one that cannot be repaired
//! later: whatever ends up as the *principal* is what `channel_for`
//! turns into a channel, which `signing_hash` covers, which the log
//! keeps forever. A display name that reached the principal by any route
//! would be unrewritable, so these assert on where the readable name is
//! **not**.
//!
//! What changed at D75 is who chooses the permanent half. D46 stopped an
//! operator from welding somebody's real name into the log by having the
//! node mint an opaque handle instead, which is one step better and one
//! step short: the person it is about still never got asked. The seat on
//! an invite is now left open and the holder picks their own username on
//! the page that redeems it. D46's rule is not weakened by that; it is
//! the reason for it.

use std::collections::BTreeSet;

use choir_node::accounts::Accounts;

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("test json parses")
}

/// A store of its own. These modules share a process and run on parallel
/// threads, so a shared directory name is two tests overwriting each
/// other's file.
fn store(tag: &str) -> (Accounts, std::path::PathBuf, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-handles-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let path = work.join("accounts.json");
    let store = Accounts::open(path.clone(), None, BTreeSet::new()).expect("store opens");
    (store, path, work)
}

const NAME: &str = "Ada Lovelace";

/// The invite id out of a mint, for the tests that go on to redeem one.
fn invite_id(body: &str) -> String {
    json(body)["invite"]
        .as_str()
        .expect("invite")
        .split_once(':')
        .expect("id:secret")
        .0
        .to_string()
}

/// The whole point of D75: the issuer supplies what to call somebody and
/// gets back no principal at all, because that is not theirs to decide.
#[test]
fn an_invite_with_a_display_name_leaves_the_username_to_its_holder() {
    let (store, _path, work) = store("mints");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let issued = json(&body);
    assert!(
        issued["user"].is_null(),
        "the node decided a principal for somebody it has not met: {body}"
    );
    // Returned at issue because this is the only moment the issuer
    // learns what was recorded.
    assert_eq!(issued["display_name"].as_str(), Some(NAME), "{body}");

    // And the holder names themselves.
    let (status, body) = store.redeem(&invite_id(&body), &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["user"].as_str(), Some("ada"), "{body}");
    assert!(store.has_account("ada"));

    std::fs::remove_dir_all(&work).ok();
}

/// An open seat cannot be redeemed without a name. There is no fallback
/// that would quietly reintroduce a chosen-for-you principal.
#[test]
fn an_open_seat_refuses_to_pick_a_name_for_anybody() {
    let (store, _path, work) = store("noname");
    let (status, body) = store.invite("alice", &json(r#"{"grants":["agents/demo read"]}"#));
    assert_eq!(status, 200, "{body}");
    let id = invite_id(&body);

    let (status, body) = store.redeem(&id, &json("{}"));
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("pick your name"), "{body}");
    // And the invite is intact, so the repair is retrying with a name.
    let (status, body) = store.redeem(&id, &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{body}");

    std::fs::remove_dir_all(&work).ok();
}

/// A name somebody else holds, or held, is refused at the moment it is
/// chosen -- which is the moment a person can act on the answer.
#[test]
fn a_username_already_spoken_for_is_refused_when_it_is_typed() {
    let (store, _path, work) = store("taken");
    let seat = |grants: &str| {
        let (status, body) = store.invite("alice", &json(grants));
        assert_eq!(status, 200, "{body}");
        invite_id(&body)
    };

    let first = seat(r#"{"grants":["agents/demo read"]}"#);
    let (status, body) = store.redeem(&first, &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{body}");

    let second = seat(r#"{"grants":["agents/demo read"]}"#);
    let (status, body) = store.redeem(&second, &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("taken"), "{body}");

    // Revoked is not free either: the log still attributes that name's
    // history to whoever held it.
    let (status, body) = store.revoke(&json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{body}");
    let third = seat(r#"{"grants":["agents/demo read"]}"#);
    let (status, body) = store.redeem(&third, &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("never reused"), "{body}");

    std::fs::remove_dir_all(&work).ok();
}

/// A chosen name goes through the same grammar an issued one does, so
/// nothing a person types can reach the log that an operator could not
/// have written.
#[test]
fn a_chosen_username_is_graded_by_the_same_rule_as_an_issued_one() {
    let (store, _path, work) = store("grammar");
    for bad in [
        "Ada Lovelace",
        "ada/agent",
        "invite-x",
        "ask-x",
        "open-seat",
        "",
        "@node",
    ] {
        let (status, body) = store.invite("alice", &json(r#"{"grants":["agents/demo read"]}"#));
        assert_eq!(status, 200, "{body}");
        let id = invite_id(&body);
        let (status, body) = store.redeem(&id, &serde_json::json!({ "user": bad }));
        assert_eq!(status, 400, "`{bad}` was accepted: {body}");
    }

    std::fs::remove_dir_all(&work).ok();
}

/// The property stated directly against the function that welds a name
/// into the log. Asserted here rather than inferred from the invite,
/// because this is the join D46 exists to break.
#[test]
fn a_display_name_never_reaches_the_channel() {
    let (store, _path, work) = store("channel");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = store.redeem(&invite_id(&body), &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{body}");
    let user = json(&body)["user"]
        .as_str()
        .expect("a principal")
        .to_string();

    let channel = choir_node::quota::channel_for(&user);
    assert_eq!(channel, format!("git/{user}"));
    assert!(
        !channel.contains("Ada") && !channel.contains("Lovelace"),
        "the readable name reached the channel: {channel}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// Two different decisions about what the log records forever, so
/// sending both is a question the node must not answer by guessing.
///
/// Sending *neither* is no longer a refusal (D75): it is an open seat
/// with nothing written down, which is exactly what a link handed to
/// somebody you have not met yet is.
#[test]
fn naming_an_account_and_describing_it_are_not_both_allowed() {
    let (store, _path, work) = store("both");
    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"bob","display_name":"Bob","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("not both"), "{body}");

    let (status, body) = store.invite("alice", &json(r#"{"grants":["agents/demo read"]}"#));
    assert_eq!(
        status, 200,
        "an invite that names nobody is the ordinary one: {body}"
    );
    assert!(json(&body)["user"].is_null(), "{body}");

    std::fs::remove_dir_all(&work).ok();
}

/// `user` still means "this exact string is the principal". A bot or an
/// operator credential wants it, and it must keep working unchanged --
/// including refusing to let the redeemer rename themselves out of it.
#[test]
fn an_explicit_user_is_still_the_principal_and_gains_no_display_name() {
    let (store, path, work) = store("explicit");
    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"buildbot","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["user"].as_str(), Some("buildbot"));
    assert!(json(&body)["display_name"].is_null(), "{body}");

    // A `user` in the body is ignored, not honoured: the issuer decided.
    let (status, out) = store.redeem(&invite_id(&body), &json(r#"{"user":"somebodyelse"}"#));
    assert_eq!(status, 200, "{out}");
    assert_eq!(json(&out)["user"].as_str(), Some("buildbot"), "{out}");
    assert!(!store.has_account("somebodyelse"));

    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(
        !on_disk.contains("display_name"),
        "an account that has no display name should not render the key: {on_disk}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// Persistence, asserted against the file rather than the live store:
/// "returned by a lookup" and "written down" are different promises and
/// only the second survives a restart.
#[test]
fn a_username_and_its_display_name_survive_a_reopen() {
    let (store, path, work) = store("reopen");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");

    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(on_disk.contains(NAME), "the display name was not persisted");

    let (status, out) = store.redeem(&invite_id(&body), &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{out}");

    let reopened = Accounts::open(path, None, BTreeSet::new()).expect("store reopens");
    assert!(reopened.has_account("ada"), "the username did not survive");
    assert_eq!(reopened.display_name("ada").as_deref(), Some(NAME));

    std::fs::remove_dir_all(&work).ok();
}

/// The claim the whole decision rests on: revoking an account destroys
/// the readable name while the username -- the half the log already
/// keeps forever -- survives as a retired entry that names nobody.
///
/// Asserted against the file on disk as well as the live store, because
/// "not returned by a lookup" and "not written down" are different
/// promises and only the second survives a restart.
#[test]
fn revoking_forgets_the_name_and_keeps_the_username() {
    let (store, path, work) = store("forget");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let (status, out) = store.redeem(&invite_id(&body), &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{out}");
    assert_eq!(store.display_name("ada").as_deref(), Some(NAME));

    let (status, out) = store.revoke(&json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{out}");

    assert_eq!(
        store.display_name("ada"),
        None,
        "the name outlived the account"
    );
    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(
        !on_disk.contains(NAME) && !on_disk.contains("Lovelace"),
        "a revoked account left its holder's name on disk: {on_disk}"
    );
    // The username stays, because the log already carries it and a name
    // that could be reissued would hand a second person the first
    // person's signed attribution.
    assert!(
        on_disk.contains("ada"),
        "the username should be retired, not forgotten: {on_disk}"
    );
    assert!(!store.has_account("ada"));

    std::fs::remove_dir_all(&work).ok();
}

/// The roster is the input `choir acl render` regenerates comments from,
/// so it must pair only the accounts that have something to say.
#[test]
fn the_roster_pairs_usernames_with_names_and_omits_the_rest() {
    let (store, _path, work) = store("roster");

    // Both invites are redeemed, because the roster reads *accounts*.
    // An earlier draft redeemed only one and asserted
    // `roster.is_empty() || ...`, which is satisfied by the roster never
    // being populated at all -- a test that passes by nothing happening.
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let (status, out) = store.redeem(&invite_id(&body), &json(r#"{"user":"ada"}"#));
    assert_eq!(status, 200, "{out}");

    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"buildbot","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 200, "{body}");
    let (status, out) = store.redeem(&invite_id(&body), &json("{}"));
    assert_eq!(status, 200, "{out}");

    let roster = store.roster();
    assert_eq!(
        roster.get("ada").map(String::as_str),
        Some(NAME),
        "the account with a name is missing from the roster: {roster:?}"
    );
    assert!(
        !roster.contains_key("buildbot"),
        "an account with no display name should not appear: {roster:?}"
    );
    assert_eq!(
        roster.len(),
        1,
        "exactly one account has a name: {roster:?}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A store written before D46 has no `display_name` anywhere, and must
/// load as an ordinary store whose accounts simply have none. Written as
/// a raw string rather than built with `json!`, because the input this
/// claim is about is a file this binary did not write.
#[test]
fn a_store_written_before_d46_still_loads() {
    let (_store, path, work) = store("legacy");
    std::fs::write(
        &path,
        r#"{"format_version":1,"accounts":[{"user":"carol","token_hash":"1e-00","grants":["agents/demo read"],"ssh_keys":[],"passkeys":[],"created_at":1}],"invites":[],"retired":[]}
"#,
    )
    .expect("write legacy store");

    let reopened = Accounts::open(path, None, BTreeSet::new()).expect("a pre-D46 store loads");
    assert!(reopened.has_account("carol"));
    assert_eq!(reopened.len(), 1);

    std::fs::remove_dir_all(&work).ok();
}

/// A display name spelled like a handle is refused at issue.
///
/// The page renders an unresolvable handle as itself, silently, because
/// that is the only way a deleted account can look like nothing. So a
/// display name of twelve hex characters renders exactly as somebody
/// else's deleted account, and the person it impersonates cannot correct
/// the record -- their name is precisely what was deleted. The refusal
/// belongs here rather than in the renderer, which by then is holding
/// two identical strings with no way to tell which is which.
///
/// Still enforced after D75, because accounts issued before it hold
/// handles and those handles are exactly what cannot be taken back.
#[test]
fn a_display_name_spelled_like_a_handle_is_refused() {
    let (store, _path, work) = store("handle-shaped");

    // Upper case as well: a reader comparing a name against a handle is
    // not comparing bytes.
    for shaped in ["7f3ac2ab19cd", "7F3AC2AB19CD", "000000000000"] {
        let (status, body) = store.invite(
            "alice",
            &json(&format!(
                r#"{{"display_name":"{shaped}","grants":["agents/demo read"]}}"#
            )),
        );
        // The consequence first: no invite exists to redeem. A status
        // code alone is satisfied by a refusal arriving for any reason.
        assert!(
            store.list_json()["invites"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "a handle-shaped name was issued an invite: {body}"
        );
        assert_eq!(status, 400, "{shaped} was accepted: {body}");
        assert!(
            body.contains("handle"),
            "the refusal does not say what is wrong with {shaped}: {body}"
        );
    }

    // ...and the shape is the whole of the rule: eleven characters, or
    // twelve with a non-hex one, is an ordinary name.
    for fine in ["7f3ac2ab19c", "7f3ac2ab19cdz", "Ada Lovelace"] {
        let (status, body) = store.invite(
            "alice",
            &json(&format!(
                r#"{{"display_name":"{fine}","grants":["agents/demo read"]}}"#
            )),
        );
        assert_eq!(status, 200, "{fine} was refused: {body}");
    }

    std::fs::remove_dir_all(&work).ok();
}
