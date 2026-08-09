//! Synchronous stdio MCP adapter over a choir node's HTTP API.
//!
//! The adapter owns no platform state and implements no platform policy.
//! Every tool call shells out to `curl` and returns the node's exact body;
//! [`crate::surface::ENDPOINTS`] supplies both the tool catalog and the
//! HTTP route, so the two surfaces cannot drift.
//!
//! Legacy 2025 clients use `initialize`; modern 2026-07-28 clients carry
//! their protocol version and capabilities in each request's `_meta`.
//! Neither path creates a session. One synchronous OS thread handles each
//! newline-delimited request, and no async runtime is involved.
//!
//! # Examples
//!
//! ```
//! let tools = choir_cli::surface::mcp_tools();
//! assert_eq!(tools[0]["name"], "choir_submit");
//! ```

use crate::surface::{self, Endpoint, McpArguments};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Modern stateless protocol revision served by this adapter.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// Cache lifetime for the immutable-per-process discovery and tool lists.
pub const CATALOG_TTL_MS: u64 = 300_000;

const SUPPORTED_VERSIONS: &[&str] = &[MODERN_PROTOCOL_VERSION, "2025-11-25", "2025-06-18"];
const SERVER_NAME: &str = "choir-mcp";
static BODY_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Credentials {
    user: String,
    token: String,
}

/// Blocking HTTP client used by MCP tool calls.
///
/// It shells out to `curl`, matching the repository's outbound-HTTP
/// convention. Credentials, when configured, are read from a named file
/// and sent to curl over stdin rather than placed in its argument vector.
#[derive(Clone)]
pub struct HttpClient {
    api: String,
    credentials: Option<Credentials>,
}

impl HttpClient {
    /// Builds a client for `api`.
    ///
    /// `auth_file` uses the node's `user:token`-per-line format. When it
    /// contains more than one credential, `auth_user` must select one.
    ///
    /// # Errors
    ///
    /// Returns a description for an invalid URL, unreadable or malformed
    /// auth file, or an ambiguous credential selection.
    pub fn new(
        api: &str,
        auth_file: Option<&Path>,
        auth_user: Option<&str>,
    ) -> Result<Self, String> {
        if !(api.starts_with("http://") || api.starts_with("https://")) {
            return Err("<api> must start with http:// or https://".to_string());
        }
        if auth_user.is_some() && auth_file.is_none() {
            return Err("--auth-user needs --auth-file".to_string());
        }
        let credentials = auth_file
            .map(|path| read_credentials(path, auth_user))
            .transpose()?;
        Ok(Self {
            api: api.trim_end_matches('/').to_string(),
            credentials,
        })
    }

    fn call(&self, endpoint: &Endpoint, arguments: &Value) -> Result<HttpResponse, String> {
        let object = arguments
            .as_object()
            .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;
        let base_path = endpoint.path.split('?').next().unwrap_or(endpoint.path);
        let mut command = Command::new("curl");
        command.args([
            "-sS",
            "-w",
            "\n%{http_code}",
            "--config",
            "-",
            "-X",
            endpoint.method,
        ]);

        let body_file = match endpoint.mcp.as_ref().expect("MCP endpoint").arguments {
            McpArguments::Empty => {
                if !object.is_empty() {
                    return Err("this tool takes no arguments".to_string());
                }
                None
            }
            McpArguments::Body => {
                let file = BodyFile::new(arguments)?;
                command.args(["-H", "Content-Type: application/json", "--data-binary"]);
                command.arg(format!("@{}", file.path.display()));
                Some(file)
            }
            McpArguments::Query { parameter } => {
                let value = object
                    .get(parameter)
                    .ok_or_else(|| format!("missing `{parameter}` argument"))?;
                let value = match value {
                    Value::String(value) => value.clone(),
                    Value::Number(value) if value.is_i64() || value.is_u64() => value.to_string(),
                    _ => return Err(format!("`{parameter}` must be a string or integer")),
                };
                command.args(["--get", "--data-urlencode"]);
                command.arg(format!("{parameter}={value}"));
                None
            }
        };
        command.arg(format!("{}{base_path}", self.api));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Curl errors can include private node addresses. The MCP
            // result names the transport failure without reflecting them.
            .stderr(Stdio::null());

        let mut child = command
            .spawn()
            .map_err(|_| "could not start curl".to_string())?;
        let config = self.curl_config();
        child
            .stdin
            .take()
            .expect("piped curl stdin")
            .write_all(config.as_bytes())
            .map_err(|_| "could not configure curl".to_string())?;
        let output = child
            .wait_with_output()
            .map_err(|_| "could not wait for curl".to_string())?;
        drop(body_file);
        if !output.status.success() {
            return Err("curl could not reach the choir node".to_string());
        }
        let output = String::from_utf8(output.stdout)
            .map_err(|_| "the choir node returned non-UTF-8 data".to_string())?;
        let (body, status) = output
            .rsplit_once('\n')
            .ok_or_else(|| "curl returned no HTTP status".to_string())?;
        let status = status
            .trim()
            .parse::<u16>()
            .map_err(|_| "curl returned an invalid HTTP status".to_string())?;
        Ok(HttpResponse {
            status,
            body: body.to_string(),
        })
    }

