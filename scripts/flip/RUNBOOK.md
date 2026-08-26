# Canonical-node flip runbook

Nothing below needs memorizing: `./choirctl` with no arguments
lists every command. The long forms are kept here so the procedure is
auditable, but `choirctl install`, `choirctl status`, `choirctl sync`,
and `choirctl logs` are the usual operator path.

This is the D20 operator procedure. The flip makes the choir node canonical
and Forgejo a follower (D21 single-canonical invariant — one direction,
never dual-write). The
git-bundle cron on the mirror VM continues as insurance, cut from the
node's own bare repos since the flip (see "The bundle cron" below).

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
   re-run after any rebuild. The served repos live in
   `~/.choir/repos.list` (one `owner/name.git` per line, `#` comments
   skipped); the optional repo argument seeds the list on first install
   and is appended on a later run if missing. Adding a repo is an
   appended line followed by a reinstall on the node host — `--create`
   is idempotent, so existing repos are untouched, and an empty list
   refuses to render rather than starting a node that serves nothing.
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

To close replay (D26), create `~/.choir/scope-required.enabled` (mode 0600)
and rerun the installer. The node then admits only ops whose signed payload
carries an `OpScope` naming this node's log and a head still in its window,
refusing the rest as `scope_required`, `foreign_scope`, or `stale_scope` —
so a captured op no longer replays after an ABA ref move, and an op signed
for another node's log is refused by name. This is a client-compatibility
step, not just a flag: every submitter must sign a scope from then on.
`choir submit` (and every CLI verb, MCP included) and the bridge already
do; anything hand-rolling `/api/submit` with `curl` must first read
`log.node` and `log.head` from `GET /api/view` and sign them into the op.
`/api/view.log.scope_required` reports whether the gate is on. Removing
the marker and reinstalling returns to transport containment (loopback
bind plus the auth token), which is D26's documented fallback, not a
misconfiguration.

Verify the daemon:

```sh
launchctl print gui/$(id -u)/com.choir.node | head       # loaded, KeepAlive
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8417/api/view   # 401
target/release/choir --auth-file ~/.choir/auth --auth-user choir view http://127.0.0.1:8417
tail ~/.choir/node.log
```

The unauthenticated request must return 401. A 200 means the real node was
started without the required auth file.

`choirctl status` also answers two questions the process table cannot:

- `build:` is the commit the running binary was built from, stamped in by
  the installer. It is compared against the checkout's HEAD, so a rebuild
  that never reached the running process reads `STALE` instead of reading
  like success. `UNSTAMPED` means the binary was built outside the
  installer; rebuild through it rather than trusting the path.
- `lag:` is what the sequencer measured on the traffic this node actually
  served: `durable` percentiles include the durability barrier and are
  what a submitter waits out, `decision` is the append alone (the Phase-0
  gate as originally written). Both are since process start and are not
  replayed, so a restart resets them. Any op at or past the gate is
  appended to `~/.choir/repos/.choir/lag.jsonl` as one JSON object naming
  the `seq` it happened to, and `status` says how many there were.

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
4. Repoint the follower: from here on use `./choirctl sync`,
   which pushes to the node first and the Forgejo mirror second, and
   stops before the mirror if the canonical push failed — so the
   follower can never get ahead of the node. `push` and `mirror` remain
   available separately. Do not add a push from Forgejo back to the
   node (D21 single-canonical: one direction, never dual-write).

## Rollback

The flip is reversible and cheap: the Forgejo mirror still holds every
ref, and the bundle cron holds a daily copy.

```sh
./choirctl stop         # unload it for this boot only
./choirctl uninstall    # unload it and remove the LaunchAgent
```

`stop` alone is not permanent: the plist stays in `~/Library/LaunchAgents`,
so launchd starts the daemon again at the next login. `uninstall` removes
it. Neither touches `~/.choir`.

Nothing is lost by stopping: `~/.choir/repos` and the op log at
`~/.choir/repos/.choir/ops.jsonl` persist, and the daemon replays the
log on restart (rehearsed: refs and workspaces survived a kill).

## The bundle cron

The daily insurance bundle on the mirror VM (03:17, `# choir-bundle`
tag in `crontab -l`) is cut from the node's own bare repos, one bundle
per line of `~/.choir/repos.list`:

```
17 3 * * * for r in $(grep -v "^#" $HOME/.choir/repos.list); do b=$(basename $r .git); git -C $HOME/.choir/repos/$r bundle create $HOME/bundles/$b-$(date +\%F).bundle --all && find $HOME/bundles -name "$b-*.bundle" -mtime +13 -delete; done 2>>$HOME/bundles/cron.err # choir-bundle
```

