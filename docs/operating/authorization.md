# Authorization

Five separate questions:

| Question | Answered by |
|:--|:--|
| Who are you? | `--auth-file`, or an account issued under `--accounts-file` |
| Which repositories may you reach, and how far? | `--acl-file` (D29) |
| May this particular landing happen? | `--protected-refs` + `--require-review`, narrowed by `own` (D42) |
| Why was it allowed? | the `Submit` op's authorization basis (D43) |
| Whose key signed it, and when? | key bindings and revocations (D44) |

> [!IMPORTANT]
> Without `--acl-file`, every credential reaches every repository.

## Per-repository authorization (D29)

Three whitespace-separated columns, an optional fourth, `#` comments:

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

Each level adds to the one above:

| Level | Adds |
|:--|:--|
| `read` | cloning and fetching |
| `propose` | opening a review |
| `write` | pushing any other ref, workspace provisioning, submitting ops that touch that repository |
| `own` | authorizing a landing on a protected ref (D42) |

**`propose` admits one thing (D60): a push to
`refs/for/<branch>/<user>/<topic>`, which opens a review (D53).** Every
other ref is refused. The pusher's own name is a required segment. The
grant is checked at the smart-HTTP boundary and again when the
`pre-receive` hook reports the refs.

The operator's credential usually wants:

```text
myself     *      write
myself     @node  write
```

`*` never covers `@node`. `@node auditor` reads `/api/log` and
`/api/ref-agreement`; `@node write` is needed for ops naming no repository,
such as key bindings. Vouching (D65) needs only `@node auditor`.

## A grant that ends (D66)

The fourth column is a deadline in unix seconds. The table is dated on every
request; no restart.

A deadline **lapses downward**. Pair a permanent `read` with a `write` that
ends:

```text
frank      owner/demo       read
frank      owner/demo       write      until=1788000000
```

A single expiring grant leaves the holder with a `404`.

- A deadline in the past parses and never matches; startup says
  `acl enabled (7 grants, 1 expired)`.
- `until=` is absolute: `date -v+90d +%s` on macOS, `date -d '+90 days' +%s`
  on GNU.
- The node's clock decides.

`own` may carry a deadline; when it lapses the landing gate returns to the
approval-weight rule (D42).

Fail closed: anything not granted is refused. An unreadable repository
answers `404`, never `403`. The flag requires `--auth-file`. A malformed file
refuses to start; a malformed *edit* keeps the previous table and complains.

`/api/view`, `/api/reviews` and the browser page are narrowed to readable
repositories. Node-wide sections (ref-state attestation, key bindings, vouch
graph, telemetry) need `@node auditor`; the log head and build stamp reach
everyone. A review you were assigned to reaches you on any repository.

## Repository ownership (D42)

With `--protected-refs` and `--require-review`, a protected ref needs
approval weight 2 from two distinct operators. `own` changes the question:

```text
myself     owner/demo       own
```

**On a protected ref of an owned repository, one owner's assent is necessary
and sufficient**: the owner lands it, or the owner approved a review naming
that exact `(ref, commit)`.

- **An owner's key is equivalent to the repositories they own.**
- **An owner submitting directly must have their key bound** in
  `--keys-file` (`<channel> <64-hex>`). Approving a review does not need
  this.
- **`own` is granted in the file you write**; self-service cannot issue it.

The file is re-read per landing. An unreadable or malformed file refuses the
landing.

## Landing a review with the reason it was allowed (D43)

`Submit` is a ref move with the gate's answer attached:

```json
{"Submit": {"review": "r1", "name": "demo.git:refs/heads/main",
            "commit": {...}, "prev": {...},
            "authorization": {"format_version": 1,
                              "basis": {"OwnerApproved": {"owner": "myself"}},
                              "approvers": [{...}]}}}
```

Basis is `OwnerLanded`, `OwnerApproved`, or `ApprovalWeight {required, met}`.

**The authorization is never a client's to assert.** Post a `Submit`; on
mismatch the rejection's `expected` field is the gate's record. Sign that
verbatim and post again:

```bash
curl -u "$USER" -X POST https://<HOST>/api/submit -d "$FIRST_ATTEMPT" \
  | jq -r .expected          # the authorization the gate produced
```

- **`this landing cannot name its approvers`**: an approving channel has no
  key binding (or two). Bind the reviewer's key and merge again.
- **`is not gated on this node`**: unprotected ref, or no `--require-review`.
  Use `SetRef`.

Archiving a review discards its verdicts; the entry bytes remain the answer
to who approved a landing.

## Rotating a key without breaking anything (D44)

An approval is credited against the key live when the verdict was cast.

