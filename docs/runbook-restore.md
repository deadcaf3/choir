# Runbook: restoring a node from a backup

You have a backup directory written by `scripts/flip/pull_backup.sh` and no
working node. This turns the first into the second, and refuses to say it
worked until the restored node has accepted a write.

First, satisfy yourself the backup is one you can restore *from*. That is
a different claim from "a backup was written", and it is the one that
matters here:

```bash
choir backup verify ~/choir-backup
```

Every check it makes is local: it opens no connection and reads nothing
from the node, because a backup you can only verify by asking the thing
it is a copy of is not a backup. It exits 0 when the copy is restorable
and 1 when it is not, and warnings — no attestation, no ACL in the
policy archive — are not failures.

Then restore:

```bash
choir backup restore ~/choir-backup /srv/choir-repos
```

Expect it to stop the first time, and re-run it after supplying what it
asked for: a target root holding this backup's log unchanged is treated
as its own earlier placement and resumed, not refused. It stops on the
things a backup cannot contain, and those are yours to supply.

## Exit codes

| Exit | Meaning |
|---|---|
| `0` | Restored, replayed, attested and written to. The root is ready for your supervisor. |
| `1` | A check failed. Nothing was left half-done that the message does not name. |
| `2` | Usage. |
| `3` | An operator decision is required — a secret, see below. Files are placed; nothing else is pending. |

## What a backup does not contain

By design, and checked on both sides: `pull_backup.sh` refuses to pull a
secret and this refuses to restore one. So three things are missing, and
all three are yours.

### 1. The node's signing key — the one that matters

`node.key` is the identity that signed every git-derived op in the log.
The backup carries only `node.fingerprint`, the public hash of it, which
is what makes the loss *visible* instead of silent: a daemon whose key
file is absent mints a fresh one and appends happily, and both identities
are individually valid, so nothing downstream marks the seam.

You have two options and the restore will not choose for you.

**(a) You still have the key.** Put it back, then re-run:

```bash
install -m 600 /path/to/your/held/node.key /srv/choir-repos/.choir/node.key
```

This is the good outcome. The log keeps one author across the restore.

**(b) The key is gone.** Then the log changes author at this point, and
that is a fact about the log, not a formality:

```bash
rm /srv/choir-repos/.choir/node.fingerprint
```

The daemon mints a new key on the next start and pins it. The re-run
does not put the fingerprint back, and it does not stop again: it names
the seq the seam falls at and carries on. Every op after the seam is
signed by a different actor from every op before it. Nothing that
already verified stops verifying — historical signatures are still good
— but from here on, "the node" is a different key, and anyone holding
the old fingerprint should be told.

Keep the node key somewhere the node host's disk failure cannot reach.
It is the one piece of state with no second copy anywhere, because every
mechanism in this repository is built to stop it having one.

#### Where it is kept, and how to put it there (D70)

A Keychain secure note on the operator's laptop, holding the base64 of
the raw 32 bytes. Both halves are done by hand in Keychain Access: the
`security` CLI is not an option here, because its only working form
puts the secret in argv where `ps` can read it.

**Escrow, once, from the host that holds the key.** The base64 is 44
characters and lands in terminal scrollback, so do this in a window you
are willing to close afterwards:

```bash
ssh <choir-user>@<SERVER_IP> 'base64 < ~/.choir/repos/.choir/node.key'
```

Copy that line, then Keychain Access, File, New Secure Note Item. Name
it for the fingerprint it belongs to, `choir node key 1e-...`, so a
restore can tell two identities apart. Paste, save, close the terminal
window.

**Retrieval, at restore time.** Open the note, copy its contents, and
decode through the clipboard rather than the command line:

```bash
tmp=$(mktemp)
pbpaste | base64 -d > "$tmp"
if [ "$(wc -c < "$tmp")" -eq 32 ]; then
  install -m 600 "$tmp" /srv/choir-repos/.choir/node.key && echo "key installed"
else
  echo "REFUSED: clipboard decoded to $(wc -c < "$tmp") bytes, not 32"
fi
rm -f "$tmp"
```

Decode to a temporary file and install only on 32 bytes. Writing the
redirect straight at `node.key` truncates it before the decode is known
to have worked, so a clipboard holding anything else -- the command you
just copied, most likely -- leaves a zero-byte key behind. The daemon
does catch that, `node.key must be 32 bytes`, but it catches it three
steps later and the original is gone by then.

Then re-run the restore. It compares the key against
`node.fingerprint` and refuses a mismatch, so a wrong note is caught
rather than appended.

