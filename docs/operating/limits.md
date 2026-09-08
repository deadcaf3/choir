# Rate limits, quotas and fairness

| Bound | Flag | Bounds |
|:--|:--|:--|
| Requests per minute | `--rate-limit-api`, `--rate-limit-git` (D33) | how often one user may ask |
| Bytes in one push | `--quota-push-bytes` (D37) | how large one git request may be |
| Workspaces held at once | `--quota-workspaces` (D37) | how much disk one user may hold |
| Ops awaiting a decision | none, deliberately | how far one actor may get ahead of the writer |

The flagged ones require `--auth-file` and share three exemptions.

## Request log and rate limiting (D33)

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --auth-file ~/.choir/auth \
  --acl-file ~/.choir/acl \
  --request-log ~/.choir/requests.jsonl \
  --rate-limit-api 600 \
  --rate-limit-git 120
```

**The request log** is one JSON object per served request:

```json
{"format_version":1,"at_unix_ms":1755100000000,"user":"alice","method":"GET","path":"/api/view","us":812,"status":200,"bytes":4310}
```

Refusals are recorded too: a `401` as `anon` (never the attempted
username), a `429` against the charged user, a failed response with
`"status":0`. The path is truncated at `?`; headers and bodies never reach
the file.

Rotation: past `--request-log-max-bytes` (32 MiB default) the file becomes
`<path>.1` and a fresh one starts. Two generations kept. Writes are
unbuffered and unsynced.

**Rate limiting** is a token bucket per user per class, in memory,
requests per minute with one minute's burst. Over-limit answers `429` with
`Retry-After` in seconds. The browser pages count against the API bucket.

Exempt:

| Exempt | Why |
|:--|:--|
| The loopback hook callback | A push of N refs makes N `/api/git-update` calls; throttling one fails the push halfway. |
| Any holder of an `@node` grant | Already total authority; throttling the one actor who can repair the node is worse than the flood. |
| Every request on a node with no `--auth-file` | No per-user identity; the daemon refuses the flags. |

With `--auth-file` but no `--acl-file`, only the hook callback is exempt.

## Per-user quotas (D37)

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --auth-file ~/.choir/auth \
  --acl-file ~/.choir/acl \
  --quota-push-bytes 268435456 \
  --quota-workspaces 20
```

**`--quota-push-bytes`** bounds one git request body. Over it: `413` with
both numbers. Checked before `git http-backend` spawns, so a refused push
runs no hook. The body is drained so the client reads the `413`.

**`--quota-workspaces`** bounds workspaces per user: `403` with
`quota_exceeded`. The count is folded out of the op log at startup.

- Checked on `POST /api/workspace`, not `POST /api/submit`.
- Read outside the provisioning lock, so two simultaneous creations at the
  ceiling can both pass.

## Fairness at the sequencer's door

Each actor holds at most 512 ops awaiting a decision (twice the writer's
256-op batch), served round-robin. The total order is untouched: the writer
stamps `seq` alone.

Over the bound, a submission is rejected naming the count and the limit;
retry when one of your own ops completes.

- **The actor key is a claim**, verified later on the writer thread. It
  bounds an honest flooder.
- **The wait is unmeasured.** `decision_latency` starts when the writer
  picks an op up.

> [!IMPORTANT]
> Nothing here may become load-bearing for authorization.

The bound has no flag.
