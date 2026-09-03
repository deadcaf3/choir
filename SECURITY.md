# Security

## Reporting a vulnerability

**Do not open a public issue for a security report.**

Use GitHub's private vulnerability reporting on this repository
(*Security* → *Report a vulnerability*), which opens a private advisory
visible only to the maintainers. If that is unavailable to you, write to
`<security-contact>`.

Include what you need to make the problem reproducible: the version or
commit, the configuration flags the node was started with, and the
smallest sequence of requests that shows it. A proof of concept is
welcome and never required.

This project has one maintainer and no service level agreement. You will
get an acknowledgement, a verdict on whether it is in scope, and a fix or
a written reason there will not be one. Nothing here is a promise about
how fast.

## Supported versions

Pre-1.0, and there are no released versions. **Only `main` is
supported.** A fix lands on `main` and nothing is backported, because
there is nothing to backport to.

## What the design assumes

Read these before deciding whether something is a bug. Several of them
look like vulnerabilities and are not.

- **The operation log is readable by anyone holding a read grant on the
  repository.** It carries every ref update, review, verdict and key
  binding, with author signatures. It is designed to be replayed and
  checked by people who do not trust the node. Treat anything you submit
  as durable and visible to that audience.
- **A compare-and-swap rejection is a normal outcome**, not a denial of
  service. Two writers racing one ref means one of them is told which
  head it lost to.
- **A merge conflict is a committed value**, not a failure state. A
  strategy that declines to merge and records a conflict is behaving
  correctly.
- **The node refuses to bind a non-loopback address without TLS.** This
  is enforced in code rather than documented as advice. A configuration
  that appears to bypass it is a report worth making.
- **Secrets live outside the repository**, under `~/.choir/` at mode
  `0600` and the daemon key at `<root>/.choir/node.key`. A backup carries
  the log, the node fingerprint, the policy files and one git bundle per
  repository, and carries **no** key, token, or PEM.

## In scope

Reports in these areas are the ones worth your time and mine:

- Forging or replaying a signed operation, or getting one accepted whose
  hash chain does not verify.
- Landing on a protected ref without the authorization the policy
  requires, by any transport.
- Reading a repository, review or log entry without a grant that permits
  it, including through the browser surface or an error message.
- Any path that puts a key, token or PEM somewhere it should not be: a
  backup, an export, a log line, a rendered page, a subprocess argument.
- Escaping the sandbox a CI executor runs a candidate merge in, or
  reaching the host filesystem from inside one.
- Authorization checks that leak their answer through timing. Token
  comparison does not exit early on length or content, on purpose; a place
  that does is a bug.

## Out of scope

- **Mergiraf's licence or behaviour.** It is an optional external program
  executed as a subprocess, never linked, and the platform works without
  it. Report Mergiraf issues upstream.
- **Defaults you chose yourself.** Rate limits, body limits, batch limits
  and quotas are flags. A node started without them is configured that
  way, not vulnerable.
- **Denial of service by volume** against a node you control or were
  invited to. Report resource exhaustion that a single well-formed
  request can cause; a flood is not a finding.
- Missing hardening headers on a page that serves no credential, absent a
  concrete attack that uses their absence.
- Reports produced only by an automated scanner, with no demonstrated
  impact on this codebase.

## Verifying a release yourself

You do not have to trust a node to check what it served you. `choir log
--verify` walks the hash chain, recomputes every hash, and verifies the
signatures for the keys you hold. [`SYNC.md`](SYNC.md) is the contract it
implements, and every node serves that document at `GET /sync.md`.

Release artifacts carry a `SHA256SUMS` file and a CycloneDX SBOM per
binary.
