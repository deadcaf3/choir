# Authorization

Five separate questions, kept apart:

| Question | Answered by |
|:--|:--|
| Who are you? | `--auth-file`, or an account issued under `--accounts-file` |
| Which repositories may you reach, and how far? | `--acl-file` (D29) |
| May this particular landing happen? | `--protected-refs` + `--require-review`, narrowed by `own` (D42) |
| Why was it allowed? | the `Submit` op's authorization basis (D43) |
| Whose key signed it, and when? | key bindings and revocations (D44) |

> [!IMPORTANT]
> Without `--acl-file`, every credential reaches every repository. The auth
> file establishes identity; the ACL establishes reach.

## Per-repository authorization (D29)

`--acl-file` gates each repository per user. Three whitespace-separated columns, an optional fourth, `#` comments, and the same append-a-line discipline as the keys file:

```text
# <user>   <repo|*|@node>   <level>    [until=<unix seconds>]
alice      owner/demo       write
bob        owner/demo       read
bob        owner/notes      write
carol      *                read
dave       @node            auditor
erin       owner/demo       propose
frank      owner/demo       write      until=1788000000
```

Each level adds to the one above it:

| Level | Adds |
|:--|:--|
| `read` | cloning and fetching |
| `propose` | opening a review |
| `write` | pushing any other ref, workspace provisioning, submitting ops that touch that repository |
| `own` | authorizing a landing on a protected ref (D42, below) |

`own` is the only repository-scoped administrative action.

**`propose` is how a repository takes a contribution from somebody it does not trust with its branches (D60).** It admits one thing, a push to `refs/for/<branch>/<user>/<topic>`, which opens a review (D53); every other ref is refused with a message naming that spelling. The pusher's own name is a required segment, and it keeps concurrent `propose` holders from overwriting each other's proposals. A `write` holder is exempt, having every ref already. The grant is checked twice: the smart-HTTP boundary sees no refname, so the push is admitted there and the refs are judged when the `pre-receive` hook reports them, with nothing applied in between. Both transports get it.

The operator's own credential usually wants two lines:

```text
myself     *      write
myself     @node  write
```

`*` covers every repository and never covers `@node`. `@node` is the node itself: `auditor` reads `/api/log` and `/api/ref-agreement`, which are gated rather than filtered. `@node write` is needed for ops that name no repository, such as key bindings.

Vouching (D65) is the one node-wide op that needs only `@node auditor`: it names no repository and reports what its signer thinks. To put somebody in the web of trust, grant `@node auditor`, which is read-only.

## A grant that ends (D66)

The fourth column is a deadline in unix seconds: it lends a privilege instead of handing over an account. `frank` above may push until that second. The table is dated on every request, so nothing sweeps and no restart is needed.

A deadline **lapses downward**. Pair a permanent `read` with a `write` that ends, and when the write ends they are a reader:

```text
frank      owner/demo       read
frank      owner/demo       write      until=1788000000
```

That is the shape worth using. A single expiring grant leaves the holder with a repository that answers `404`, which reads to them like it was deleted.

Three things not to be surprised by:

- **A deadline already in the past parses.** The line never matches, and the startup and reload lines say `acl enabled (7 grants, 1 expired)`, so it is visible rather than silent.
- **The deadline is absolute, not a duration.** `until=` is a moment: `date -v+90d +%s` on macOS, `date -d '+90 days' +%s` on GNU.
- **It is the node's clock that decides**, exactly as it already does for invite expiry.

`own` may carry a deadline too, and when it lapses the repository has no owner: the landing gate returns to the approval-weight rule (D42).

This is the mechanism D24's T1 tripwire response names, "time-locks + bonds only". A grant lives in this file and in the self-service store rather than in the op log, which is D29's design and the single element replay cannot rederive.

Fail closed: with the flag set, anything not granted is refused. A repository you cannot read answers `404` rather than `403`, so a denial never confirms that it exists. The flag requires `--auth-file`. A malformed file refuses to start; a malformed *edit* keeps the previous table and complains, so a typo cannot silently revoke access.

`/api/view`, `/api/reviews` and the browser page are narrowed to the repositories a credential may read, so a grant on one repository does not disclose that the others exist. Node-wide sections of the view (the ref-state attestation, key bindings, the vouch graph, and the concentration, growth, newcomer and lag telemetry) need `@node auditor`; the log head and build stamp reach everyone, since a writer needs them to submit. A review you were assigned to reaches you on any repository.

