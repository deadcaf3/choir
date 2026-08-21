# Troubleshooting

Symptoms an operator or an agent actually hits, and what each one means. The
machine-readable half of this is `ERRORS.md`, which is generated from the
node's own rejection table and is the authority on any `code` field.

| Symptom | Likely cause | Fix |
|:--|:--|:--|
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

Rejection code table: [`ERRORS.md`](../../ERRORS.md).
