# Transports and the browser surface

Four ways to reach a node, in the order most deployments add them:

| Surface | For | Decision |
|:--|:--|:--|
| Git over HTTPS | agents, CI, anything holding a token | — |
| The read-only browser page | a person who wants to look | D28 |
| Repository browsing under `/r/` | reading code and reviews without a clone | D30 |
| Git over SSH | people who expect the `user@host:owner/repo.git` spelling | D31 |

Every one of them sits behind the same auth wall and the same `--acl-file`
grants. A repository you hold no grant on answers `404` on all four, so no
surface confirms that a repository exists.

## Browser surface

Open the node's base URL (`/`) in a browser with a credential and it serves the repository index, and behind it one read-only page per repository: refs, the review queue with approval weights and verdicts, the latest ref-state attestation, workspaces, and sequencer health against the 100 ms gate. It is behind the same auth wall as everything else, so a browser prompts for a `--auth-file` user and token, and the API answers `401` to a request carrying none.

**The one exception is the bare address itself (D57).** A `GET /` that presents *no* credential gets a static front page instead of a password box: what a choir node is, the three commands it takes to use one (`choir join`, `choir git-credential`, `choir propose`), and that the node is invite-only. It names no repository, no sequence number and not even this node's own address — it takes no store, no platform and no view, so there is nothing on it that could grow node state without somebody adding a parameter on purpose. A credential that is presented and *wrong* still gets the `401` and the `WWW-Authenticate` challenge, because a reader who mistyped a password needs the browser to ask again rather than a page explaining what choir is.

It is deliberately not an app. The page is server-rendered from the same `/api/view` payload the API serves (so it cannot drift from the API), cached by view sequence, and revalidated with an `ETag`: a repeat visit on unchanged state returns `304` with no body, so refreshing or polling it costs the node nothing. No JavaScript, no build step, no external fetch, so it works offline and inside networks with no route to the internet.

Writes from a browser exist in exactly one place, under its own decision (D39): a reviewer can cast a verdict or leave a comment on a review page, and a person can enrol a passkey on `/account`. The browser signs the operation with a key that never leaves the device, so the node cannot forge it, the same property the CLI's actor key has, which is why this is a second signature scheme rather than a second write path. Everything else still goes through the signed-operation API.

That client half is one same-origin file, `/static/webauthn.js`: no library, no build step, nothing from another host, and no page-embedded code. The two pages that use it are the only ones served with a `script-src 'self'` policy; every other page, including all of `/r/`, is served `default-src 'none'` and runs nothing at all. With scripting off, those two sections are a sentence naming the CLI rather than a control that cannot work.

### The pages that are not repositories

| URL | Anonymous | Signed in |
|:--|:--|:--|
| `/`, `/index.html` | the front page (D57) | the repository index |
| `/signin` | the sign-in form (D74) | the same form |
| `/join?i=&k=` | an invite to redeem (D57) | the same |
| `/account` | `401` | passkeys, and the token git speaks (D71, D75) |
| `/people` | `401` | the operator console — `@node write` only, `403` otherwise (D72) |
| `/status` | `401` | node telemetry and the view |
| `/p/<channel>` | `401` | one actor's standing (D63) |
| `/robots.txt` | the crawl policy | the same |
| `/static/card.png` | the social-preview card | the same |
| `/llms.txt`, `/sync.md` | `401` | the machine-readable surface |

A `401` on this surface is not a dead end. A request that asked for `text/html` and is not naming a `.git` path gets the sign-in page as the body (D74); everything else — git, `curl`, every API client — gets a bare `401` and a `WWW-Authenticate` challenge. `/signin` itself answers `401` with the form, deliberately and with no challenge header, because a challenge is what opens the browser dialog the page exists to replace.

The complete table, with the expected status of every route for three readers, is `crates/choir-node/tests/it/routes.rs`. It is a test rather than a document: it crawls the surface anonymously and signed in, and fails on a link to a route it does not list or a route nothing reaches.

### The other half of the site (D76)

A node and this book are two halves of one site on two hosts: the node at the apex, the book on a `docs.` subdomain. Neither names the other in a tracked file, so each is told where the other is at run time.

**Point the node at the book.** One line, no restart:

```bash
mkdir -p <root>/.choir
printf 'https://<docs-host>\n' > <root>/.choir/docs-url
```

`<root>` is the repositories directory the daemon was started with. The value must be an absolute `http://` or `https://` address; anything else is ignored, because the string reaches an `href` on a page that answers anybody. With the file absent — the default — no `docs` link is rendered at all. It is read per request, so an edit takes effect on the next page load, the same way `<root>/.choir/contact` does.

**Point the book at the node.** Two repository variables, read by `.github/workflows/pages.yml`:

| Variable | Value | Effect |
|:--|:--|:--|
| `NODE_URL` | `https://<node-host>` | the book's front page links back to the node |
| `DOCS_DOMAIN` | `<docs-host>` | writes `CNAME` into the Pages artifact, and switches `site-url` to `/` |

