# The sync contract

`GET /api/log?from=N` is how a reader catches up: every admitted
operation, in one total order, from a cursor the reader controls. It is
the only endpoint you need to build a replica, and this file is what
another implementation should be written against.

Two things are specified here: what the cursor means, and how to check
that the pages you were handed really are the chain — without trusting
the node that served them.

Every node serves this file at `GET /sync.md`, so an agent that can
reach a node can read the contract without cloning anything.

## The cursor

`from` is an **absolute sequence number**, not a page index and not an
offset into whatever the node currently holds. `from=0` is the first
operation ever admitted. Omitting it means `from=0`.

A page carries at most **500** entries. Ask again with
`from = last_seq + 1` until you get an empty page; an empty `entries`
array means you are caught up, not that something went wrong.

```json
{
  "entries": [ { "seq": 41, "workspace": "...", "...": "..." } ],
  "window_base": 0,
  "source": "window"
}
```

- `window_base` — the oldest seq still in the node's memory. Below it,
  answers come from the persisted log if there is one.
- `source` — `"window"` (served from memory) or `"log"` (served from
  the persisted file). It is provenance, not a difference in content:
  the two paths render entries identically, so a reader that crosses
  the boundary mid-catch-up sees one continuous chain. Use it for
  diagnosis, and expect a single catch-up run to change `source` from
  `log` to `window` as it approaches the head.
- `workspace` is the frozen format-v1 name of the signature-covered
  attribution channel. It is not a workspace id; current submit clients
  call this concept `channel`.

### Outcomes, and what to do about each

| Outcome | Meaning | Do |
|---|---|---|
| `200`, entries non-empty | A page starting at `from` | Verify (below), apply, ask again from `last_seq + 1` |
| `200`, entries empty | You are at the head | Poll again later; there is no push channel by design |
| `409`, `code: "log_evicted"` | `from` is below `window_base` and this node has no persisted log | Restart from `window_base` (the body carries it) and treat that entry as a new anchor — see the gap rule below |
| `500`, `code: "unclassified"` | The node failed to read its own log | Retry; nothing about your request can fix it |

Every rejection body carries `code`, `error` and `next`; the
[rejection-code catalog](ERRORS.md) lists the codes. The 409 additionally
carries `window_base` as a number so you do not have to parse it back out
of prose.

**The gap rule.** If you resume at `window_base` after a 409, the chain
you now hold is in two pieces with a hole between them. Continuity
across that hole is unverifiable *by definition* — the entries that
would prove it are gone. Say so in your own state rather than papering
over it: record the resume point as a new trust anchor, and do not
report the earlier piece and the later piece as one verified chain.

## Verifying a page

Three checks, in increasing strength. They are independent; do as many
as your client can afford.

### 1. Continuity — did I get the chain, whole and in order?

Within a page, for each entry after the first:

```
entry[i].parent == entry[i-1].hash
entry[i].seq    == entry[i-1].seq + 1
```

Across a page boundary, keep the last entry's `hash` from page *N* and
check it against the first entry's `parent` on page *N+1*. This is the
check that makes paging safe: a page that begins at the wrong place, or
a page with an entry dropped out of the middle, fails it. It holds
across a `source` change too — a page served from disk chains to the
next page served from memory.

`parent` is `null` on exactly one entry: seq 0, the genesis.

### 2. Recomputation — is `hash` the truth about this entry?

`hash` is the node's claim. Check it: rebuild the entry's canonical
bytes from the fields in the response, hash them with BLAKE3-256, and
compare. The canonical form is `serde_json` compact output with fields
in this order, no whitespace:

```json
{"format_version":1,"parent":{"codec":30,"digest":[..32 bytes..]},"seq":7,"workspace":"alice","payload":[..bytes..],"witnesses":[],"author_sig":{"key_id":"1e-<64 hex>","signature":[..64 bytes..]}}
```

Rules that are easy to get wrong:

- `parent` is `null` for genesis; otherwise the object above. A
  `ContentHash` is always `{"codec":N,"digest":[bytes]}` — the codec
  byte is part of the hashed form, never a bare digest. `30` is
  `0x1e`, BLAKE3-256.
- `payload` and `signature` are JSON **arrays of byte values**, not hex
  strings. The response hands them to you as hex (`payload_hex`,
  `author_sig_hex`) because hex survives a JSON round-trip without
  anything re-encoding the author's exact bytes; decode to bytes before
  rebuilding.
- `author_sig` is **omitted entirely** when absent, not `null`. An
  unsigned entry's canonical form ends `..."witnesses":[]}`.
- `witnesses` is `[]` today and is still part of the hashed bytes.
  Include it. It becomes non-empty in Phase 2 (D16) and clients that
  guessed it away will break then rather than now.
- `format_version` tells you which of these rules applied. It is `1`.
  New fields only ever arrive additively and omitted-when-absent, so a
  version-1 reader keeps hashing version-1 entries correctly forever.

Then: `hash == "1e-" + hex(blake3(canonical_bytes))`.

### 3. Authorship — did the actor it names actually sign it?

The strongest check, and the one that does not depend on the node at
all. The author signs *content*, not position, so this survives any
lie about ordering:

1. Build the signing bytes: `serde_json` of the two-element tuple
   `[channel, payload]`, taking `channel` from the entry's legacy
   `workspace` field — compact, e.g. `["alice",[104,105]]`.
