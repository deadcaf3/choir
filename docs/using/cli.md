# The `choir` CLI and the node's HTTP API

The complete surface, rendered from one table so that the CLI's `--help`,
`agents.md`, `/llms.txt`, `/api/schema` and this page cannot disagree with
each other. A staleness test fails the release gate when they do.

## Authentication and exit codes

Auth on the CLI is flags, not env:

```bash
choir --auth-file ~/.choir/auth --auth-user choir <command> ...
```

Exit codes: **0** accepted, **1** rejected (JSON body printed, see `ERRORS.md`), **2** usage.

The signed-operation API is the primary agent path: it carries actor identity and batches many operations behind one durability barrier. `git push` remains the compatibility and bulk-transfer path.

## Signed-operation CLI and API (primary agent path)

<!-- generated: choir surface, do not edit -->

### HTTP endpoints

| Endpoint | Purpose |
|---|---|
| `POST /api/submit` | Submit one signed operation (hex payload, hex signature) |
| `POST /api/submit-batch` | Same, in array order; the primary path for agent workloads (throughput figures live in the build log, not here, so they cannot go stale) |
| `GET /api/view?limit=N&offset=M` | The materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate. On a node running an ACL you are served your own slice: the repositories your credential may read, plus reviews you were assigned to; the node-wide sections need a node-wide grant. A repository missing from the response is one you were not granted, not one that is gone. Every map-shaped section is bounded: `limit` rows each (200 by default, 1000 at most), `offset` rows skipped in key order, `<section>_omitted` counting what this page left out, and `paging.next` naming the request that fetches the rest or being null when there is none |
| `POST /api/appeal` | Record an appeal for a rejected newcomer attempt; it requests operator adjudication and never changes privilege |
| `GET /api/log?from=N` | Ordered log entries, the catch-up and sync primitive. Absolute `from`: entries evicted from the in-memory window are served from the persisted log (`source` says which), and a node that cannot reach that far back answers 409 rather than a page with a hole in it. Each entry carries its hash, parent and author signature so pages can be chained and verified without trusting the node; SYNC.md is that procedure |
| `POST /api/workspace` | Provision a CoW workspace; optional exact base/change binding makes retries idempotent |
| `POST /api/workspace/archive` | Recoverably archive a change-bound workspace and remove it from the active view |
| `GET /api/reviews?reviewer=X` | One actor's pending review queue |
| `GET /api/schema` | This surface, machine-readable and versioned, plus what this particular node will accept — the description an agent generates a client from (D17) |
| `GET /api/search?q=X&in=code&repo=owner/name&rev=R&limit=N` | Search repository contents, file names or commit messages. Node-wide by default: every repository your credential may read, each at its own HEAD, which is why `rev` is accepted only alongside a single `repo`. A repository you were not granted is absent from the results and, asked for by name, is answered exactly as one that does not exist. Unindexed -- one `git grep` per repository -- so `limit` bounds what comes back while `matches` still counts everything found, and `truncated` says which happened. The same search the browser pages run, so the two cannot disagree about what a match is |
| `GET /api/profile?channel=X` | One actor's standing, counted out of the view you may already see: the keys bound to them and how many ops ago, changes they own, reviews they were assigned and the verdicts they gave, approvals slashed, checks they reported. It is a reading of `/api/view` and never a wider one -- two callers with different grants get different numbers about the same actor, which is the point. `vouches` names the operator whose graph it is (a vouch is between operators, so `ops/agent` reads `ops`), who vouches for them and whether each edge points both ways. D24 wants key age, vouches, scoped grants and bonds for Sybil resistance; two of the four are reported here as inputs, because a score would be a weighting of one against the other that nobody has measured. Scoped grants exist since D66 and are absent from this reading on purpose: a time-locked grant lives in the node's authorization files rather than in the log, so nothing counted out of the view can see one |
| `GET /llms.txt` | This surface, as text, for an agent that has never seen choir |
| `GET /sync.md` | The sync contract, in full: cursor semantics and how to verify a page's hash chain and author signatures without trusting the node serving them |
| `GET /api/repos` | Which repositories this credential can see, from the filesystem rather than the view — a repository is not an entity in the view, and `portable::repos` already answers which ones exist by walking the root. Narrowed rather than refused, like the other aggregate reads: `narrowed` says whether an ACL was applied, so "you can see none" is distinguishable from "there are none". |
| `POST /api/repo` | Create a repository on a running node, with the `pre-receive` hook that puts its pushes in the sequencer's order; needs a node-wide write grant, because there is no repository yet to be scoped to. Nothing is appended to the log: a repository is not a value in the view, and which ones exist is answered by the filesystem, as the export already does |
| `GET /api/ref-agreement` | Where the op log and the bare repos disagree about a ref, read-only |
| `POST /api/accounts/invite` | Mint a single-use, expiring invite for a new account and the grants it will hold; needs a node-wide write grant, and can never issue one. A grant may carry its own deadline, `owner/repo write until=<unix seconds>` (D66); the invite's expiry bounds redemption, never what redemption hands over |
| `POST /api/accounts/redeem` | Redeem an invite — presented as the credential — for a token, once, and register an ssh key with it |
| `POST /api/accounts/request/grant` | Answer somebody who asked for access (D72): turns their pending request into an invite under the id and secret they already hold, so the link they were given starts working with nothing sent |
| `POST /api/accounts/request/decline` | Drop a pending access request. Their link then says only that it is not valid, the same as a link that never existed |
| `POST /api/accounts/revoke` | Delete an account: its token stops authenticating on the next request, and its grants and keys go with it |
| `GET /api/accounts` | Who holds an account, what they were granted, and which invites are outstanding; never a secret or its hash |
| `POST /api/git-update` | Internal: the pre-receive hook callback |
| `POST /api/git-abort` | Internal: retracts a refused push's already-accepted refs |