## Repository ownership (D42)

With `--protected-refs` and `--require-review`, a protected ref normally needs approval weight 2 from two distinct operators, and nobody is exempt. `own` changes which question the gate asks for that repository:

```text
myself     owner/demo       own
```

**On a protected ref of an owned repository, one owner's assent is necessary and sufficient.** Assent takes either form:

- the owner performs the landing themselves, or
- the owner approved a review naming that exact `(ref, commit)`.

So an owner can land alone, with no review in existence. A non-owner reaches an owned ref only with an owner's approval.

Three things worth knowing before granting it:

- **An owner's key is equivalent to the repositories they own.** Under the weight rule a stolen key buys one unit and still needs a second, conflict-graph-separated operator. Here it buys the repository. Make the trade deliberately.
- **An owner submitting directly must have their key bound** in `--keys-file` (`<channel> <64-hex>`): an unbound key is unconstrained in what channel it claims, so the gate refuses to read ownership off one. Approving a review does not need this; landing under your own key does.
- **`own` is granted in the file you write.** Self-service (D36) contributes to the merged table the HTTP layer enforces, but the landing gate reads the operator's file directly, so ownership cannot be self-issued.

A repository nobody owns keeps the weight rule. The file is re-read per landing, so a grant takes effect with no restart; an unreadable or malformed file refuses the landing rather than concluding there are no owners.

## Landing a review with the reason it was allowed (D43)

A `SetRef` on a protected ref records that a merge happened. The gate runs at admission against the ACL and the protected-ref list, and the log carries neither. `Submit` is the same ref move with the gate's own answer attached.

```json
{"Submit": {"review": "r1", "name": "demo.git:refs/heads/main",
            "commit": {...}, "prev": {...},
            "authorization": {"format_version": 1,
                              "basis": {"OwnerApproved": {"owner": "myself"}},
                              "approvers": [{...}]}}}
```

The basis is one of three, matching the three ways a landing is allowed: `OwnerLanded`, `OwnerApproved`, or `ApprovalWeight {required, met}`. Under D42 a landing can be authorized with **zero** approvals.

**The authorization is never a client's to assert.** Build a `Submit` and post it; if it does not match, the rejection's `expected` field is the gate's own record as JSON. Sign that verbatim and post again:

```bash
curl -u "$USER" -X POST https://<HOST>/api/submit -d "$FIRST_ATTEMPT" \
  | jq -r .expected          # the authorization the gate produced
```

Two refusals to expect:

- **`this landing cannot name its approvers`.** Approvers are recorded as actor ids, read from the log's own `BindKey` records, so an approving channel with no binding (or two) refuses the landing rather than recording it with a gap. Bind the reviewer's key and merge again.
- **`is not gated on this node`.** A `Submit` on an unprotected ref, or on a node not running `--require-review`, is refused. Move the ref with `SetRef`.

Archiving a review discards its verdicts, leaving the entry bytes as the only surviving answer to who approved a landing. Replay still verifies each, at its own position in the log.

## Rotating a key without breaking anything (D44)

A reviewer's approval is recorded against the key that was **live when the verdict was cast**. The ordinary lifecycle keeps telling the truth:

```bash
choir revoke <api> <node-key-file> <old-pubkey-hex> "laptop lost"
choir bind   <api> <node-key-file> <operator> <new-pubkey-hex> [channel]
```

Both take the **public key hex** that `choir key` prints and the trusted-keys file already carries; the actor id is derived for you. Both are node-signed, so they need the node's key file.

Approvals the old key already cast still land, and still name the old key. Crediting reads the bindings live at the verdict, so adding a key leaves open approvals unambiguous.

Two consequences worth knowing:

- **An approval cast by an already-revoked key cannot be credited**, and the landing is refused. That reviewer needs a fresh key and a fresh verdict.
- **`choir log --verify` checks signatures as of their own position.** An entry signed before its key's revocation verifies forever; one signed at or after it is a failure, not merely unverified. The client fetches revocation positions from `/api/view`, so verification needs API access as well as a keys file.

> [!WARNING]
> **Keep revoked keys in the trusted-keys file.** The log stores a key id, never the public key, so deleting the line makes every entry that key ever signed permanently unverifiable. Deletion causes that decay, and nothing in the code can stop it.

