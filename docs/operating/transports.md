# Transports and the browser surface

| Surface | For | Decision |
|:--|:--|:--|
| Git over HTTPS | agents, CI, anything holding a token | |
| The read-only browser page | a person who wants to look | D28 |
| Repository browsing under `/r/` | reading code and reviews without a clone | D30 |
| Git over SSH | people who expect `user@host:owner/repo.git` | D31 |

All four sit behind the same auth wall and `--acl-file` grants; a
repository you hold no grant on answers `404` on all four.

## Browser surface

`/` with a credential serves the repository index and one read-only page per
repository: refs, the review queue, the latest ref-state attestation,
workspaces, and sequencer health.

**Exception: the bare address (D57).** `GET /` with *no* credential gets a
static front page: what a node is, the commands to use one, and that it is
invite-only. A wrong credential still gets `401`.

Pages are server-rendered from `/api/view`, cached by view sequence, and
revalidated with an `ETag`.

Browser writes exist in one place (D39): a verdict or comment on a review
page, and passkey enrolment on `/account`, signed with a key that never
leaves the device.

The client half is `/static/webauthn.js`. Its two pages are served
`script-src 'self'`; every other page, including `/r/`, is
`default-src 'none'`.

### The pages that are not repositories

| URL | Anonymous | Signed in |
|:--|:--|:--|
| `/`, `/index.html` | the front page (D57) | the repository index |
| `/signin` | the sign-in form (D74) | the same form |
| `/join?i=&k=` | an invite to redeem (D57) | the same |
| `/account` | `401` | passkeys, and the token git speaks (D71, D75) |
| `/people` | `401` | the operator console, `@node write` only, `403` otherwise (D72) |
| `/status` | `401` | node telemetry and the view |
| `/p/<channel>` | `401` | one actor's standing (D63) |
| `/robots.txt` | the crawl policy | the same |
| `/static/card.png` | the social-preview card | the same |
| `/llms.txt`, `/sync.md` | `401` | the machine-readable surface |

A `text/html` request not naming a `.git` path gets the sign-in page as the
body of its `401` (D74); git, `curl` and API clients get a bare `401` with
`WWW-Authenticate`. Repositories granted to `@anon` are the exception (D78).

`crates/choir-node/tests/it/routes.rs` holds the expected status of every
route for three readers and crawls the surface.

### The other half of the site (D76)

The node is at the apex; the book is on a `docs.` subdomain. Each is told
where the other is at run time.

**Point the node at the book.** One line, no restart:

```bash
mkdir -p <root>/.choir
printf 'https://<docs-host>\n' > <root>/.choir/docs-url
```

`<root>` is the repositories directory. The value must be absolute `http://`
or `https://`. Read per request.

**Point the book at the node.** Two repository variables, read by
`.github/workflows/pages.yml`:

| Variable | Value | Effect |
|:--|:--|:--|
| `NODE_URL` | `https://<node-host>` | the book's front page links back to the node |
| `DOCS_DOMAIN` | `<docs-host>` | writes `CNAME` into the Pages artifact, and switches `site-url` to `/` |

Also enter the domain under **Settings → Pages** and add a DNS `CNAME` for
`<docs-host>` pointing at the Pages host.

The workflow passes `CHOIR_DOCS_REPO_BASE`, which repoints links to files
outside `docs/` at the commit being published.

## Repository browsing

`/r/` lists the repositories your credential may read:

| URL | Shows |
|:--|:--|
| `/r/<owner>/<repo>` | the default branch at the repository root |
| `/r/<owner>/<repo>/tree/<rev>/<path>` | a directory listing |
| `/r/<owner>/<repo>/blob/<rev>/<path>` | one file, with line numbers |
| `/r/<owner>/<repo>/commits/<rev>` | recent history |
| `/r/<owner>/<repo>/commit/<oid>` | one commit and its diff |
| `/r/<owner>/<repo>/reviews` | reviews proposing to land here |
| `/r/<owner>/<repo>/review/<id>` | one review: proposal, reviewers, verdicts, diff |

Same `read` grant a clone needs. Content pages revalidate on the commit oid.

Files over 512 KiB are described, binaries are named, diffs truncate at
2,000 lines. Revision arithmetic, traversal and option-shaped input are
refused at the router.

A review page shows the target ref, reviewers and verdicts, approval weight,
slashing, and a three-dot diff. Comments are signed `PostComment` operations
(D38).

## Git compatibility path

A person holding an invite needs none of this: `choir join '<link>'` stores
the credential, wires the helper below and clones each repository the
invite names. By hand:

```bash
choir repo url owner/repo.git      # prints the URL, and the git config for the credential
git clone http://127.0.0.1:8417/owner/repo.git
git -C repo config credential.helper '!choir git-credential ~/.choir/auth'

git push origin HEAD:main
```

The credential is not in the URL. Pushes are CAS-sequenced: on rejection,
fetch, rebase or merge, push again.

> [!WARNING]
> Never force-push over a sequencer rejection. See
> [The contribution workflow](../using/workflow.md).

## Git over SSH (D31)

The host's `sshd` serves `git@host:owner/repo.git` through a forced command
that hands each connection to `choir-ssh`.

Start the node with a handoff file, which carries the daemon's address and
loopback secret:

```bash
choir-node <repo-root> 8417 --auth-file <auth-file> --keys-file <keys-file> \
  --acl-file <acl-file> --ssh-handoff <handoff-file>
```

One `authorized_keys` line per registered key:

```text
command="/usr/local/bin/choir-ssh --root <repo-root> --user <choir-user> --acl-file <acl-file> --handoff <handoff-file> --git-binary /usr/bin/git",restrict ssh-ed25519 AAAA... <user>@<host>
```

`--user` is the choir username for that key. `restrict` turns off pty,
agent, port and X11 forwarding. Set `--git-binary` explicitly.

```bash
git clone ssh://<ssh-account>@<SERVER_IP>/owner/demo.git
git clone <ssh-account>@<SERVER_IP>:owner/demo.git
```

The shim serves:

- exactly `git-upload-pack '<repo>'` and `git-receive-pack '<repo>'`, one
  argument, never a shell. Anything else is refused.
- `owner/repo` or `owner/repo.git`, two ASCII segments, none starting with a
  dot.
- the same `--acl-file` as HTTP: `read` to fetch, `write` to push. Omit
  `--acl-file` to use the daemon's.
- pushes through the repository's `pre-receive` hook, sequenced like HTTPS.

> [!CAUTION]
> A shim installed without `--handoff` serves fetches and **refuses pushes**.

Limits:

- the handoff file holds the loopback secret at `0600`, so the SSH account
  and the daemon must be the same uid.
- one line per key; revocation is deleting the line. With `--accounts-file`
  the node writes those lines
  ([above](authorization.md#issuing-a-credential-without-editing-a-file-d36));
  point `AuthorizedKeysFile` at the generated file.
- `sshd` stays the operator's.
