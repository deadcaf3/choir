# Canonical-node flip runbook

Nothing below needs memorizing: `sh scripts/choirctl` with no arguments
lists every command. The long forms are kept here so the procedure is
auditable, but `choirctl install`, `choirctl status`, `choirctl sync`,
and `choirctl logs` are the usual operator path.

This is the D20 operator procedure. The flip makes the choir node canonical
and Forgejo a follower (D21 single-canonical invariant — one direction,
never dual-write). The
git-bundle cron on the mirror VM continues unchanged as insurance.

Every step below was rehearsed on a throwaway loopback node; the
verification commands are the ones that were actually run.

## Before flip day

Nothing here is destructive and nothing touches the mirror VM.

1. `cargo test --workspace` green, `cargo clippy --workspace --all-targets`
   at no new warnings.
2. `sh scripts/flip/install_node.sh [port] [owner/repo.git]` — builds release binaries,
   mints `~/.choir/auth` (0600) and `~/.choir/agent.key`, creates an
   empty `~/.choir/reviewers` pool plus 0600 newcomer audit/adjudication files, writes and loads the
   `com.choir.node` LaunchAgent serving the named repo. Idempotent:
   re-run after any rebuild.
3. Add reviewer names to `~/.choir/reviewers`, one per line. Re-read on
   every draw, so no restart. An empty pool means review requests come
   back with `assignment_error` and stay unassigned (never approved).

Trusted keys, channel bindings, and push-certificate signers also hot-reload
from the keys file. Registering a key does not require a daemon restart.

The installer enables sparse D24 T4 newcomer-harm evidence. Keys already trusted
at first activation are durably marked as incumbents. A later verified signed-API
actor receives `newcomer_attempt_id` on its first outcome; if that outcome was a
rejection, it may file `choir appeal <api> <attempt-id>`. Appeals do not alter
admission. The operator adjudicates each observed attempt by appending exactly one
row to `~/.choir/newcomer-adjudications.jsonl`:

```json
{"format_version":1,"attempt_id":0,"legitimate":true}
```

Use `false` for an invalid or abusive attempt. `/api/view.newcomer_harm` reports
adjudication coverage and hashes the current adjudication snapshot. Unknown-key
claims and the separate Git/HTTP credential lane are outside this metric. The T4
thresholds remain unset until a real measurement supplies the adoption bound.

To persistently require assigned review for protected refs, create
`~/.choir/review-gates.enabled` and `~/.choir/protected-refs`, both mode 0600,
then rerun `choirctl install`. The protected-ref file carries one namespaced
pattern per line, for example `<owner>/<repo>.git:refs/heads/main`. The installer
fails closed unless the reviewer pool has at least two prefixes, every reviewer
has a bound key, and the protected-ref file is non-empty. Removing the marker
and reinstalling deliberately returns to the ungated policy.

Verify the daemon:

```sh
launchctl print gui/$(id -u)/com.choir.node | head       # loaded, KeepAlive
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8417/api/view   # 401
target/release/choir --auth-file ~/.choir/auth --auth-user choir view http://127.0.0.1:8417
tail ~/.choir/node.log
```

The unauthenticated request must return 401. A 200 means the real node was
started without the required auth file.

## Flip day

1. The canonical repo already exists: the installer put `--create` in
   the plist, and creation is idempotent (an existing repo is skipped).
   Creation is also what installs the `pre-receive` hook that turns
   pushes into signed ops, so a repo made any other way will not be
   sequenced.
2. `sh scripts/flip/push_canonical.sh [port] [owner/repo.git]` pushes the
   current branch by name, then pushes all tags. It deliberately avoids
   `--all`, which would include transient worktree branches.

   Checklist item 3, measured: `git push --all` sent 1 branch and **0
   tags**; the view's `refs` gained the tag only after the second push.
   Both are required or release history is silently lost.
3. Verify every ref is in the log as a sequenced op:

```sh
target/release/choir --auth-file ~/.choir/auth --auth-user choir view http://127.0.0.1:8417
git rev-parse HEAD     # must equal the oid under refs/heads/main
```

   The oid in the view carries codec byte `11` (git oid) — a bare
   BLAKE3 digest there means something bypassed the git path.
4. Repoint the follower: from here on use `sh scripts/choirctl sync`,
   which pushes to the node first and the Forgejo mirror second, and
   stops before the mirror if the canonical push failed — so the
   follower can never get ahead of the node. `push` and `mirror` remain
   available separately. Do not add a push from Forgejo back to the
   node (D21 single-canonical: one direction, never dual-write).

## Rollback

The flip is reversible and cheap: the Forgejo mirror still holds every
ref, and the bundle cron holds a daily copy.

```sh
sh scripts/choirctl stop         # unload it for this boot only
sh scripts/choirctl uninstall    # unload it and remove the LaunchAgent
```

`stop` alone is not permanent: the plist stays in `~/Library/LaunchAgents`,
so launchd starts the daemon again at the next login. `uninstall` removes
it. Neither touches `~/.choir`.

Nothing is lost by stopping: `~/.choir/repos` and the op log at
`~/.choir/repos/.choir/ops.jsonl` persist, and the daemon replays the
log on restart (rehearsed: refs and workspaces survived a kill).

## Restoring the op log

Stopping is safe; losing the disk was not. The bundle cron copies refs
and objects, and `ops.jsonl` is neither — it is not a git object, so no
bundle has ever contained it. `sh scripts/choirctl sync` now copies it
to `~/choir-oplog/ops.jsonl` on the mirror VM and refuses the run if the
far checksum disagrees, so a truncated copy fails loudly instead of
sitting there looking like a backup.

Two things travel: the log and `node.fingerprint`. The signing key does
not, and must not — a backup carrying it would let whoever holds the
backup keep signing as this node. That split is what makes a restore
safe rather than convenient:

```sh
scp choir@<SERVER_IP>:choir-oplog/ops.jsonl         ~/.choir/repos/.choir/ops.jsonl
scp choir@<SERVER_IP>:choir-oplog/node.fingerprint  ~/.choir/repos/.choir/node.fingerprint
sh scripts/choirctl install
```

On a host that still holds the original key this starts and replays. On
any other host it **refuses to start**, naming the pinned id, because the
restored fingerprint will not match a freshly generated key. That refusal
is the intended outcome, not a failure: continuing would append to a
signed chain under a new identity. Deleting `node.fingerprint` overrides
it, and means accepting that the log changes author at that point.

Restoring refs is separate and unchanged: the Forgejo mirror holds every
ref, and the daily bundle holds a copy.

## Still open

- This dogfood installation has no TLS, so its bind stays loopback
  (invariant 9 refuses anything else without a cert). Remote access is an
  SSH tunnel.
- A node without a persisted log returns `log_evicted` when a reader falls
  behind its in-memory window. The reader can resume at `window_base`, but
  cannot verify continuity across the gap. See the [sync contract](../../SYNC.md).