### The `choir` CLI

**getting started**

- `choir init [<state-dir>] [--port <n>] [--force]`  
  set up a node on this machine from nothing: a repository root, a credential at 0600, an actor key the node will trust, and a .choir/config so the other commands stop asking which node you mean; refuses and names what exists rather than overwriting a token nothing can reissue
- `choir key <key-file> [name]`  
  mint a key and print the line the operator registers; pass your channel name to print the bound form
- `choir git-credential <auth-file> [--auth-user <name>] get|store|erase`  
  git credential helper: hands git your token on stdin so it never lives in a remote URL; configure once with `git config credential.helper '''!choir git-credential <auth-file>'''`
- `choir join <api> <invite-file> <key-file> [--user <name>] [--channel <name>] [--ssh-key <path>] [--token-file <path>]`  
  redeem an operator's invite and mint your actor key in one step; --user names the account, which most invites leave for you to pick and which the op log then keeps forever; writes the issued token to an auth file at 0600, and on a node started with --invite-binds-keys the key is registered by the redemption itself
- `choir docs [--open]`  
  build this repository's documentation: the book from `docs/`, and the API documentation inside it at `book/api/` so the prose can link to a type; needs a checkout and `mdbook`, and refuses with the command that installs it
- `choir skill install [--into <dir>]`  
  install the choir agent skill (default .claude/skills), rendered from this binary's own surface table so it can never document another version; re-run after upgrading and unchanged files are left alone

**changing code**

- `choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>] [--path <prefix>]...`  
  provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree this change declares it works within
- `choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>`  
  publish an immutable change revision after committing and pushing its Git object
- `choir propose <key-file> <channel> [--api <url>] [--repo <owner/repo>] [--remote <name>] [--onto <branch>] [--change <id>] [--path <prefix>]... [reviewer]...`  
  create a change, push its commits and request review; run it from a git checkout, which is where the node and the repository come from, the revision is checkpointed on the way, and the branch name is the change identity, so re-running after an amend updates the same proposal
- `choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>`  
  owner-sign and recoverably archive a bound workspace; exact retries are idempotent
- `choir submit <api> <key-file> <channel> '<op-json>'`  
  sign and submit one raw operation
- `choir batch <api> <key-file> <channel> <ops-file>`  
  sign and submit many operations as one batch — the primary path for agent workloads; one op per line, `-` reads stdin, one result line per op in order
- `choir intent <api> <key-file> <channel> <subject> <kind> '<body>'`  
  publish a task spec or plan so other agents can see intent
- `choir state <api> <channel>`  
  list what you owe and what you are waiting on; bounded, and every row carries the command that answers it and that command's risk

