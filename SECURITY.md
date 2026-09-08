# Security

## Reporting a vulnerability

**Do not open a public issue for a security report.**

Use GitHub's private vulnerability reporting on this repository
(*Security* → *Report a vulnerability*), or write to `deadcaf3@pm.me`.

Include the version or commit, the node's startup flags, and the smallest
reproducing request sequence.

One maintainer, no service level agreement: an acknowledgement, a scope
verdict, and a fix or a written reason there will not be one.

## Supported versions

Pre-1.0, no released versions. **Only `main` is supported.** Nothing is
backported.

## What the design assumes

- **The operation log is readable by anyone with a read grant.** It
  carries every ref update, review, verdict and key binding, signed. What
  you submit is durable and visible.
- **A compare-and-swap rejection is a normal outcome**, not a denial of
  service.
- **A merge conflict is a committed value**, not a failure state.
- **The node refuses to bind a non-loopback address without TLS.**
  Enforced in code; a bypass is worth reporting.
- **Secrets live outside the repository**: `~/.choir/` at mode `0600`,
  the daemon key at `<root>/.choir/node.key`. A backup carries the log,
  node fingerprint, policy files and one git bundle per repository, and
  **no** key, token or PEM.

## In scope

- Forging or replaying a signed operation, or getting one accepted whose
  hash chain does not verify.
- Landing on a protected ref without the authorization the policy
  requires, by any transport.
- Reading a repository, review or log entry without a grant, via any
  surface including error messages.
- A key, token or PEM reaching a backup, export, log line, rendered page
  or subprocess argument.
- Escaping the sandbox a CI executor runs a candidate merge in.
- Authorization checks that leak through timing. Token comparison does
  not exit early, on purpose.

## Out of scope

- **Mergiraf's licence or behaviour.** Optional, subprocess only, never
  linked. Report upstream.
- **Defaults you chose yourself.** Rate, body and batch limits and quotas
  are flags.
- **Denial of service by volume** against a node you control or were
  invited to. Exhaustion from one well-formed request is in scope; a
  flood is not.
- Missing hardening headers on a page that serves no credential, absent a
  concrete attack.
- Scanner output with no demonstrated impact.

## Verifying a release yourself

`choir log --verify` walks the hash chain, recomputes every hash and
verifies signatures for the keys you hold. Not a transparency log: no
inclusion proofs, so divergent histories are caught by two readers
comparing. [`SYNC.md`](SYNC.md) is the contract, served at
`GET /sync.md`.

Release artifacts carry a `SHA256SUMS` file and a CycloneDX SBOM per
binary.
