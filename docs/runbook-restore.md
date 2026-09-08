# Runbook: restoring a node from a backup

Turn a backup written by `scripts/flip/pull_backup.sh` into a node.

First confirm the backup can be restored *from*:

```bash
choir backup verify ~/choir-backup
```

Every check is local; it opens no connection. Exit 0 means restorable, 1
means not. Warnings (no attestation, no ACL) are not failures.

Then restore:

```bash
choir backup restore ~/choir-backup /srv/choir-repos
```

It exits 0 only once the restored node has accepted a write. Expect it to
stop the first time; supply what it asks for and re-run into the same root,
which is resumed.

## Exit codes

| Exit | Meaning |
|---|---|
| `0` | Restored, replayed, attested and written to. Ready for your supervisor. |
| `1` | A check failed. The message names anything left half-done. |
| `2` | Usage. |
| `3` | An operator decision is required: a secret, below. Files are placed. |

## What a backup does not contain

A backup carries the operation log, `node.fingerprint`, the ref attestation,
one git bundle per repository, `acl`, `repos.list`, the review policy and
adjudication files, and the private-beta manifest. It never carries a
secret, so three things are yours.

### 1. The node's signing key

`node.key` signed every git-derived op. The backup carries only
`node.fingerprint`, its public hash.

**(a) You still have the key.** Put it back, then re-run:

```bash
install -m 600 /path/to/your/held/node.key /srv/choir-repos/.choir/node.key
```

**(b) The key is gone.** Drop the fingerprint, then re-run. The daemon
mints a new key, the re-run names the seq the seam falls at, historical
signatures still verify, and holders of the old fingerprint should be told.

```bash
rm /srv/choir-repos/.choir/node.fingerprint
```

Keep the node key where the node host's disk failure cannot reach it.

#### Where it is kept, and how to put it there (D70)

Keep the base64 of the raw 32 bytes in a Keychain secure note, both halves
by hand in Keychain Access, because the `security` CLI puts the secret in
argv.

**Escrow, once, from the host holding the key.** The base64 lands in
scrollback, so use a window you will close:

```bash
ssh <choir-user>@<SERVER_IP> 'base64 < ~/.choir/repos/.choir/node.key'
```

Keychain Access, File, New Secure Note Item. Name it for the fingerprint,
`choir node key 1e-...`. Paste, save, close the window.

**Retrieval, at restore time.** Decode through the clipboard:

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

Install only on 32 bytes. Then re-run the restore, which refuses a key that
mismatches `node.fingerprint`.

Recorded gap: the second operator in receipt 4 of
`docs/private-beta-runbook.md` cannot reach a personal Keychain.

### 2. Auth tokens

```bash
printf '<operator>:%s\n' "$(openssl rand -hex 32)" > /srv/choir-repos/.choir/auth
chmod 600 /srv/choir-repos/.choir/auth
```

Mint new tokens rather than reusing old ones. The `acl` travels in the
backup; the tokens it grades do not. The rehearsal username must match an
operator ACL entry owning at least one restored repository, or the canary
push is refused.

### 3. TLS material

Certificate and key are yours. Restore TLS at the reverse proxy after the
loopback rehearsal passes.

## The ordering rule

**Git objects go in before the daemon starts, never after.**

Startup reconciliation compares the log against each repo. A ref naming a
commit the repo lacks is repaired with a compensating op: a restored node
started against empty repos retracts your ref state.

The script unbundles first and boots second, and treats any
`choir: retracted` line on that first start as a failure. If you see it,
start again into a clean root.

> [!CAUTION]
> A repo restored from a bundle has no `pre-receive` hook. The daemon adopts
> repos named by `--create` at startup, installing the hook and re-pointing
> `gpg.ssh.allowedSignersFile`. List every repo in `--create`.

`repos.list` is in the backup for the same reason: without it a restore
serves only the default repo and reconciliation retracts the rest.

## What the restore proves before exiting 0

1. Format, sequence, parent chain and recomputed hashes verify to the end.
2. `keys`, `reviewers` and `repos.list` are present and no secret is. The
   other six policy files are named individually when absent.
3. Every repo in `repos.list` has a bundle.
4. The target root holds no log; an existing one is never overwritten.
5. The node boots, replays, and retracts nothing.
6. The served view matches the D25 ref attestation `refs.snapshot`, when
   present.
7. A real `git push` over HTTP lands through `http-backend`, the
   `pre-receive` hook, the sequencer and the log.
8. The appended entry's `parent` is the head served before the push.
9. The backup is a byte-exact prefix of the restored log.

## After it exits 0

The canary ref `refs/heads/restore-canary-<unix>` stays as evidence. Delete
it when done:

```bash
git push <node-url>/<repo> :refs/heads/restore-canary-<unix>
```

Render the hardened service with
`scripts/flip/render_private_beta_service.sh`. Re-point the off-host backup
job if the host moved.

Pull a backup from the restored node before trusting it:

```bash
./choirctl pull-backup          # still shell: it ssh's to the node host
choir backup verify ~/choir-backup
```