It used to fetch from Forgejo and bundle that clone, which quietly made
the insurance depend on the follower: a dead or stale Forgejo meant
stale bundles of a repo whose canonical copy was healthy next to it.
Bundling the bare repos directly removes Forgejo, the `~/choir-src`
clone, and the fetch step from the failure chain — the bundle can now
only be as stale as the node itself. Errors append to
`~/bundles/cron.err` instead of `/dev/null`; a silently failing backup
is the failure mode this repository keeps re-learning about. Pruning
keeps roughly two weeks per repo and is gated on the day's bundle
having been created (`&&`): a repo whose bundling starts failing stops
pruning too, so a persistent failure leaves the old bundles in place
instead of quietly eroding them while the error sits in `cron.err`.

## Restoring the op log

Stopping is safe; losing the disk was not. The bundle cron copies refs
and objects, and `ops.jsonl` is neither — it is not a git object, so no
bundle has ever contained it. `./choirctl sync` now copies it
to `~/choir-oplog/ops.jsonl` on the mirror VM and refuses the run if the
far checksum disagrees, so a truncated copy fails loudly instead of
sitting there looking like a backup.

The log alone is not enough, and finding that out is what the rehearsal
was for. A node rebuilt from `ops.jsonl` only **refuses to boot** —
`review assignment policy needs --reviewers-file` — because the node's
policy is configuration, not sequenced fact, and so lives outside the
log. `sync` therefore also ships five policy files to
`~/choir-oplog/policy/` as one tar.

What travels: the log, `node.fingerprint`, the six policy files —
`keys`, `reviewers`, `protected-refs`, `newcomer-audit.jsonl`,
`newcomer-adjudications.jsonl`, `repos.list` — and, since the host
move, one full `--all` bundle per served repo under `repos/`, verified
complete on arrival and re-pulled only when the refs hash moves. The
bundles are what make the git *objects* restorable off-host; the log
alone only proves which commits the refs named.

What does not, and must not: the signing key, `auth`, and any
`*.key`/`*.pem`. A backup carrying the key would let whoever holds the
backup keep signing as this node, and one carrying `auth` would ship a
bearer token. **`auth` is re-issued on restore, not recovered** — pick a
fresh token, it is not derived from anything.

```sh
scp choir@<SERVER_IP>:choir-oplog/ops.jsonl        ~/.choir/repos/.choir/ops.jsonl
scp choir@<SERVER_IP>:choir-oplog/node.fingerprint ~/.choir/repos/.choir/node.fingerprint
scp -r choir@<SERVER_IP>:choir-oplog/policy        ~/.choir/restored-policy
# then write a fresh ~/.choir/auth as user:token before starting
```

Rehearsed 2026-08-11: a node started on that set with `--keys-file`,
`--reviewers-file`, `--protected-refs`, `--newcomer-audit` and
`--newcomer-adjudications` served a view **byte-identical to the live
node**, including all 44 reviews, 3 bindings, 2 refs and both the D23
and D24 metric blocks. Omitting the two newcomer flags is not an error
but leaves those blocks reading `configured: false`.

On a host that still holds the original key this starts and replays. On
any other host it **refuses to start**, naming the pinned id, because the
restored fingerprint will not match a freshly generated key (verified:
exit 1). That refusal is the intended outcome, not a failure: continuing
would append to a signed chain under a new identity. Deleting
`node.fingerprint` overrides it, and means accepting that the log changes
author at that point.

Restoring refs is separate and unchanged: the Forgejo mirror holds every
ref, and the daily bundle holds a copy.

## Host move: the canonical node leaves the laptop

The D20 follow-on, rehearsed on the VM before it was done for real. The
node moves to the mirror VM (built there by hand: rustup + a temporary
swapfile — the host has under a gigabyte of RAM); the laptop becomes a
client over an SSH tunnel, and the backup direction inverts: the laptop
pulls (`choirctl pull-backup`), because a live log and its only copy on
one disk is not a backup.

### What a bare Linux host needs first

Two prerequisites are not obvious and neither names itself in the
failure it causes. Both were hit building a node on a fresh Debian 13
host.

`libssl-dev` must be installed before the first build. Without it
`openssl-sys` fails with "Could not find directory of OpenSSL
installation", which reads as a toolchain problem and is not one. The
full apt set is `git build-essential pkg-config curl certbot libssl-dev`,
plus `zsh` if the operator scripts written for macOS are to be run on
the host, and `rustup` — which Debian packages, so the toolchain needs
no pipe-to-shell installer. `rust-toolchain.toml` pins the version the
first `cargo` invocation then fetches.

**The node must not run as a cloud-provisioned login account.** On GCE
with OS Login the account has a dynamic uid that `systemd-logind`
cannot resolve, so `loginctl enable-linger` fails with "No such
process" and writing `/var/lib/systemd/linger/<user>` by hand does not
help either. Without linger the user unit stops at logout, which means
the node dies when the operator's ssh session ends and does not come
back at boot. Create an ordinary unprivileged local account for it
(`useradd -m choir`), enable linger on that, and install as it. That
account is also the better home for the node's identity: it is stable
across IAM changes, which the login account is not.

