//! Op payloads built for a browser to sign (D39).
//!
//! The browser never learns the op format. A page that serialized a
//! [`choir_view::ViewOp`] in JavaScript would be a second implementation
//! of a *hashed* format — invariant 3 is enforced only by convention, so
//! a second encoder is exactly how it breaks silently, and it would break
//! in a language this workspace has no test harness for. So the node
//! builds the bytes and the page signs them.
//!
//! Two shapes need it, and they differ in one way that decides the API.
//! A verdict is fully determined by the review, the reviewer and which
//! button was pressed, so it can be rendered into the page. A comment
//! carries text the person has not typed yet, so it cannot — it needs a
//! round trip once the text exists, which is what
//! [`crate::handle_prepare`] serves.
//!
//! Preparing grants nothing. The bytes returned are bytes the caller
//! could have assembled themselves; what makes an op admissible is the
//! signature over them and the ACL check at `/api/submit`, both of which
//! are untouched by anything here.

use choir_view::{OpKind, Verdict, ViewOp};

/// One op as the node would accept it: the payload bytes in hex, and the
/// base64url challenge an authenticator must sign so that the signature
/// attests to *this* operation.
///
/// The challenge is [`choir_identity::webauthn_challenge`] — the hex form
/// of `signing_hash(channel, payload)`, the same bytes the ed25519 path
/// signs. One definition of "what was approved" across both schemes.
pub(crate) struct Prepared {
    /// `payload_hex` for `/api/submit`.
    pub payload_hex: String,
    /// The `challenge` for `navigator.credentials.get`.
    pub challenge: String,
}

fn prepared(channel: &str, op: &ViewOp) -> Prepared {
    let payload = op.to_payload();
    let signing = choir_oplog::signing_hash(channel, &payload);
    Prepared {
        payload_hex: crate::platform::hex_encode(&payload),
        challenge: base64url_nopad(&choir_identity::webauthn_challenge(&signing)),
    }
}

/// A verdict on review `id`, cast by `reviewer`.
///
/// `None` for a verdict name that is not one of the two the view knows —
/// the page only emits those two, and refusing an unknown one here is
/// cheaper than discovering it at admission.
pub(crate) fn verdict(id: &str, reviewer: &str, verdict: &str) -> Option<Prepared> {
    let verdict = match verdict {
        "Approve" => Verdict::Approve,
        "RequestChanges" => Verdict::RequestChanges,
        _ => return None,
    };
    Some(prepared(
        reviewer,
        &ViewOp::new(OpKind::PostVerdict {
            id: id.to_string(),
            reviewer: reviewer.to_string(),
            verdict,
            note: String::new(),
        }),
    ))
}

/// A comment on review `id`, by `author`, with a caller-chosen id.
///
/// The comment id is the author's retry identity: resubmitting the same
/// one is refused rather than duplicated. It is minted per prepare call
/// rather than taken from the request, so a browser that retries gets a
/// fresh identity and a browser that double-submits one prepared payload
/// gets the refusal that identity exists to give.
pub(crate) fn comment(id: &str, author: &str, comment_id: &str, body: &str) -> Prepared {
    prepared(
        author,
        &ViewOp::new(OpKind::PostComment {
            id: id.to_string(),
            comment: comment_id.to_string(),
            author: author.to_string(),
            body: body.to_string(),
        }),
    )
}

/// Base64url without padding, WebAuthn's challenge encoding.
pub(crate) fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// Both shapes commit to the channel they name. A challenge computed
    /// over a different channel than the payload claims would produce a
    /// signature the node accepts for an operation nobody agreed to,
    /// which is the forgery-class failure D39 carries a tripwire for.
    #[test]
    fn a_prepared_op_carries_the_challenge_its_own_payload_hashes_to() {
        for (channel, prepared) in [
            ("carol", super::verdict("r-1", "carol", "Approve").expect("a known verdict")),
            ("dave", super::comment("r-1", "dave", "c-1", "looks fine")),
        ] {
            let payload = crate::platform::hex_decode(&prepared.payload_hex).expect("hex");
            let signing = choir_oplog::signing_hash(channel, &payload);
            assert_eq!(
                prepared.challenge,
                super::base64url_nopad(&choir_identity::webauthn_challenge(&signing)),
                "{channel}: the challenge is not this payload's signing hash"
            );
        }
    }

    /// The author in the payload and the channel the challenge covers are
    /// one name. They are separate arguments to the view's own check —
    /// `PostComment`'s author must be the submitting channel — so a
    /// prepare that let them differ would build an op the node refuses.
    #[test]
    fn the_payload_names_the_same_principal_the_challenge_binds() {
        let prepared = super::comment("r-1", "dave", "c-1", "looks fine");
        let payload = crate::platform::hex_decode(&prepared.payload_hex).expect("hex");
        match choir_view::ViewOp::from_payload(&payload).expect("a ViewOp").kind {
            choir_view::OpKind::PostComment { id, comment, author, body } => {
                assert_eq!((id.as_str(), comment.as_str()), ("r-1", "c-1"));
                assert_eq!(author, "dave");
                assert_eq!(body, "looks fine");
            }
            other => panic!("wrong op kind: {other:?}"),
        }
        assert!(super::verdict("r-1", "carol", "Maybe").is_none());
    }
}
