//! The agent-facing surface, as data, and the generators that render it.
//!
//! `choir --help`, `docs/using/cli.md`, the README's cheat-sheet, the
//! three `templates/` snippets, the root `AGENTS.md` and the node's
//! `llms.txt` all describe one surface. Hand-maintained, they drift —
//! and they had already started to: the API table carried a throughput
//! figure that three later measurements had superseded.
//!
//! So the surface is described once, here, and everything else is
//! rendered from it. [`crate::surface`] is the source; a staleness test
//! re-renders and compares, so a committed artifact cannot silently fall
//! behind the table.
//!
//! # What is generated and what is not
//!
//! Only the *signatures* — command names, arguments, endpoints,
//! purposes. The conventions around them stay hand-written, because
//! `templates/` is a product deliverable whose value is judgement
//! ("name no reviewers; the node draws them") rather than syntax, and
//! generating prose would flatten exactly the part worth shipping. In
//! the templates the generated region is bounded by
//! [`GEN_START`]/[`GEN_END`] markers and the prose lives outside them.
//!
//! No dependency is added for any of this: rendering markdown from a
//! const table is a few `format!` calls.

/// Opening marker of a generated region in an otherwise authored file.
pub const GEN_START: &str = "<!-- generated: choir surface, do not edit -->";
/// Closing marker of a generated region.
pub const GEN_END: &str = "<!-- /generated -->";

/// The same pair for a shell file, where an HTML comment is a syntax
/// error rather than a comment.
///
/// Learned by running `sh -n` on the first generated copy: the markers
/// spliced in cleanly, the file was well-formed markdown, and `sh`
/// refused it at line 120. A generated artifact nobody executes is one
/// nobody notices is broken.
pub const SH_GEN_START: &str = "# --- generated: choir surface, do not edit ---";
/// Closing marker of a generated region in a shell file.
pub const SH_GEN_END: &str = "# --- /generated ---";

/// Explicit global options for authenticated node access.
pub const AUTH_OPTIONS: &str = "[--auth-file <path>] [--auth-user <name>]";

/// Version of the machine-readable API description (D17).
///
/// Bumped only for a change an existing client cannot ignore: an
/// endpoint removed, a required field added, a field's meaning changed.
/// Adding an endpoint or an optional field is additive and keeps the
/// version, which is the same evolution rule every persisted struct in
/// this workspace follows (invariant 1).
///
/// The row this implements is **one-way-leaning**: third parties build
/// against this surface, so the fallback is a *versioned* API plus a
/// deprecation policy rather than a way back. That is why the version is
/// in the document from its first byte, before anyone depends on it.
pub const API_VERSION: u32 = 1;

use crate::style::Style;

/// Wire names this API still accepts and no longer documents, with what
/// replaced them.
///
/// Stated in the schema rather than left to prose, because a client
/// generated from the schema is exactly the reader who would otherwise
/// build on an alias without knowing it is one.
pub const DEPRECATIONS: &[(&str, &str, &str)] = &[(
    "workspace",
    "channel",
    "v1 alias for the signature-covered attribution channel; accepted on \
     submission for compatibility, and refused when it disagrees with \
     `channel` rather than one silently winning",
)];

/// One `choir` subcommand.
pub struct Command {
    /// Subcommand name.
    pub name: &'static str,
    /// Argument spec as shown in help, e.g. `<api> <key-file>`.
    pub args: &'static str,
    /// One line, imperative, no trailing period.
    pub summary: &'static str,
    /// Whether an agent is expected to reach for this routinely.
    pub agent_facing: bool,
    /// Which section of `--help` this belongs under.
    ///
    /// Help used to be one flat list of every command in the order they
    /// were added, which is the order they were *written* rather than any
    /// order they are read in. Thirty-two lines like that is a wall, and
    /// the reader's actual question — "what do I run to get a change
    /// reviewed" — was answered nowhere on the page.
    ///
    /// A field rather than a lookup table beside the list, so a new
    /// command cannot be added without deciding where it belongs; the
    /// compiler asks.
    pub group: &'static str,
}

/// The help sections, in reading order: what you do first, then the loop
/// you live in, then the things you reach for when something is wrong.
pub const GROUPS: &[&str] = &[
    "getting started",
    "changing code",
    "review",
    "checks",
    "trust",
    "reading the node",
    "operating a node",
];

/// One HTTP endpoint on the node.
pub struct Endpoint {
    /// HTTP method.
    pub method: &'static str,
    /// Path, including any query parameter that is part of the contract.
    pub path: &'static str,
    /// What it is for, one line.
    pub purpose: &'static str,
    /// MCP tool metadata when this endpoint is safe for agents to call.
    /// Internal hook endpoints deliberately carry `None`.
    pub mcp: Option<McpTool>,
}

/// How an MCP tool's arguments become one HTTP request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpArguments {
    /// The endpoint takes no arguments or request body.
    Empty,
    /// The argument object is forwarded as the JSON request body.
    Body,
    /// The named arguments are URL-encoded as query parameters.
    ///
    /// A slice rather than one name because the bounded reads take
    /// `limit` and `offset` alongside whatever they already took, and a
    /// generator that could only carry one would have silently dropped
    /// the paging contract out of every generated client.
    Query {
        /// Names of the arguments, which are also the parameter names.
        parameters: &'static [&'static str],
    },
}

/// The MCP-specific part of one HTTP endpoint.
pub struct McpTool {
    /// Stable programmatic tool name.
    pub name: &'static str,
    /// JSON Schema for the tool's argument object.
    pub input_schema: &'static str,
    /// How those arguments map onto the endpoint.
    pub arguments: McpArguments,
}

const EMPTY_MCP_SCHEMA: &str = r#"{"type":"object","additionalProperties":false}"#;

const SUBMISSION_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "channel": { "type": "string", "description": "Signature-covered attribution channel" },
    "workspace": { "type": "string", "description": "Deprecated v1 alias for channel" },
    "payload_hex": { "type": "string", "description": "Hex-encoded ViewOp payload bytes" },
    "key_id": { "type": "string", "description": "Actor key id" },
    "signature_hex": { "type": "string", "description": "Hex-encoded submission signature" }
  },
  "required": ["payload_hex", "key_id", "signature_hex"],
  "anyOf": [
    { "required": ["channel"] },
    { "required": ["workspace"] }
  ],
  "additionalProperties": false
}"#;

const BATCH_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "ops": {
      "type": "array",
      "description": "Signed operations, admitted in array order",
      "items": {
        "type": "object",
        "properties": {
          "channel": { "type": "string", "description": "Signature-covered attribution channel" },
          "workspace": { "type": "string", "description": "Deprecated v1 alias for channel" },
          "payload_hex": { "type": "string" },
          "key_id": { "type": "string" },
          "signature_hex": { "type": "string" }
        },
        "required": ["payload_hex", "key_id", "signature_hex"],
        "anyOf": [
          { "required": ["channel"] },
          { "required": ["workspace"] }
        ],
        "additionalProperties": false
      }
    }
  },
  "required": ["ops"],
  "additionalProperties": false
}"#;

