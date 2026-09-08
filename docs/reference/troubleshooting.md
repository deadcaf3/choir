# Troubleshooting

Symptoms and fixes. `ERRORS.md`, generated from the node's rejection table,
is the authority on any `code` field.

| Symptom | Likely cause | Fix |
|:--|:--|:--|
| `cargo build` pulls huge tree / sqlite errors | `choir-actor` / rivetkit | Keep `.cargo/config.toml`. Default members exclude actor; use `-p choir-actor` only when needed. |
| `choir-actor` ignored test fails / download broken | rivetkit 2.3.10 auto-download | `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored` |
| Node refuses bind address | Non-loopback without TLS | Add `--tls-cert` / `--tls-key`, or stay on `127.0.0.1` / SSH tunnel |
| `/api/view` → 401 | Auth enabled (expected) | Pass `-u user:token` or `--auth-file` / `--auth-user` |
| Browser asks for a username/password | Auth is mandatory on every endpoint | Enter a user and token from `--auth-file`. Only repositories granted to `@anon` are served anonymously (D78) |
| Push not in `/api/view` | Repository served without its `pre-receive` hook | `choir repo create <owner/repo.git>`, or restart the node |
| `unknown_key` | Key not in `--keys-file` | `choir key … [channel] >> keys-file` (hot-reloaded) |
| `bad_signature` | Signature does not cover the bytes sent; key **is** trusted | Re-sign the exact `(channel, payload)`. Unexpected: someone replayed a signature |
| `stale_head` | CAS lost the race | Re-read `/api/view`, rebase on `actual`, resubmit |
| `assignment_error` / empty reviewers | Empty `--reviewers-file` | Add at least two `operator/…` channels |
| `review_required` | Protected ref, insufficient weight | Node-drawn review, two operators approve, then push |
| Workspace slow / fails | No CoW FS | Use APFS or btrfs |
| Lost submit response | Network blip after accept | Resubmit **identical** signed bytes: `already_applied: true` (`ERRORS.md`) |
| Node will not start, log reported corrupt | A fully-written record breaks the chain mid-log | `choir repair <log> --verify` names the first bad record. Mid-log damage is a restore |
| A `<log>.torn-<offset>` file appeared | Killed mid-write; the partial tail was quarantined | Expected. Nothing reads it |
| A submitter is told its quota is exhausted | That actor has 512 ops awaiting a decision | Not the D37 quota. Retry when one completes |
| A restored node printed `choir: retracted` and refs are gone | Started before the git objects were in place | Start again from the backup into a clean root. Objects go in **before** first boot: `docs/runbook-restore.md` |

Rejection code table: [`ERRORS.md`](../../ERRORS.md).
