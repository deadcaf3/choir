# Transports and the browser surface

Four ways to reach a node, in the order most deployments add them:

| Surface | For | Decision |
|:--|:--|:--|
| Git over HTTPS | agents, CI, anything holding a token | — |
| The read-only browser page | a person who wants to look | D28 |
| Repository browsing under `/r/` | reading code and reviews without a clone | D30 |
| Git over SSH | people who expect the `user@host:owner/repo.git` spelling | D31 |

All four sit behind the same auth wall and the same `--acl-file` grants: a
repository you hold no grant on answers `404` on all four.

## Browser surface

Open the node's base URL (`/`) with a credential and it serves the repository index, and behind it one read-only page per repository: refs, the review queue with approval weights and verdicts, the latest ref-state attestation, workspaces, and sequencer health against the 100 ms gate. A browser prompts for a `--auth-file` user and token.

**The one exception is the bare address itself (D57).** A `GET /` that presents *no* credential gets a static front page instead of a password box: what a choir node is, the three commands it takes to use one (`choir join`, `choir git-credential`, `choir propose`), and that the node is invite-only. A credential that is presented and *wrong* still gets the `401` and the `WWW-Authenticate` challenge.

The page is server-rendered from the same `/api/view` payload the API serves, cached by view sequence, and revalidated with an `ETag`: a repeat visit on unchanged state returns `304` with no body. It is plain HTML, so it works offline.

Writes from a browser exist in one place (D39): a reviewer casts a verdict or leaves a comment on a review page, and a person enrols a passkey on `/account`. The browser signs the operation with a key that never leaves the device. Everything else goes through the signed-operation API.

The client half is one same-origin file, `/static/webauthn.js`. Its two pages are the only ones served `script-src 'self'`; every other page, including all of `/r/`, is served `default-src 'none'`. With scripting off, those two sections render a sentence naming the CLI.

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

A request that asked for `text/html` and is not naming a `.git` path gets the sign-in page as the body of its `401` (D74); git, `curl` and every other API client get a bare `401` and a `WWW-Authenticate` challenge. `/signin` answers `401` with the form and no challenge header, keeping the browser dialog closed.

The expected status of every route for three readers is `crates/choir-node/tests/it/routes.rs`. It crawls the surface anonymously and signed in, and fails on an unlisted route or a route nothing reaches.

### The other half of the site (D76)

A node and this book are two halves of one site on two hosts: the node at the apex, the book on a `docs.` subdomain. Each is told where the other is at run time.

**Point the node at the book.** One line, no restart:

```bash
mkdir -p <root>/.choir
printf 'https://<docs-host>\n' > <root>/.choir/docs-url
```

`<root>` is the repositories directory the daemon was started with. The value must be an absolute `http://` or `https://` address; anything else is ignored. The `docs` link is rendered only when the file is present, and it is read per request, so an edit takes effect on the next page load, like `<root>/.choir/contact`.

**Point the book at the node.** Two repository variables, read by `.github/workflows/pages.yml`:

| Variable | Value | Effect |
|:--|:--|:--|
| `NODE_URL` | `https://<node-host>` | the book's front page links back to the node |
| `DOCS_DOMAIN` | `<docs-host>` | writes `CNAME` into the Pages artifact, and switches `site-url` to `/` |

Setting `DOCS_DOMAIN` is two of three steps: also enter the domain under **Settings → Pages**, and add a DNS `CNAME` record for `<docs-host>` pointing at the Pages host. With neither variable set, the book publishes to the default repository path.

The workflow also passes `CHOIR_DOCS_REPO_BASE`, which repoints the book's links to files outside `docs/` (`../README.md` and its siblings) at the commit being published. A local `choir docs` renders the book unchanged.

## Repository browsing

`/r/` lists the repositories your credential may read, and each one browses:

| URL | Shows |
|:--|:--|
| `/r/<owner>/<repo>` | the default branch at the repository root |
| `/r/<owner>/<repo>/tree/<rev>/<path>` | a directory listing |
| `/r/<owner>/<repo>/blob/<rev>/<path>` | one file, with line numbers |
| `/r/<owner>/<repo>/commits/<rev>` | recent history |
| `/r/<owner>/<repo>/commit/<oid>` | one commit and its diff |
| `/r/<owner>/<repo>/reviews` | reviews proposing to land here |
| `/r/<owner>/<repo>/review/<id>` | one review: proposal, reviewers, verdicts, diff |