const WORKSPACE_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "repo": { "type": "string", "description": "Repository name as owner/repo" },
    "name": { "type": "string", "description": "Workspace name" },
    "base": { "type": "string", "description": "Exact full Git commit oid; advanced requests provide this with owner, change and idempotency_key" },
    "owner": { "type": "string", "description": "Registered signing channel allowed to checkpoint and archive the stable change" },
    "change": { "type": "string", "description": "Stable logical change id" },
    "idempotency_key": { "type": "string", "description": "Owner-scoped create retry identity" },
    "channel": { "type": "string", "description": "Owner and signature-covered attribution channel" },
    "payload_hex": { "type": "string", "description": "Hex-encoded CreateAuthorization for the exact binding" },
    "key_id": { "type": "string", "description": "Owner key id" },
    "signature_hex": { "type": "string", "description": "Owner signature over the create authorization" }
  },
  "required": ["repo", "name"],
  "oneOf": [
    {
      "not": {
        "anyOf": [
          { "required": ["base"] }, { "required": ["owner"] },
          { "required": ["change"] }, { "required": ["idempotency_key"] },
          { "required": ["channel"] }, { "required": ["payload_hex"] },
          { "required": ["key_id"] }, { "required": ["signature_hex"] }
        ]
      }
    },
    {
      "required": ["base", "owner", "change", "idempotency_key", "channel", "payload_hex", "key_id", "signature_hex"]
    }
  ],
  "additionalProperties": false
}"#;

const WORKSPACE_ARCHIVE_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "repo": { "type": "string", "description": "Repository name as owner/repo" },
    "name": { "type": "string", "description": "Workspace name" },
    "change": { "type": "string", "description": "Stable logical change id returned by creation" },
    "idempotency_key": { "type": "string", "description": "Bound create retry identity" },
    "channel": { "type": "string", "description": "Bound change owner and signature-covered attribution channel" },
    "payload_hex": { "type": "string", "description": "Hex-encoded ArchiveAuthorization naming this exact change, workspace and revision" },
    "key_id": { "type": "string", "description": "Owner key id" },
    "signature_hex": { "type": "string", "description": "Hex-encoded owner submission signature" }
  },
  "required": ["repo", "name", "change", "idempotency_key", "channel", "payload_hex", "key_id", "signature_hex"],
  "additionalProperties": false
}"#;

const LOG_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "from": { "type": "integer", "minimum": 0, "description": "Absolute sequence cursor" }
  },
  "required": ["from"],
  "additionalProperties": false
}"#;

/// The two parameters every bounded read takes.
///
/// Neither is required, and that is the contract: a caller who names
/// nothing is still served a bounded page. There is no value of `limit`
/// that turns the budget off — the node clamps to `1..=1000` — so paging
/// is the way to read a large view, not a fallback for one.
const PAGING_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Rows per section; defaults to 200 and clamps to this range" },
    "offset": { "type": "integer", "minimum": 0, "description": "Rows skipped per section, in key order" }
  },
  "additionalProperties": false
}"#;

const REVIEWS_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "reviewer": { "type": "string", "description": "Reviewer channel name" },
    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Rows per section; defaults to 200 and clamps to this range" },
    "offset": { "type": "integer", "minimum": 0, "description": "Rows skipped per section, in key order" }
  },
  "required": ["reviewer"],
  "additionalProperties": false
}"#;

const PROFILE_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "channel": { "type": "string", "description": "The name an actor signs as" }
  },
  "required": ["channel"],
  "additionalProperties": false
}"#;

const SEARCH_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "q": { "type": "string", "description": "Literal term, not a pattern; matching is case-insensitive" },
    "in": { "type": "string", "enum": ["files", "code", "commits"], "description": "What to look through; defaults to code" },
    "repo": { "type": "string", "description": "One repository as owner/name; omit to search every repository you may read" },
    "rev": { "type": "string", "description": "Revision to search, for a single repo= only; defaults to HEAD" },
    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Matches returned; defaults to 200. `matches` counts everything found either way" }
  },
  "required": ["q"],
  "additionalProperties": false
}"#;

const APPEAL_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "attempt_id": { "type": "integer", "minimum": 0, "description": "Newcomer attempt id returned with the rejection" }
  },
  "required": ["attempt_id"],
  "additionalProperties": false
}"#;