Setting `DOCS_DOMAIN` is two of three steps: also enter the domain under **Settings → Pages**, and add a DNS `CNAME` record for `<docs-host>` pointing at the Pages host. With neither variable set, the book publishes to the default repository path exactly as before.

The workflow also passes `CHOIR_DOCS_REPO_BASE`, which repoints the book's links to files outside `docs/` (`../README.md` and its siblings) at the commit being published. Those files are in the checkout but not in the artifact, so without it they 404 on the published site while working correctly for anybody reading the repository. Nothing here is needed for a local `choir docs`, which renders the book unchanged.

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

Same auth wall, and the same `read` grant a clone needs: a repository you hold no grant on answers `404` here too, so browsing never confirms that one exists. Content pages revalidate on the **commit oid** rather than the view sequence, because file content lives in the bare repository and the sequence describes the op log; a `304` here means "this commit's bytes have not changed", which is true forever.

Large files are described rather than dumped (512 KiB), binary files are named rather than rendered, and long diffs truncate at 2,000 lines; the clone path exists for all three. Nothing from a URL reaches `git` unvalidated: revision arithmetic (`main~3`, `HEAD@{1}`), traversal and anything option-shaped are refused at the router rather than escaped later.

A review page shows what commit lands on what ref, who was asked and what each said (with their verdict notes), the approval weight, any retroactive slashing, and the diff between the proposal and its destination. That diff is three-dot: it shows what the proposal added since it diverged, not every difference between two branches, so work that landed on the target while the review was open is never attributed to the author under review. The page adds no state; every field comes from the same payload `/api/view` serves. The review discussion is on the page too, under its own decision (D38): a comment is a signed `PostComment` operation folded into the review, so the page renders the thread and the log carries every entry.

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

Pushes are CAS-sequenced. On rejection: fetch, rebase/merge, push again. **Never force-push** over a sequencer rejection.

## Git over SSH (D31)

Agents are content with HTTPS and a token. People expect `git@host:owner/repo.git`. The node does not run an SSH server; the host's `sshd` does, and a forced command hands each connection to the `choir-ssh` shim, which is how Gitea and gitolite do it. There is no in-process SSH server and there is not going to be one: every Rust SSH library within reach is tokio-based, and this daemon is synchronous threads.

Start the node with a handoff file. It carries the daemon's address and the loopback secret its git hooks authenticate with, both of which change on every start, which is why they cannot live in an `authorized_keys` line:

```bash
choir-node <repo-root> 8417 --auth-file <auth-file> --keys-file <keys-file> \
  --acl-file <acl-file> --ssh-handoff <handoff-file>
```

Then give the SSH account one line per registered key, all on one line:

```text
command="/usr/local/bin/choir-ssh --root <repo-root> --user <choir-user> --acl-file <acl-file> --handoff <handoff-file> --git-binary /usr/bin/git",restrict ssh-ed25519 AAAA... <user>@<host>
```

`--user` is the choir username that key belongs to, and it is the entire key-to-actor mapping. The client cannot reach it: sshd runs the forced command and puts whatever the client asked for in `SSH_ORIGINAL_COMMAND`, which is the shim's only untrusted input. `restrict` turns off pty, agent, port and X11 forwarding. `--git-binary` is worth setting explicitly, because sshd runs the forced command through a non-interactive shell whose `PATH` is often not the operator's.

Clone with either spelling:

```bash
git clone ssh://<ssh-account>@<SERVER_IP>/owner/demo.git
git clone <ssh-account>@<SERVER_IP>:owner/demo.git
```

What the shim serves:

- exactly `git-upload-pack '<repo>'` and `git-receive-pack '<repo>'` (the dashless `git upload-pack` spelling too), one argument, never a shell. Anything else, `git-upload-archive` and interactive logins included, is refused with a message the client prints.
- `owner/repo` or `owner/repo.git`, two segments, ASCII, no segment starting with a dot, so the node's own `.choir` state directory is not addressable.
- the same `--acl-file` the HTTP path reads, demanding the same level: `read` to fetch, `write` to push. A repository you may not read is refused in the same words as one that does not exist. Leave `--acl-file` off the line and the shim uses whatever the daemon named in the handoff, so a forgotten flag is not the difference between a gated repository and an open one.
- pushes that run the repository's `pre-receive` hook, so an SSH push is sequenced exactly like an HTTPS one and lands in the log under the same `owner/repo.git:refs/heads/...` name. A shim installed without `--handoff` serves fetches and **refuses pushes**, rather than let one through unsequenced.

Before deploying it, three limits:

- the handoff file holds the daemon's loopback secret at `0600`, so the SSH account and the daemon must be the same uid. If your deployment needs them separate, stay on HTTPS: do not widen who can read that secret.
- one line per key, and revocation is deleting the line. There is no expiry and no rotation. With `--accounts-file` the node writes those lines for you from the keys people registered when they redeemed an invite ([above](authorization.md#issuing-a-credential-without-editing-a-file-d36)); point `AuthorizedKeysFile` at the generated file instead of maintaining one by hand.
- choir does not manage `sshd`. Its port, host keys, and account are the operator's, exactly as they were before choir was installed.