Same auth wall, and the same `read` grant a clone needs. Content pages revalidate on the **commit oid** rather than the view sequence: a `304` here means this commit's bytes have not changed.

Large files are described rather than dumped (512 KiB), binary files are named rather than rendered, and long diffs truncate at 2,000 lines; clone for all three. Everything from a URL is validated before it reaches `git`: revision arithmetic (`main~3`, `HEAD@{1}`), traversal and anything option-shaped are refused at the router.

A review page shows what commit lands on what ref, who was asked and what each said with their verdict notes, the approval weight, any retroactive slashing, and the diff between the proposal and its destination. That diff is three-dot: it shows what the proposal added since it diverged, so work that landed on the target during the review is not attributed to the author. Every field comes from the payload `/api/view` serves. The discussion is on the page too (D38): a comment is a signed `PostComment` operation folded into the review.

## Git compatibility path

```bash
choir repo url owner/repo.git      # prints the URL, and the git config for the credential
git clone http://127.0.0.1:8417/owner/repo.git
git -C repo config credential.helper '!choir git-credential ~/.choir/auth'

git push origin HEAD:main
```

The credential is deliberately not in the URL. A clone URL is pasted into
shells, screenshots and issue trackers, and a token in one is a token in
all three; the helper line does the same job and leaves no copy behind.

Pushes are CAS-sequenced. On rejection: fetch, rebase/merge, push again.

> [!WARNING]
> Never force-push over a sequencer rejection. See
> [The contribution workflow](../using/workflow.md).

## Git over SSH (D31)

People expect `git@host:owner/repo.git`, and the host's `sshd` serves it: a forced command hands each connection to the `choir-ssh` shim.

Start the node with a handoff file. It carries the daemon's address and the loopback secret its git hooks authenticate with, both of which change on every start:

```bash
choir-node <repo-root> 8417 --auth-file <auth-file> --keys-file <keys-file> \
  --acl-file <acl-file> --ssh-handoff <handoff-file>
```

Then give the SSH account one line per registered key:

```text
command="/usr/local/bin/choir-ssh --root <repo-root> --user <choir-user> --acl-file <acl-file> --handoff <handoff-file> --git-binary /usr/bin/git",restrict ssh-ed25519 AAAA... <user>@<host>
```

`--user` is the choir username that key belongs to, the entire key-to-actor mapping. sshd puts whatever the client asked for in `SSH_ORIGINAL_COMMAND`, the shim's only untrusted input. `restrict` turns off pty, agent, port and X11 forwarding. Set `--git-binary` explicitly: sshd runs the forced command through a non-interactive shell whose `PATH` is often not the operator's.

Clone with either spelling:

```bash
git clone ssh://<ssh-account>@<SERVER_IP>/owner/demo.git
git clone <ssh-account>@<SERVER_IP>:owner/demo.git
```

What the shim serves:

- exactly `git-upload-pack '<repo>'` and `git-receive-pack '<repo>'` (the dashless `git upload-pack` spelling too), one argument, never a shell. Anything else, `git-upload-archive` and interactive logins included, is refused with a message the client prints.
- `owner/repo` or `owner/repo.git`, two segments, ASCII, no segment starting with a dot, so the node's own `.choir` state directory is not addressable.
- the same `--acl-file` the HTTP path reads, demanding the same level: `read` to fetch, `write` to push. A repository you may not read is refused in the same words as one that does not exist. Leave `--acl-file` off the line and the shim uses whatever the daemon named in the handoff.
- pushes that run the repository's `pre-receive` hook, so an SSH push is sequenced exactly like an HTTPS one and lands in the log under the same `owner/repo.git:refs/heads/...` name.

> [!CAUTION]
> A shim installed without `--handoff` serves fetches and **refuses pushes**,
> rather than let one through unsequenced.

Before deploying it, three limits:

- the handoff file holds the daemon's loopback secret at `0600`, so the SSH account and the daemon must be the same uid. If your deployment needs them separate, stay on HTTPS: do not widen who can read that secret.
- one line per key, and it lives until deleted: revocation is deleting the line. With `--accounts-file` the node writes those lines from the keys people registered when they redeemed an invite ([above](authorization.md#issuing-a-credential-without-editing-a-file-d36)); point `AuthorizedKeysFile` at the generated file rather than maintaining one by hand.
- `sshd` stays the operator's: its port, host keys, and account are unchanged by choir.
