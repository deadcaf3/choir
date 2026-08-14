//! The agent-facing surface, as data, and the generators that render it.
//!
//! `choir --help`, the README's API and CLI sections, the three
//! `templates/` snippets, the root `agents.md` and the node's `llms.txt`
//! all describe one surface. Hand-maintained, they drift — and they had
//! already started to: the README's API table carried a throughput
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
}

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
    /// One argument is URL-encoded as a query parameter.
    Query {
        /// Name of the argument and query parameter.
        parameter: &'static str,
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

const REVIEWS_MCP_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "reviewer": { "type": "string", "description": "Reviewer channel name" }
  },
  "required": ["reviewer"],
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
        name: "key",
        args: "<key-file> [name]",
        summary: "mint a key and print the line the operator registers; \
                  pass your channel name to print the bound form",
        agent_facing: true,
    },
    Command {
        name: "workspace",
        args: "<api> <owner/repo> <name> [--base <git-oid> --owner <channel> --key-file <path> --change <id> --idempotency-key <key>]",
        summary: "provision a CoW workspace; advanced flags owner-sign an exact base and stable change",
        agent_facing: true,
    },
    Command {
        name: "checkpoint",
        args: "<api> <key-file> <channel> <change-id> <workspace-id> <git-oid>",
        summary: "publish an immutable change revision after committing and pushing its Git object",
        agent_facing: true,
    },
    Command {
        name: "workspace-archive",
        args: "<api> <key-file> <channel> <owner/repo> <name> <change-id> <idempotency-key>",
        summary: "owner-sign and recoverably archive a bound workspace; exact retries are idempotent",
        agent_facing: true,
    },
    Command {
        name: "runner",
        args: "<config-file>",
        summary: "drive one workspace lifecycle step for an orchestrator; \
                  a JSON request on stdin, a JSON result on stdout",
        agent_facing: false,
    },
    Command {
        name: "submit",
        args: "<api> <key-file> <channel> '<op-json>'",
        summary: "sign and submit one raw operation",
        agent_facing: false,
    },
    Command {
        name: "schema",
        args: "<api>",
        summary: "print this node's machine-readable API description and its live capabilities",
        agent_facing: true,
    },
    Command {
        name: "log",
        args: "<api> [--from <n>] [--verify] [--keys <file>]",
        summary: "read log entries from a cursor; --verify checks continuity, recomputes every \
                  hash, and verifies the signatures whose keys you hold — SYNC.md as a flag",
        agent_facing: true,
    },
    Command {
        name: "batch",
        args: "<api> <key-file> <channel> <ops-file>",
        summary: "sign and submit many operations as one batch — the primary path for agent \
                  workloads; one op per line, `-` reads stdin, one result line per op in order",
        agent_facing: true,
    },
    Command {
        name: "review",
        args: "<api> <key-file> <channel> <id> <git-oid> [--ref <repo:ref>] [reviewer]...",
        summary: "request review on a commit; name no reviewers and the node draws them",
        agent_facing: true,
    },
    Command {
        name: "verdict",
        args: "<api> <key-file> <reviewer> <id> approve|request-changes [note]",
        summary: "answer a review you were assigned",
        agent_facing: true,
    },
    Command {
        name: "comment",
        args: "<api> <key-file> <channel> <review-id> <comment-id> '<body>'",
        summary: "say something on a review; append-only and permanent, \
                  and the comment id is your retry identity",
        agent_facing: true,
    },
    Command {
        name: "slash",
        args: "<api> <node-key-file> <id> <reviewer> '<reason>'",
        summary: "invalidate one reviewer's approval; operator-only and never moves a ref",
        agent_facing: false,
    },
    Command {
        name: "abandon",
        args: "<api> <node-key-file> <id>",
        summary: "archive a stale incomplete review as lapsed, settling it unapproved; \
                  operator-only and never moves a ref",
        agent_facing: false,
    },
    Command {
        name: "bind",
        args: "<api> <node-key-file> <operator> <key-hex> [channel]",
        summary: "record in the log that a key belongs to an operator; operator-only and never moves a ref",
        agent_facing: false,
    },
    Command {
        name: "revoke",
        args: "<api> <node-key-file> <key-hex> '<reason>'",
        summary: "withdraw a key binding; terminal, and the attribution row survives",
        agent_facing: false,
    },
    Command {
        name: "appeal",
        args: "<api> <attempt-id>",
        summary: "appeal a rejected newcomer attempt for operator adjudication; never grants privilege",
        agent_facing: true,
    },
    Command {
        name: "intent",
        args: "<api> <key-file> <channel> <subject> <kind> '<body>'",
        summary: "publish a task spec or plan so other agents can see intent",
        agent_facing: true,
    },
    Command {
        name: "reviews",
        args: "<api> <reviewer>",
        summary: "your pending review queue",
        agent_facing: true,
    },
    Command {
        name: "triage",
        args: "<api>",
        summary: "every review and change classified into a bucket — landed, awaiting \
                  verdicts, changes requested, approved-but-target-moved — ranked \
                  most-actionable-first, capped, with truncation marked in-band",
        agent_facing: true,
    },
    Command {
        name: "state",
        args: "<api> <channel>",
        summary: "your bounded next-actions document: verdicts you owe, what your changes \
                  need, what you are waiting on, each with a command and its risk",
        agent_facing: true,
    },
    Command {
        name: "view",
        args: "<api>",
        summary: "the materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate",
        agent_facing: true,
    },
];

