# D20 flip runbook

Nothing below needs memorizing: `sh scripts/choirctl` with no arguments
lists every command. The long forms are kept here so the procedure is
auditable, but `choirctl install`, `choirctl status`, `choirctl push`
and `choirctl mirror` are the four you actually run.

The flip makes the choir node canonical and Forgejo a follower (D21
single-canonical invariant — one direction, never dual-write). The
git-bundle cron on the mirror VM continues unchanged as insurance.

Every step below was rehearsed on a throwaway loopback node; the
verification commands are the ones that were actually run.

## Before flip day

Nothing here is destructive and nothing touches the mirror VM.

1. `cargo test --workspace` green, `cargo clippy --workspace --all-targets`
   at no new warnings.
2. `sh scripts/flip/install_node.sh [port] [owner/repo.git]` — builds release binaries,
   mints `~/.choir/auth` (0600) and `~/.choir/agent.key`, creates an
   empty `~/.choir/reviewers` pool, writes and loads the
   `com.choir.node` LaunchAgent serving the named repo. Idempotent:
   re-run after any rebuild.
3. Add reviewer names to `~/.choir/reviewers`, one per line. Re-read on
   every draw, so no restart. An empty pool means review requests come
   back with `assignment_error` and stay unassigned (never approved).

Verify the daemon:

```sh
launchctl print gui/$(id -u)/com.choir.node | head       # loaded, KeepAlive
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8417/api/view   # 401
curl -s -u choir:$(cut -d: -f2 ~/.choir/auth) http://127.0.0.1:8417/api/view
tail ~/.choir/node.log
```

The 401 is the point of checklist item 1: the rehearsal node ran with
no auth at all.

## Flip day

1. The canonical repo already exists: the installer put `--create` in
   the plist, and creation is idempotent (an existing repo is skipped).
   Creation is also what installs the `pre-receive` hook that turns
   pushes into signed ops, so a repo made any other way will not be
   sequenced.
2. `sh scripts/flip/push_canonical.sh [port] [owner/repo.git]` — pushes
   `--all` **and** `--tags`.

   Checklist item 3, measured: `git push --all` sent 1 branch and **0
   tags**; the view's `refs` gained the tag only after the second push.
   Both are required or release history is silently lost.
3. Verify every ref is in the log as a sequenced op:

```sh
curl -s -u choir:$(cut -d: -f2 ~/.choir/auth) http://127.0.0.1:8417/api/view
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

## Still open

- Push-cert `allowed_signers` is written at startup only, so registering
  a *signing* key still needs a restart (the trusted-keys file does hot
  reload).
- No TLS, so the bind stays loopback (invariant 9 refuses anything else
  without a cert). Remote access is an SSH tunnel.
- Readers more than 100k entries behind the `/api/log` window have no
  resync path.
