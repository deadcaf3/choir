# Authorization

Four separate questions, deliberately kept apart, and a fifth that is not
authorization at all:

| Question | Answered by |
|:--|:--|
| Who are you? | `--auth-file`, or an account issued under `--accounts-file` |
| Which repositories may you reach, and how far? | `--acl-file` (D29) |
| May this particular landing happen? | `--protected-refs` + `--require-review`, narrowed by `own` (D42) |
| Why was it allowed? | the `Submit` op's authorization basis (D43) |
| Whose key signed it, and when? | key bindings and revocations (D44) |

Authentication says who you are and nothing else. Without an ACL every
credential reaches every repository, which is the first section below.

## Per-repository authorization (D29)

`--acl-file` gates each repository per user. Three whitespace-separated columns, `#` comments, and the same append-a-line discipline as the keys file:

```text
# <user>   <repo|*|@node>   <level>
alice      owner/demo       write
bob        owner/demo       read
bob        owner/notes      write
carol      *                read
dave       @node            auditor
erin       owner/demo       propose
```

`read` clones and fetches; `propose` adds opening a review, and nothing else; `write` adds pushing any other ref, workspace provisioning, and submitting ops that touch that repository; `own` adds authorizing a landing on a protected ref (D42, below). There is no `admin`: the only repository-scoped administrative action that exists is the landing gate, and `own` is it.

**`propose` is how a repository takes a contribution from somebody it does not trust with its branches (D60).** It admits exactly one thing, a push to `refs/for/<branch>/<user>/<topic>`, which opens a review (D53); every other ref is refused with a message naming that spelling. The pusher's own name is a required segment, and it is what keeps two `propose` holders apart: several people hold that grant at once, so without it whoever pushed second would take over or delete the first one's proposal. A `write` holder is not held to the rule, having every ref already. Until it existed this was not expressible, because opening a review is a push and `write` reaches every unprotected ref, so inviting an outsider to propose meant handing them the repository. The grant is checked twice, and it has to be: the smart-HTTP boundary sees no refname, since git sends the ref list only after the server agrees to receive the pack, so the push is admitted there and the refs are judged when the `pre-receive` hook reports them. Nothing is applied in between. Both transports get it, because the SSH shim borrows the HTTP mapping rather than restating it.

The operator's own credential usually wants two lines, since neither covers the other:

```text
myself     *      write
myself     @node  write
```

`*` covers every repository and never covers `@node`. `@node` is the node itself: `auditor` reads `/api/log` and `/api/ref-agreement`, which are gated rather than filtered because the log is a hash chain and the attestation covers the complete ref state. `@node write` is needed for ops that name no repository, such as key bindings.

Fail closed: with the flag set, anything not granted is refused. A repository you cannot read answers `404` rather than `403`, so a denial never confirms that it exists. The flag requires `--auth-file`, since an ACL over anonymous requests would grade everyone the same. A malformed file refuses to start; a malformed *edit* keeps the previous table and complains, so a typo cannot silently revoke access.

`/api/view`, `/api/reviews` and the browser page are narrowed to the repositories a credential may read, so a grant on one repository does not disclose that the others exist. Node-wide sections of the view (the ref-state attestation, key bindings, and the concentration, growth, newcomer and lag telemetry) need `@node auditor`; the log head and build stamp reach everyone, since a writer needs them to submit. A review you were assigned to still reaches you, on any repository; that is what an invitation is.

## Repository ownership (D42)

With `--protected-refs` and `--require-review`, a protected ref normally needs approval weight 2 from two distinct operators, and nobody is exempt. Granting somebody `own` over a repository changes which question the gate asks for that repository:

```text
myself     owner/demo       own
```

**On a protected ref of an owned repository, one owner's assent is necessary and sufficient.** Assent takes either form, and the gate does not care which:

- the owner performs the landing themselves, or
- the owner approved a review naming that exact `(ref, commit)`.

So an owner can land alone, with no review in existence. A non-owner reaches an owned ref only with an owner's approval, and no number of non-owner approvals substitutes for it: the two-operator rule is not a second route to the same place.

Three things worth knowing before granting it:

- **An owner's key is equivalent to the repositories they own.** Under the weight rule a stolen key buys one unit and still needs a second, conflict-graph-separated operator. Here it buys the repository. That is the trade the level exists to make; make it deliberately.
- **An owner submitting directly must have their key bound** in `--keys-file` (`<channel> <64-hex>`). An unbound key is unconstrained in what channel it claims, so the gate refuses to read ownership off an unbound one; otherwise any trusted key could name itself an owner. Approving a review does not need this; landing under your own key does.
- **`own` is granted in the file you write.** Self-service (D36) contributes to the merged table the HTTP layer enforces, but the landing gate reads the operator's file directly, so ownership cannot be self-issued.

A repository nobody owns keeps the weight rule exactly as it was, and a node with no `--acl-file` behaves as it did before D42. The file is re-read per landing, so a grant takes effect with no restart; an unreadable or malformed file refuses the landing rather than concluding there are no owners.

## Landing a review with the reason it was allowed (D43)

A `SetRef` on a protected ref records that a merge happened. It cannot record *why* it was permitted: the gate runs at admission against the ACL and the protected-ref list, neither of which is in the log. `Submit` is the same ref move with the gate's own answer attached.

```json
{"Submit": {"review": "r1", "name": "demo.git:refs/heads/main",
            "commit": {...}, "prev": {...},
            "authorization": {"format_version": 1,
                              "basis": {"OwnerApproved": {"owner": "myself"}},
                              "approvers": [{...}]}}}
```

