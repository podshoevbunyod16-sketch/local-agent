//! Model Context Protocol (MCP) — protocol layer.
//!
//! The transport (stdio child process / HTTP+SSE) lives on the Java side, where
//! TLS and process spawning are already available and don't need to be
//! cross-compiled. This module owns the parts that must be exactly right and are
//! worth unit-testing on the host:
//!
//! * JSON-RPC 2.0 envelope building (`initialize`, `tools/list`, `tools/call`);
//! * response parsing, including `isError` results and content arrays;
//! * conversion of MCP tool definitions into our [`crate::tools::ToolSpec`];
//! * framing helpers for the two transports (`\n`-delimited JSON and
//!   `Content-Length` headers).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::ToolSpec;

/// Protocol revision we speak. Servers negotiate down, so a modern value is safe.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Child process speaking newline-delimited JSON-RPC on stdin/stdout.
    Stdio,
    /// HTTP POST endpoint that answers with JSON or `text/event-stream`.
    Http,
}

/// How to reach one MCP server (as declared in a plugin manifest or typed in by
/// the user in Settings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: Option<Transport>,
    /// For `http`.
    #[serde(default)]
    pub url: String,
    /// For `stdio`: executable and args (e.g. `python3 -m mcp_server_fetch`).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Static headers for `http` (e.g. Authorization).
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    pub fn effective_transport(&self) -> Transport {
        self.transport.unwrap_or(if self.url.is_empty() {
            Transport::Stdio
        } else {
            Transport::Http
        })
    }
}

// ---------------------------------------------------------------- JSON-RPC

#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }

    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_default();
        s.push('\n');
        s
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    /// Parse a single JSON-RPC message that is not a server notification.
    pub fn parse(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        if v.get("method").is_some() && v.get("id").is_none() {
            // server notification (e.g. notifications/tools/list_changed)
            return None;
        }
        serde_json::from_value(v).ok()
    }
}

pub fn initialize_request(id: u64, client_name: &str, client_version: &str) -> RpcRequest {
    RpcRequest::new(
        id,
        "initialize",
        Some(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "roots": { "listChanged": false }, "tools": {} },
            "clientInfo": { "name": client_name, "version": client_version }
        })),
    )
}

pub fn initialized_notification() -> String {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string() + "\n"
}

pub fn tools_list_request(id: u64) -> RpcRequest {
    RpcRequest::new(id, "tools/list", Some(json!({})))
}

pub fn tools_call_request(id: u64, name: &str, arguments: &Value) -> RpcRequest {
    RpcRequest::new(
        id,
        "tools/call",
        Some(json!({ "name": name, "arguments": arguments })),
    )
}

/// Convert `tools/list` output into app tool specs, namespaced by server.
///
/// Namespacing matters: two servers may expose `search`, and the model output is
/// a flat string. We use `server__tool`, which is also what we strip back before
/// calling `tools/call`.
pub fn parse_tools_list(server: &str, result: &Value) -> Vec<ToolSpec> {
    let mut out = Vec::new();
    let Some(items) = result.get("tools").and_then(|t| t.as_array()) else {
        return out;
    };
    for item in items {
        let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let description = item
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let schema = item
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        out.push(ToolSpec {
            name: namespaced(server, name),
            description,
            parameters: schema,
            kind: "mcp".to_string(),
            server: server.to_string(),
        });
    }
    out
}

pub fn namespaced(server: &str, tool: &str) -> String {
    let clean: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{clean}__{tool}")
}

/// Split a namespaced name back into `(server, tool)`.
pub fn split_namespaced(full: &str) -> Option<(&str, &str)> {
    full.split_once("__")
}

/// Turn a `tools/call` result into the text we feed back to the model.
pub fn parse_tool_result(result: &Value) -> (String, bool) {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        for item in content {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
                Some("resource") => {
                    if let Some(t) = item
                        .get("resource")
                        .and_then(|r| r.get("text"))
                        .and_then(Value::as_str)
                    {
                        parts.push(t.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        if let Some(v) = result.get("structuredContent") {
            parts.push(v.to_string());
        }
    }
    (parts.join("\n"), is_error)
}

// ---------------------------------------------------------------- framing

/// `stdio` transport: one JSON object per line (no embedded raw newlines).
pub fn encode_stdio_frame(json_line: &str) -> String {
    let mut s = json_line.trim().replace('\n', " ");
    s.push('\n');
    s
}

/// Older MCP servers on stdio use LSP-style `Content-Length` framing.
pub fn encode_content_length_frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

/// Returns `(payload, rest)` if `buf` holds a complete `Content-Length` frame.
pub fn take_content_length_frame(buf: &str) -> Option<(String, String)> {
    let header_end = buf.find("\r\n\r\n")?;
    let header = &buf[..header_end];
    let len: usize = header.lines().find_map(|l| {
        l.strip_prefix("Content-Length:")
            .or_else(|| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
    })?;
    let body_start = header_end + 4;
    if buf.len() < body_start + len {
        return None;
    }
    Some((
        buf[body_start..body_start + len].to_string(),
        buf[body_start + len..].to_string(),
    ))
}

/// `http` transport with SSE: collect `data:` payloads of one event.
pub fn parse_sse_data(chunk: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in chunk.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(rest.trim_start());
        } else if line.trim().is_empty() && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_initialize() {
        let line = initialize_request(1, "TurboLLM", "0.2.0").to_line();
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn parses_tools_list_and_namespaces() {
        let result = json!({"tools": [
            {"name": "fetch", "description": "get url",
             "inputSchema": {"type":"object","properties":{"url":{"type":"string"}}}}
        ]});
        let tools = parse_tools_list("web server", &result);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "web_server__fetch");
        assert_eq!(tools[0].kind, "mcp");
        assert_eq!(
            split_namespaced(&tools[0].name),
            Some(("web_server", "fetch"))
        );
    }

    #[test]
    fn parses_text_content_result() {
        let result = json!({"content":[{"type":"text","text":"42"},{"type":"image"}]});
        let (text, err) = parse_tool_result(&result);
        assert!(!err);
        assert!(text.starts_with("42"));
    }

    #[test]
    fn parses_error_result() {
        let result = json!({"isError": true, "content":[{"type":"text","text":"boom"}]});
        let (text, err) = parse_tool_result(&result);
        assert!(err);
        assert_eq!(text, "boom");
    }

    #[test]
    fn content_length_roundtrip() {
        let frame = encode_content_length_frame(r#"{"id":1,"result":{}}"#);
        let (payload, rest) = take_content_length_frame(&frame).unwrap();
        assert_eq!(payload, r#"{"id":1,"result":{}}"#);
        assert!(rest.is_empty());
        assert!(take_content_length_frame(&frame[..10]).is_none());
    }

    #[test]
    fn sse_events() {
        let chunk = "event: message\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let ev = parse_sse_data(chunk);
        assert_eq!(ev, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }

    #[test]
    fn rpc_error_and_notification() {
        assert!(RpcResponse::parse(r#"{"jsonrpc":"2.0","method":"x"}"#).is_none());
        let r = RpcResponse::parse(
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}"#,
        )
        .unwrap();
        assert_eq!(r.error.unwrap().code, -32601);
    }
}