/// Every `choir` subcommand, in help order.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "host",
        args: "[--domain <name> | --public [--ip <addr>] | --public-name <name>] \
               [--port <n>] [--repo <owner/name.git>] [--invite <name>] [--state <dir>] \
               [--yes] [--dry-run] [--foreground] [-- <daemon flags>]",
        summary: "take this machine from nothing to a running node and print its URL; bare binds loopback, --domain issues a Let's Encrypt certificate for a name you own, --public uses a magic-DNS name over this box's address, --foreground execs the daemon instead of installing a unit",
        agent_facing: false,
        group: "getting started",
    },
    Command {
        name: "init",
        args: "[<state-dir>] [--port <n>] [--force]",
        summary: "set up a node's layout on this machine: repository root, credential at 0600, an actor key the node trusts, and .choir/config; refuses to overwrite what exists",
        agent_facing: false,
        group: "getting started",
    },
    Command {
        name: "key",
        args: "<key-file> [name]",
        summary: "mint a key and print the line the operator registers; pass your channel name to print the bound form",
        agent_facing: true,
        group: "getting started",
    },
    Command {
        name: "git-credential",
        args: "<auth-file> [--auth-user <name>] get|store|erase",
        summary: "git credential helper: hands git your token on stdin so it never lives in a remote URL; configure with `git config credential.helper '!choir git-credential <auth-file>'`",
        agent_facing: false,
        group: "getting started",
    },
    Command {
        name: "join",
        args: "<link> | <api> <invite-file> <key-file>  [--user <name>] [--channel <name>] [--key-file <path>] [--ssh-key <path>] [--token-file <path>]",
        summary: "redeem an invite link and set this machine up: actor key at ~/.choir/agent.key, token at ~/.choir/auth (0600), a git credential helper for that node, and the node URL in ~/.choir/config; --user names the account when the invite left it open, asked on the terminal otherwise; the three-argument form takes the invite from a file, answers JSON and touches neither git nor your home directory",
        agent_facing: true,
        group: "getting started",
    },
    Command {
        name: "invite",
        args: "<api> <name> <owner/repo> [read|write]",
        summary: "mint an invite and print the one link to send; the same thing the /people page does",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "asks",
        args: "<api>",
        summary: "who has asked for access and is waiting on an answer (D72)",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "grant",
        args: "<api> <request-id> <owner/repo> [read|write]",
        summary: "let one of them in; the link they already hold becomes their invite",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "decline",
        args: "<api> <request-id>",
        summary: "drop a pending request; their link then reads as never valid",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "workspace",
        args: "<api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>] [--path <prefix>]...",
        summary: "provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "checkpoint",
        args: "<api> <key-file> <channel> <change-id> <workspace-id> <git-oid>",
        summary: "publish an immutable change revision after committing and pushing its git object",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "propose",
        args: "[reviewer]... [--key-file <path>] [--channel <name>] [--api <url>] [--repo <owner/repo>] [--remote <name>] [--onto <branch>] [--change <id>] [--path <prefix>]...",
        summary: "create a change, push its commits and request review, with no arguments; run from a git checkout, with the key and channel from ~/.choir, every value overridable by flag; re-running after an amend updates the same proposal; a leading `<key-file> <channel>` pair is still accepted",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "workspace-archive",
        args: "<api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>",
        summary: "owner-sign and recoverably archive a bound workspace; exact retries are idempotent",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "runner",
        args: "<config-file>",
        summary: "drive one workspace lifecycle step for an orchestrator; JSON request on stdin, JSON result on stdout",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "submit",
        args: "<api> <key-file> <channel> '<op-json>'",
        summary: "sign and submit one raw operation",
        agent_facing: false,
        group: "changing code",
    },
    Command {
        name: "schema",
        args: "<api>",
        summary: "print this node's machine-readable API description and its live capabilities",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "log",
        args: "<api> [--from <n>] [--verify] [--keys <file>]",
        summary: "read log entries from a cursor; --verify checks continuity, recomputes every hash and verifies the signatures whose keys you hold",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "batch",
        args: "<api> <key-file> <channel> <ops-file>",
        summary: "sign and submit many operations as one batch, the primary path for agent workloads; one op per line, `-` reads stdin, one result line per op",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "review",
        args: "<api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...",
        summary: "request review on a commit; name no reviewers and the node draws them",
        agent_facing: true,
        group: "review",
    },
    Command {
        name: "verdict",
        args: "<api> <key-file> <reviewer> <id> approve|request-changes [note]",
        summary: "answer a review you were assigned",
        agent_facing: true,
        group: "review",
    },
    Command {
        name: "comment",
        args: "<api> <key-file> <channel> <review-id> <comment-id> '<body>'",
        summary: "say something on a review; append-only, and the comment id is your retry identity",
        agent_facing: true,
        group: "review",
    },
    Command {
        name: "viewed",
        args: "<api> <key-file> <viewer> <review-id>",
        summary: "record that you read a review; first read only, resubmitting is refused",
        agent_facing: true,
        group: "review",
    },
    Command {
        name: "witness",
        args: "<api> <key-file> <channel>",
        summary: "cosign the node's current ref-state attestation (D67); the snapshot id is read from the view, and the node may not witness its own",
        agent_facing: true,
        group: "trust",
    },
    Command {
        name: "vouch",
        args: "<api> <key-file> <channel> <subject> [note]",
        summary: "vouch for another operator; both ends need a key bound in the log, and it authorizes nothing on its own",
        agent_facing: true,
        group: "trust",
    },
    Command {
        name: "unvouch",
        args: "<api> <key-file> <channel> <subject> '<reason>'",
        summary: "withdraw a vouch; both ops stay in the log, and vouching again starts a fresh clock",
        agent_facing: true,
        group: "trust",
    },
    Command {
        name: "slash",
        args: "<api> <node-key-file> <id> <reviewer> '<reason>'",
        summary: "invalidate one reviewer's approval; operator-only and never moves a ref",
        agent_facing: false,
        group: "review",
    },
    Command {
        name: "abandon",
        args: "<api> <node-key-file> <id>",
        summary: "archive a stale incomplete review as lapsed, settling it unapproved; operator-only and never moves a ref",
        agent_facing: false,
        group: "review",
    },
    Command {
        name: "bind",
        args: "<api> <node-key-file> <operator> <key-hex> [channel]",
        summary: "record in the log that a key belongs to an operator; operator-only and never moves a ref",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "revoke",
        args: "<api> <node-key-file> <key-hex> '<reason>'",
        summary: "withdraw a key binding; terminal, and the attribution row survives",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "appeal",
        args: "<api> <attempt-id>",
        summary: "appeal a rejected newcomer attempt for operator adjudication; never grants privilege",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "intent",
        args: "<api> <key-file> <channel> <subject> <kind> '<body>'",
        summary: "publish a task spec or plan so other agents can see intent",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "check",
        args: "<api> <key-file> <channel> <git-oid> <name> passed|failed|running|errored [evidence] [--ref <repo:ref>]",
        summary: "report one automated check's outcome on a commit; any runner or person can report by signing, and the node never runs the check",
        agent_facing: true,
        group: "checks",
    },
    Command {
        name: "checks",
        args: "<api> <git-oid>",
        summary: "every check reported on a commit, and one verdict; exits 0 passed, 1 failed or unreported, 3 still running, 4 could not be run",
        agent_facing: true,
        group: "checks",
    },
    Command {
        name: "profile",
        args: "<api> <channel>",
        summary: "what the log records about one actor: keys and their age, changes owned, verdicts given, checks reported",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "search",
        args: "<api> <term> [--in files|code|commits] [--repo owner/name] [--rev R] [--limit N]",
        summary: "find a literal term across every repository you may read",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "reviews",
        args: "<api> <reviewer>",
        summary: "your pending review queue",
        agent_facing: true,
        group: "review",
    },
    Command {
        name: "acl render",
        args: "<api> <acl-file>",
        summary: "rewrite an ACL file's trailing comments to name the person behind each handle; grants are copied through unchanged",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "triage",
        args: "<api>",
        summary: "every review and change in a bucket (landed, awaiting verdicts, changes requested, approved awaiting landing), most actionable first, capped, with truncation marked in-band",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "funnel",
        args: "<api>",
        summary: "the contribution funnel from admission to first verdict and the steepest drop between stages; counts what this credential may read, and reports an unmeasured stage as null",
        agent_facing: false,
        group: "reading the node",
    },
    Command {
        name: "state",
        args: "<api> <channel>",
        summary: "list what you owe and what you are waiting on; every row carries the command that answers it and its risk",
        agent_facing: true,
        group: "changing code",
    },
    Command {
        name: "docs",
        args: "[--open]",
        summary: "build the book from `docs/` with the API documentation inside it at `book/api/`; needs a checkout and `mdbook`, and names the install command if it is missing",
        // A contributor's command, not an agent's: it builds a local
        // tree from a checkout and touches no node. An agent that wants
        // this surface reads `/llms.txt` from a running one.
        agent_facing: false,
        group: "getting started",
    },
    Command {
        name: "skill",
        args: "install [--into <dir>]",
        summary: "install the choir agent skill (default .claude/skills), rendered from this binary's own surface table; re-run after upgrading",
        agent_facing: true,
        group: "getting started",
    },
    Command {
        name: "view",
        args: "<api> [--limit <n>] [--offset <n>]",
        // The endpoint row in `docs/using/cli.md` enumerates every
        // section this returns and is the reference for them. This line
        // named all eight, which made it the longest summary in the
        // table by a factor of four and a second copy of that list.
        summary: "read the materialized view, its ref-state attestation and the node's health counters; map-shaped sections page 200 rows at a time, with `<section>_omitted` and `paging.next`",
        agent_facing: true,
        group: "reading the node",
    },
    Command {
        name: "repo create",
        args: "<api> <owner/repo.git>",
        summary: "create a repository on a running node, sequenced from its first push; needs a node-wide write grant, answers 409 when it already exists, and prints the clone URL",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "repo list",
        args: "<api>",
        summary: "the repositories on a node this credential can read, one per line; an ACL narrows the list rather than refusing it",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "repo url",
        args: "<api> <owner/repo.git>",
        summary: "the clone URL for a repository, and the one line of git configuration that makes pushing work; the credential is never put in the URL",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node serve",
        args: "[--state <dir>] [--port <n>] [--create <owner/repo.git>] [-- <daemon flags>]",
        summary: "run the node in this terminal, deriving root, credential and trusted keys from what `choir init` wrote; execs the daemon so signals and the exit code reach the real process",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node install",
        args: "[--state <dir>] [--port <n>] [-- <daemon flags>]",
        summary: "hand the node to launchd (macOS) or a systemd user unit (Linux) so it survives logout, crash and reboot; the unit runs `choir node serve`",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node tls",
        args: "<domain> --user <account> [--port <n>] [--dry-run | --staging]",
        summary: "obtain a Let's Encrypt certificate for this node and wire up renewal: certbot, a deploy hook that re-projects the pair and restarts the node, and the marker `node serve` reads; the only command here that expects root, and `--user` is required",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node stop",
        args: "",
        summary: "stop the supervised node for this boot, leaving the unit in place; `node uninstall` is the one that ends it",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node restart",
        args: "",
        summary: "reload the unit and start it again, which is how a rebuilt binary reaches the running node",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node uninstall",
        args: "",
        summary: "stop the node and remove its unit; the state directory, with the keys, repositories and op log, is kept",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node logs",
        args: "[<lines>] [--state <dir>]",
        summary: "the tail of the node's log; defaults to the last 30 lines",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "node status",
        args: "[<api>]",
        summary: "health, the commit serving, the sequencer's position and its p99 against the 100 ms gate, and how much this credential can see",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "doctor",
        args: "[<api>] [--state <dir>]",
        summary: "check everything the other commands assume: the binaries shelled out to, the auth file and its mode, and whether a node answers; each failure prints the fix; on a hosting machine it adds bind address, TLS, certificate expiry, linger, unit state and whether the public URL answers",
        // The one command worth reaching for when nothing else works,
        // so it is not gated on being an agent's habit.
        agent_facing: true,
        group: "operating a node",
    },
    Command {
        name: "backup verify",
        args: "<backup-dir>",
        summary: "whether a backup can be restored from: the four files, the manifest checksum, the hash chain, the policy archive and every git bundle, refusing a backup that carries a key or credential; every check local",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "backup restore",
        args: "<backup-dir> <target-root>",
        summary: "turn a backup back into a node and prove it by accepting a real push: reads and refuses before writing, unbundles git objects before the first boot, rehearses on a port it picks; exit 3 means a secret only you can supply is missing",
        agent_facing: false,
        group: "operating a node",
    },
    Command {
        name: "repair",
        args: "<log-file> --verify | --truncate-tail",
        summary: "inspect a stopped node's op log, or repair a tail that was still being written; `--verify` changes nothing, `--truncate-tail` quarantines the partial record before cutting, and damage anywhere but the tail is refused",
        agent_facing: false,
        group: "operating a node",
    },
];

