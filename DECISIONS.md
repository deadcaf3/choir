# Decision register

Code comments across this workspace cite decisions by number (`D6`, `D29`,
`D41`). This table is what those numbers mean.

Each row records **what** was decided and whether it is a **one-way door** — a
choice that is a data migration to reverse rather than a refactor. One-way rows
are the ones to read before changing anything a comment tags.

The rationale behind each decision, the fallback chosen for it, and the tripwire
that would reverse it are not published.

| # | Decision | Door |
|---|---|---|
| D1 | Op-log / workspaces as the version model (L1) | One-way-leaning |
| D2 | Single-writer per-repo sequencer | Two-way |
| D3 | Rivet Actors as the actor runtime | Two-way |
| D4 | Mergiraf for structured merge, subprocess only | Two-way |
| D5 | Speculative merge queue with a TCP-like window | Two-way |
| D6 | BLAKE3 + FastCDC content addressing (L0) | **One-way** |
| D7 | S3-API object store, no vendor extensions | Two-way |
| D8 | Firecracker microVM + copy-on-write provisioning | Two-way |
| D9 | Key-per-actor identity and signed-ref format (L8) | **One-way** (trust root) |
| D10 | Signed CRDT record schemas for collaboration objects (L9) | **One-way** once exported |
| D11 | Content-addressed provenance format (L5) | **One-way** (append-only audit) |
| D12 | Forked knot daemon for the node (L3) | Two-way |
| D13 | Zoekt for code search (L7) | Two-way |
| D14 | Centralized transport now, P2P later (L10) | Gated |
| D15 | CRDTs for collaboration objects only, never for code | Two-way |
| D16 | Append-only log now, witnessed later | Gated |
| D17 | Typed, self-describing API as the agent SDK surface | One-way-leaning |
| D18 | Nix-based CI with a shared remote build cache (L6) | Two-way |
| D19 | LLM auto-resolve limited to low-risk hunks, never silent | Two-way |
| D20 | Dogfood: the canonical repo migrates onto the platform | Two-way |
| D21 | Mirror-first forge bridge as the adoption surface | Two-way |
| D22 | Provenance-conditioned merging as the research bet | Two-way |
| D23 | CI green is necessary but never sufficient to land; the queue adds differential tests | Two-way |
| D24 | Operator-anchored trust and Sybil resistance | Two-way |
| D25 | Signed ref-state snapshot (`RefSnapshot`) as the attestation unit | **One-way** once written |
| D26 | A signature pins position and log identity, not just `(channel, payload)` | **One-way-leaning** |
| D27 | Semantic-merge observatory: replay landed merges to harvest specimens | Two-way |
| D28 | A read-only browser surface served by the node itself | Two-way |
| D29 | Per-repository authorization via an ACL file with read/write levels | Two-way |
| D30 | Repository browsing: server-rendered tree, blob, commit list and diff | Two-way |
| D31 | SSH transport via the host's `sshd` and a forced command, not an in-daemon server | Two-way |
| D32 | Outbound ref-landed webhooks from an operator-side hooks file | Two-way |
| D33 | Attributed request log and per-request rate limits | Two-way |
| D34 | Review pages on the browse surface | Two-way |
| D35 | Change lifecycle as three op variants (`CreateChange`, `CheckpointChange`, `ArchiveChange`) | **One-way for readers** |
| D36 | Account and credential self-service, deliberately outside the op log | Two-way |
| D37 | Per-user quotas as a request ceiling plus a replay-derived projection | Two-way |
| D38 | PR discussion as one additive op variant, `PostComment` | **One-way for readers** |
| D39 | Browser writes via passkeys, narrowing D28's no-JavaScript rule | **One-way for the signature format** |
| D40 | Replay purity and conflict quarantine, promoted to contract | **One-way if broken** |
| D41 | Push provenance as a schema field, not a channel-string convention | **One-way for readers** |
| D42 | Repository ownership as a landing gate, granted as an ACL level | Two-way |
| D43 | `Submit`: a landing carries the authorization basis that admitted it | **One-way for readers** |
| D44 | An approval resolves to the key that was live when it was cast, not the one holding the channel now | Two-way |
| D45 | A passkey signature carries the credential key it is checked against, instead of a delegation op | Two-way |
| D46 | The log names an account by an opaque handle; the readable name sits in the store, where it can be deleted | Two-way |
| D47 | Refs the node serves but never sequenced are outside attestation; a backup reports them unverified rather than failing | Two-way |
| D48 | Large-repo checkout is git's partial clone and sparse checkout, enabled in repository config, not a filesystem driver of ours | Two-way |
| D49 | Automated check results are signed ops the node orders and never runs; the CLI answers pass, fail and not-yet as 0, 1 and 3 | **One-way for readers** |
| D50 | A change declares its path cone in the owner-signed authorization, and a conflict outside that cone is reported as a path with no content | **One-way for readers** |
| D51 | Redeeming an invite may bind the newcomer's actor key, by appending it to the same trusted-keys file an operator would have edited; opt-in per node, and the invite remains the operator's assertion | Two-way |
| D52 | A proposal's change identity is derived client-side from the branch name, so an amend or a rebase updates one change rather than opening a second | Two-way |
| D53 | A push to `refs/for/<branch>/<topic>` opens a review through the existing pre-receive path and really creates the ref, rather than synthesizing one through a proc-receive hook | Two-way |
| D54 | The software keeps the singular name `choir` even where the registered domain is plural: a choir is already many voices under one score, which is the unit the model has | Two-way |
| D55 | Answering a review is a read-level act: a verdict, a comment and a viewing receipt authorize at `read` on the repository under review, because admission already binds each of them to the channel that signed it | Two-way |
| D56 | The browser surface picks its palette from a cookie the node reads server-side, rendering `data-theme` into the page, rather than from a script: the read surface still runs none, and "follow the system" stays a real third state | Two-way |
| D57 | The node has a public front door: a landing page at `/` for a request carrying no credential, and `GET /join?i=<id>&k=<secret>` rendering an invite that only a `POST` redeems. The link is a bearer credential, bounded by a username and grants the issuer freezes at minting; `GET` never consumes, so a chat client's preview cannot spend it; every failure renders one byte-identical page, so the node is not a directory of pending accounts; the secret rides in the query string, which the request log drops. The route and its two parameter names are the one-way part, because links outlive the channels they were pasted into | One-way for readers |
| D58 | The `choir` CLI answers two audiences on two streams: stdout carries the document, byte for byte the same whether or not a terminal is attached, and stderr carries diagnostics and a short human summary, coloured only when stderr is a terminal, `NO_COLOR` is unset and `TERM` is not `dumb`. A refusal says the smallest true thing -- the command's own spec when a known command was given wrong arguments, the name and a near match when it was not -- and prints the full index only when the invocation asked what the tool can do. There is deliberately no `--color` flag: a flag cannot know what is attached, and one would become a second way to be wrong | Two-way |
| D59 | The deployment handles no client address. The proxy writes no access log, keeps no per-address rate or connection zone, discards its error log, and clears `X-Forwarded-For` rather than appending to it, so nothing downstream can read one; the record that survives is the node's own request log, which never had an address field to drop. No code reads one either: the pre-auth limiter's 256-slot per-address table was deleted rather than left dormant, so this is a property of the code instead of a property of the node happening to sit behind a loopback proxy, and a later topology change cannot quietly revive it. The cost is accepted rather than mitigated: the pre-auth surface keeps one node-wide ceiling and no per-client fairness at either layer, so a flood from any number of sources can deny the join page until the window rolls, because every key that would restore fairness is either a client address or a value the caller chooses. That is an outage and never a disclosure, which is D57's doing: the invite link is a bearer secret, `GET` never spends one, and every failure renders one byte-identical page | Two-way |
| D60 | A `propose` grant, ranked between `read` and `write`: it admits a push to `refs/for/<branch>/<user>/<topic>` (D53), where the pusher's own name is a required segment because several people hold this grant at once and it is what keeps them from reaching one another's proposals, and refuses every other ref, so a repository can take a contribution from somebody who is not trusted with its branches. Until it existed that was not expressible, because opening a review is a push and `write` reaches every unprotected ref, so inviting an outsider to propose meant handing them the repository. It is enforced at two points because it must be: the smart-HTTP boundary has no refname to judge, since git sends the ref list only after the server has agreed to receive the pack, so the level is admitted there and the refs are judged when the `pre-receive` hook reports them -- before any op is submitted, and while git has applied nothing. The refname check reads the merged table rather than the operator's file, because a `write` grant issued by self-service (D36) is as real as one typed by hand and only `own` is file-only. Both transports inherit it without restating it: the SSH shim borrows the HTTP mapping and runs the same hook | Two-way |