    fn curl_config(&self) -> String {
        self.credentials
            .as_ref()
            .map_or_else(String::new, |credentials| {
                // read_credentials rejects curl-config metacharacters, so
                // this quoted line cannot grow a second config directive.
                format!("user = \"{}:{}\"\n", credentials.user, credentials.token)
            })
    }
}

struct HttpResponse {
    status: u16,
    body: String,
}

/// Private request-body file passed to curl by path, keeping large batches
/// and signed payloads out of the process argument vector.
struct BodyFile {
    path: std::path::PathBuf,
}

impl BodyFile {
    fn new(body: &Value) -> Result<Self, String> {
        use std::fs::OpenOptions;
        use std::io::Write as _;

        for _ in 0..100 {
            let id = BODY_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("choir-mcp-body-{}-{id}.json", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    serde_json::to_writer(&mut file, body)
                        .map_err(|_| "could not encode tool arguments".to_string())?;
                    file.flush()
                        .map_err(|_| "could not write tool arguments".to_string())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err("could not create a private request-body file".to_string()),
            }
        }
        Err("could not allocate a unique request-body file".to_string())
    }
}

impl Drop for BodyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serves newline-delimited JSON-RPC until `reader` reaches EOF.
///
/// Each message is handled on a fresh synchronous OS thread. Responses
/// are flushed one per line; notifications produce no output.
///
/// # Errors
///
/// Returns an I/O error when stdin cannot be read, stdout cannot be
/// written, or a request thread panics.
pub fn serve<R, W>(reader: R, mut writer: W, client: &HttpClient) -> std::io::Result<()>
where
    R: BufRead,
    W: Write + Send,
{
    for line in reader.lines() {
        let line = line?;
        let response = std::thread::scope(|scope| {
            scope
                .spawn(|| dispatch_line(&line, client))
                .join()
                .map_err(|_| std::io::Error::other("MCP request thread panicked"))
        })?;
        if let Some(response) = response {
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}

fn dispatch_line(line: &str, client: &HttpClient) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(_) => return Some(error(Value::Null, -32700, "Parse error", None)),
    };
    dispatch(&request, client)
}

/// Dispatches one decoded JSON-RPC message.
///
/// Notifications return `None`; requests return a JSON-RPC result or
/// error response. This function stores no negotiated-version state.
#[must_use]
pub fn dispatch(request: &Value, client: &HttpClient) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(error(Value::Null, -32600, "Invalid Request", None));
    };
    let id = object.get("id").cloned();
    let valid_id = id
        .as_ref()
        .is_none_or(|id| id.is_string() || id.is_number());
    let method = object.get("method").and_then(Value::as_str);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() || !valid_id
    {
        return Some(error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
            None,
        ));
    }
    let method = method.expect("checked");
    let Some(id) = id else {
        // The only notification observed from the supported clients is
        // notifications/initialized. Unknown notifications also have no
        // JSON-RPC response, as required by the base protocol.
        return None;
    };
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "initialize" {
        let Some(version) = params.get("protocolVersion").and_then(Value::as_str) else {
            return Some(error(id, -32602, "initialize needs protocolVersion", None));
        };
        return Some(success(
            id,
            complete(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": "Use signed operations for writes; git push is the compatibility path."
            })),
        ));
    }

    let modern = match modern_request(&params) {
        Ok(modern) => modern,
        Err(response) => return Some(with_id(response, id)),
    };
    match method {
        "server/discover" => {
            if !modern {
                return Some(error(
                    id,
                    -32602,
                    "server/discover needs 2026-07-28 request metadata",
                    None,
                ));
            }
            Some(success(
                id,
                complete(json!({
                    "supportedVersions": SUPPORTED_VERSIONS,
                    "capabilities": { "tools": {} },
                    "instructions": "Use signed operations for writes; git push is the compatibility path.",
                    "ttlMs": CATALOG_TTL_MS,
                    "cacheScope": "public"
                })),
            ))
        }
        "tools/list" => Some(success(
            id,
            complete(json!({
                "tools": surface::mcp_tools(),
                "ttlMs": CATALOG_TTL_MS,
                "cacheScope": "public"
            })),
        )),
        "tools/call" => Some(call_tool(id, &params, client)),
        _ => Some(error(id, -32601, "Method not found", None)),
    }
}