/// Every endpoint the node serves, in the order the README lists them.
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
        purpose: "Same, in array order; the primary path for agent workloads \
                  (throughput figures live in PHASE0.md, not here, so they cannot go stale)",
        mcp: Some(McpTool {
            name: "choir_submit_batch",
            input_schema: BATCH_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/view",
        purpose: "The materialized view plus the latest ref-state attestation, durable key bindings, T2 new-actor review outcomes, T3 concentration, T4 newcomer harm, complete-view growth, the commit this daemon was built from, and the sequencer's measured decision latency against the 100 ms gate. On a node running an ACL you are served your own slice: the repositories your credential may read, plus reviews you were assigned to; the node-wide sections need a node-wide grant. A repository missing from the response is one you were not granted, not one that is gone",
        mcp: Some(McpTool {
            name: "choir_view",
            input_schema: EMPTY_MCP_SCHEMA,
            arguments: McpArguments::Empty,
        }),
    },
    Endpoint {
        method: "POST",
        path: "/api/appeal",
        purpose: "Record an appeal for a rejected newcomer attempt; it requests operator adjudication and never changes privilege",
        mcp: Some(McpTool {
            name: "choir_appeal",
            input_schema: APPEAL_MCP_SCHEMA,
            arguments: McpArguments::Body,
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/log?from=N",
        purpose: "Ordered log entries, the catch-up and sync primitive. Absolute `from`: \
                  entries evicted from the in-memory window are served from the persisted log \
                  (`source` says which), and a node that cannot reach that far back answers 409 \
                  rather than a page with a hole in it. Each entry carries its hash, parent and \
                  author signature so pages can be chained and verified without trusting the \
                  node; SYNC.md is that procedure",
        mcp: Some(McpTool {
            name: "choir_log",
            input_schema: LOG_MCP_SCHEMA,
            arguments: McpArguments::Query { parameter: "from" },
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
                parameter: "reviewer",
            },
        }),
    },
    Endpoint {
        method: "GET",
        path: "/api/schema",
        purpose: "This surface, machine-readable and versioned, plus what this particular \
                  node will accept — the description an agent generates a client from (D17)",
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
        path: "/llms.txt",
        purpose: "This surface, as text, for an agent that has never seen choir",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/sync.md",
        purpose: "The sync contract, in full: cursor semantics and how to verify a page's \
                  hash chain and author signatures without trusting the node serving them",
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
        purpose: "Mint a single-use, expiring invite for a new account and the grants it will \
                  hold; needs a node-wide write grant, and can never issue one",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/redeem",
        purpose: "Redeem an invite — presented as the credential — for a token, once, and \
                  register an ssh key with it",
        mcp: None,
    },
    Endpoint {
        method: "POST",
        path: "/api/accounts/revoke",
        purpose: "Delete an account: its token stops authenticating on the next request, and \
                  its grants and keys go with it",
        mcp: None,
    },
    Endpoint {
        method: "GET",
        path: "/api/accounts",
        purpose: "Who holds an account, what they were granted, and which invites are \
                  outstanding; never a secret or its hash",
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
/// shared with the README and discovery documents.
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

/// The `choir` usage block, as `--help` and a bare invocation print it.
#[must_use]
pub fn usage() -> String {
    let mut out = format!("usage:\n  choir {AUTH_OPTIONS} <command> ...\n\ncommands:\n");
    for c in COMMANDS {
        out.push_str(&format!("  choir {} {}\n", c.name, c.args));
    }
    out.push_str(
        "\nExit codes: 0 accepted, 1 the node rejected (its JSON error body is printed), \
         2 usage error.\n",
    );
    out
}

/// The README's endpoint table.
#[must_use]
pub fn api_table() -> String {
    let mut out = String::from("| Endpoint | Purpose |\n|---|---|\n");
    for e in ENDPOINTS {
        out.push_str(&format!("| `{} {}` | {} |\n", e.method, e.path, e.purpose));
    }
    out
}

/// The README's generated API and CLI reference.
#[must_use]
pub fn readme_surface() -> String {
    format!(
        "#### HTTP endpoints\n\n{}\n#### The `choir` CLI\n\n```text\n{}```\n",
        api_table(),
        usage()
    )
}

/// The command list the agent templates carry, as a markdown bullet list.
#[must_use]
pub fn command_bullets() -> String {
    let mut out = format!(
        "For an authenticated node, place `{AUTH_OPTIONS}` before the subcommand. \
         Credentials are read from the named file, never an environment variable.\n\n"
    );
    for c in COMMANDS.iter().filter(|c| c.agent_facing) {
        out.push_str(&format!("- `choir {} {}` — {}\n", c.name, c.args, c.summary));
    }
    out
}

/// `agents.md`: the generated choir reference for coding agents.
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
         primary agent path: it is faster, it carries your identity, and it is the only way \
         to say what you are doing. For anything more than one change at a time, use \
         `POST /api/submit-batch` rather than a loop over `POST /api/submit` — a batch is one \
         durability barrier, a loop is one per operation.\n\n\
         ## Commands\n\n{}\n\
         ## Endpoints\n\n{}\n\
         ## Conventions that are not obvious\n\n\
         - A conflict is a committed value, not a failure. Commit it, keep working, resolve in \
         a follow-up.\n\
         - You do not choose who reviews you. Request review naming no reviewers and the node \
         draws them; a review with no reviewers never counts as approved.\n\
         - Channel names are conventionally `operator/agent`. The node will not draw a reviewer \
         sharing your operator prefix, so agents run by the same person cannot review each other.\n\
         - Publish your task spec with `choir intent` when you pick up work, and update it when \
         scope changes. Other agents and the merge machinery can both see it.\n\
         - For a stable change, commit and push the Git object before `choir checkpoint`; the \
         signed checkpoint advances identity and CAS, it does not transfer workspace-local objects.\n\
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
         A node may enforce per-repository grants. 404 on a repository means you hold no \
         read grant on it, and says nothing about whether it exists; 403 means you can read \
         it but not write it, or the operation needs a node-wide grant (the op log, the \
         ref-state attestation, and ops naming no repository). Neither is retryable — ask \
         the operator for a grant line.\n",
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

/// The machine-readable API description (D17), as pretty JSON.
///
/// The Code-Mode bet is that an agent writes code against a typed API
/// rather than making many tool calls. Whatever language that code is
/// eventually written in, it needs one description of the surface that
/// cannot drift from the surface — so this is rendered from the same
/// table that already renders `--help`, the README, `llms.txt`, the
/// three `templates/` snippets and the MCP tool list, and lands in
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
            // `McpArguments::Query` names the parameter.
            let (path, query) = match e.mcp.as_ref().map(|m| m.arguments) {
                Some(McpArguments::Query { parameter }) => (
                    e.path.split('?').next().unwrap_or(e.path),
                    vec![parameter],
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
                // the README show it and a client comparing the two
                // should not have to wonder whether they disagree.
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
    let start = doc.find(start_marker).ok_or("missing generated-start marker")?;
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
        (root.join("agents.md"), agents_md()),
        (root.join("crates/choir-node/src/llms.txt"), llms_txt()),
        (root.join("crates/choir-node/src/schema.json"), schema_json()),
        // Owned by choir-node's reject module, generated here so one
        // staleness test covers every generated artifact rather than two
        // tests each covering half.
        (root.join("ERRORS.md"), choir_node::reject::errors_md()),
        (
            root.join("README.md"),
            splice(&read("README.md")?, &readme_surface())
                .map_err(|e| format!("README.md: {e}"))?,
        ),
    ];
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
