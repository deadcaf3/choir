# Day one: what the operator sets in the GitHub UI

Everything that cannot live in a tracked file, in the order it wants
doing. Nothing here is set by a workflow; a workflow only reads it.

D76 is the constraint behind half of this list: **the node and the book
must not name each other in a tracked file.** Both host-specific values
arrive as repository variables, read by `.github/workflows/pages.yml`.
`docs/operating/transports.md` is the long form.

## 1. Before making the repository public

- [ ] **`SECURITY.md` line 10** still says `<security-contact>`. Replace
      it with a real address, or delete the sentence if private
      vulnerability reporting is the only channel you want.
- [ ] `./gate` green on the tree you are about to publish. The scan stage
      needs `internal/scan-needles`; without that file the gate says the
      scan was skipped, which on this machine means the guard did not run.
- [ ] `git ls-files internal` is empty. The gate checks this too.

## 2. Repository settings

| Setting | Where | Value |
|:--|:--|:--|
| Description | Settings → General | `Agent-first code collaboration: many agents on one repository, ordered by a single-writer sequencer, with merge conflicts as first-class values.` |
| Topics | Settings → General | `rust`, `git`, `version-control`, `ai-agents`, `developer-tools`, `merge-queue`, `code-review` |
| Wikis, Projects, Discussions | Settings → General → Features | Off. `DECISIONS.md` and the issue tracker are the whole surface. |
| Issues | Settings → General → Features | **On.** `.github/ISSUE_TEMPLATE/` disables blank issues and offers three forms. |
| Private vulnerability reporting | Settings → Security → Reporting | **Enable.** `SECURITY.md` and `03-security.yml` both send people to it. |
| Default branch | Settings → Branches | `main`. Every workflow's `push` trigger names it. |
| Allow merge commits / squash / rebase | Settings → General → Pull Requests | Your call; nothing depends on it. |

## 3. Actions

- [ ] Settings → Actions → General → **Workflow permissions: read-only**.
      Nothing here writes to the repository; `pages.yml` gets what it
      needs from its own `permissions:` block.
- [ ] Settings → Environments → create **`github-pages`**. `pages.yml`'s
      `deploy` job names it. GitHub creates it on first deploy, but a
      protection rule on it is only possible if it already exists.

## 4. Pages and the custom domain

Three steps, all three needed; doing two of them publishes a book whose
404 page has a broken `<base href>`.

- [ ] Settings → Pages → **Source: GitHub Actions** (not "Deploy from a
      branch").
- [ ] Settings → Pages → **Custom domain: `docs.choirs.dev`**, then wait
      for the certificate and tick *Enforce HTTPS*.
- [ ] DNS: a `CNAME` record for `docs.choirs.dev` pointing at the GitHub
      Pages host.

## 5. Repository variables

Settings → Secrets and variables → **Actions → Variables** tab. These are
variables, not secrets — none of them is confidential, and a secret would
be masked in the logs where you want to read it.

| Variable | Value | Read by | What breaks without it |
|:--|:--|:--|:--|
| `DOCS_DOMAIN` | `docs.choirs.dev` | `pages.yml` | No `CNAME` in the artifact, and `site-url` stays at the repository path. Unset is a valid state: the book publishes to the default Pages path. |
| `NODE_URL` | the node's `https://` address | `pages.yml` | The book's front page renders no link back to the node. Unset is valid; the marker in `docs/README.md` renders as nothing. |

`CHOIR_DOCS_REPO_BASE` needs no variable — `pages.yml` derives it from
`github.server_url`, `github.repository` and `github.sha`.

**Not needed on day one**, and only if you ever run
`private-beta-release.yml`: `CHOIR_INSTALL_ROOT`, `CHOIR_SERVICE`,
`CHOIR_BASE_URL`, `CHOIR_AUTH_FILE`, `CHOIR_SMOKE_REPO`, plus `staging`
and `production` environments and two self-hosted runners labelled
`choir-staging` and `choir-production`. That workflow is
`workflow_dispatch` only and its `package` job is the half that runs
without any of it.

## 6. Point the node back at the book

Not a GitHub setting — the other half of D76, on the node's own host:

```bash
mkdir -p <root>/.choir
printf 'https://docs.choirs.dev\n' > <root>/.choir/docs-url
```

Read per request, so no restart. Absent, the node simply renders no
`docs` link.

## 7. After the first push

- [ ] The **CI** run is green. It is `./gate quick`, which compiles
      nothing and says so; it is not evidence that the tree builds.
- [ ] The **Pages** run is green and `https://docs.choirs.dev/` serves the
      book, with the API reference at `/api/`.
- [ ] `https://docs.choirs.dev/404-does-not-exist` renders the book's 404
      page with working navigation. That is the one page `site-url`
      affects, and the only way to see `DOCS_DOMAIN` took effect.
- [ ] The **Nightly gate** either ran or can be started by hand from
      Actions → Nightly gate → Run workflow. It is `./gate fast`, which
      skips the history scan by design.
