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

# choir join <api> <invite-file> <key-file> [--user <name>] [--channel <name>] [--ssh-key <path>] [--token-file <path>]
#   redeem an operator's invite and mint your actor key in one step; --user names the account, which most invites leave for you to pick and which the op log then keeps forever; writes the issued token to an auth file at 0600, and on a node started with --invite-binds-keys the key is registered by the redemption itself
choir_join() {
	choir_run join "$@"
}

# choir workspace <api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>] [--path <prefix>]...
#   provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree this change declares it works within
choir_workspace() {
	choir_run workspace "$@"
}

# choir checkpoint <api> <key-file> <channel> <change-id> <workspace-id> <git-oid>
#   publish an immutable change revision after committing and pushing its Git object
choir_checkpoint() {
	choir_run checkpoint "$@"
}

# choir propose <key-file> <channel> [--api <url>] [--repo <owner/repo>] [--remote <name>] [--onto <branch>] [--change <id>] [--path <prefix>]... [reviewer]...
#   propose from a git checkout in one command: create the change, push the commits, checkpoint the revision and request review; the node and repository come from the git remote, and the branch name is the change identity, so re-running after an amend updates the same proposal
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
#   read log entries from a cursor; --verify checks continuity, recomputes every hash, and verifies the signatures whose keys you hold — SYNC.md as a flag
choir_log() {
	choir_run log "$@"
}

# choir batch <api> <key-file> <channel> <ops-file>
#   sign and submit many operations as one batch — the primary path for agent workloads; one op per line, `-` reads stdin, one result line per op in order
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
#   say something on a review; append-only and permanent, and the comment id is your retry identity
choir_comment() {
	choir_run comment "$@"
}

# choir viewed <api> <key-file> <viewer> <review-id>
#   record that you read a review, so its author can tell "reviewed and ignored" from "nobody looked"; first read only, resubmitting is refused
choir_viewed() {
	choir_run viewed "$@"
}

# choir witness <api> <key-file> <channel>
#   cosign the node's current ref-state attestation (D67); the snapshot id is read from the view rather than passed, so a witness cannot attest a ref-state it did not look at, and the node may not witness its own
choir_witness() {
	choir_run witness "$@"
}

# choir vouch <api> <key-file> <channel> <subject> [note]
#   vouch for another operator; both ends need a key bound in the log, it authorizes nothing on its own, and there is no score
choir_vouch() {
	choir_run vouch "$@"
}

# choir unvouch <api> <key-file> <channel> <subject> '<reason>'
#   withdraw a vouch; the edge leaves the view and both ops stay in the log, so vouching again is allowed and starts a fresh clock
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
#   report one automated check's outcome on a commit; any runner or a person can report by signing, and the node never runs the check
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
#   find a term across every repository you may read; the term is literal, not a pattern
choir_search() {
	choir_run search "$@"
}

# choir reviews <api> <reviewer>
#   your pending review queue
choir_reviews() {
	choir_run reviews "$@"
}

# choir triage <api>
#   every review and change classified into a bucket — landed, awaiting verdicts, changes requested, approved awaiting landing — ranked most-actionable-first, capped, with truncation marked in-band
choir_triage() {
	choir_run triage "$@"
}

# choir state <api> <channel>
#   your bounded next-actions document: verdicts you owe, what your changes need, what you are waiting on, each with a command and its risk
choir_state() {
	choir_run state "$@"
}

# choir skill install [--into <dir>]
#   install the choir agent skill (default .claude/skills), rendered from this binary's own surface table so it can never document another version; re-run after upgrading and unchanged files are left alone
choir_skill() {
	choir_run skill "$@"
}

# choir view <api> [--limit <n>] [--offset <n>]
#   the materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate — every map-shaped section bounded to 200 rows by default, with `<section>_omitted` counting what was left out and `paging.next` naming the request that fetches the rest
choir_view() {
	choir_run view "$@"
}
# --- /generated ---