That warning is about ed25519 keys only. A passkey-signed entry carries its own credential key (D45), so it survives a restore that keeps the log and loses everything else, which is what `scripts/flip/pull_backup.sh` does. `choir log --verify` counts those entries on their own line, as *intact but unanchored*: the bytes are proven, and the binding of credential to account lives in the accounts store rather than in the log.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

## Issuing a credential without editing a file (D36)

`--accounts-file <path>` turns on invite-only self-service. The auth file and the ACL file stay yours, and issued credentials are added to what they say rather than written into them.

Passkeys are a second switch, `--passkeys`. With `--accounts-file` alone, `POST /api/accounts/passkey` and the `/account` enrolment page both answer 503 and say which switch is missing.

```bash
choir-node ./repos 8417 --auth-file ~/.choir/auth --acl-file ~/.choir/acl \
  --keys-file ~/.choir/keys --accounts-file ./repos/.choir/accounts.json \
  --ssh-handoff ./repos/.choir/ssh-handoff \
  --ssh-authorized-keys ./repos/.choir/authorized_keys
```

### The console (D72)

`/people` is those three operations as a page, for a credential holding `@node write`: the queue of people asking for access, a button to let one in, and a form that mints an invite link. It renders plain forms.

A stranger who reaches the node's front page can ask for access there. They keep the link the page gives them, and granting the request turns that same link into their invite. Each request costs a proof of work in the asker's browser, and the queue is capped at 64 unanswered requests; the endpoint that fills it needs no credential.

A `POST` whose `Origin` names another site is refused, on the console and on the JSON endpoints below.

### The same three by hand

Mint an invite, as a credential holding `@node write`:

```bash
curl -u "$OPERATOR" -X POST https://<HOST>/api/accounts/invite \
  -d '{"user":"bob","grants":["owner/demo read","owner/notes write"]}'
```

That invite names nobody (D75). `display_name` records what to call them; the **username is theirs to pick** when they redeem. Send `{"user":"buildbot"}` when the name has to be exact, which is what a bot or a script wants.

The response carries `invite`, an `id:secret` pair, once. Hand it over out of band. The invite *is* the credential: it is presented as basic auth and reaches this one endpoint.

```bash
curl -u "<INVITE>" -X POST https://<HOST>/api/accounts/redeem \
  -d "{\"ssh_key\":\"$(cat ~/.ssh/id_ed25519.pub)\"}"
```

That answers, once, with the token to clone with and registers the key for SSH. Invites expire (a day by default, `expires_in_secs` to choose) and are single use.

**The browser route enrols a passkey in place of a password.** Opening the link in a browser gives a page that asks for a username and runs the passkey ceremony; the account is created with the passkey enrolled, and the redemption opens a browser session. Git and the CLI speak basic auth, so a token is minted on request from the account page (`POST /account/token`), one per account, replacing any it had. A browser without WebAuthn gets the password route.

`GET /api/accounts` lists who holds what: accounts, live invites and the pending request queue. `POST /api/accounts/request/grant` with `{"request_id":"ask-...","grants":[...]}` answers one of those requests, `POST /api/accounts/request/decline` drops it, and `POST /api/accounts/revoke` with `{"user":"bob"}` deletes an account: the token stops authenticating on the next request, the grants leave the table, and the key leaves the generated `authorized_keys`. Revocation is deletion rather than a record: the log is append-only, so none of this lives in it.

> [!CAUTION]
> **`@node` can never be issued by self-service.** Node-wide authority, the
> auditor role and the rate-limit exemption that comes with it, stays in the
> ACL file you write by hand. The flag requires both `--auth-file` and
> `--acl-file`.

Two rules worth knowing before you rely on it:

- **An issued grant may carry a deadline too**, in the same spelling: `{"grants":["owner/demo write until=1788000000"]}`. The join page says so in words the holder can act on ("push to owner/demo, until in about 90 days"). The invite is single-use and short-lived; the grants it hands over last as long as their own fourth column says, or forever without one.
- **The generated `authorized_keys` is generated.** Point `sshd` at it once (`AuthorizedKeysFile /path/to/repos/.choir/authorized_keys` in `sshd_config`, alongside the account setup in [Git over SSH](transports.md#git-over-ssh-d31)) and never edit it: it is rewritten on every account change, and a hand-added line disappears with the next one.