A non-interactive `ssh <host> --command` also has no session bus, so
`systemctl --user` fails there with a `DBUS_SESSION_BUS_ADDRESS`
complaint even once linger is on. Export both when installing over ssh:

```sh
sudo -u choir env HOME=/home/choir \
  XDG_RUNTIME_DIR=/run/user/$(id -u choir) \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u choir)/bus \
  sh ~choir/choir-build/scripts/flip/install_node_linux.sh 8417 <owner/repo>.git ~choir/bin ~choir/bin
```

Verify with `systemctl --user is-enabled choir-node` under the same
environment: `enabled` plus `Linger=yes` is what survives a reboot.
`active` alone does not.

Rebuilding the VM node after a landing (`choirctl install` prints this
too): check `/proc/swaps` still lists the 3 GiB swapfile, then on the VM

```
git -C ~/choir-build fetch ~/.choir/repos/choir/choir.git main && git -C ~/choir-build merge --ff-only FETCH_HEAD
CHOIR_GIT_HEAD=$(git -C ~/choir-build rev-parse HEAD) cargo build --release --manifest-path ~/choir-build/Cargo.toml -p choir-node -p choir-cli
sh ~/choir-build/scripts/flip/install_node_linux.sh 8417 '' ~/bin ~/choir-build/target/release
```

The fetch source is the node's own bare repo, so the build is always of
what the node itself serves as canonical. `CHOIR_GIT_HEAD` feeds the
build stamp's sound path (`build.rs` source 1) — the git fallback reads
right in a clean checkout, but the env var is the one cargo is
guaranteed to rebuild on, and it is why `choirctl status` can compare
`build:` against HEAD honestly.

**Order matters, and the rehearsal is why.** A node booting over a
restored log with an empty git repo does not wait: `reconcile_refs`
appends a compensating retraction for every ref whose commit git does
not hold ("the log named a commit this repo does not have"). Rehearsed:
an unprotected ref was retracted from the log copy on first boot. So the
repo is seeded before the node ever sees the restored log:

1. On the laptop: `git --git-dir ~/.choir/repos/choir/choir.git bundle
   create node-repo.bundle --all`, ship it to the VM. Stop the laptop
   node (`choirctl uninstall` — `stop` alone returns at next login).
2. On the VM: restore policy files from the backup into `~/.choir`
   (keys, reviewers, protected-refs, both newcomer files, the
   `review-gates.enabled` marker), **not** the log yet, **never**
   `auth`/`*.key`/`*.pem`. Run `scripts/flip/install_node_linux.sh` —
   first boot creates the repo and the pre-receive hook over an empty
   log and pins a fresh key.
3. Stop the unit. Seed the bare repo: `git --git-dir
   ~/.choir/repos/choir/choir.git fetch <bundle>
   '+refs/heads/*:refs/heads/*'` (fetch writes refs directly and runs no
   hooks). Copy the final `ops.jsonl` into `~/.choir/repos/.choir/`,
   keeping the fingerprint the first boot pinned. Start the unit;
   reconcile must be silent.
4. On the laptop: create `~/.choir/node-remote`. From then on `choirctl
   status|push|sync|url` tunnel automatically, `install|stop|uninstall`
   refuse, `push_mirror.sh` refuses (its oplog leg would clobber the
   historical backup), and `sync` = canonical push through the tunnel,
   follower fed on-box (node repo -> Forgejo, never ahead), then
   `pull_backup.sh`.

**The identity changes at the move, on purpose.** The signing key never
travels (same rule as every backup), so the moved node mints and pins a
fresh key and the log changes author at that seq. The old identity's pin
travels with the historical backup; the refusal it would produce on any
other host is the intended behaviour, and PHASE0 records the seq where
the seam sits. Old ops replay fine: replay is a pure fold, policy runs
at admission only.

The rehearsal also proved the moved node re-registers its own fresh key
(a push under the new identity was sequenced) and that the review gate
survives the restore (an unreviewed push to protected `main` was still
refused).

## Going public: the TLS bind

The node goes from tunnel-only to publicly reachable in three operator
steps, all on the node host, all reversible by deleting one marker file:

1. Pick the challenge method, because it decides whether the origin
   address becomes public. Writing `~/.choir/cloudflare.ini` at 0600,
   holding `dns_cloudflare_api_token = <token>` for a token scoped to
   Zone:DNS:Edit on that zone alone, selects DNS-01: no inbound port, and
   the record can stay proxied throughout. Without that file the script
   falls back to HTTP-01 on port 80, which needs the firewall open to 80
   permanently (renewals rebind it every ~60 days, and nothing else ever
   serves on 80) and needs the record unproxied for issuance and for every
   renewal. Unproxying publishes the origin IP, passive-DNS services
   archive it permanently, and re-enabling the proxy afterwards does not
   take it back. Behind a proxying CDN, prefer DNS-01.
