#!/usr/bin/env sh
# choir shell library — source this, then call the functions.
#
#   . ./choir.sh
#   choir_env https://node.example ~/.choir/agent.key my-operator/my-agent
#   choir_verify_log
#
# Every agent harness this project ships a snippet for drives the world
# through a shell, so this is the client they can all use. It wraps the
# `choir` binary rather than reimplementing it: signing is ed25519 over
# (channel, payload) plus a log-scope read, which no pure-shell client
# can do and the binary already does correctly.
#
# The per-command wrappers below the marker are generated from the same
# table as `choir --help`, the README, `llms.txt` and `/api/schema`, so
# they cannot drift from the binary. The flows above it are hand-written,
# because their value is judgement about what to do in what order, and
# generating that would flatten the part worth shipping — the same split
# `templates/*/` already uses for prose.
#
# Credentials are never printed, interpolated into a command line, or put
# in an environment variable. `$CHOIR_AUTH_FILE` names a file and the
# binary reads it; a secret on an argv is visible to every process on the
# machine through `ps`.

# --- configuration -----------------------------------------------------

# One place to set the node and the key, so no call site repeats them.
choir_env() {
	CHOIR_API="$1"
	CHOIR_KEY="${2-$CHOIR_KEY}"
	CHOIR_CHANNEL="${3-$CHOIR_CHANNEL}"
	export CHOIR_API CHOIR_KEY CHOIR_CHANNEL
}

# Refuses early, by name, rather than letting the binary fail on an empty
# positional argument three layers down.
choir_require() {
	for _v in "$@"; do
		eval "_set=\${$_v-}"
		if [ -z "$_set" ]; then
			echo "choir.sh: $_v is not set; call choir_env first" >&2
			return 2
		fi
	done
}

# Runs the binary with the auth flags when an auth file is configured.
# Every wrapper goes through this, so the credential path is one line
# rather than one per command.
choir_run() {
	if [ -n "${CHOIR_AUTH_FILE-}" ] && [ -n "${CHOIR_AUTH_USER-}" ]; then
		choir --auth-file "$CHOIR_AUTH_FILE" --auth-user "$CHOIR_AUTH_USER" "$@"
	elif [ -n "${CHOIR_AUTH_FILE-}" ]; then
		choir --auth-file "$CHOIR_AUTH_FILE" "$@"
	else
		choir "$@"
	fi
}

# --- flows -------------------------------------------------------------
#
# The multi-step sequences an agent otherwise rebuilds from curl and jq
# every time. Hand-written on purpose.

# Submit many ops as one batch, reading one JSON op per line from stdin.
#
#   printf '%s\n' "$op1" "$op2" | choir_submit_all
#
# One durability barrier rather than one per op, and one result line per
# op in request order. Nonzero if any op was refused; the lines say which,
# which an exit code cannot carry. A batch is not a transaction — the ops
# that landed, landed.
choir_submit_all() {
	choir_require CHOIR_API CHOIR_KEY CHOIR_CHANNEL || return 2
	choir_run batch "$CHOIR_API" "$CHOIR_KEY" "$CHOIR_CHANNEL" -
}

# Fetch the log from a cursor and verify it: continuity, every hash
# recomputed, and the signatures whose keys are in $CHOIR_KEYS.
#
# A clean exit means the chain holds, not that every author was
# authenticated: an entry whose key you do not hold is reported
# unverified, never verified.
choir_verify_log() {
	choir_require CHOIR_API || return 2
	_from="${1-0}"
	if [ -n "${CHOIR_KEYS-}" ]; then
		choir_run log "$CHOIR_API" --from "$_from" --verify --keys "$CHOIR_KEYS"
	else
		choir_run log "$CHOIR_API" --from "$_from" --verify
	fi
}

# What this node is and what it will accept: the versioned surface plus
# its live capabilities. Read this before branching on whether accounts
# or an ACL exist, rather than probing an endpoint and reading the
# refusal.
choir_capabilities() {
	choir_require CHOIR_API || return 2
	choir_run schema "$CHOIR_API"
}

# Request review and let the node draw the reviewers.
#
# Naming no reviewers is the preferred form: you never pick who reviews
# you, and a review with no reviewers never counts as approved.
choir_request_review() {
	choir_require CHOIR_API CHOIR_KEY CHOIR_CHANNEL || return 2
	_id="$1"
	_oid="$2"
	_ref="${3-}"
	if [ -n "$_ref" ]; then
		choir_run review "$CHOIR_API" "$CHOIR_KEY" "$CHOIR_CHANNEL" "$_id" "$_oid" --ref "$_ref"
	else
		choir_run review "$CHOIR_API" "$CHOIR_KEY" "$CHOIR_CHANNEL" "$_id" "$_oid"
	fi
}

# --- generated: choir surface, do not edit ---

# One function per agent-facing command, forwarding its arguments
# to the binary. Generated from the same table as `choir --help`;
# edit `crates/choir-cli/src/surface.rs` and regenerate.

# choir key <key-file> [name]
#   mint a key and print the line the operator registers; pass your channel name to print the bound form
choir_key() {
	choir_run key "$@"
}

# choir join <link> | <api> <invite-file> <key-file>  [--user <name>] [--channel <name>] [--key-file <path>] [--ssh-key <path>] [--token-file <path>] [--no-clone]
#   redeem an invite link and set this machine up: actor key at ~/.choir/agent.key, token at ~/.choir/auth (0600), a git credential helper for that node, the node URL in ~/.choir/config, and a clone of each repository the invite names in the current directory (--no-clone skips it); the same link run again on this machine clones only what is missing; --user names the account when the invite left it open, asked on the terminal otherwise; the three-argument form takes the invite from a file, answers JSON and touches neither git nor your home directory
choir_join() {
	choir_run join "$@"
}

# choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>] [--path <prefix>]...
#   provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree
choir_workspace() {
	choir_run workspace "$@"
}

# choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
#   publish an immutable change revision after committing and pushing its git object
choir_checkpoint() {
	choir_run checkpoint "$@"
}

# choir propose [reviewer]... [--key-file <path>] [--channel <name>] [--api <url>] [--repo <owner/repo>] [--remote <name>] [--onto <branch>] [--change <id>] [--path <prefix>]...
#   create a change, push its commits and request review, with no arguments; run from a git checkout, with the key and channel from ~/.choir, every value overridable by flag; re-running after an amend updates the same proposal; a leading `<key-file> <channel>` pair is still accepted
choir_propose() {
	choir_run propose "$@"
}

# choir workspace-archive <api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>
#   owner-sign and recoverably archive a bound workspace; exact retries are idempotent
choir_workspace_archive() {
	choir_run workspace-archive "$@"
}

# choir schema <api>
#   print this node's machine-readable API description and its live capabilities
choir_schema() {
	choir_run schema "$@"
}

# choir log <api> [--from <n>] [--verify] [--keys <file>]
#   read log entries from a cursor; --verify checks continuity, recomputes every hash and verifies the signatures whose keys you hold
choir_log() {
	choir_run log "$@"
}

# choir batch <api> <key-file> <channel> <ops-file>
#   sign and submit many operations as one batch, the primary path for agent workloads; one op per line, `-` reads stdin, one result line per op
choir_batch() {
	choir_run batch "$@"
}

# choir review <api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...
#   request review on a commit; name no reviewers and the node draws them
choir_review() {
	choir_run review "$@"
}

# choir verdict <api> <key-file> <reviewer> <id> approve|request-changes [note]
#   answer a review you were assigned
choir_verdict() {
	choir_run verdict "$@"
}

# choir comment <api> <key-file> <channel> <review-id> <comment-id> '<body>'
#   say something on a review; append-only, and the comment id is your retry identity
choir_comment() {
	choir_run comment "$@"
}

# choir viewed <api> <key-file> <viewer> <review-id>
#   record that you read a review; first read only, resubmitting is refused
choir_viewed() {
	choir_run viewed "$@"
}

# choir witness <api> <key-file> <channel>
#   cosign the node's current ref-state attestation (D67); the snapshot id is read from the view, and the node may not witness its own
choir_witness() {
	choir_run witness "$@"
}

# choir vouch <api> <key-file> <channel> <subject> [note]
#   vouch for another operator; both ends need a key bound in the log, and it authorizes nothing on its own
choir_vouch() {
	choir_run vouch "$@"
}

# choir unvouch <api> <key-file> <channel> <subject> '<reason>'
#   withdraw a vouch; both ops stay in the log, and vouching again starts a fresh clock
choir_unvouch() {
	choir_run unvouch "$@"
}

# choir appeal <api> <attempt-id>
#   appeal a rejected newcomer attempt for operator adjudication; never grants privilege
choir_appeal() {
	choir_run appeal "$@"
}

# choir intent <api> <key-file> <channel> <subject> <kind> '<body>'
#   publish a task spec or plan so other agents can see intent
choir_intent() {
	choir_run intent "$@"
}

# choir check <api> <key-file> <channel> <git-oid> <name> passed|failed|running|errored [evidence] [--ref <repo:ref>]
#   report one automated check's outcome on a commit; any runner or person can report by signing, and the node never runs the check
choir_check() {
	choir_run check "$@"
}

# choir checks <api> <git-oid>
#   every check reported on a commit, and one verdict; exits 0 passed, 1 failed or unreported, 3 still running, 4 could not be run
choir_checks() {
	choir_run checks "$@"
}

# choir profile <api> <channel>
#   what the log records about one actor: keys and their age, changes owned, verdicts given, checks reported
choir_profile() {
	choir_run profile "$@"
}

# choir search <api> <term> [--in files|code|commits] [--repo owner/name] [--rev R] [--limit N]
#   find a literal term across every repository you may read
choir_search() {
	choir_run search "$@"
}

# choir reviews <api> <reviewer>
#   your pending review queue
choir_reviews() {
	choir_run reviews "$@"
}

# choir triage <api>
#   every review and change in a bucket (landed, awaiting verdicts, changes requested, approved awaiting landing), most actionable first, capped, with truncation marked in-band
choir_triage() {
	choir_run triage "$@"
}

# choir state <api> <channel>
#   list what you owe and what you are waiting on; every row carries the command that answers it and its risk
choir_state() {
	choir_run state "$@"
}

# choir skill install [--into <dir>]
#   install the choir agent skill (default .claude/skills), rendered from this binary's own surface table; re-run after upgrading
choir_skill() {
	choir_run skill "$@"
}

# choir view <api> [--limit <n>] [--offset <n>]
#   read the materialized view, its ref-state attestation and the node's health counters; map-shaped sections page 200 rows at a time, with `<section>_omitted` and `paging.next`
choir_view() {
	choir_run view "$@"
}

# choir doctor [<api>] [--state <dir>]
#   check everything the other commands assume: the binaries shelled out to, the auth file and its mode, and whether a node answers; each failure prints the fix; on a hosting machine it adds bind address, TLS, certificate expiry, linger, unit state and whether the public URL answers; with `seeds =` in .choir/config, a fork check per seed
choir_doctor() {
	choir_run doctor "$@"
}
# --- /generated ---