Two gaps this leaves open, recorded rather than solved. A single laptop
is a single point of failure unless Time Machine covers it, and iCloud
Keychain would close that only by putting the node identity on a third
party's servers, which nobody has decided. And a personal Keychain
cannot be reached by the second operator that
`docs/private-beta-runbook.md` receipt 4 asks to rehearse a restore.

### 2. Auth tokens

```bash
printf '<operator>:%s\n' "$(openssl rand -hex 32)" > /srv/choir-repos/.choir/auth
chmod 600 /srv/choir-repos/.choir/auth
```

Mint a new one rather than reusing the old. The old token was last seen
on a host you are restoring away from, and you are restoring because
something happened to that host.

Re-issue per-user credentials the same way, and remember that the ACL
file travels in the backup while the tokens it grades do not — a restored
`acl` naming users whose tokens no longer exist is the correct state, not
a broken one.

The rehearsal credential's username must match an operator entry in the
restored ACL with ownership of at least one restored repository. Otherwise
the canary push is correctly refused.

### 3. TLS material

Certificate and key are not in the backup. The node always binds loopback
for the private beta. Restore TLS only at the reverse proxy after the
loopback rehearsal passes.

## The ordering rule, and why the script is strict about it

**Git objects go in before the daemon starts, never after.**

Startup reconciliation compares the log against what each repo holds. A
ref naming a commit the repo does not have is classed as unbackable, and
the repair for that is to *append a compensating op* so the log agrees
with git. Start a restored node against empty repos and it will do
exactly that, correctly, for every ref you were restoring — and the
result is a log that has retracted your ref state in signed ops.

The script therefore unbundles first and boots second, and it treats any
`choir: retracted` line on that first start as a failure. If you see it:
the restored log has been appended to and is no longer your backup. Start
again from the backup into a clean root.

Two consequences worth knowing separately:

- A repo restored from a bundle has **no `pre-receive` hook**. It serves
  normally, and every push into it bypasses the sequencer. The daemon now
  adopts existing repos named by `--create` at startup — installing the
  hook and re-pointing `gpg.ssh.allowedSignersFile`, which was written as
  an absolute path into whatever root existed when the repo was made. So
  **list every repo in `--create`**, restored or not. A repo you forget is
  a repo whose pushes are invisible to the log.
- `repos.list` is in the backup precisely because of this. A restore
  missing it serves only the default repo and reconciliation retracts the
  rest.

## What the restore proves before exiting 0

Not "the files copied". In order:

1. The backup's supported format, sequence, parent chain, and recomputed
   entry hashes verify through the final record.
2. The three files a node cannot boot or serve restored refs without are
   present — `keys`, `reviewers`, `repos.list` — and no secret is. The
   other six beta policy files are named one by one when the backup lacks
   them, and the restored node starts without them, enforcing less than
   the node it replaces. `protected-refs` is one of those six: a backup
   from a node that protects no ref restores into a node that protects
   no ref, review gate and all.
3. Every repo in `repos.list` has a bundle.
4. The target root holds no log — an existing one is never overwritten.
5. The node boots, replays, and retracts nothing.
6. The view it serves matches the D25 ref attestation, if the backup
   carried one — `refs.snapshot`, which both backup legs now pull. Bytes can arrive perfectly and still replay into a
   different view; this is the only check that sees that.
7. A real `git push` over HTTP lands: through `http-backend`, the
   `pre-receive` hook, the sequencer, and into the log. Anything less
   does not distinguish a node from a directory of files.
8. The appended entry's `parent` is the head the node served before the
   push — the log's own statement that it was folded to exactly the point
   its bytes imply.
9. The backup is a byte-exact prefix of the restored log. A restore that
   rewrote history passes every other check here.

## After it exits 0

The canary ref is left in place, named `refs/heads/restore-canary-<unix>`.
It is evidence that this root took a write and when. Delete it whenever
you no longer want it:

```bash
git push <node-url>/<repo> :refs/heads/restore-canary-<unix>
```

Then render the hardened service with
`scripts/flip/render_private_beta_service.sh`. The rehearsal used the same
ACL, scope, review, read-only browser, logging, limit, and quota policy as
the private-beta manifest. Re-point the off-host backup job at the new host
if it moved.

Finally, pull a backup *from the restored node* before trusting it. The
node you just restored has no offsite copy of its own until you do, and
the first thing that goes wrong after a restore is usually the backup
that was never re-armed:

```bash
./choirctl pull-backup          # still shell: it ssh's to the node host
choir backup verify ~/choir-backup
```

`pull-backup` is the one leg still in shell, because it ssh's to a
specific host. Both the checks and the restore are `choir` commands and
ship in the release.