2. `signing_hash = "1e-" + hex(blake3(those bytes))`.
3. Verify the ed25519 signature `author_sig_hex` over the **ASCII of
   that hex string** (not the raw digest) using the public key you hold
   for `author_key`.

`author_key` is `"1e-" + hex(blake3(public_key_32_bytes))`, so it also
tells you which key to look up, and binds the id to the key material.

**A passkey signature (D39, D45) carries four more fields, and they are
inside the hashed form.** When `author_scheme` is present the signature
is not raw ed25519 over the signing hash: `2` means WebAuthn ES256, an
ECDSA P-256 signature over `authenticator_data ‖ SHA-256(client_data_json)`,
where the `challenge` member inside `client_data_json` is the base64url
of the signing hash from step 2. Both byte strings are served, hex, as
`authenticator_data_hex` and `client_data_json_hex`.

The fourth is `credential_key_hex` (D45): the credential's public key as
SubjectPublicKeyInfo DER, written by the node from the credential it
verified the submission against. **Verify a passkey signature against
this and nothing else.** For an ed25519 entry `author_key` is
`blake3(pubkey)` and you supply the key; for a passkey entry
`author_key` is the credential id, which resolves only inside the
node's account store — so without this field there is no key to check
against and never will be. Entries written before D45 do not carry it
and cannot be checked by anyone.

What that establishes is narrower than the ed25519 case, and the
difference matters: the key arrives *with* the signature, so verifying
proves the bytes are intact and were signed by the credential named, and
proves nothing about whether that credential belonged to `workspace`.
That binding lives in server state which is not part of the log. Report
the two outcomes separately; a verifier that counts them together
reports the weaker claim in the stronger word.

All four are omitted entirely when absent, exactly as they are in the
hashed form, so an ed25519 entry's JSON is unchanged and an older client
sees no new keys. **But a client that ignores them cannot recompute the
hash of a passkey-signed entry** — they are in the canonical bytes, so
leaving them out yields a different digest and a legitimate entry looks
tampered with. Absent `author_scheme` means ed25519, which is the only
scheme that existed before D39; an unrecognised value is a scheme this
client cannot check, which is a different thing from a bad signature and
should be reported as such.

An entry with `author_key: null` is unsigned. The API admits no such
op — even the node's own housekeeping (ref updates converted from a git
push, reviewer draws) is signed with the node's key — so in practice
this is history written before signatures existed. Decide deliberately
what you do with it; do not treat unsigned as verified.

## What none of this catches

**Equivocation.** Every check above tells you that *the chain you were
shown* is internally consistent and authentically authored. None of
them tells you that another reader was shown the same chain. A node
that forks its history and serves two self-consistent versions passes
all three.

One piece of the answer exists already, and it is worth using even
though it does not close the gap. The node emits a **ref-state
attestation** after every accepted ref update: a signed op carrying the
complete namespaced ref map, the seq it describes, and a pointer to the
previous attestation. `/api/view` projects the latest one as
`snapshot: {id, at_seq, prev_snapshot}`. Because the pointer lives
inside the signed payload, the attestations form their own chain that
survives being copied out of the log, and a stale one cannot be served
forever as current.

What that buys a client: something small and comparable. Two readers
can exchange `snapshot.id` at a given `at_seq` and find out whether
they were shown the same ref state, without either of them shipping a
log. A mirror or backup can carry the detached attestation beside its
bundles and check the bundles against something other than themselves.
It is still the node's own signature — a forking node signs both forks
happily — so this is a comparison primitive, not proof.

## Witnesses (D67)

Closing it for real needs a signature that is not the node's. A witness
is an independent operator that reads the attestation and cosigns it,
saying "I saw this complete ref-state at this position". `/api/view`
reports them as `witnessed: {<operator>: {snapshot, at}}`, and
`view_growth.counts` reports two numbers that are not the same one:
`witnesses` is how many have ever cosigned, `witnesses_current` is how
many cosigned the attestation being served right now. The second is the
one to act on. They diverge the moment the refs move, silently, which is
why they are counted separately.

**A cosignature is its own op, not a field on the entry.** The
`witnesses` field on an op-log entry is inside the bytes that entry's
content hash covers, so adding a cosignature after the fact would
rewrite the entry and orphan every entry after it. Witnessing in place
is therefore only possible *before* the append — signatures gathered on
the sequencer's critical path, which is what D16's latency tripwire
exists to avoid. So `OpEntry.witnesses` stays empty, exactly as it
always has, and a witness cosigns the attestation instead. That is the
async branch D16 named as its own alternative, taken for a structural
reason rather than a preference.

What to check as a client:

1. Read `snapshot.id` and `witnessed` from `/api/view`.
2. Count the rows whose `snapshot` equals `snapshot.id`. Rows naming
   anything else are witnesses that have fallen behind, not witnesses of
   what you are being served.
3. Decide what that count has to be before you trust the ordering. There
   is no threshold in the node, deliberately: how many independent
   observers you need is a property of your situation, not of ours.

Two limits worth stating plainly. Only the *latest* attestation can be
cosigned, so a witness that races a new one must re-read and re-sign —
by design, because refs can return to a value they held before, and a
cosignature over the older snapshot would read as a statement about now.
And a witness count is only as independent as the witnesses are: the
node cannot cosign its own attestation, but nothing here can tell you
whether two operator names are two people. Until you have witnesses you
have reason to believe are independent, treat a single node's ordering
as trusted-by-configuration and say so out loud in anything you build on
top.
