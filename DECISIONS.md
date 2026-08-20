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

## Reading the door column

- **Two-way** — reversible by deleting code. No persisted format, op, hash or
  signature is involved, so reversing leaves nothing to migrate.
- **One-way** — the choice is baked into bytes that already exist. Reversing it
  means migrating persisted data.
- **One-way for readers** — the feature can stop being *written* at any time and
  nothing breaks, but the log is append-only, so the first accepted entry of
  that shape permanently ends replay compatibility with earlier binaries.
- **Gated** — deliberately deferred, with the swap planned rather than assumed.