```bash
choir revoke <api> <node-key-file> <old-pubkey-hex> "laptop lost"
choir bind   <api> <node-key-file> <operator> <new-pubkey-hex> [channel]
```

Both take the public key hex `choir key` prints and are node-signed.

- **An approval cast by an already-revoked key cannot be credited**; the
  reviewer needs a fresh key and verdict.
- **`choir log --verify` checks signatures as of their own position.** It
  fetches revocation positions from `/api/view`, so it needs API access.

> [!WARNING]
> **Keep revoked keys in the trusted-keys file.** The log stores a key id,
> never the public key, so deleting the line makes every entry that key
> signed permanently unverifiable.

That applies to ed25519 keys only. A passkey-signed entry carries its own
credential key (D45); `choir log --verify` counts those as *intact but
unanchored*.

Review retention is opt-in: `--review-retention N` archives completed
reviews when more than `N` remain live. `--review-lapse-after-secs` is
invalid without it.

## Issuing a credential without editing a file (D36)

`--accounts-file <path>` turns on invite-only self-service; issued
credentials are added to what the auth and ACL files say.

Passkeys are `--passkeys`. Without it, `POST /api/accounts/passkey` and
`/account` answer 503.

```bash
choir-node ./repos 8417 --auth-file ~/.choir/auth --acl-file ~/.choir/acl \
  --keys-file ~/.choir/keys --accounts-file ./repos/.choir/accounts.json \
  --ssh-handoff ./repos/.choir/ssh-handoff \
  --ssh-authorized-keys ./repos/.choir/authorized_keys
```

### The console (D72)

`/people`, for `@node write`: the queue of people asking for access, a
button to let one in, and a form that mints an invite link.

A stranger can ask for access on the front page and keeps the link it
gives them; granting turns that link into their invite. Each request costs
a proof of work; the queue caps at 64.

A `POST` whose `Origin` names another site is refused.

### The same three by hand

Mint an invite, as `@node write`:

```bash
curl -u "$OPERATOR" -X POST https://<HOST>/api/accounts/invite \
  -d '{"user":"bob","grants":["owner/demo read","owner/notes write"]}'
```

The invite names nobody (D75); the username is theirs to pick. Send
`{"user":"buildbot"}` when the name must be exact.

The response carries `invite`, an `id:secret` pair, once. It is presented as
basic auth to one endpoint:

```bash
curl -u "<INVITE>" -X POST https://<HOST>/api/accounts/redeem \
  -d "{\"ssh_key\":\"$(cat ~/.ssh/id_ed25519.pub)\"}"
```

That answers with the clone token and registers the key for SSH. Invites
are single use and expire in a day (`expires_in_secs`).

**In a browser the link enrols a passkey** and opens a session. A token for
git is minted from `/account` (`POST /account/token`), one per account.

`GET /api/accounts` lists accounts, live invites and pending requests.
`POST /api/accounts/request/grant` with `{"request_id":"ask-...","grants":[...]}`
answers one; `POST /api/accounts/request/decline` drops one;
`POST /api/accounts/revoke` with `{"user":"bob"}` deletes an account, its
grants and its `authorized_keys` line. Revocation is deletion.

> [!CAUTION]
> **`@node` can never be issued by self-service.** The flag requires both
> `--auth-file` and `--acl-file`.

- **An issued grant may carry a deadline**: `{"grants":["owner/demo write until=1788000000"]}`.
- **The generated `authorized_keys` is generated.** Point `sshd` at it
  (`AuthorizedKeysFile /path/to/repos/.choir/authorized_keys`, see
  [Git over SSH](transports.md#git-over-ssh-d31)) and never edit it.

## Publishing a repository to everybody (D78)

One line in the ACL opens one repository to readers with no account:

```text
@anon    owner/project.git    read
```

`@anon` is the reader who presented no credential. An unauthenticated browse
or fetch is evaluated under that principal; every check after the gate is
unchanged. No flag.

**It cannot be authenticated as.** Account names are ASCII letters, digits,
`-`, `_` and `.`, so `@` is unspellable. A user called `anon` is an ordinary
account.

**Three grants it will not take**, refused at parse time:

| Written | Refused because |
|:--|:--|
| `@anon @node auditor` | the op log and the audit surface |
| `@anon * read` | name each public repository explicitly |
| `@anon o/r write` | a write path with no credential |

**What opens:** the browse surface for that repository and the read half of
git smart-HTTP, so `git clone` works with no credentials. **What does not:**
`/api/view`, `/api/log`, `/reviews`, and `git-receive-pack`. An unpublished
repository answers exactly as one that does not exist.

Pair with `--site-repo owner/project` to make that repository the front page.