**review**

- `choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...`  
  request review on a commit; name no reviewers and the node draws them
- `choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]`  
  answer a review you were assigned
- `choir comment <api> <key-file> <channel> <review-id> <comment-id> '<body>'`  
  say something on a review; append-only and permanent, and the comment id is your retry identity
- `choir viewed <api> <key-file> <viewer> <review-id>`  
  record that you read a review, so its author can tell "reviewed and ignored" from "nobody looked"; first read only, resubmitting is refused
- `choir slash <api> <node-key-file> <id> <reviewer> '<reason>'`  
  invalidate one reviewer's approval; operator-only and never moves a ref
- `choir abandon <api> <node-key-file> <id>`  
  archive a stale incomplete review as lapsed, settling it unapproved; operator-only and never moves a ref
- `choir reviews <api> <reviewer>`  
  your pending review queue

**checks**

- `choir check <api> <key-file> <channel> <git-oid> <name> passed|failed|running|errored [evidence] [--ref <repo:ref>]`  
  report one automated check's outcome on a commit; any runner or a person can report by signing, and the node never runs the check
- `choir checks <api> <git-oid>`  
  every check reported on a commit, and one verdict; exits 0 passed, 1 failed or unreported, 3 still running, 4 could not be run

**trust**

- `choir witness <api> <key-file> <channel>`  
  cosign the node's current ref-state attestation (D67); the snapshot id is read from the view rather than passed, so a witness cannot attest a ref-state it did not look at, and the node may not witness its own
- `choir vouch <api> <key-file> <channel> <subject> [note]`  
  vouch for another operator; both ends need a key bound in the log, it authorizes nothing on its own, and there is no score
- `choir unvouch <api> <key-file> <channel> <subject> '<reason>'`  
  withdraw a vouch; the edge leaves the view and both ops stay in the log, so vouching again is allowed and starts a fresh clock

**reading the node**

- `choir schema <api>`  
  print this node's machine-readable API description and its live capabilities
- `choir log <api> [--from <n>] [--verify] [--keys <file>]`  
  read log entries from a cursor; --verify checks continuity, recomputes every hash, and verifies the signatures whose keys you hold — SYNC.md as a flag
- `choir appeal <api> <attempt-id>`  
  appeal a rejected newcomer attempt for operator adjudication; never grants privilege
- `choir profile <api> <channel>`  
  what the log records about one actor: keys and their age, changes owned, verdicts given, checks reported
- `choir search <api> <term> [--in files|code|commits] [--repo owner/name] [--rev R] [--limit N]`  
  find a term across every repository you may read; the term is literal, not a pattern
- `choir triage <api>`  
  every review and change classified into a bucket — landed, awaiting verdicts, changes requested, approved awaiting landing — ranked most-actionable-first, capped, with truncation marked in-band
- `choir funnel <api>`  
  the contribution funnel from admission to first verdict, and the steepest drop between two stages; counts what this credential may read, and reports the first-contact stage as null rather than inventing a zero
- `choir view <api> [--limit <n>] [--offset <n>]`  
  read the materialized view, its ref-state attestation and the node's health counters; map-shaped sections page 200 rows at a time, with `<section>_omitted` and `paging.next` describing the rest

**operating a node**

- `choir invite <api> <name> <owner/repo> [read|write]`  
  mint an invite and print the one link to send; the same thing the node's /people page does, for when a terminal is where you are
- `choir asks <api>`  
  who has asked for access and is waiting on an answer (D72)
- `choir grant <api> <request-id> <owner/repo> [read|write]`  
  let one of them in; the link they already hold becomes their invite, so there is nothing to send
- `choir decline <api> <request-id>`  
  drop a pending request; their link then reads as one that was never valid
- `choir runner <config-file>`  
  drive one workspace lifecycle step for an orchestrator; a JSON request on stdin, a JSON result on stdout
- `choir bind <api> <node-key-file> <operator> <key-hex> [channel]`  
  record in the log that a key belongs to an operator; operator-only and never moves a ref
- `choir revoke <api> <node-key-file> <key-hex> '<reason>'`  
  withdraw a key binding; terminal, and the attribution row survives