| D61 | Export/import is a *format* tool in the daemon binary, not a second deployment script. `--export <root> <dir>` writes the op log, a git bundle per repository and a versioned manifest; `--verify-export <dir>` settles what that directory claims by folding the log and requiring every ref the view names to be in a bundle at the same oid; `--import <dir> <root>` places one into a fresh root, refusing a root that already holds a log or a named repository, and leaves proving it to the boot that accepts a write. The plan's §E standing rule asks for this and `scripts/pull_backup.sh` does not satisfy it: that script pulls over ssh from a fixed remote path, refuses to run on the machine holding the log, takes its repository list from a policy file, needs python3 to project a hash, and checks bundles against the D25 attestation rather than the log. All of that is right for recovering one deployment and none of it travels, so the script keeps its transport and this owns the format. The check runs in one direction on purpose -- a bundle may be ahead of the log, a log may never name a commit no bundle holds -- which is what makes exporting a running node meaningful, since the log is read before the bundles are taken; the benign direction is counted and reported rather than hidden. Secrets are excluded by copying four names and then walking the finished directory and refusing anything whose name has the shape of one, so the guarantee is a property of the output. Reversal costs nothing to third parties, which is the property being bought: an export nobody can read with our code is still a jsonl log and a set of git bundles | Two-way |

## Reading the door column

- **Two-way** — reversible by deleting code. No persisted format, op, hash or
  signature is involved, so reversing leaves nothing to migrate.
- **One-way** — the choice is baked into bytes that already exist. Reversing it
  means migrating persisted data.
- **One-way for readers** — the feature can stop being *written* at any time and
  nothing breaks, but the log is append-only, so the first accepted entry of
  that shape permanently ends replay compatibility with earlier binaries.
- **Gated** — deliberately deferred, with the swap planned rather than assumed.
