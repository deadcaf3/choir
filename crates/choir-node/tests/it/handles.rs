//! D46: an account can be named by an opaque handle, with the readable
//! name held beside it where revoking can delete it.
//!
//! The property every test here circles is the one that cannot be
//! repaired later: whatever ends up as the *principal* is what
//! `channel_for` turns into a channel, which `signing_hash` covers,
//! which the log keeps forever. A display name that reached the
//! principal by any route would be unrewritable, so these assert on
//! where the readable name is **not**.

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

/// The whole point: the issuer supplies a person's name and the
/// principal that comes back is not it.
#[test]
fn an_invite_with_a_display_name_mints_a_handle_instead() {
    let (store, _path, work) = store("mints");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let issued = json(&body);
    let user = issued["user"].as_str().expect("a principal");

    assert_ne!(user, NAME, "the principal is the person's name");
    assert!(
        !user.to_lowercase().contains("ada") && !user.to_lowercase().contains("lovelace"),
        "the principal carries part of the name: {user}"
    );
    assert!(
        user.len() == 12 && user.chars().all(|c| c.is_ascii_hexdigit()),
        "a handle should be 12 hex characters, got {user}"
    );
    // Returned at issue because this is the only moment the issuer
    // learns the pairing from the node.
    assert_eq!(issued["display_name"].as_str(), Some(NAME), "{body}");

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
#[test]
fn naming_an_account_and_describing_it_are_not_both_allowed() {
    let (store, _path, work) = store("both");
    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"bob","display_name":"Bob","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("not both"), "{body}");

    // And neither is also a refusal, rather than an anonymous account.
    let (status, body) = store.invite("alice", &json(r#"{"grants":["agents/demo read"]}"#));
    assert_eq!(status, 400, "{body}");

    std::fs::remove_dir_all(&work).ok();
}

/// `user` is the pre-D46 spelling and still means "this exact string is
/// the principal". A bot or an operator credential wants it, and it must
/// keep working unchanged.
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
fn a_handle_and_its_display_name_survive_a_reopen() {
    let (store, path, work) = store("reopen");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let user = json(&body)["user"]
        .as_str()
        .expect("a principal")
        .to_string();
    let invite = json(&body)["invite"].as_str().expect("invite").to_string();

    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(on_disk.contains(NAME), "the display name was not persisted");
    assert!(on_disk.contains(&user), "the handle was not persisted");

    // Redeem, so the name has to travel from the invite to the account.
    let (id, secret) = invite.split_once(':').expect("invite is id:secret");
    let (status, body) = store.redeem(id, &json(&format!(r#"{{"secret":"{secret}"}}"#)));
    assert_eq!(status, 200, "{body}");

    let reopened = Accounts::open(path.clone(), None, BTreeSet::new()).expect("store reopens");
    assert!(reopened.has_account(&user), "the handle did not survive");
    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(
        on_disk.contains(NAME),
        "the display name did not survive redemption: {on_disk}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The claim the whole decision rests on: revoking an account destroys
/// the readable name while the handle -- the half the log already keeps
/// forever -- survives as a retired entry that names nobody.
///
/// Asserted against the file on disk as well as the live store, because
/// "not returned by a lookup" and "not written down" are different
/// promises and only the second survives a restart.
#[test]
fn revoking_forgets_the_name_and_keeps_the_handle() {
    let (store, path, work) = store("forget");
    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let user = json(&body)["user"]
        .as_str()
        .expect("a principal")
        .to_string();
    let invite = json(&body)["invite"].as_str().expect("invite").to_string();
    let (id, secret) = invite.split_once(':').expect("invite is id:secret");
    let (status, body) = store.redeem(id, &json(&format!(r#"{{"secret":"{secret}"}}"#)));
    assert_eq!(status, 200, "{body}");
    assert_eq!(store.display_name(&user).as_deref(), Some(NAME));

    let (status, body) = store.revoke(&json(&format!(r#"{{"user":"{user}"}}"#)));
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        store.display_name(&user),
        None,
        "the name outlived the account"
    );
    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(
        !on_disk.contains(NAME) && !on_disk.contains("Lovelace"),
        "a revoked account left its holder's name on disk: {on_disk}"
    );
    // The handle stays, because the log already carries it and a name
    // that could be reissued would hand a second person the first
    // person's signed attribution.
    assert!(
        on_disk.contains(&user),
        "the handle should be retired, not forgotten: {on_disk}"
    );
    assert!(!store.has_account(&user));

    std::fs::remove_dir_all(&work).ok();
}

/// The roster is the input `choir acl render` regenerates comments from,
/// so it must pair only the accounts that have something to say.
#[test]
fn the_roster_pairs_handles_with_names_and_omits_the_rest() {
    let (store, _path, work) = store("roster");

    // Both invites are redeemed, because the roster reads *accounts*.
    // An earlier draft redeemed only one and asserted
    // `roster.is_empty() || ...`, which is satisfied by the roster never
    // being populated at all -- a test that passes by nothing happening.
    let redeem = |body: &str| {
        let issued = json(body);
        let user = issued["user"].as_str().expect("a principal").to_string();
        let pair = issued["invite"].as_str().expect("invite").to_string();
        let (id, secret) = pair.split_once(':').expect("id:secret");
        let (status, out) = store.redeem(id, &json(&format!(r#"{{"secret":"{secret}"}}"#)));
        assert_eq!(status, 200, "{out}");
        user
    };

    let (status, body) = store.invite(
        "alice",
        &json(&format!(
            r#"{{"display_name":"{NAME}","grants":["agents/demo read"]}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    let handle = redeem(&body);

    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"buildbot","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 200, "{body}");
    let bot = redeem(&body);
    assert_eq!(bot, "buildbot");

    let roster = store.roster();
    assert_eq!(
        roster.get(&handle).map(String::as_str),
        Some(NAME),
        "the handle with a name is missing from the roster: {roster:?}"
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
/// the record — their name is precisely what was deleted. The refusal
/// belongs here rather than in the renderer, which by then is holding
/// two identical strings with no way to tell which is which.
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