fn modern_request(params: &Value) -> Result<bool, Value> {
    let version = params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"));
    let Some(version) = version else {
        return Ok(false);
    };
    if version.as_str() != Some(MODERN_PROTOCOL_VERSION) {
        return Err(error(
            Value::Null,
            -32022,
            "Unsupported protocol version",
            Some(json!({ "supported": SUPPORTED_VERSIONS })),
        ));
    }
    let capabilities = params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"));
    if !capabilities.is_some_and(Value::is_object) {
        return Err(error(
            Value::Null,
            -32602,
            "2026-07-28 requests need clientCapabilities metadata",
            None,
        ));
    }
    Ok(true)
}

fn call_tool(id: Value, params: &Value, client: &HttpClient) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error(id, -32602, "tools/call needs a tool name", None);
    };
    let Some(endpoint) = surface::mcp_endpoint(name) else {
        return error(id, -32602, "Unknown tool", None);
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match client.call(endpoint, &arguments) {
        Ok(response) => {
            let structured_body = serde_json::from_str(&response.body)
                .unwrap_or_else(|_| Value::String(response.body.clone()));
            success(
                id,
                complete(json!({
                    "content": [{ "type": "text", "text": response.body }],
                    "structuredContent": {
                        "status": response.status,
                        "body": structured_body
                    },
                    "isError": !(200..300).contains(&response.status)
                })),
            )
        }
        Err(message) => success(
            id,
            complete(json!({
                "content": [{ "type": "text", "text": message }],
                "structuredContent": {
                    "status": null,
                    "error": "node_transport_failed"
                },
                "isError": true
            })),
        ),
    }
}

fn complete(mut result: Value) -> Value {
    result["resultType"] = Value::String("complete".to_string());
    result["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": server_info() });
    result
}

fn server_info() -> Value {
    json!({ "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    if let Some(data) = data {
        body["error"]["data"] = data;
    }
    body
}

fn with_id(mut response: Value, id: Value) -> Value {
    response["id"] = id;
    response
}

fn read_credentials(path: &Path, selected: Option<&str>) -> Result<Credentials, String> {
    let text = std::fs::read_to_string(path).map_err(|_| "could not read auth file".to_string())?;
    let mut credentials = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((user, token)) = line.split_once(':') else {
            return Err("auth file lines must be user:token".to_string());
        };
        if user.is_empty()
            || token.is_empty()
            || user
                .chars()
                .chain(token.chars())
                .any(|c| matches!(c, '\r' | '\n' | '"' | '\\'))
        {
            return Err("auth file contains an unsafe credential".to_string());
        }
        credentials.push(Credentials {
            user: user.to_string(),
            token: token.to_string(),
        });
    }
    if let Some(selected) = selected {
        let mut matches = credentials
            .into_iter()
            .filter(|entry| entry.user == selected);
        let Some(found) = matches.next() else {
            return Err("requested auth user was not found".to_string());
        };
        if matches.next().is_some() {
            return Err("auth file contains the selected user more than once".to_string());
        }
        return Ok(found);
    }
    match credentials.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err("auth file contains no credentials".to_string()),
        _ => Err("auth file has multiple users; pass --auth-user".to_string()),
    }
}
