# Rate limits, quotas and fairness

Three ceilings on three different things, plus one that is not a ceiling at
all:

| Bound | Flag | Bounds |
|:--|:--|:--|
| Requests per minute | `--rate-limit-api`, `--rate-limit-git` (D33) | how often one user may ask |
| Bytes in one push | `--quota-push-bytes` (D37) | how large one git request may be |
| Workspaces held at once | `--quota-workspaces` (D37) | how much disk one user may hold |
| Ops awaiting a decision | none, deliberately | how far one actor may get ahead of the writer |

Each of the flagged ones requires `--auth-file`, because each keys on the
authenticated username, and each carries the same three exemptions.

## Request log and rate limiting (D33)

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

Every served request produces a line, including refusals: a `401` is recorded as `anon` (never the attempted username, which is attacker-chosen), a `429` is recorded against the user it was charged to, and a response that failed midway is recorded with `"status":0` and the write error's kind.

What is never written: any header, any request body, and any query string. The path is truncated at `?` before it is captured, so `GET /api/log?from=0&token=…` is recorded as `/api/log`. A token that reaches a log file is a token that has to be rotated, and this file exists to be read during an incident by whoever is handling it.

Rotation is size-bounded and single-generation. Past `--request-log-max-bytes` (32 MiB by default) the file is renamed to `<path>.1`, replacing any earlier `.1`, and a fresh one is started. Disk is therefore bounded at about twice that number with no cron entry, no timer and no logrotate config; exactly two generations are kept, so an operator who wants deeper history copies the file out on their own schedule. Writes are unbuffered and unsynced: one `write_all` per request, no `fsync`, because a log that loses its tail to a buffer is worthless and a disk round-trip per request is not something a response path should carry.

**Rate limiting** is a token bucket per user per class, in memory. Each ceiling is requests per minute; capacity is one minute's worth, so an agent may burst a minute's allowance at once and then proceeds at the sustained rate, which is the shape agent traffic actually has. An over-limit request is answered `429` with `Retry-After` in whole seconds, JSON on an API path and plain text on a git path (git shows the operator the body and nothing else).

The two classes carry separate flags because their costs are unrelated. A clone is one request that streams an entire pack; a `POST /api/submit-batch` is one request carrying many operations. One shared ceiling would either throttle an ordinary fetch loop or leave the operation path effectively unmetered. Set both, or set one and leave the other unlimited. The browser page and the D30 browsing pages count against the API bucket even though they shell out to `git`, which is a reason to give that number a real value rather than a huge one.

Three things are never limited, and each is a deliberate refusal to build a lockout:

| Exempt | Why |
|:--|:--|
| The loopback hook callback | A push of N refs makes N `/api/git-update` calls. Throttling the fourth fails the push halfway and drives the retraction path for refs git will never create. It carries a node-minted secret over loopback and is not an untrusted caller. |
| Any holder of an `@node` grant | That grant is already total authority over the node. Throttling the one actor who can repair it, during the incident the limiter is reporting, is worse than the flood. |
| Every request on a node with no `--auth-file` | There is no per-user identity to meter, and a single shared `anon` bucket is a self-inflicted outage rather than a limit. The daemon refuses the flags outright in that configuration. |

On a node with `--auth-file` but no `--acl-file` nobody is exempt except the hook callback, because there is no `@node` grant to hold. An operator who wants an exemption grants themselves one, which is the same two lines the ACL section already recommends.

## Per-user quotas (D37)

A rate does not imply a size. One push a minute is still an unbounded pack, and one workspace a minute is still unbounded disk. Two more ceilings, on the same subject as the D33 flags and so with the same `--auth-file` requirement and the same three exemptions:

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --auth-file ~/.choir/auth \
  --acl-file ~/.choir/acl \
  --quota-push-bytes 268435456 \
  --quota-workspaces 20
```

**`--quota-push-bytes`** bounds the body of one git request. Over it, the node answers `413` with both numbers (what you sent and what is allowed), because a refusal that says only "too large" leaves the pusher guessing how much to split by.

Where that check sits is the whole design rather than an implementation detail. Git applies no ref until the `pre-receive` hook exits zero, so a size check *inside* the hook would already have submitted ops for the refs it did reach (ops for refs git will never create) and would have to drive the compensating retraction pass to take them back. The ceiling is checked before `git http-backend` is spawned, and `git http-backend` is what runs the hook. So a refused push never runs a hook, never submits an op, and never enters the retraction path. It also closes an unbounded read that buffered every pushed pack in memory.

The cost is stated rather than hidden: an over-limit body is drained to a sink before the refusal is written, so the client reads a `413` instead of a broken connection. The bytes still cross the network. What the ceiling buys is that they never reach memory beyond the ceiling, never reach `git`, and never reach the log.

**`--quota-workspaces`** bounds how many workspaces one user holds at once, answered `403` with the `quota_exceeded` code and the action that frees room. The count is **not a new persisted file**: it is folded out of the op log during the replay the node already performs at startup, keyed on the attribution channel every workspace-creating operation already carries. A restart rebuilds it from the same log that rebuilds the view, so the ceiling survives a restart with no new format, no new file and no second durability barrier.

Two boundaries, named rather than left to be discovered:

- The ceiling is checked on `POST /api/workspace` and not on `POST /api/submit`. A credential that signs a `SetWorkspaceHead` itself still creates a view entry nothing refuses. Enforcing on the submission path means enforcing inside the sequencer's admission check, which cannot see an `@node` grant and would therefore throttle the one actor who can repair the node, the lockout this whole family of features exists not to cause. The sequencer does bound one thing at that door, but it is a different kind of thing: how many ops one actor may have *awaiting a decision* at once, a window that empties as the writer works rather than a holding a credential can exhaust. See below.
- The count is read outside the provisioning lock, so two simultaneous creations by a user at their ceiling can both pass. The overshoot is bounded by that user's own concurrency, not unbounded.

## Fairness at the sequencer's door

One actor submitting a thousand operations must not put another actor's single operation behind all thousand of them. So in front of the single writer, each actor holds a bounded number of ops awaiting a decision (512, twice the writer's 256-op batch), and the writer serves what it drained round-robin across actors rather than in arrival order.

The total order is untouched. Round-robin decides who is asked next; the writer still stamps `seq` one at a time, alone, and appends in exactly the order it decided.

Over the bound, a submission is **answered rather than parked**: a rejection naming the count and the limit, so a flooding client is told to retry when one of its own ops completes instead of discovering the queue by waiting in it.

Two limits, named rather than left to be discovered:

- **The actor key is a claim, not a verified identity.** It is the key id the submission carries; the signature is verified later, on the writer thread. That is enough to bound an honest flooder and nothing more. Invented identities are the rate limiter's problem (D33), and claiming someone else's key id is a bounded denial of service against that one actor. **Nothing here may ever become load-bearing for authorization.**
- **Nothing measures the wait.** `decision_latency` is stamped when the writer picks an op up, so time spent in front of the writer has never been visible to the node's own instruments. This adds a second place to wait that they still cannot see.

There is no flag. The bound is a backstop well above a working agent's concurrency, not a policy dial.
