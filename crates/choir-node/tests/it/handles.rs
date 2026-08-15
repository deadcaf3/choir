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
    let user = json(&body)["user"].as_str().expect("a principal").to_string();

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
    let user = json(&body)["user"].as_str().expect("a principal").to_string();
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