2. `sh ~/choir-build/scripts/flip/setup_tls.sh <domain> [port]` issues
   the Let's Encrypt cert (account registered without an email, per the
   standing privacy rule), installs a deploy hook that re-projects the
   pair to `~/.choir/tls/` at 0600 and restarts the unit on every
   renewal, runs that hook once now so it is proven today rather than at
   the first renewal, writes `~/.choir/tls.enabled` (two lines: cert
   path, key path), and writes `~/.choir-public-url` with the
   certificate-valid API base. An existing TLS node can add only the
   route marker with `sh scripts/flip/configure_public_url.sh <domain>
   [port]`; that does not renew or restart anything.
3. Re-run the installer. The marker flips the rendered unit to
   `--bind 0.0.0.0 --tls-cert ... --tls-key ...`; the same one-way
   marker discipline as the review and scope gates, so every later
   reinstall keeps the public bind. A marker naming unreadable files
   refuses to render (fail closed) — and the node itself refuses a
   non-loopback bind without TLS (invariant 9), so there is no
   configuration in which plaintext basic auth crosses a real network.

After TLS is enabled, node-local and remote CLI calls use the same verified
route:

```sh
BASE=$(cat ~/.choir-public-url)
~/bin/choir --auth-file ~/.choir/auth --auth-user <user> view "$BASE"
```

Do not use `https://127.0.0.1` with `-k`. It is useful only as a narrow
reachability diagnostic because `-k` disables the server identity check.
The public-name route was chosen over a permanent loopback exception so
normal commands keep certificate verification and do not depend on an SSH
tunnel.

Access for a new user is one appended `user:token` line in
`~/.choir/auth` (0600; mint the token with `openssl rand -hex 32`,
hand it over out of band). Credentials are per-user, so one person can
be revoked without disturbing anyone else.

**That line authenticates and nothing more.** Write the matching grant
into `~/.choir/acl` (0600) in the same breath, one line per repository:

```
<user>   <owner/repo>   read      # clone and fetch
<user>   <owner/repo>   write     # and push, provision, submit
```

The file's existence is its own marker: create `~/.choir/acl`, re-run the
installer, and both supervisors render `--acl-file`. No enable-flag, because
an empty ACL and an absent one mean opposite things and a separate marker
could disagree with the file it guards. Every install prints which way it
went — `per-repository authorization enabled (...)` or `no ~/.choir/acl:
every authenticated credential reaches every repository` — so the posture is
in the install output, not in anyone's memory.

What the flag does:
anything ungranted is refused, and a repository the user cannot read
answers `404` rather than `403`, so a denial never confirms it exists.
Grants reload on mtime, so appending a line needs no restart, and a
malformed edit keeps the previous table rather than revoking anyone.
`--acl-file` requires `--auth-file`; the node refuses the combination
the other way round.

**Without that flag, one credential reaches every repository** — clone
anything, push to any unprotected ref, provision workspaces anywhere.
The node prints a line saying so on every start that lacks an ACL. What
still holds either way is everything keyed to the ref rather than the
identity: protected refs, the review requirement, and the sequencer's
ordering — so a new user cannot land on a gated `main` without a
node-assigned review reaching approval weight two.

With the flag set, the inventory is gated too: `/api/view`, `/api/reviews`
and the browser page show only the repositories that credential may
read. The node-wide sections of the view need `@node auditor`, and a
review the holder was assigned to still reaches them wherever it lives.
Without the flag none of that applies — one credential still sees
everything, which is the line the node prints at startup.

After the flip, the operator's own tooling must switch schemes too: a
plaintext `http://` through the tunnel now hits a TLS listener and gets
nothing. Write the public base to the untracked `~/.choir-public-url`
on the operator machine (e.g. `https://<domain>:<port>`) and every
`choirctl` HTTP leg — status, review, verdict, the canonical push —
goes straight to the public name with a verifying cert; the HTTP
tunnel stops being load-bearing (ssh legs never used it). The hairpin
from the node host to its own public name works, so on-node CLI calls
use the same base. Deleting the file restores the pre-flip
tunnel-and-plaintext behaviour, matching a marker rollback on the node.

## Still open

- A node without a persisted log returns `log_evicted` when a reader falls
- A node without a persisted log returns `log_evicted` when a reader falls
  behind its in-memory window. The reader can resume at `window_base`, but
  cannot verify continuity across the gap. See the [sync contract](../../SYNC.md).
