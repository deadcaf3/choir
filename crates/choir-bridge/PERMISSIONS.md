# choir-bridge: minimum GitHub App permissions

Derived from the API calls the code actually makes, not from what looked
convenient. Every entry names the call that needs it; if a call is
removed, the permission goes with it.

Grant an App **only** these. The bridge holds the highest-value secret in
the system (risk #15) and is the lethal-trifecta exposure (risk #16), so
the permission set is part of the mitigation, not paperwork.

## Required for `mirror` (read-only replica)

| Permission | Level | Needed by |
|---|---|---|
| Metadata | read | mandatory for every App; also `GET /repos/{repo}` for the default branch |
| Contents | read | `GET /repos/{repo}/commits/HEAD`, and the git fetch of the upstream |

## Additionally required for `queue` (verdict-only)

| Permission | Level | Needed by |
|---|---|---|
| Pull requests | read | `GET /repos/{repo}/pulls?state=open` — which PRs are in the train |
| Checks | read | `GET /repos/{repo}/commits/{sha}/check-runs` — the CI signal the verdict is derived from |
| Commit statuses | write | `POST /repos/{repo}/statuses/{sha}` — publishing the `choir/queue` verdict |

## Additionally required for `queue --land`

| Permission | Level | Needed by |
|---|---|---|
| Contents | **write** | pushing the green train to the base branch, and pushing the revert if post-land CI goes red |

## Not needed, and must not be granted

`Administration`, `Actions` (write), `Workflows`, `Secrets`,
`Environments`, `Members`, `Packages`, `Deployments`, `Issues`,
`Pages`, `Webhooks`. The bridge makes no call that touches any of them.

`Actions: read` is deliberately **not** listed: the verdict comes from
the check-runs API, not from workflow-run introspection.

## Residual risk

- **`Pull requests: read` cannot be dropped.** Knowing which PRs to test
  is the queue's whole job, so the untrusted-input leg of the trifecta is
  permanent. What is mitigated is the *channel*: no PR text reaches a
  decision. The behavior is enforced by
  [`tests/bridge_trifecta.rs`](tests/bridge_trifecta.rs).
- **`Contents: write` cannot be dropped while `--land` exists.** It is
  the external-write leg. Mitigations: `--land` is per-invocation with no
  config default, so a bridge started without it cannot land whatever
  happens; the push is non-force, so git's fast-forward rule refuses a
  race; and post-land CI failure triggers an auto-revert that is itself a
  non-force push.
- **The App private key is a single point of compromise.** It lives at
  `~/.choir/github-app.pem` at 0600 and is never read into the bridge's
  address space — RS256 signing shells out to `openssl`. That narrows
  exposure but does not remove it.
- **Per-repo scoping is the operator's job.** Install the App on the
  specific repositories the bridge serves, never organisation-wide. The
  code cannot enforce this.
