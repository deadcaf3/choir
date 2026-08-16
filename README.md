# choir

Agent-first code collaboration: many agents on one repo, one total order from a single-writer sequencer, merge conflicts as first-class values.

**Status:** research prototype with a private-beta release path. Phase-0 gate passed; Phase 1 in progress. Not production software. Keep beta ingress closed until the [private-beta runbook](docs/private-beta-runbook.md) go-live receipts are complete.

| You want to… | Start here |
|---|---|
| See it work once | [Try it](#try-it) |
| Install tools / unblock build | [Prerequisites](#prerequisites) |
| Run a local node | [Run a node](#run-a-node) |
| Prepare a private beta | [`docs/private-beta-runbook.md`](docs/private-beta-runbook.md) |
| Push, review, provision workspaces | [Use the node](#use-the-node) |
| Wire coding agents | [Agent templates](#agent-templates) |
| Why a decision was made the way it was | [`DECISIONS.md`](DECISIONS.md) |

## Prerequisites

| Tool | Required? | Notes |
|---|---|---|
| Rust stable + Cargo | **yes** | Built with **1.97.1**, edition 2021. Install via [rustup](https://rustup.rs/) or Homebrew. |
| `git` | **yes** | Smart-HTTP CGI + all integration tests. |
| `curl` | **yes** | Only HTTP client the crates use. |
| `openssl` | **yes** | Auth tokens, bridge RS256; tests shell out to it. |
| `ssh-keygen` | **yes** | Integration tests / signed-push setup. |
| `mergiraf` | optional | Structured merge slot. Without it, line merge + first-class conflicts still work. Homebrew: `brew install mergiraf`. |
| `jj` | optional | Not required to build or run choir. |

**OS / filesystem**

- **macOS (APFS):** supported. Fast CoW workspaces via `clonefile` / `cp -Rc`. Dogfood installer uses **launchd** (`scripts/choirctl`).
- **Linux:** supported. Prefer a **btrfs** volume for workspace snapshots (Phase-0 gate used btrfs). Without CoW, provisioning still works but is slower.
- **Non-loopback bind** requires TLS (`--tls-cert` + `--tls-key`). Plain HTTP is loopback-only by design.

**Do not remove** `.cargo/config.toml` (`LIBSQLITE3_FLAGS`). Every workspace build needs it (rivetkit / sqlite workaround).

**Secrets:** keys, tokens, PEMs under `~/.choir/` at mode `0600` (daemon key: `<repo-root>/.choir/node.key`). Never commit them.

## Build

```bash
git clone <this-repo> && cd choir
cargo build --release -p choir-node -p choir-cli
```

Default `cargo build` / `cargo test` skip `choir-actor` (heavy Rivet dep). Full release gate:

```bash
sh scripts/gate full
```

Put the CLI on your PATH (or use `cargo run -p choir-cli -- …`):

```bash
export PATH="$PWD/target/release:$PATH"
```

## Try it

```bash
cargo run -p choir-demo         # narrated walkthrough: keys, ops, conflict, real git push
cargo run -p choir-spike --release   # Phase-0 gate binary; nonzero exit = gate fail
cargo test --workspace          # hermetic: no network, no external services
```

`choir-demo` is the fastest “what is this?” path.

## Run a node

The daemon serves **git smart-HTTP** and the **platform API** on one port (default **8417**). Repos must be created with `--create` (or the installer) so the `pre-receive` hook is installed — a bare repo made any other way is **not** sequenced. With no arguments, the binary uses `./repos` and port 8417; configured invocations must supply both `<repo-root>` and `<port>` before any flags.

### Option A — macOS dogfood (supervised)

```bash
sh scripts/choirctl install              # build, mint ~/.choir secrets, load launchd
sh scripts/choirctl status
sh scripts/choirctl url                  # clone/push URL with credentials
sh scripts/choirctl logs
# sh scripts/choirctl stop | uninstall   # stop keeps data under ~/.choir
```

Override port with `CHOIR_PORT`. Full flip procedure: `scripts/flip/RUNBOOK.md`.

### Option A2: private beta behind a TLS proxy

For the private beta, keep `choir-node` bound to `127.0.0.1` and terminate TLS at a hardened reverse proxy. Do not use the legacy direct-TLS installer. The beta service renderer requires an ACL, protected-ref review policy, scoped operations, operator-issued auth, and the read-only browser mode. The proxy renderer preserves authentication, streams Git separately, and applies route-specific limits.

See [`docs/private-beta-runbook.md`](docs/private-beta-runbook.md) for the network hold, service and proxy renderers, backups, CI packaging, staging promotion, monitoring, rollback, and go-live receipts. Every Choir route remains authenticated. A separate anonymous marketing page must use another host and origin.

### Option B — any Unix (foreground)

```bash
mkdir -p /tmp/choir-repos ~/.choir
# user:token per line, mode 0600
printf 'choir:%s\n' "$(openssl rand -hex 32)" > ~/.choir/auth && chmod 600 ~/.choir/auth
: > ~/.choir/keys && chmod 600 ~/.choir/keys
cargo run -p choir-cli -- key ~/.choir/agent.key myop/agent >> ~/.choir/keys
printf '# reviewer channels, one per line\n' > ~/.choir/reviewers && chmod 600 ~/.choir/reviewers

cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --create owner/demo.git \
  --auth-file ~/.choir/auth \
  --keys-file ~/.choir/keys \
  --reviewers-file ~/.choir/reviewers
```

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--acl-file <file>` (required before a second credential), `--request-log <file>` and `--rate-limit-api` / `--rate-limit-git` (also required before a second credential), `--quota-push-bytes` / `--quota-workspaces`, `--api-body-limit`, `--batch-limit`, `--ready-min-free-bytes`, `--read-only-browser`, `--journal <file>`, `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--reviewer-conflict-graph <file>` with `--reviewer-conflict-distance <hops>`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Authenticated operations endpoints are `/healthz`, `/readyz`, and `/metrics`. Flag reference: module docs at the top of `crates/choir-node/src/main.rs`, or `agents.md`.

**File formats (all mode 0600)**

| File | Format |
|---|---|
| `--auth-file` | `user:token` per line — authentication only; pair with `--acl-file` |
| `--acl-file` | `<user> <repo\|*\|@node> <level>` per line; levels `read` < `write` < `own`, and `auditor` on `@node` |
| `--keys-file` | `<64-hex>` or `<channel> <64-hex>` (bound key) |
| `--reviewers-file` | channel name per line; re-read on each draw |
| `--protected-refs` | `owner/repo.git:refs/heads/main` (trailing `*` ok) |
| `--reviewer-conflict-graph` | undirected `operator operator` edges; pair with an explicit maximum hop distance |

Hot-reload: trusted keys, channel bindings, push-certificate signers, reviewers, and ACL grants take effect on the next request.

> **Without `--acl-file`, every credential reaches every repository.** The auth file authenticates and nothing else; the node prints a line saying so at startup. A second `user:token` line can then clone every repo, push to any unprotected ref, and provision workspaces anywhere. Protected refs and the review requirement still hold, so it cannot land on a gated `main` unreviewed. **Do not issue a second credential without an ACL.**

### Per-repository authorization (D29)

`--acl-file` gates each repository per user. Three whitespace-separated columns, `#` comments, and the same append-a-line discipline as the keys file:

```
# <user>   <repo|*|@node>   <level>
alice      owner/demo       write
bob        owner/demo       read
bob        owner/notes      write
carol      *                read
dave       @node            auditor
```

`read` clones and fetches; `write` adds push, workspace provisioning, and submitting ops that touch that repository; `own` adds authorizing a landing on a protected ref (D42, below). There is no `admin`: the only repository-scoped administrative action that exists is the landing gate, and `own` is it.

The operator's own credential usually wants two lines, since neither covers the other:

```
myself     *      write
myself     @node  write
```

`*` covers every repository and never covers `@node`. `@node` is the node itself: `auditor` reads `/api/log` and `/api/ref-agreement`, which are gated rather than filtered because the log is a hash chain and the attestation covers the complete ref state. `@node write` is needed for ops that name no repository, such as key bindings.

Fail closed: with the flag set, anything not granted is refused. A repository you cannot read answers `404` rather than `403`, so a denial never confirms that it exists. The flag requires `--auth-file` — an ACL over anonymous requests would grade everyone the same. A malformed file refuses to start; a malformed *edit* keeps the previous table and complains, so a typo cannot silently revoke access.

`/api/view`, `/api/reviews` and the browser page are narrowed to the repositories a credential may read, so a grant on one repository does not disclose that the others exist. Node-wide sections of the view (the ref-state attestation, key bindings, and the concentration, growth, newcomer and lag telemetry) need `@node auditor`; the log head and build stamp reach everyone, since a writer needs them to submit. A review you were assigned to still reaches you, on any repository — that is what an invitation is.

### Repository ownership (D42)

With `--protected-refs` and `--require-review`, a protected ref normally needs approval weight 2 from two distinct operators, and nobody is exempt. Granting somebody `own` over a repository changes which question the gate asks for that repository:

```
myself     owner/demo       own
```

**On a protected ref of an owned repository, one owner's assent is necessary and sufficient.** Assent takes either form, and the gate does not care which:

- the owner performs the landing themselves, or
- the owner approved a review naming that exact `(ref, commit)`.

So an owner can land alone, with no review in existence. A non-owner reaches an owned ref only with an owner's approval, and no number of non-owner approvals substitutes for it — the two-operator rule is not a second route to the same place.

Three things worth knowing before granting it:

- **An owner's key is equivalent to the repositories they own.** Under the weight rule a stolen key buys one unit and still needs a second, conflict-graph-separated operator. Here it buys the repository. That is the trade the level exists to make; make it deliberately.
- **An owner submitting directly must have their key bound** in `--keys-file` (`<channel> <64-hex>`). An unbound key is unconstrained in what channel it claims, so the gate refuses to read ownership off an unbound one — otherwise any trusted key could name itself an owner. Approving a review does not need this; landing under your own key does.
- **`own` is granted in the file you write.** Self-service (D36) contributes to the merged table the HTTP layer enforces, but the landing gate reads the operator's file directly, so ownership cannot be self-issued.

A repository nobody owns keeps the weight rule exactly as it was, and a node with no `--acl-file` behaves as it did before D42. The file is re-read per landing, so a grant takes effect with no restart; an unreadable or malformed file refuses the landing rather than concluding there are no owners.

### Landing a review with the reason it was allowed (D43)

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

### Rotating a key without breaking anything (D44)

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

**Keep revoked keys in the trusted-keys file.** The log stores a key id, never the public key, so deleting the line makes every entry that key ever signed permanently unverifiable. Revocation does not cause that decay; deletion does, and nothing in the code can stop it.

That warning is about ed25519 keys only. A passkey-signed entry carries its own credential key (D45), so it needs no keys file and survives a restore that keeps the log and loses everything else — which is what `scripts/pull_backup.sh` does, deliberately. `choir log --verify` counts those entries on their own line, as *intact but unanchored*: the bytes are proven, and what is not proven is that the credential belonged to that account, because that binding lives in the accounts store rather than in the log. Read it as a real check that stops one step short, not as a weaker version of the ed25519 one.

Review retention is opt-in. `--review-retention N` archives completed reviews when more than `N` remain live. Incomplete reviews never lapse unless `--review-lapse-after-secs` is also set; that flag is invalid without a retention count.

### Issuing a credential without editing a file (D36)

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

The response carries `invite`, an `id:secret` pair, once. Hand it over out of band. The holder redeems it — the invite *is* the credential, so it is presented as basic auth and reaches nothing else:

```bash
curl -u "<INVITE>" -X POST https://<HOST>/api/accounts/redeem \
  -d "{\"ssh_key\":\"$(cat ~/.ssh/id_ed25519.pub)\"}"
```

That answers, once, with the token to clone with (`https://bob:<TOKEN>@<HOST>/owner/demo.git`) and registers the key for SSH. Invites expire — a day by default, `expires_in_secs` to choose — and are single use.

`GET /api/accounts` lists who holds what, and `POST /api/accounts/revoke` with `{"user":"bob"}` deletes an account: the token stops authenticating on the next request, the grants leave the table, and the key leaves the generated `authorized_keys`. Revocation is deletion rather than a record, which is one reason none of this is in the op log — the log is append-only and cannot forget a credential.

Two rules worth knowing before you rely on it:

- **`@node` can never be issued.** Node-wide authority — the auditor role, and the rate-limit exemption that comes with it — stays in the ACL file you write by hand, so self-service cannot escalate itself. The flag needs both `--auth-file` and `--acl-file` for the same reason: a token issued with nothing to grade it against is a token to every repository.
- **The generated `authorized_keys` is generated.** Point `sshd` at it once (`AuthorizedKeysFile /path/to/repos/.choir/authorized_keys` in `sshd_config`, alongside the account setup in [Git over SSH](#git-over-ssh-d31)) and never edit it: it is rewritten on every account change, and a hand-added line disappears with the next one.

### Webhooks: something landed, go run this (D32)

`--hooks-file` posts to a URL you name whenever a ref you name moves. One subscription per line, `#` comments, and the same append-a-line discipline as every other policy file:

```
# <repo:refname pattern>        <url>                        <secret>   [allow-private]
owner/demo:refs/heads/main      https://ci.example/choir      <SECRET>
owner/demo:refs/heads/*         https://ci.example/branches   <SECRET>
owner/notes:refs/tags/*         http://127.0.0.1:9000/hook    <SECRET>   allow-private
```

Patterns are the `--protected-refs` grammar: a trailing `*` is a prefix, anything else is exact, and the string matched is the view's `<repo>:<refname>` key. The file is re-read when its mtime moves, so adding a subscription is appending a line. It needs `--keys-file`, since refs reach the log through the platform sequencer. Delivery records go to `<repo-root>/.choir/hooks.jsonl`.

The body says what moved, and nothing else:

```json
{"format_version":1,"event":"ref-landed","repo":"owner/demo","ref":"refs/heads/main",
 "ref_key":"owner/demo:refs/heads/main","old":"<git oid>","new":"<git oid>","seq":41,
 "entry":"<entry hash>","actor":"<channel>","key_id":"<signing key id>"}
```

`old` is null for a created ref and `new` is null for a deleted one. `entry` is the log entry's content hash: it is unique per event, so a receiver that has already acted on one can discard a repeat.

**Verify the secret.** The delivery carries `X-Choir-Hook-Secret: <secret>`, the secret on that subscription's line, and a receiver should compare it before acting — otherwise anything that can reach the URL can pretend to be your node. It is a bearer secret rather than a signature over the body, so give each subscription its own (`openssl rand -hex 32`) and keep the file mode 0600. A non-loopback target must therefore be `https`; the node refuses to send the secret in clear.

**Best-effort, never silent.** Three attempts per delivery, and every attempt, every refusal and every dropped event is a JSON line in `hooks.jsonl`. Deliveries are *not* at-least-once: a webhook runs on its own thread behind a bounded queue, and when a receiver is slower than the node produces refs, events are dropped and counted rather than allowed to delay op admission. A receiver that may not miss a ref polls `GET /api/log?from=N` instead, which is what agents already do for catch-up.

**Targets are vetted.** This is the node's only outbound request to an address someone else chose, so it refuses loopback, private, carrier-NAT, link-local (including the `169.254.169.254` metadata service), unique-local and unspecified addresses unless the line ends in `allow-private`; it connects to the address it vetted rather than re-resolving the name; and it follows no redirects.

### Request log and rate limiting (D33)

Authentication says who you are and the ACL says what you may reach. Neither leaves a record of what you did, and neither bounds how much of it you do. These two flags are the rest of the floor a second credential needs, and like `--acl-file` both require `--auth-file`, since both key on the authenticated username.

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --auth-file ~/.choir/auth \
  --acl-file ~/.choir/acl \
  --request-log ~/.choir/requests.jsonl \
  --rate-limit-api 600 \
  --rate-limit-git 120
```

**The request log** is one JSON object per served request, the same shape as the op log and the lag log:

```json
{"format_version":1,"at_unix_ms":1755100000000,"user":"alice","method":"GET","path":"/api/view","us":812,"status":200,"bytes":4310}
```

Every served request produces a line, including refusals — a `401` is recorded as `anon` (never the attempted username, which is attacker-chosen), a `429` is recorded against the user it was charged to, and a response that failed midway is recorded with `"status":0` and the write error's kind.

What is never written: any header, any request body, and any query string. The path is truncated at `?` before it is captured, so `GET /api/log?from=0&token=…` is recorded as `/api/log`. A token that reaches a log file is a token that has to be rotated, and this file exists to be read during an incident by whoever is handling it.

Rotation is size-bounded and single-generation. Past `--request-log-max-bytes` (32 MiB by default) the file is renamed to `<path>.1`, replacing any earlier `.1`, and a fresh one is started. Disk is therefore bounded at about twice that number with no cron entry, no timer and no logrotate config — and exactly two generations are kept, so an operator who wants deeper history copies the file out on their own schedule. Writes are unbuffered and unsynced: one `write_all` per request, no `fsync`, because a log that loses its tail to a buffer is worthless and a disk round-trip per request is not something a response path should carry.

**Rate limiting** is a token bucket per user per class, in memory. Each ceiling is requests per minute; capacity is one minute's worth, so an agent may burst a minute's allowance at once and then proceeds at the sustained rate, which is the shape agent traffic actually has. An over-limit request is answered `429` with `Retry-After` in whole seconds, JSON on an API path and plain text on a git path (git shows the operator the body and nothing else).

The two classes carry separate flags because their costs are unrelated. A clone is one request that streams an entire pack; a `POST /api/submit-batch` is one request carrying many operations. One shared ceiling would either throttle an ordinary fetch loop or leave the operation path effectively unmetered. Set both, or set one and leave the other unlimited. The browser page and the D30 browsing pages count against the API bucket even though they shell out to `git`, which is a reason to give that number a real value rather than a huge one.

Three things are never limited, and each is a deliberate refusal to build a lockout:

| Exempt | Why |
|---|---|
| The loopback hook callback | A push of N refs makes N `/api/git-update` calls. Throttling the fourth fails the push halfway and drives the retraction path for refs git will never create. It carries a node-minted secret over loopback and is not an untrusted caller. |
| Any holder of an `@node` grant | That grant is already total authority over the node. Throttling the one actor who can repair it, during the incident the limiter is reporting, is worse than the flood. |
| Every request on a node with no `--auth-file` | There is no per-user identity to meter, and a single shared `anon` bucket is a self-inflicted outage rather than a limit. The daemon refuses the flags outright in that configuration. |

On a node with `--auth-file` but no `--acl-file` nobody is exempt except the hook callback, because there is no `@node` grant to hold. An operator who wants an exemption grants themselves one — which is the same two lines the ACL section already recommends.

### Per-user quotas (D37)

A rate does not imply a size. One push a minute is still an unbounded pack, and one workspace a minute is still unbounded disk. Two more ceilings, on the same subject as the D33 flags and so with the same `--auth-file` requirement and the same three exemptions:

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --auth-file ~/.choir/auth \
  --acl-file ~/.choir/acl \
  --quota-push-bytes 268435456 \
  --quota-workspaces 20
```

**`--quota-push-bytes`** bounds the body of one git request. Over it, the node answers `413` with both numbers — what you sent and what is allowed — because a refusal that says only "too large" leaves the pusher guessing how much to split by.

Where that check sits is the whole design rather than an implementation detail. Git applies no ref until the `pre-receive` hook exits zero, so a size check *inside* the hook would already have submitted ops for the refs it did reach — ops for refs git will never create — and would have to drive the compensating retraction pass to take them back. The ceiling is checked before `git http-backend` is spawned, and `git http-backend` is what runs the hook. So a refused push never runs a hook, never submits an op, and never enters the retraction path. It also closes an unbounded read that buffered every pushed pack in memory.

The cost is stated rather than hidden: an over-limit body is drained to a sink before the refusal is written, so the client reads a `413` instead of a broken connection. The bytes still cross the network. What the ceiling buys is that they never reach memory beyond the ceiling, never reach `git`, and never reach the log.

**`--quota-workspaces`** bounds how many workspaces one user holds at once, answered `403` with the `quota_exceeded` code and the action that frees room. The count is **not a new persisted file**: it is folded out of the op log during the replay the node already performs at startup, keyed on the attribution channel every workspace-creating operation already carries. A restart rebuilds it from the same log that rebuilds the view, so the ceiling survives a restart with no new format, no new file and no second durability barrier.

Two boundaries, named rather than left to be discovered:

- The ceiling is checked on `POST /api/workspace` and not on `POST /api/submit`. A credential that signs a `SetWorkspaceHead` itself still creates a view entry nothing refuses. Enforcing on the submission path means enforcing inside the sequencer's admission check, which cannot see an `@node` grant and would therefore throttle the one actor who can repair the node — the lockout this whole family of features exists not to cause. The sequencer does bound one thing at that door, but it is a different kind of thing: how many ops one actor may have *awaiting a decision* at once, a window that empties as the writer works rather than a holding a credential can exhaust. See below.
- The count is read outside the provisioning lock, so two simultaneous creations by a user at their ceiling can both pass. The overshoot is bounded by that user's own concurrency, not unbounded.

### Fairness at the sequencer's door

One actor submitting a thousand operations must not put another actor's single operation behind all thousand of them. So in front of the single writer, each actor holds a bounded number of ops awaiting a decision — 512, twice the writer's 256-op batch — and the writer serves what it drained round-robin across actors rather than in arrival order.

The total order is untouched. Round-robin decides who is asked next; the writer still stamps `seq` one at a time, alone, and appends in exactly the order it decided.

Over the bound, a submission is **answered rather than parked**: a rejection naming the count and the limit, so a flooding client is told to retry when one of its own ops completes instead of discovering the queue by waiting in it.

Two limits, named rather than left to be discovered:

- **The actor key is a claim, not a verified identity.** It is the key id the submission carries; the signature is verified later, on the writer thread. That is enough to bound an honest flooder and nothing more. Invented identities are the rate limiter's problem (D33), and claiming someone else's key id is a bounded denial of service against that one actor. **Nothing here may ever become load-bearing for authorization.**
- **Nothing measures the wait.** `decision_latency` is stamped when the writer picks an op up, so time spent in front of the writer has never been visible to the node's own instruments. This adds a second place to wait that they still cannot see.

There is no flag. The bound is a backstop well above a working agent's concurrency, not a policy dial.

### Decision journal (`--journal`)

The request log records what was *asked*. It cannot say what the sequencer *decided*, because an accepted op and a refused one are both a `200` on `POST /api/submit`. `--journal <file>` appends one JSON object per admission decision, plus the events that explain the decisions around it:

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --keys-file ~/.choir/keys \
  --journal ~/.choir/decisions.jsonl
```

```json
{"format_version":1,"kind":"decision","actor_id":"8f3a…","workspace":"op/agent","op_type":"SetRef","decision":"accepted","reject_reason":null,"seq":41,"parent":"…","decision_latency_us":812}
```

Four kinds, every one a flat object carrying `kind`, so `jq 'select(.kind == "cas_failure")'` works with no schema to hand:

| `kind` | What it records |
|---|---|
| `decision` | Every accept and every refusal — the author when the policy identified one, the op type when it decoded one, the refusal verbatim, and the time from dequeue to decision |
| `queue_depth` | How many commands the writer drained in one wake-up: its own view of the backlog, sampled rather than continuous |
| `window_resize` | The speculative merge window moving, **with its cause**, so a shrinking window is attributable and not merely visible |
| `cas_failure` | A lost compare-and-swap, recorded separately from the rejection it also produces — "two writers raced this ref" is a fact about contention that a rejection count cannot separate from a client sending nonsense |

**It is derived data and nothing else.** No hash covers it, nothing replays it, and losing the whole file changes no decision the node has made. That is what licenses the two properties it has: the writing happens on its own thread, and a record is dropped rather than allowed to stall the writer. A journal that could block the single writer would be a durability barrier wearing an observability costume.

Without the flag nothing is recorded and nothing is *built*. Each event owns its strings, so constructing one only to discard it costs several allocations per op on the writer thread — wiring the journal in without that guard moved the submit path from 174 allocations per op to 212, and `submit_path_allocation_budget` failed, which is that test doing its job.

### Repairing a log (`choir repair`)

A node killed mid-write leaves a partial final record. `FileLog::open` truncates that torn tail automatically, so a daemon comes back after a power cut without a human — but it copies the bytes to a `<log>.torn-<offset>` sidecar first, synced before the truncation is issued. Nothing in this workspace deletes log bytes.

Everything else is a tool, and the tool makes the operator choose the mode:

```bash
cargo run -p choir-cli -- repair ~/.choir/repos/.choir/ops.jsonl --verify
```

| Mode | What it does | Exit |
|---|---|---|
| `--verify` | Walks the chain, reports the first bad record and how much was intact before it. Changes nothing, quarantines nothing. | `0` usable, `1` damaged |
| `--truncate-tail` | Only when the damage is a torn final record: quarantines those bytes, truncates to the last complete entry, syncs. | `0` repaired, `1` refused |
| neither, or both | Usage error. `repair` alone is never read as permission to modify a log. | `2` |

`--verify` reads the file with its own reader rather than opening it as a log, because opening a log repairs it. A verify built on `FileLog::open` would truncate the tail as a side effect and then report the file intact, having caused the change it failed to mention.

Damage anywhere but the tail is **refused, not patched**. Cutting the file back past a mid-log break would drop records already acknowledged to clients, so the tool prints restore-from-backup steps and exits `1`. Refusing also means quarantining nothing: there is no suffix there it would be safe to remove.

## Use the node

Auth on the CLI is flags, not env:

```bash
choir --auth-file ~/.choir/auth --auth-user choir <command> ...
```

Exit codes: **0** accepted, **1** rejected (JSON body printed — see `ERRORS.md`), **2** usage.

The signed-operation API is the primary agent path: it carries actor identity and batches many operations behind one durability barrier. `git push` remains the compatibility and bulk-transfer path.

### Browser surface

Open the node's base URL (`/`) in a browser and it serves one read-only page: refs grouped by repository, the review queue with approval weights and verdicts, the latest ref-state attestation, workspaces, and sequencer health against the 100 ms gate. It is behind the same auth wall as everything else, so a browser prompts for a `--auth-file` user and token — anonymous readers get `401`, on the page exactly as on the API.

It is deliberately not an app. The page is server-rendered from the same `/api/view` payload the API serves (so it cannot drift from the API), cached by view sequence, and revalidated with an `ETag` — a repeat visit on unchanged state returns `304` with no body, so refreshing or polling it costs the node nothing. No JavaScript, no build step, no external fetch, so it works offline and inside networks with no route to the internet.

Writes from a browser exist in exactly one place, under its own decision (D39): a reviewer can cast a verdict or leave a comment on a review page, and a person can enrol a passkey on `/account`. The browser signs the operation with a key that never leaves the device, so the node cannot forge it — the same property the CLI's actor key has, which is why this is a second signature scheme rather than a second write path. Everything else still goes through the signed-operation API.

That client half is one same-origin file, `/static/webauthn.js`: no library, no build step, nothing from another host, and no page-embedded code. The two pages that use it are the only ones served with a `script-src 'self'` policy; every other page, including all of `/r/`, is served `default-src 'none'` and runs nothing at all. With scripting off, those two sections are a sentence naming the CLI rather than a control that cannot work.

### Repository browsing

`/r/` lists the repositories your credential may read, and each one browses:

| URL | Shows |
|---|---|
| `/r/<owner>/<repo>` | the default branch at the repository root |
| `/r/<owner>/<repo>/tree/<rev>/<path>` | a directory listing |
| `/r/<owner>/<repo>/blob/<rev>/<path>` | one file, with line numbers |
| `/r/<owner>/<repo>/commits/<rev>` | recent history |
| `/r/<owner>/<repo>/commit/<oid>` | one commit and its diff |
| `/r/<owner>/<repo>/reviews` | reviews proposing to land here |
| `/r/<owner>/<repo>/review/<id>` | one review: proposal, reviewers, verdicts, diff |

Same auth wall, and the same `read` grant a clone needs — a repository you hold no grant on answers `404` here too, so browsing never confirms that one exists. Content pages revalidate on the **commit oid** rather than the view sequence, because file content lives in the bare repository and the sequence describes the op log; a `304` here means "this commit's bytes have not changed", which is true forever.

Large files are described rather than dumped (512 KiB), binary files are named rather than rendered, and long diffs truncate at 2,000 lines — the clone path exists for all three. Nothing from a URL reaches `git` unvalidated: revision arithmetic (`main~3`, `HEAD@{1}`), traversal and anything option-shaped are refused at the router rather than escaped later.

A review page shows what commit lands on what ref, who was asked and what each said (with their verdict notes), the approval weight, any retroactive slashing, and the diff between the proposal and its destination. That diff is three-dot: it shows what the proposal added since it diverged, not every difference between two branches — so work that landed on the target while the review was open is never attributed to the author under review. The page adds no state; every field comes from the same payload `/api/view` serves. Comments are not implemented: a discussion record is a persisted operation and needs its own decision, so verdict notes are the discussion the log actually carries.

### Git compatibility path

```bash
# after choirctl install:
git clone "$(sh scripts/choirctl url owner/repo.git)"
# or manually:
# git clone http://choir:<token>@127.0.0.1:8417/owner/demo.git

git push origin HEAD:main
```

Pushes are CAS-sequenced. On rejection: fetch, rebase/merge, push again — **never force-push** over a sequencer rejection.

### Git over SSH (D31)

Agents are content with HTTPS and a token. People expect `git@host:owner/repo.git`. The node does not run an SSH server; the host's `sshd` does, and a forced command hands each connection to the `choir-ssh` shim, which is how Gitea and gitolite do it. There is no in-process SSH server and there is not going to be one: every Rust SSH library within reach is tokio-based, and this daemon is synchronous threads.

Start the node with a handoff file. It carries the daemon's address and the loopback secret its git hooks authenticate with, both of which change on every start, which is why they cannot live in an `authorized_keys` line:

```bash
choir-node <repo-root> 8417 --auth-file <auth-file> --keys-file <keys-file> \
  --acl-file <acl-file> --ssh-handoff <handoff-file>
```

Then give the SSH account one line per registered key, all on one line:

```
command="/usr/local/bin/choir-ssh --root <repo-root> --user <choir-user> --acl-file <acl-file> --handoff <handoff-file> --git-binary /usr/bin/git",restrict ssh-ed25519 AAAA... <user>@<host>
```

`--user` is the choir username that key belongs to, and it is the entire key-to-actor mapping. The client cannot reach it: sshd runs the forced command and puts whatever the client asked for in `SSH_ORIGINAL_COMMAND`, which is the shim's only untrusted input. `restrict` turns off pty, agent, port and X11 forwarding. `--git-binary` is worth setting explicitly, because sshd runs the forced command through a non-interactive shell whose `PATH` is often not the operator's.

Clone with either spelling:

```bash
git clone ssh://<ssh-account>@<SERVER_IP>/owner/demo.git
git clone <ssh-account>@<SERVER_IP>:owner/demo.git
```

What the shim serves:

- exactly `git-upload-pack '<repo>'` and `git-receive-pack '<repo>'` (the dashless `git upload-pack` spelling too), one argument, never a shell. Anything else, `git-upload-archive` and interactive logins included, is refused with a message the client prints.
- `owner/repo` or `owner/repo.git`, two segments, ASCII, no segment starting with a dot — so the node's own `.choir` state directory is not addressable.
- the same `--acl-file` the HTTP path reads, demanding the same level: `read` to fetch, `write` to push. A repository you may not read is refused in the same words as one that does not exist. Leave `--acl-file` off the line and the shim uses whatever the daemon named in the handoff, so a forgotten flag is not the difference between a gated repository and an open one.
- pushes that run the repository's `pre-receive` hook, so an SSH push is sequenced exactly like an HTTPS one and lands in the log under the same `owner/repo.git:refs/heads/...` name. A shim installed without `--handoff` serves fetches and **refuses pushes**, rather than let one through unsequenced.

Before deploying it, three limits:

- the handoff file holds the daemon's loopback secret at `0600`, so the SSH account and the daemon must be the same uid. If your deployment needs them separate, stay on HTTPS: do not widen who can read that secret.
- one line per key, and revocation is deleting the line. There is no expiry and no rotation. With `--accounts-file` the node writes those lines for you from the keys people registered when they redeemed an invite ([above](#issuing-a-credential-without-editing-a-file-d36)); point `AuthorizedKeysFile` at the generated file instead of maintaining one by hand.
- choir does not manage `sshd`. Its port, host keys, and account are the operator's, exactly as they were before choir was installed.

### Signed-operation CLI and API (primary agent path)

<!-- generated: choir surface, do not edit -->

#### HTTP endpoints

| Endpoint | Purpose |
|---|---|
| `POST /api/submit` | Submit one signed operation (hex payload, hex signature) |
| `POST /api/submit-batch` | Same, in array order; the primary path for agent workloads (throughput figures live in the build log, not here, so they cannot go stale) |
| `GET /api/view?limit=N&offset=M` | The materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate. On a node running an ACL you are served your own slice: the repositories your credential may read, plus reviews you were assigned to; the node-wide sections need a node-wide grant. A repository missing from the response is one you were not granted, not one that is gone. Every map-shaped section is bounded: `limit` rows each (200 by default, 1000 at most), `offset` rows skipped in key order, `<section>_omitted` counting what this page left out, and `paging.next` naming the request that fetches the rest or being null when there is none |
| `POST /api/appeal` | Record an appeal for a rejected newcomer attempt; it requests operator adjudication and never changes privilege |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it. Each entry carries its hash, parent and author signature so pages can be chained and verified without trusting the node; SYNC.md is that procedure |
| `POST /api/workspace` | Provision a CoW workspace; optional exact base/change binding makes retries idempotent |
| `POST /api/workspace/archive` | Recoverably archive a change-bound workspace and remove it from the active view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `GET /api/schema` | This surface, machine-readable and versioned, plus what this particular node will accept — the description an agent generates a client from (D17) |
| `GET /llms.txt` | This surface, as text, for an agent that has never seen choir |
| `GET /sync.md` | The sync contract, in full: cursor semantics and how to verify a page's hash chain and author signatures without trusting the node serving them |
| `GET /api/ref-agreement` | Where the op log and the bare repos disagree about a ref, read-only |
| `POST /api/accounts/invite` | Mint a single-use, expiring invite for a new account and the grants it will hold; needs a node-wide write grant, and can never issue one |
| `POST /api/accounts/redeem` | Redeem an invite — presented as the credential — for a token, once, and register an ssh key with it |
| `POST /api/accounts/revoke` | Delete an account: its token stops authenticating on the next request, and its grants and keys go with it |
| `GET /api/accounts` | Who holds an account, what they were granted, and which invites are outstanding; never a secret or its hash |
| `POST /api/git-update` | Internal: the pre-receive hook callback |
| `POST /api/git-abort` | Internal: retracts a refused push's already-accepted refs |

#### The `choir` CLI

```text
usage:
  choir [--auth-file <path>] [--auth-user <name>] <command> ...

commands:
  choir key <key-file> [name]
  choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]
  choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
  choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
  choir runner <config-file>
  choir submit <api> <key-file> <channel> '<op-json>'
  choir schema <api>
  choir log <api> [--from <n>] [--verify] [--keys <file>]
  choir batch <api> <key-file> <channel> <ops-file>
  choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
  choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
  choir comment <api> <key-file> <channel> <review-id> <comment-id> '<body>'
  choir viewed <api> <key-file> <viewer> <review-id>
  choir slash <api> <node-key-file> <id> <reviewer> '<reason>'
  choir abandon <api> <node-key-file> <id>
  choir bind <api> <node-key-file> <operator> <key-hex> [channel]
  choir revoke <api> <node-key-file> <key-hex> '<reason>'
  choir appeal <api> <attempt-id>
  choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
  choir reviews <api> <reviewer>
  choir acl render <api> <acl-file>
  choir triage <api>
  choir state <api> <channel>
  choir skill install [--into <dir>]
  choir view <api> [--limit <n>] [--offset <n>]
  choir repair <log-file> --verify | --truncate-tail

Exit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), 2 usage error.
```
<!-- /generated -->

Live surface on a running node: `GET /llms.txt`. Sync verification: `SYNC.md` / `GET /sync.md`.

### MCP adapter

For MCP clients, run the synchronous stdio adapter. It maps generated tools onto the same HTTP endpoints and owns no second implementation or session state.

```bash
choir-mcp http://127.0.0.1:8417 --auth-file ~/.choir/auth --auth-user choir
```

It serves the measured legacy handshakes and the stateless 2026-07-28 request path. Tool order and schemas come from `crates/choir-cli/src/surface.rs`.

### Minimal day-one loop

```bash
API=http://127.0.0.1:8417
A=(--auth-file "$HOME/.choir/auth" --auth-user choir)
OWNER=myop/agent
CHANGE=change-1
WORKSPACE=owner/demo/agent-a

# 1. Confirm the node
choir "${A[@]}" view "$API"

# 2. Exact-base CoW workspace and stable change
choir "${A[@]}" workspace "$API" owner/demo agent-a \
  --base "$(git rev-parse HEAD)" --owner "$OWNER" --change "$CHANGE" \
  --idempotency-key request-1

# 3. Publish intent. After editing, commit and push before checkpointing.
choir "${A[@]}" intent "$API" "$HOME/.choir/agent.key" "$OWNER" "$CHANGE" task 'ship feature X'
git push origin HEAD:refs/heads/agent-a
choir "${A[@]}" checkpoint "$API" "$HOME/.choir/agent.key" "$OWNER" \
  "$CHANGE" "$WORKSPACE" "$(git rev-parse HEAD)"

# 4. Request review with no reviewer names so the node draws them
choir "${A[@]}" review "$API" "$HOME/.choir/agent.key" "$OWNER" rev-1 "$(git rev-parse HEAD)" \
  --ref owner/demo.git:refs/heads/main

# 5. Drawn reviewers answer
choir "${A[@]}" reviews "$API" otherop/reviewer
choir "${A[@]}" verdict "$API" "$HOME/.choir/other.key" otherop/reviewer rev-1 approve
```

**Review rules that matter in practice**

- Name **no** reviewers on `choir review`; empty list ⇒ node assignment. Self-picked lists may be refused under `--require-assignment` / protected refs.
- Channel names: `operator/agent`. Same-operator agents cannot review each other.
- Bind keys when registering: `choir key ~/.choir/agent.key myop/agent >> ~/.choir/keys`.
- Protected landing with `--require-review` needs approval weight **2** (two distinct operators), unless somebody holds `own` over the repository, in which case one owner's assent lands it and nothing else does (D42). See `scripts/flip/RUNBOOK.md` to enable gates on the dogfood node.
- Operators can invalidate a bad approval with `choir slash`; it lowers future approval weight and marks re-review required, but never rewrites an already-landed ref.
- An optional reviewer conflict graph excludes operators within the configured hop distance from the requester. It is re-read per draw and fails closed by leaving the review unassigned.
- `choir view` reports T3 concentration using exact counts and integer shares. Active branches mean last attributable mover, and protected updates mean admitted ref updates under the current policy; unknown and ambiguous attribution stay visible and make the overall status `indeterminate` rather than a pass.
- `choir view` also reports `view_growth`: record counts and compact JSON bytes for workspaces, refs, reviews, and provenance. `total_authoritative_view` covers exactly those four sections and excludes runtime projections. This measures complete-view growth; it does not prune or expire anything.
- `choir view` reports `newcomer_harm` when the operator enables the two 0600 audit files. A rejected signed-API newcomer can run `choir appeal <api> <attempt-id>`; the appeal requests separate operator adjudication and never grants privilege. Thresholds stay unset until the first real adoption-gate measurement.
- Prefer `POST /api/submit-batch` for multiple ops (one durability barrier).

Optional forge follower / speculative GitHub queue: `choir-bridge` — see the crate docs (`cargo doc -p choir-bridge`).

Bridge utility modes mint or inspect its identity (`--pubkey`), inspect GitHub App installations (`app-debug`), exercise one commit-status write (`post-status`), replay existing merge commits for offline D23 calibration (`calibrate`), and mine a mirrored foreign history for real semantic-conflict specimens (`harvest`, D27 — offline, no forge access, landing policy untouched). Queue mode can optionally run the advisory three-worktree D23 detector; it never changes the landing condition. Grant only the permissions in the [bridge permission model](crates/choir-bridge/PERMISSIONS.md); `queue --land` is the only routine mode that needs contents write access.

### Agent templates

Teach Claude Code / Codex / Cursor to speak choir: see [`templates/README.md`](templates/README.md).
That guide also includes a tested Claude Code `WorktreeCreate` and
`WorktreeRemove` adapter for Choir-backed isolated sessions.

```bash
source templates/choir.env.sh   # sets CHOIR_API; optional user/token/key
# then install the harness snippet listed in templates/README.md
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `cargo build` pulls huge tree / sqlite errors | `choir-actor` / rivetkit | Keep `.cargo/config.toml`. Default members already exclude actor; use `-p choir-actor` only when needed. |
| `choir-actor` ignored test fails / download broken | rivetkit 2.3.10 auto-download | Run: `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored` |
| Node refuses bind address | Non-loopback without TLS | Add `--tls-cert` / `--tls-key`, or stay on `127.0.0.1` / SSH tunnel |
| `/api/view` → 401 | Auth enabled (expected) | Pass `-u user:token` or `--auth-file` / `--auth-user` |
| Browser asks for a username/password | Auth is mandatory on every endpoint, including public TLS binds | Enter a user and token from `--auth-file`. Nothing is served anonymously by design |
| Push not in `/api/view` | Repo created without `--create` | Recreate via node/`choirctl` so `pre-receive` exists |
| `unknown_key` | Key not in `--keys-file` | `choir key … [channel] >> keys-file` (hot-reloaded) |
| `bad_signature` | Signature does not cover the bytes sent; key **is** trusted | Re-sign the exact `(channel, payload)`. Registering a key does not help. Unexpected → someone replayed a signature |
| `stale_head` | CAS lost the race | Re-read `/api/view`, rebase on `actual`, resubmit |
| `assignment_error` / empty reviewers | Empty `--reviewers-file` | Add at least two `operator/…` channels for meaningful review |
| `review_required` | Protected ref, insufficient weight | Node-drawn review + two operators approve, then push |
| Workspace slow / fails | No CoW FS | Use APFS or btrfs |
| Lost submit response | Network blip after accept | Resubmit **identical** signed bytes → `already_applied: true` (`ERRORS.md`) |
| Node will not start, log reported corrupt | A fully-written record breaks the chain mid-log | `choir repair <log> --verify` names the first bad record. Mid-log damage is a restore, not a repair |
| A `<log>.torn-<offset>` file appeared | The node was killed mid-write; the partial tail was quarantined, then truncated | Expected, and not an error. Nothing reads it; keep it as long as you want the evidence |
| A submitter is told its quota is exhausted | That actor already has 512 ops awaiting a decision | Not the D37 quota. Retry when one of your own ops completes; it is a bounded in-flight window, not a holding |
| A restored node printed `choir: retracted` and refs are gone | It was started before the git objects were in place, so reconciliation made the log agree with the emptiness | Start again from the backup into a clean root. Objects go in **before** the first boot: `docs/runbook-restore.md` |

Rejection code table: [`ERRORS.md`](ERRORS.md).

## Docs map

| Doc | What it is |
|---|---|
| [`agents.md`](agents.md) | Agent-facing surface (generated; edit `crates/choir-cli/src/surface.rs`) |
| [`ERRORS.md`](ERRORS.md) | Rejection codes and repair hints |
| [`SYNC.md`](SYNC.md) | Log catch-up + hash/signature verification |
| [`templates/`](templates/README.md) | Drop-in agent harness snippets |
| [`scripts/choirctl`](scripts/choirctl) | Dogfood node operator entrypoint |
| [`scripts/flip/RUNBOOK.md`](scripts/flip/RUNBOOK.md) | Supervised install + protected-ref gates |
| [`docs/runbook-restore.md`](docs/runbook-restore.md) | Rebuilding a node from a backup, and the secrets a backup never holds |
| [`DECISIONS.md`](DECISIONS.md) | Decision register: what was decided, and which choices are one-way |

## License

Workspace crates: [`MIT`](LICENSE-MIT) OR [`Apache-2.0`](LICENSE-APACHE), at your option. Mergiraf (optional subprocess) is GPLv3 and is never linked, only executed.