- `choir acl render <api> <acl-file>`  
  rewrite an ACL file's trailing comments to name the person behind each handle; the grants themselves are copied through unchanged, and a handle the node can no longer name loses its comment
- `choir repo create <api> <owner/repo.git>`  
  create a repository on a running node, hooked into the sequencer from its first push, without restarting anything; needs a node-wide write grant, answers 409 rather than an error when it already exists, and prints the clone URL because cloning is what happens next
- `choir repo list <api>`  
  the repositories on a node this credential can read, one per line; an ACL narrows the list rather than refusing it, and the command says which of the two empty answers it is giving
- `choir repo url <api> <owner/repo.git>`  
  the clone URL for a repository, with the one line of git configuration that makes pushing to it work; the credential is never put in the URL, because a URL is pasted into shells, screenshots and issue trackers and a token in one is a token in all three
- `choir node serve [--state <dir>] [--port <n>] [--create <owner/repo.git>] [-- <daemon flags>]`  
  run the node in this terminal, deriving its repository root, credential and trusted-key file from the layout `choir init` wrote, so starting one takes the same arguments as creating one — none; execs the daemon rather than wrapping it, so signals and the exit code reach the real process
- `choir node install [--state <dir>] [--port <n>] [-- <daemon flags>]`  
  hand the node to this machine's service manager — a launchd agent on macOS, a systemd user unit on Linux — so it survives a logout, a crash and a reboot; the unit runs `choir node serve`, so a flag changing later never means re-rendering it
- `choir node stop `  
  stop the supervised node for this boot, leaving the unit in place so it returns at next login; `node uninstall` is the one that ends it
- `choir node restart `  
  reload the unit and start it again, which is how a rebuilt binary reaches the running node; the unit is torn down and re-bootstrapped rather than kicked, because a kick relaunches the arguments the service manager cached rather than the ones on disk
- `choir node uninstall `  
  stop the node and remove its unit so it does not come back; the state directory is kept, because the keys, the repositories and the op log are in it and no command of ours deletes those
- `choir node logs [<lines>] [--state <dir>]`  
  the tail of the node's log, wherever this machine's service manager was told to write it; defaults to the last 30 lines
- `choir node status [<api>]`  
  what the node is doing right now: health, the commit actually serving, the sequencer's position and its measured p99 against the 100 ms gate, and how much this credential can see; sections a narrower credential may not read say so rather than reading as an idle node
- `choir doctor [<api>]`  
  check everything the other commands assume: the binaries this workspace shells out to, the auth file and its mode, and whether a node answers; each failure prints the command that fixes it, and a missing optional tool warns rather than fails
- `choir backup verify <backup-dir>`  
  whether a backup can be restored from, which is a different claim from whether one was written; checks the four files, the manifest checksum, the hash chain, the policy archive and every git bundle, and refuses a backup that carries a key or a credential — every check local, nothing asked of the node it is a copy of
- `choir backup restore <backup-dir> <target-root>`  
  turn a backup back into a node, and refuse to say it worked until the restored node has accepted a real push: it reads and refuses before it writes a byte, unbundles the git objects before the first boot, then rehearses on a port it picks itself and proves the append chained onto what it replayed; exit 3 means a secret only you can supply is missing
- `choir repair <log-file> --verify | --truncate-tail`  
  inspect a stopped node's op log, or repair a tail that was still being written; `--verify` walks the hash chain and changes nothing, `--truncate-tail` quarantines the partial record to a sidecar before cutting, and damage anywhere but the tail is refused rather than patched over

Most commands take the node's URL first. Put `node = <url>` in `.choir/config`, in the working directory or any parent, and it is filled in when omitted. `choir <command> --help` prints one command's spec.

Exit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), 2 usage error.
<!-- /generated -->

Live surface on a running node: `GET /llms.txt`. Sync verification: `SYNC.md` / `GET /sync.md`.

## MCP adapter

For MCP clients, run the synchronous stdio adapter. It maps generated tools onto the same HTTP endpoints and owns no second implementation or session state.

```bash
choir-mcp http://127.0.0.1:8417 --auth-file ~/.choir/auth --auth-user choir
```

It serves the measured legacy handshakes and the stateless 2026-07-28 request path. Tool order and schemas come from `crates/choir-cli/src/surface.rs`.