The basis is one of three, matching the three ways a landing is currently allowed: `OwnerLanded`, `OwnerApproved`, or `ApprovalWeight {required, met}`. Under D42 a landing can be authorized with **zero** approvals, so an approver list alone would not distinguish them.

**The authorization is never a client's to assert.** Build a `Submit` and post it; if it does not match, the rejection's `expected` field is the gate's own record as JSON. Sign that verbatim and post again:

```bash
curl -u "$USER" -X POST https://<HOST>/api/submit -d "$FIRST_ATTEMPT" \
  | jq -r .expected          # the authorization the gate produced
```

Two round trips, deliberately. The alternative is a second copy of the landing rule on the read path so a client could ask in advance, and a record that can disagree with the decision it describes is worth nothing.

Two refusals to expect:

- **`this landing cannot name its approvers`.** Approvers are recorded as actor ids, read from the log's own `BindKey` records, so an approving channel with no binding (or two) refuses the landing rather than recording it with a gap. Bind the reviewer's key and merge again.
- **`is not gated on this node`.** A `Submit` on an unprotected ref, or on a node not running `--require-review`, is refused: a landing record for a decision nothing made reads exactly like one that was checked. Move the ref with `SetRef`.

Archiving a review discards its verdicts, so after that the entry bytes are the only surviving answer to who approved a landing. Replay still verifies every one of them, because each is checked at its own position in the log.

## Rotating a key without breaking anything (D44)

A reviewer's approval is recorded against the key that was **live when the verdict was cast**, not whichever key holds that channel now. So the ordinary lifecycle works and keeps telling the truth:

```bash
choir revoke <api> <node-key-file> <old-pubkey-hex> "laptop lost"
choir bind   <api> <node-key-file> <operator> <new-pubkey-hex> [channel]
```

Both take the **public key hex** that `choir key` prints and the trusted-keys file already carries, not an actor id; the id is derived for you. Both are node-signed, so they need the node's key file.

Approvals the old key already cast still land, and still name the old key. The new key is credited with nothing it did not do. A key bound *after* a verdict is not a candidate for crediting it either, so adding a key never retroactively makes open approvals ambiguous.

Two consequences worth knowing:

- **An approval cast by an already-revoked key cannot be credited at all**, and the landing is refused rather than attributed to nobody. That reviewer needs a fresh key and a fresh verdict.
- **`choir log --verify` checks signatures as of their own position.** An entry signed before its key's revocation verifies forever; one signed at or after it is a failure, not merely unverified. The client fetches revocation positions from `/api/view`, so verification needs API access as well as a keys file.

> [!WARNING]
> **Keep revoked keys in the trusted-keys file.** The log stores a key id, never the public key, so deleting the line makes every entry that key ever signed permanently unverifiable. Revocation does not cause that decay; deletion does, and nothing in the code can stop it.

That warning is about ed25519 keys only. A passkey-signed entry carries its own credential key (D45), so it needs no keys file and survives a restore that keeps the log and loses everything else, which is what `scripts/pull_backup.sh` does, deliberately. `choir log --verify` counts those entries on their own line, as *intact but unanchored*: the bytes are proven, and what is not proven is that the credential belonged to that account, because that binding lives in the accounts store rather than in the log. Read it as a real check that stops one step short, not as a weaker version of the ed25519 one.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

## Issuing a credential without editing a file (D36)

`--accounts-file <path>` turns on invite-only self-service. Nothing above changes: the auth file and the ACL file stay yours, and issued credentials are added to what they say rather than written into them.

```bash
choir-node ./repos 8417 --auth-file ~/.choir/auth --acl-file ~/.choir/acl \
  --keys-file ~/.choir/keys --accounts-file ./repos/.choir/accounts.json \
  --ssh-handoff ./repos/.choir/ssh-handoff \
  --ssh-authorized-keys ./repos/.choir/authorized_keys
```

Mint an invite, as a credential holding `@node write`:

```bash
curl -u "$OPERATOR" -X POST https://<HOST>/api/accounts/invite \
  -d '{"user":"bob","grants":["owner/demo read","owner/notes write"]}'
```

The response carries `invite`, an `id:secret` pair, once. Hand it over out of band. The holder redeems it; the invite *is* the credential, so it is presented as basic auth and reaches nothing else:

```bash
curl -u "<INVITE>" -X POST https://<HOST>/api/accounts/redeem \
  -d "{\"ssh_key\":\"$(cat ~/.ssh/id_ed25519.pub)\"}"
```

That answers, once, with the token to clone with (`https://bob:<TOKEN>@<HOST>/owner/demo.git`) and registers the key for SSH. Invites expire (a day by default, `expires_in_secs` to choose) and are single use.

`GET /api/accounts` lists who holds what, and `POST /api/accounts/revoke` with `{"user":"bob"}` deletes an account: the token stops authenticating on the next request, the grants leave the table, and the key leaves the generated `authorized_keys`. Revocation is deletion rather than a record, which is one reason none of this is in the op log: the log is append-only and cannot forget a credential.

Two rules worth knowing before you rely on it:

- **`@node` can never be issued.** Node-wide authority (the auditor role, and the rate-limit exemption that comes with it) stays in the ACL file you write by hand, so self-service cannot escalate itself. The flag needs both `--auth-file` and `--acl-file` for the same reason: a token issued with nothing to grade it against is a token to every repository.
- **The generated `authorized_keys` is generated.** Point `sshd` at it once (`AuthorizedKeysFile /path/to/repos/.choir/authorized_keys` in `sshd_config`, alongside the account setup in [Git over SSH](transports.md#git-over-ssh-d31)) and never edit it: it is rewritten on every account change, and a hand-added line disappears with the next one.
