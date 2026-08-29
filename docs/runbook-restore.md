# Runbook: restoring a node from a backup

Turn a backup directory written by `scripts/flip/pull_backup.sh` into a node:

```bash
./choirctl restore-from-backup ~/choir-backup /srv/choir-repos
```

It exits 0 only once the restored node has accepted a write. Expect it to stop
the first time; supply what it asks for and re-run into the same root, which is
resumed rather than refused.

## Exit codes

| Exit | Meaning |
|---|---|
| `0` | Restored, replayed, attested and written to. The root is ready for your supervisor. |
| `1` | A check failed. The message names anything left half-done. |
| `2` | Usage. |
| `3` | An operator decision is required: a secret, see below. Files are placed; nothing else is pending. |

## What a backup does not contain

A backup carries the operation log, `node.fingerprint`, the ref attestation,
one git bundle per repository, `acl`, `repos.list`, the review policy and
adjudication files, and the private-beta manifest. `pull_backup.sh` refuses to
pull a secret and this refuses to restore one, so three things are yours.

### 1. The node's signing key

`node.key` signed every git-derived op in the log. The backup carries only
`node.fingerprint`, its public hash, which makes a loss visible: a daemon with
no key file mints a fresh one and appends, and both identities verify.

**(a) You still have the key.** Put it back, then re-run. The log keeps one
author across the restore.

```bash
install -m 600 /path/to/your/held/node.key /srv/choir-repos/.choir/node.key
```

**(b) The key is gone.** Drop the fingerprint, then re-run: the daemon mints a
new key on the next start, and the re-run names the seq the seam falls at. Ops
after the seam carry a different actor, historical signatures still verify, and
anyone holding the old fingerprint should be told.

```bash
rm /srv/choir-repos/.choir/node.fingerprint
```

Keep the node key where the node host's disk failure cannot reach it.

#### Where it is kept, and how to put it there (D70)

Keep the base64 of the raw 32 bytes in a Keychain secure note, both halves done
by hand in Keychain Access: the `security` CLI puts the secret in argv.

**Escrow, once, from the host that holds the key.** The 44-character base64
lands in terminal scrollback, so use a window you will close afterwards:

```bash
ssh <choir-user>@<SERVER_IP> 'base64 < ~/.choir/repos/.choir/node.key'
```

Copy that line, then Keychain Access, File, New Secure Note Item. Name it for
the fingerprint it belongs to, `choir node key 1e-...`, so a restore can tell
two identities apart. Paste, save, close the terminal window.

**Retrieval, at restore time.** Open the note, copy its contents, and decode
through the clipboard rather than the command line:

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

Decode to a temporary file and install only on 32 bytes; redirecting straight
at `node.key` truncates it before the decode is known to have worked. Then
re-run the restore, which refuses a key that mismatches `node.fingerprint`.

Recorded gap: the second operator asked to rehearse a restore in
`docs/private-beta-runbook.md` receipt 4 cannot reach a personal Keychain.

### 2. Auth tokens

```bash
printf '<operator>:%s\n' "$(openssl rand -hex 32)" > /srv/choir-repos/.choir/auth
chmod 600 /srv/choir-repos/.choir/auth
```

Mint a new token rather than reusing the old, which was last seen on the host
you are restoring away from, and re-issue per-user credentials the same way.
The `acl` travels in the backup while the tokens it grades do not, so a restored
`acl` naming users whose tokens are gone is correct. The rehearsal credential's
username must match an operator entry in that ACL owning at least one restored
repository, or the canary push is refused.

### 3. TLS material

Certificate and key are yours. The node binds loopback for the private beta, so
restore TLS at the reverse proxy after the loopback rehearsal passes.

## The ordering rule

**Git objects go in before the daemon starts, never after.**

Startup reconciliation compares the log against what each repo holds. A ref
naming a commit the repo lacks is unbackable, and the repair is a compensating
op: start a restored node against empty repos and it retracts your ref state.

The script therefore unbundles first and boots second, and treats any
`choir: retracted` line on that first start as a failure. If you see it, start
again from the backup into a clean root.

> [!CAUTION]
> A repo restored from a bundle has no `pre-receive` hook, so every push into
> it bypasses the sequencer. The daemon adopts existing repos named by
> `--create` at startup, installing the hook and re-pointing
> `gpg.ssh.allowedSignersFile`. List every repo in `--create`, restored or not.

`repos.list` is in the backup for the same reason: a restore missing it serves
only the default repo, and reconciliation retracts the rest.

## What the restore proves before exiting 0

In order:

1. The backup's supported format, sequence, parent chain, and recomputed entry
   hashes verify through the final record.
2. `keys`, `reviewers` and `repos.list` are present and no secret is. The other
   six beta policy files, `protected-refs` among them, are named individually
   when absent, and the restored node starts without them, enforcing less than
   the node it replaces.
3. Every repo in `repos.list` has a bundle.
4. The target root holds no log; an existing one is never overwritten.
5. The node boots, replays, and retracts nothing.
6. The view it serves matches the D25 ref attestation `refs.snapshot`, when the
   backup carried one: the only check that sees bytes arrive intact and replay
   into a different view.
7. A real `git push` over HTTP lands through `http-backend`, the `pre-receive`
   hook, the sequencer, and into the log.
8. The appended entry's `parent` is the head the node served before the push.
9. The backup is a byte-exact prefix of the restored log, which catches a
   restore that rewrote history.

## After it exits 0

The canary ref `refs/heads/restore-canary-<unix>` stays as evidence that this
root took a write and when. Delete it when you no longer want it:

```bash
git push <node-url>/<repo> :refs/heads/restore-canary-<unix>
```

Then render the hardened service with
`scripts/flip/render_private_beta_service.sh`. The rehearsal used the same ACL,
scope, review, read-only browser, logging, limit, and quota policy as the
private-beta manifest. Re-point the off-host backup job if the host moved.

Finally, pull a backup from the restored node before trusting it. It has no
offsite copy of its own until you do:

```bash
./choirctl pull-backup
./choirctl verify-backup
```