/// Every endpoint the node serves, in the order `docs/using/cli.md`
/// lists them.
pub const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        method: "POST",
        path: "/api/submit",
        purpose: "Submit one signed operation (hex payload, hex signature)",
        mcp: Some(McpTool {
            name: "choir_submit",
            input_schema: SUBMISSION_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "POST",
        path: "/api/submit-batch",
        purpose: "Same, in array order; the primary path for agent workloads",
        mcp: Some(McpTool {
            name: "choir_submit_batch",
            input_schema: BATCH_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/view?limit=N&offset=M",
        purpose: "The materialized view plus the latest ref-state attestation, key bindings, T2 review outcomes, T3 concentration, T4 newcomer harm, view growth, the build commit, and the sequencer's p99 against the 100 ms gate. Under an ACL you get your own slice; node-wide sections need a node-wide grant, and a missing repository is one you were not granted. Map-shaped sections are bounded: `limit` rows (200 default, 1000 max), `offset`, `<section>_omitted`, and `paging.next`",
        mcp: Some(McpTool {
            name: "choir_view",
            input_schema: PAGING_MCP_SCHEMA,
            arguments: McpArguments::Query {
                parameters: &["limit", "offset"],
            },
        }),
    },
    Endpoint {
        method: "POST",
        path: "/api/appeal",
        purpose: "Record an appeal for a rejected newcomer attempt; requests operator adjudication and never changes privilege",
        mcp: Some(McpTool {
            name: "choir_appeal",
            input_schema: APPEAL_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/log?from=N",
        purpose: "Ordered log entries, the catch-up and sync primitive. Absolute `from`; evicted entries are served from the persisted log (`source` says which), and a node that cannot reach back answers 409. Each entry carries hash, parent and author signature; SYNC.md is the verification procedure",
        mcp: Some(McpTool {
            name: "choir_log",
            input_schema: LOG_MCP_SCHEMA,
            arguments: McpArguments::Query { parameters: &["from"] },
        }),
    },
    Endpoint {
        method: "POST",
        path: "/api/workspace",
        purpose: "Provision a CoW workspace; optional exact base/change binding makes retries idempotent",
        mcp: Some(McpTool {
            name: "choir_workspace",
            input_schema: WORKSPACE_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "POST",
        path: "/api/workspace/archive",
        purpose: "Recoverably archive a change-bound workspace and remove it from the active view",
        mcp: Some(McpTool {
            name: "choir_workspace_archive",
            input_schema: WORKSPACE_ARCHIVE_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/reviews?reviewer=X",
        purpose: "One actor's pending review queue",
        mcp: Some(McpTool {
            name: "choir_reviews",
            input_schema: REVIEWS_MCP_SCHEMA,
            arguments: McpArguments::Query {
                parameters: &["reviewer", "limit", "offset"],
            },
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/schema",
        purpose: "This surface, machine-readable and versioned, plus what this node will accept; the description an agent generates a client from (D17)",
        // A tool like any other: "what will this node accept" is a
        // question an agent asks before branching, and routing it
        // through the same authenticated client is what keeps a
        // credential off a `curl` command line in the shell library.
        mcp: Some(McpTool {
            name: "choir_schema",
            input_schema: EMPTY_MCP_SCHEMA,
            arguments: McpArguments::Empty,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/search?q=X&in=code&repo=owner/name&rev=R&limit=N",
        purpose: "Search repository contents, file names or commit messages across every repository you may read, each at HEAD; `rev` needs a single `repo`. Ungranted repositories are absent, or answered as nonexistent by name. Unindexed (`git grep`): `limit` bounds the results, `matches` counts everything, `truncated` says which",
        mcp: Some(McpTool {
            name: "choir_search",
            input_schema: SEARCH_MCP_SCHEMA,
            arguments: McpArguments::Query {
                parameters: &["q", "in", "repo", "rev", "limit"],
            },
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/profile?channel=X",
        purpose: "One actor's standing out of the view you may see: bound keys and their age, changes owned, reviews assigned and verdicts given, approvals slashed, checks reported, and `vouches` with direction. Two callers with different grants get different numbers. No score; time-locked grants (D66) live outside the log and are not counted",
        mcp: Some(McpTool {
            name: "choir_profile",
            input_schema: PROFILE_MCP_SCHEMA,
            arguments: McpArguments::Query {
                parameters: &["channel"],
            },
        }),
    },
    Endpoint {
        method: "GET",
        path: "/llms.txt",
        purpose: "This surface, as text, for an agent that has never seen choir",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/sync.md",
        purpose: "The sync contract: cursor semantics and how to verify a page's hash chain and author signatures",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/api/repos",
        purpose: "Which repositories this credential can see, read from the filesystem; `narrowed` says whether an ACL was applied",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/repo",
        purpose: "Create a repository on a running node with the `pre-receive` hook that sequences its pushes; needs a node-wide write grant; appends nothing to the log",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/api/ref-agreement",
        purpose: "Where the op log and the bare repos disagree about a ref, read-only",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/invite",
        purpose: "Mint a single-use, expiring invite and the grants it will hold; needs a node-wide write grant and can never issue one; a grant may carry `until=<unix seconds>` (D66)",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/redeem",
        purpose: "Redeem an invite, presented as the credential, for a token, once, and register an ssh key",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/request/grant",
        purpose: "Answer an access request (D72): turns it into an invite under the id and secret the asker already holds",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/request/decline",
        purpose: "Drop a pending access request; their link then reads as never valid",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/revoke",
        purpose: "Delete an account: its token stops authenticating on the next request, and its grants and keys go with it",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/api/accounts",
        purpose: "Who holds an account, what they were granted, and which invites are outstanding; never a secret or its hash",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/git-update",
        purpose: "Internal: the pre-receive hook callback",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/git-abort",
        purpose: "Internal: retracts a refused push's already-accepted refs",
        mcp: None,
    },
];

/// MCP tools in deterministic endpoint-table order.
///
/// The order is deliberately not sorted at runtime: stable tool order
/// improves client prompt-cache hits, and table order is the one source
/// shared with the reference page and the discovery documents.
#[must_use]
pub fn mcp_tools() -> Vec<serde_json::Value> {
    ENDPOINTS
        .iter()
        .filter_map(|endpoint| {
            let tool = endpoint.mcp.as_ref()?;
            let schema: serde_json::Value =
                serde_json::from_str(tool.input_schema).expect("static MCP schema is valid JSON");
            Some(serde_json::json!({
                "name": tool.name,
                "description": endpoint.purpose,
                "inputSchema": schema,
            }))
        })
        .collect()
}

/// Finds the HTTP endpoint backing an MCP tool.
#[must_use]
pub fn mcp_endpoint(name: &str) -> Option<&'static Endpoint> {
    ENDPOINTS
        .iter()
        .find(|endpoint| endpoint.mcp.as_ref().is_some_and(|tool| tool.name == name))
}

/// Finds an HTTP endpoint by method and path.
///
/// Separate from [`mcp_endpoint`] because a few endpoints are
/// deliberately outside the MCP surface — `GET /api/accounts` is the
/// roster, which is the operator's to read and not an agent tool — and
/// the CLI still has to reach them. Looking them up here rather than
/// spelling a path into a command keeps the table the one description of
/// what this node serves.
#[must_use]
pub fn endpoint(method: &str, path: &str) -> Option<&'static Endpoint> {
    ENDPOINTS
        .iter()
        .find(|endpoint| endpoint.method == method && endpoint.path == path)
}

/// The `choir` usage block, as a bare invocation prints it.
///
/// Names and one-line summaries only, grouped. The full argument spec of
/// a command is a line of its own and there are thirty-two of them; put
/// them all here and the reader scans a wall of `<api> <key-file>` for
/// the one word they came for. `choir <command> --help` prints the spec
/// for one command, which is the question anybody actually has.
#[must_use]
pub fn usage() -> String {
    usage_in(Style::plain())
}

/// [`usage`], styled for a terminal.
///
/// One renderer, two callers: the plain form is what the tests read,
/// and a second copy of this layout would be a second place for the
/// index to be wrong.
#[must_use]
pub fn usage_in(style: Style) -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = format!("{}\n  choir <command> [args]\n", style.bold("usage:"));
    out.push_str(&format!("  choir {AUTH_OPTIONS} <command> [args]\n"));
    out.push_str("  choir <command> --help\n");
    for group in GROUPS {
        out.push_str(&format!("\n{}\n", style.bold(group)));
        for c in COMMANDS.iter().filter(|c| &c.group == group) {
            // First clause only. A summary here earns one line, and the
            // clauses after the first are the caveats -- which belong on
            // the command's own help, next to the argument they qualify.
            let short = first_clause(c.summary, 58);
            // Padded before painting: an escape sequence has width in
            // bytes and none on screen, so `{:width$}` over a painted
            // name indents every line differently.
            let name = format!("{:width$}", c.name);
            out.push_str(&format!("  {}  {}\n", style.cyan(&name), style.dim(&short)));
        }
    }
    out.push_str(&format!(
        "\n{}\n",
        style.dim(
            "Most commands take the node's URL as their first argument, and fill it in \n\
             when you leave it out. It is looked for as `node = <url>` in `.choir/config` \n\
             in this directory, then in each directory above it, then in `~/.choir/config` \n\
             -- so a checkout that names its own node wins, and the one `choir join` \n\
             wrote answers everywhere else. An explicit URL always wins over both."
        )
    ));
    out.push_str(&format!(
        "\n{}\n",
        style.dim(
            "Exit codes: 0 accepted, 1 the node rejected (its JSON error body is \
             printed), 2 usage error."
        )
    ));
    out
}

/// A summary's opening clause, cut to `max` on a word boundary.
///
/// The index has one line per command and the summaries are written as
/// several clauses, so an uncut one wraps and the list stops being a
/// list. What is cut is always available: `choir <command> --help`
/// prints the whole thing.
fn first_clause(summary: &str, max: usize) -> String {
    let clause = summary.split(';').next().unwrap_or(summary).trim();
    if clause.chars().count() <= max {
        return clause.to_string();
    }
    let mut cut = String::new();
    for word in clause.split_whitespace() {
        // +1 for the space, +1 for the ellipsis that will follow.
        if cut.chars().count() + word.chars().count() + 2 > max {
            break;
        }
        if !cut.is_empty() {
            cut.push(' ');
        }
        cut.push_str(word);
    }
    // Trailing punctuation before an ellipsis reads as a typo.
    while cut.ends_with(',') || cut.ends_with('—') || cut.ends_with('-') {
        cut.pop();
        cut = cut.trim_end().to_string();
    }
    format!("{cut}…")
}

/// The help for one command: its full spec and its whole summary.
#[must_use]
pub fn command_help(name: &str) -> Option<String> {
    command_help_in(name, Style::plain())
}

/// [`command_help`], styled for a terminal.
#[must_use]
pub fn command_help_in(name: &str, style: Style) -> Option<String> {
    let c = COMMANDS.iter().find(|c| c.name == name)?;
    let mut out = format!("  choir {} {}\n\n", style.cyan(c.name), style.dim(c.args));
    // The summary's clauses, one per line. They are written as one
    // sentence of several clauses, and read far better as a short list
    // than as a paragraph wrapped by the terminal.
    for (n, clause) in c.summary.split(';').enumerate() {
        let clause = clause.trim();
        if n == 0 {
            out.push_str(&format!("  {clause}\n"));
        } else {
            out.push_str(&format!("    - {clause}\n"));
        }
    }
    if c.args.starts_with("<api>") {
        out.push_str(&format!(
            "\n  {}\n",
            style.dim(
                "<api> may be omitted when `.choir/config` names a node, here or in\n  \
                 any parent directory."
            )
        ));
    }
    Some(out)
}

/// The endpoint table `docs/using/cli.md` carries.
#[must_use]
pub fn api_table() -> String {
    let mut out = String::from("| Endpoint | Purpose |\n|---|---|\n");
    for e in ENDPOINTS {
        out.push_str(&format!("| `{} {}` | {} |\n", e.method, e.path, e.purpose));
    }
    out
}

/// The generated API and CLI reference in `docs/using/cli.md`.
///
/// Named for the document it fills rather than for the README, which is
/// where it used to live. The README now carries [`readme_cheatsheet`]
/// instead: a reader arriving at a repository wants the shortest path to
/// a first change, and thirty-two full argument specs is not it.
#[must_use]
pub fn cli_doc_surface() -> String {
    format!(
        "### HTTP endpoints\n\n{}\n### The `choir` CLI\n\n{}",
        api_table(),
        cli_reference()
    )
}

/// The commands a first contribution needs, in reading order.
///
/// `key` and `join` are alternatives rather than steps: an operator
/// mints a key, somebody holding an invite runs `join` and gets one.
/// The rest are the loop.
///
/// A list of names rather than a `day_one` field on [`Command`],
/// deliberately, and the argument cuts the other way from the one
/// [`Command::group`] makes. Every command must belong to *some* help
/// section, so a field is right there and the compiler should ask. No
/// command has to be on a getting-started path, so a new one is
/// presumptively absent, and a field would ask thirty-two questions
/// whose answer is `false`.
///
/// Checked against [`COMMANDS`] by the surface test: a name here that no
/// longer exists fails rather than silently rendering nothing.
pub const DAY_ONE: &[&str] = &[
    "host",
    "key",
    "join",
    "workspace",
    "propose",
    "reviews",
    "verdict",
    "state",
    "log",
];

/// The README's cheat-sheet: the day-one commands and nothing else.
///
/// Generated rather than hand-written for the reason every other table
/// here is. A short list beside a complete one is the drift this module
/// exists to stop, and a cheat-sheet is exactly the kind of document
/// that gets written once and then quietly stops being true.
#[must_use]
pub fn readme_cheatsheet() -> String {
    let mut out = String::from("| Command | What it does |\n|:--|:--|\n");
    for name in DAY_ONE {
        if let Some(c) = COMMANDS.iter().find(|c| &c.name == name) {
            // First clause only, for the reason `usage_in` takes it: the
            // clauses after the first are caveats, and a caveat in a
            // table cell wraps the row to three lines and stops the
            // table being scannable. This is the shortest of the three
            // renderings and the one read first, so it is the one that
            // can least afford them.
            let short = first_clause(c.summary, 64);
            out.push_str(&format!("| `choir {}` | {} |\n", c.name, short));
        }
    }
    out.push_str("\nFull surface, every command and every endpoint: [`docs/using/cli.md`](docs/using/cli.md).\n");
    out
}

/// The command reference in `docs/using/cli.md`: every command, its full
/// argument spec, and what it is for.
///
/// Not [`usage`]. Help is an index — the reader is at a prompt and wants
/// the name of the thing, and thirty-two full argument specs is what
/// they have to read past to find it. A reference page is the opposite
/// situation: the reader is already looking the command up, and the
/// specs are the reason they came. Rendering both from the same table
/// keeps them from disagreeing without pretending they answer the same
/// question.
///
/// [`readme_cheatsheet`] is the third answer to the same table, for the
/// third reader: somebody deciding whether to try this at all.
#[must_use]
pub fn cli_reference() -> String {
    let mut out = String::new();
    for group in GROUPS {
        out.push_str(&format!("**{group}**\n\n"));
        for c in COMMANDS.iter().filter(|c| &c.group == group) {
            out.push_str(&format!(
                "- `choir {} {}`  \n  {}\n",
                c.name, c.args, c.summary
            ));
        }
        out.push('\n');
    }
    out.push_str(
        "Most commands take the node's URL first. Put `node = <url>` in `.choir/config`, \
         in the working directory or any parent, and it is filled in when omitted. \
         `choir <command> --help` prints one command's spec.\n\n\
         Exit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), \
         2 usage error.\n",
    );
    out
}

/// The command list the agent templates carry, as a markdown bullet list.
#[must_use]
pub fn command_bullets() -> String {
    let mut out = format!(
        "For an authenticated node, place `{AUTH_OPTIONS}` before the subcommand. \
         Credentials are read from the named file, never an environment variable.\n\n"
    );
    for c in COMMANDS.iter().filter(|c| c.agent_facing) {
        out.push_str(&format!("- `choir {} {}`: {}\n", c.name, c.args, c.summary));
    }
    out
}

/// `AGENTS.md`: the generated choir reference for coding agents.
#[must_use]
pub fn agents_md() -> String {
    format!(
        "# choir, for agents\n\n\
         Generated from `crates/choir-cli/src/surface.rs`. Do not edit; edit the table.\n\n\
         choir is an agent-first code collaboration platform. Many agents work on one \
         repository at once, a single-writer sequencer puts every change in one total order, \
         and merge conflicts are first-class values rather than errors.\n\n\
         ## Use the signed-op API, not `git push`\n\n\
         `git push` works and is the compatibility path. The signed-operation API is the \
         primary agent path: faster, carries your identity, and says what you are doing. \
         For more than one change, use `POST /api/submit-batch`, not a loop over \
         `POST /api/submit`: a batch is one durability barrier.\n\n\
         ## Commands\n\n{}\n\
         ## Endpoints\n\n{}\n\
         ## Conventions that are not obvious\n\n\
         - A conflict is a committed value, not a failure. Commit it and resolve in a \
         follow-up.\n\
         - Request review naming no reviewers; the node draws them. A review with no \
         reviewers never counts as approved.\n\
         - Channel names are `operator/agent`. Agents sharing an operator prefix cannot \
         review each other.\n\
         - Publish your task spec with `choir intent` when you pick up work; update it when \
         scope changes.\n\
         - Commit and push the git object before `choir checkpoint`; the checkpoint does not \
         transfer workspace-local objects.\n\
         - Secrets live under `~/.choir/`. Never write one into the repository.\n",
        command_bullets(),
        api_table()
    )
}

/// `llms.txt`, served by the node: the same surface, compact, no markdown
/// tables — a plain list survives a small context window better.
#[must_use]
pub fn llms_txt() -> String {
    let mut out = String::from(
        "# choir\n\n\
         Agent-first code collaboration. One total order from a single-writer sequencer; \
         conflicts are values, not errors.\n\n\
         Use POST /api/submit-batch for more than one change: a batch is one durability \
         barrier, a loop is one per operation. git push is the compatibility path.\n\n\
         ## Endpoints\n",
    );
    for e in ENDPOINTS {
        out.push_str(&format!("{} {} - {}\n", e.method, e.path, e.purpose));
    }
    // What a denial means, because the two statuses carry different
    // instructions and neither is worth retrying (D29).
    out.push_str(
        "\n## Access\n\
         404 on a repository means no read grant, and says nothing about whether it exists; \
         403 means read but not write, or the operation needs a node-wide grant. Neither is \
         retryable: ask the operator for a grant line.\n",
    );
    out.push_str("\n## CLI\n");
    out.push_str(&format!(
        "For authenticated nodes: choir {AUTH_OPTIONS} <command> ...\n"
    ));
    for c in COMMANDS {
        out.push_str(&format!("choir {} {} - {}\n", c.name, c.args, c.summary));
    }
    out
}

/// The three commands a newcomer runs, as an HTML fragment the node
/// serves on its contribute page.
///
/// Generated rather than written into `browse.rs` for the reason
/// `llms.txt` is: a page that tells a newcomer which flag to pass is the
/// worst place in the system for a stale signature, because its whole
/// readership is people with no way to tell it is wrong. It lands in
/// [`artifacts`], so the one staleness test that covers `--help` and the
/// templates covers this too.
///
/// `NODE` and `REPO` are placeholders the node substitutes for its own
/// base URL and the repository being read. They are spelled in capitals
/// so that a page which somehow escapes substitution reads as obviously
/// unfinished rather than as an address somebody might try.
#[must_use]
pub fn contribute_html() -> String {
    // Pulled from the table by name, so removing or renaming a command
    // breaks the build here instead of quietly emptying the page.
    let find = |name: &str| {
        COMMANDS
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("`choir {name}` is in the command table"))
    };
    let mut out = String::new();
    for (step, name, what, example) in [
        (
            "1",
            "join",
            "Paste the whole link your operator sent you, quotes included. This mints your key, \
             stores your token, and points git at that token for this node -- so the clone \
             below needs no credential in its URL. There is no registration, and no second \
             message to wait for.",
            "choir join 'NODE/join?i=…&amp;k=…'",
        ),
        (
            "2",
            "git-credential",
            "Clone normally. The token stays in the file `choir join` wrote and never enters \
             the URL, so it cannot leak through `git remote -v` or a pasted clone line. This \
             command is here for anybody who would rather wire that up by hand.",
            "git clone NODE/REPO.git",
        ),
        (
            "3",
            "propose",
            "Commit on a branch as you always would, then run this from inside the checkout, \
             with no arguments. It creates the change, pushes it, publishes the revision and \
             requests review. Run it again after an amend and it updates the same proposal \
             rather than opening a second one -- the branch name is what identifies the \
             change.",
            "git checkout -b fix-the-thing\n\
             git commit -am 'fix the thing'\n\
             choir propose",
        ),
    ] {
        let command = find(name);
        out.push_str("<li><h3><span class=\"step\">");
        out.push_str(step);
        out.push_str("</span> <code>choir ");
        out.push_str(command.name);
        out.push_str("</code></h3><p>");
        out.push_str(what);
        out.push_str("</p><pre class=\"cmd\">");
        // Escaped like the signature below it: an example carrying
        // `<your-channel>` would otherwise be parsed as a tag and vanish
        // from the page, leaving a command that looks complete and is
        // missing its last argument.
        out.push_str(&escape_html(example));
        out.push_str("</pre><p class=\"muted mono\">choir ");
        out.push_str(command.name);
        out.push(' ');
        out.push_str(&escape_html(command.args));
        out.push_str("</p></li>\n");
    }
    out
}

/// Minimal HTML escaping for text rendered into the generated fragment.
///
/// Only the three characters that can end an element or an attribute.
/// The inputs are this file's own constants rather than anything a
/// request carries, so this is here to keep `<api>` rendering as `<api>`
/// rather than disappearing into an unknown tag.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The machine-readable API description (D17), as pretty JSON.
///
/// The Code-Mode bet is that an agent writes code against a typed API
/// rather than making many tool calls. Whatever language that code is
/// eventually written in, it needs one description of the surface that
/// cannot drift from the surface — so this is rendered from the same
/// table that already renders `--help`, `docs/using/cli.md`,
/// `llms.txt`, the three `templates/` snippets and the MCP tool list,
/// and lands in
/// [`artifacts`] beside them so the one staleness test covers it.
///
/// **Static facts only.** What a *particular* node will accept —
/// accounts, quotas, an ACL, the review gates — varies per deployment
/// and cannot be generated, so the node merges a live `capabilities`
/// object into this document when it serves it. Putting a runtime fact
/// in a committed file would be a lie with a staleness test guarding it.
#[must_use]
pub fn schema_json() -> String {
    let endpoints: Vec<serde_json::Value> = ENDPOINTS
        .iter()
        .map(|e| {
            // The table's `path` carries the query parameter that is part
            // of the contract (`/api/log?from=N`), which is right for a
            // human reading `llms.txt` and useless to a generator: it
            // would have to parse the template back out. So the two are
            // split here, from information the table already holds —
            // `McpArguments::Query` names the parameters.
            let (path, query) = match e.mcp.as_ref().map(|m| m.arguments) {
                Some(McpArguments::Query { parameters }) => (
                    e.path.split('?').next().unwrap_or(e.path),
                    parameters.to_vec(),
                ),
                _ => (e.path, Vec::new()),
            };
            serde_json::json!({
                "method": e.method,
                "path": path,
                // Present and empty rather than absent, so a generator
                // never has to distinguish "no parameters" from "this
                // build did not say".
                "query_parameters": query,
                // The documented spelling, kept because `llms.txt` and
                // `docs/using/cli.md` show it, and a client comparing
                // the two should not have to wonder whether they
                // disagree.
                "documented_as": e.path,
                "purpose": e.purpose,
                // The stable programmatic name, and the signal that this
                // endpoint is one an agent is meant to call at all:
                // internal hook endpoints carry neither.
                "name": e.mcp.as_ref().map(|m| m.name),
                "agent_facing": e.mcp.is_some(),
                "input_schema": e.mcp.as_ref().map(|m| {
                    serde_json::from_str::<serde_json::Value>(m.input_schema)
                        .expect("every input schema in this table is JSON")
                }),
            })
        })
        .collect();
    let commands: Vec<serde_json::Value> = COMMANDS
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "args": c.args,
                "summary": c.summary,
                "agent_facing": c.agent_facing,
            })
        })
        .collect();
    let deprecations: Vec<serde_json::Value> = DEPRECATIONS
        .iter()
        .map(|(name, replacement, note)| {
            serde_json::json!({ "name": name, "replaced_by": replacement, "note": note })
        })
        .collect();
    let doc = serde_json::json!({
        "api_version": API_VERSION,
        "endpoints": endpoints,
        "commands": commands,
        "deprecations": deprecations,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).expect("the schema is always serializable")
    )
}

/// The generated half of `templates/shell/choir.sh`: one function per
/// agent-facing command (D17).
///
/// Thin on purpose. A wrapper that only forwards its arguments cannot
/// drift from the binary, and the arguments it forwards come from the
/// same table `--help` prints — so a command renamed here renames the
/// shell function in the same commit or the staleness test fails.
/// Anything worth more than forwarding is a *flow*, which is judgement
/// about order and lives in the hand-written half of that file.
///
/// `sh` rather than `bash`: the harnesses that will source this run
/// whatever `/bin/sh` is, and nothing here needs an array or a
/// `[[`-test.
#[must_use]
pub fn shell_functions() -> String {
    let mut out = String::from(
        "# One function per agent-facing command, forwarding its arguments\n\
         # to the binary. Generated from the same table as `choir --help`;\n\
         # edit `crates/choir-cli/src/surface.rs` and regenerate.\n",
    );
    for c in COMMANDS.iter().filter(|c| c.agent_facing) {
        // Shell function names cannot carry a hyphen portably.
        let name = c.name.replace('-', "_");
        out.push_str(&format!(
            "\n# choir {} {}\n#   {}\nchoir_{name}() {{\n\tchoir_run {} \"$@\"\n}}\n",
            c.name, c.args, c.summary, c.name
        ));
    }
    out
}

/// Directory name the agent skill installs under; the skill frontmatter's
/// `name:` must equal it, because skill loaders resolve by directory.
pub const SKILL_DIR: &str = "choir";

/// The installable agent skill.
///
/// Rendered from the same table as `--help` and `AGENTS.md` at the moment
/// of installation, so — unlike docs baked in as static files — the
/// installed skill can never describe a different version than the binary
/// that wrote it. Re-installing after an upgrade refreshes it.
#[must_use]
pub fn skill_md() -> String {
    format!(
        "---\nname: {SKILL_DIR}\ndescription: Drive a choir node — signed operations, \
         workspaces, reviews, triage and next actions. Use when working in a repository \
         served by a choir node, or when asked to run choir commands.\n---\n\n{}",
        agents_md()
    )
}

/// The generated half of `templates/python/choir.py`: one method per
/// tool, rendered **from the schema document and nothing else** (D17).
///
/// This is the point of the artifact rather than an implementation
/// detail. D17 bets that an agent writes code against a typed API, and
/// `/api/schema` is the description that code would be generated from —
/// but a description is only sufficient if something has actually been
/// generated from it without reading choir's source. So this takes the
/// parsed schema as its argument and never touches [`ENDPOINTS`] or
/// [`COMMANDS`]. If a method comes out wrong, the schema is what was
/// insufficient, and that is the finding.
///
/// The shell library is the opposite bargain and both are wanted: it
/// wraps the binary and can therefore sign, while this needs no binary
/// and therefore cannot.
#[must_use]
pub fn python_client(schema: &serde_json::Value) -> String {
    let mut out = String::from(
        "    # One method per tool, from the node's own description.\n\
         \x20   # Generated; edit `crates/choir-cli/src/surface.rs`.\n",
    );
    let endpoints = schema["endpoints"].as_array().cloned().unwrap_or_default();
    for endpoint in endpoints {
        // Only the tools: an endpoint with no name is one the schema
        // marks as not agent-facing, and a generated client offering it
        // would be offering something the description says not to call.
        let Some(name) = endpoint["name"].as_str() else {
            continue;
        };
        let method = endpoint["method"].as_str().unwrap_or("GET");
        let path = endpoint["path"].as_str().unwrap_or_default();
        let purpose = endpoint["purpose"].as_str().unwrap_or_default();
        let query: Vec<&str> = endpoint["query_parameters"]
            .as_array()
            .map(|values| values.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // The schema says whether a tool takes anything at all. An
        // endpoint whose input schema has no properties gets a method
        // with no parameters, rather than one that accepts arguments and
        // drops them — which is what the first generated copy did, and
        // reading it is how that was found.
        let takes_arguments = endpoint["input_schema"]["properties"]
            .as_object()
            .is_some_and(|properties| !properties.is_empty());

        // A query parameter is a keyword argument; a body is one dict.
        // `from` is a Python keyword, so every argument arrives through
        // `**kwargs` rather than a signature this generator would have
        // to escape — the schema names the parameters and the docstring
        // repeats them.
        if takes_arguments {
            out.push_str(&format!("\n    def {name}(self, **arguments):\n"));
        } else {
            out.push_str(&format!("\n    def {name}(self):\n"));
        }
        out.push_str(&format!("        \"\"\"{}\n\n", wrap_python_doc(purpose)));
        if !takes_arguments {
            out.push_str("        Takes no arguments.\n");
        } else if query.is_empty() {
            out.push_str("        Arguments become the JSON request body.\n");
        } else {
            out.push_str(&format!(
                "        Arguments become the query string: {}.\n",
                query.join(", ")
            ));
        }
        out.push_str("        \"\"\"\n");
        if !takes_arguments {
            out.push_str(&format!(
                "        return self._request(\"{method}\", \"{path}\")\n"
            ));
        } else if query.is_empty() {
            out.push_str(&format!(
                "        return self._request(\"{method}\", \"{path}\", body=arguments)\n"
            ));
        } else {
            out.push_str(&format!(
                "        return self._request(\"{method}\", \"{path}\", query=arguments)\n"
            ));
        }
    }
    out
}

/// Reflows one purpose line into an indented Python docstring body.
fn wrap_python_doc(text: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for word in text.split_whitespace() {
        if column + word.len() > 64 && column > 0 {
            out.push_str("\n        ");
            column = 0;
        } else if column > 0 {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
    }
    out
}

/// Replaces the region between [`GEN_START`] and [`GEN_END`] in `doc`.
///
/// # Errors
///
/// Returns a description when the markers are missing or out of order,
/// rather than appending and quietly producing two generated regions.
pub fn splice(doc: &str, generated: &str) -> Result<String, String> {
    splice_between(doc, generated, GEN_START, GEN_END)
}

/// [`splice`] with explicit markers, for a file whose comment syntax is
/// not HTML.
///
/// # Errors
///
/// Same as [`splice`]: a missing or out-of-order marker pair.
pub fn splice_between(
    doc: &str,
    generated: &str,
    start_marker: &str,
    end_marker: &str,
) -> Result<String, String> {
    let start = doc
        .find(start_marker)
        .ok_or("missing generated-start marker")?;
    let end = doc.find(end_marker).ok_or("missing generated-end marker")?;
    if end < start {
        return Err("generated markers are out of order".to_string());
    }
    Ok(format!(
        "{}{}\n\n{}\n{}",
        &doc[..start],
        start_marker,
        generated.trim_end(),
        &doc[end..]
    ))
}

/// Every artifact rendered from this table, as `(path, full contents)`,
/// relative to the repository `root`.
///
/// Returned rather than written so the generator and the staleness test
/// share one definition of what exists — a test that enumerated the
/// artifacts separately would pass while missing a new one.
///
/// # Errors
///
/// A file that should carry generated markers and does not, or cannot be
/// read.
pub fn artifacts(root: &std::path::Path) -> Result<Vec<(std::path::PathBuf, String)>, String> {
    let read = |rel: &str| -> Result<String, String> {
        std::fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel}: {e}"))
    };
    let mut out = vec![
        (root.join("AGENTS.md"), agents_md()),
        (root.join("crates/choir-node/src/llms.txt"), llms_txt()),
        (
            root.join("crates/choir-node/src/contribute.html"),
            contribute_html(),
        ),
        (
            root.join("crates/choir-node/src/schema.json"),
            schema_json(),
        ),
        // Owned by choir-node's reject module, generated here so one
        // staleness test covers every generated artifact rather than two
        // tests each covering half.
        (root.join("ERRORS.md"), choir_node::reject::errors_md()),
        // The book's palette, cut from the daemon's own stylesheet, so
        // the documentation and the product cannot drift apart in
        // colour, type scale or spacing. Same reason it lives here: one
        // staleness test over every generated artifact.
        (
            root.join("theme/choir-tokens.css"),
            choir_node::ui_tokens_css(),
        ),
        // Two documents, two audiences, one table. `docs/using/cli.md`
        // gets the complete surface; the README gets the eight commands
        // a first change needs. Both are generated so neither can drift
        // from the other.
        (
            root.join("docs/using/cli.md"),
            splice(&read("docs/using/cli.md")?, &cli_doc_surface())
                .map_err(|e| format!("docs/using/cli.md: {e}"))?,
        ),
        (
            root.join("README.md"),
            splice(&read("README.md")?, &readme_cheatsheet())
                .map_err(|e| format!("README.md: {e}"))?,
        ),
    ];
    out.push((
        root.join("templates/python/choir.py"),
        splice_between(
            &read("templates/python/choir.py")?,
            // Fed the rendered schema, not the table it came from: a
            // client generated from the description is the only thing
            // that shows the description is sufficient.
            &python_client(
                &serde_json::from_str(&schema_json()).expect("the schema we just rendered is JSON"),
            ),
            SH_GEN_START,
            SH_GEN_END,
        )
        .map_err(|e| format!("templates/python/choir.py: {e}"))?,
    ));
    out.push((
        root.join("templates/shell/choir.sh"),
        splice_between(
            &read("templates/shell/choir.sh")?,
            &shell_functions(),
            SH_GEN_START,
            SH_GEN_END,
        )
        .map_err(|e| format!("templates/shell/choir.sh: {e}"))?,
    ));
    for rel in [
        "templates/claude-code/CLAUDE.snippet.md",
        "templates/codex/AGENTS.snippet.md",
        "templates/cursor/choir.mdc",
    ] {
        out.push((
            root.join(rel),
            splice(&read(rel)?, &command_bullets()).map_err(|e| format!("{rel}: {e}"))?,
        ));
    }
    Ok(out)
}
