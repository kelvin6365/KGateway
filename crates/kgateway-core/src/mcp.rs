//! MCP (Model Context Protocol) gateway abstraction. An [`McpClient`] connects to an
//! external tool server: it lists the tools it exposes (in the gateway's `Tool` schema)
//! and executes tool calls. The engine's agentic loop (see `engine::chat_agentic`)
//! injects these tools into the request and runs the call/execute/re-prompt cycle.
//!
//! This module is transport-agnostic: concrete clients (stdio / HTTP via `rmcp`) plug in
//! behind the trait. A `StaticMcpClient` is provided for in-process tools and tests.

use crate::error::KgError;
use crate::schema::{FunctionDef, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// A connection to an MCP tool server.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// A label for logs/diagnostics (e.g. the server name).
    fn name(&self) -> &str;

    /// Tools this server exposes, in the gateway's `Tool` schema (ready to inject into
    /// a `ChatRequest.tools`).
    async fn list_tools(&self) -> Result<Vec<Tool>, KgError>;

    /// Whether this client owns/handles the named tool (used to route execution).
    async fn has_tool(&self, name: &str) -> bool {
        self.list_tools()
            .await
            .map(|ts| ts.iter().any(|t| t.function.name == name))
            .unwrap_or(false)
    }

    /// Execute a tool call. `arguments` is the JSON-encoded argument string (OpenAI
    /// convention). Returns the tool result content to feed back to the model.
    async fn call_tool(&self, name: &str, arguments: &str) -> Result<String, KgError>;
}

/// The function a static tool runs: `(json_args) -> result_string`.
pub type ToolFn = Arc<dyn Fn(&str) -> Result<String, KgError> + Send + Sync>;

/// An in-process MCP client backed by registered closures. Useful for built-in tools,
/// demos, and tests without a real transport.
#[derive(Default, Clone)]
pub struct StaticMcpClient {
    name: String,
    tools: HashMap<String, (Tool, ToolFn)>,
}

impl StaticMcpClient {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tools: HashMap::new(),
        }
    }

    /// Register a tool: its schema (name/description/parameters) + the function to run.
    pub fn with_tool(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        run: ToolFn,
    ) -> Self {
        let name = name.into();
        let tool = Tool {
            kind: "function".to_string(),
            function: crate::schema::FunctionDef {
                name: name.clone(),
                description: Some(description.into()),
                parameters: Some(parameters),
            },
        };
        self.tools.insert(name, (tool, run));
        self
    }
}

#[async_trait]
impl McpClient for StaticMcpClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, KgError> {
        Ok(self.tools.values().map(|(t, _)| t.clone()).collect())
    }

    async fn call_tool(&self, name: &str, arguments: &str) -> Result<String, KgError> {
        let (_, run) = self
            .tools
            .get(name)
            .ok_or_else(|| KgError::internal(format!("unknown tool: {name}")))?;
        run(arguments)
    }
}

// ---------------------------------------------------------------------------
// Stdio MCP client
// ---------------------------------------------------------------------------
//
// A real MCP client that speaks newline-delimited JSON-RPC 2.0 over the
// stdin/stdout of an external tool-server subprocess.
//
// The protocol logic lives in `Connection`, which is generic over any
// `AsyncWrite` (the peer's stdin) + a line source over any `AsyncRead` (the
// peer's stdout). This keeps it unit-testable over an in-memory
// `tokio::io::duplex` pipe without spawning a real process — see the tests.
//
// Concurrency: all requests are serialized through a `Mutex<Connection>`. A
// request locks the connection, writes one JSON object + '\n', flushes, then
// reads response lines until it finds the one whose `id` matches the request
// (lines that are notifications, or carry a different id — e.g. server log
// notifications — are skipped). This avoids a full actor/dispatcher and is
// sufficient for a tool gateway.

type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;
type BoxReader = Box<dyn AsyncRead + Send + Unpin>;

/// The JSON-RPC transport + protocol over a writer (peer stdin) and a line
/// source (peer stdout). Not `Clone`; guarded by a `Mutex` in `StdioMcpClient`.
struct Connection {
    writer: BoxWriter,
    lines: Lines<BufReader<BoxReader>>,
    next_id: i64,
}

impl Connection {
    /// Build a connection from arbitrary read/write halves. Used both by
    /// `StdioMcpClient::connect` (real subprocess pipes) and by tests
    /// (in-memory duplex halves).
    pub(crate) fn new(writer: BoxWriter, reader: BoxReader) -> Self {
        Self {
            writer,
            lines: BufReader::new(reader).lines(),
            next_id: 0,
        }
    }

    /// Serialize `msg` as one line of JSON, write it, and flush.
    async fn write_line(&mut self, msg: &Value) -> Result<(), KgError> {
        let mut line = serde_json::to_string(msg)
            .map_err(|e| KgError::internal(format!("mcp: serialize request: {e}")))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| KgError::internal(format!("mcp: write: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| KgError::internal(format!("mcp: flush: {e}")))?;
        Ok(())
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn notify(&mut self, method: &str) -> Result<(), KgError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        self.write_line(&msg).await
    }

    /// Send a request with a fresh id, then read lines until the response whose
    /// id matches. Returns the `result` value, or maps a JSON-RPC `error` to a
    /// `KgError::internal`.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, KgError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            msg["params"] = params;
        }
        self.write_line(&msg).await?;

        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|e| KgError::internal(format!("mcp: read: {e}")))?
                .ok_or_else(|| KgError::internal("mcp: connection closed before response"))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Skip any line that is not a well-formed JSON object (stray logs).
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Only the response carrying our id is ours; skip notifications
            // (no id) and responses to other requests (different id).
            match value.get("id").and_then(Value::as_i64) {
                Some(rid) if rid == id => {
                    if let Some(err) = value.get("error") {
                        let message = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        return Err(KgError::internal(format!("mcp: {message}")));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => continue,
            }
        }
    }

    /// Perform the MCP handshake: `initialize` request, then the
    /// `notifications/initialized` notification.
    pub(crate) async fn handshake(&mut self) -> Result<(), KgError> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "kgateway", "version": "0.1.0" },
        });
        self.request("initialize", Some(params)).await?;
        self.notify("notifications/initialized").await?;
        Ok(())
    }

    /// `tools/list` → map each server tool into the gateway's `Tool` schema.
    pub(crate) async fn list_tools(&mut self) -> Result<Vec<Tool>, KgError> {
        let result = self.request("tools/list", None).await?;
        let raw = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut tools = Vec::with_capacity(raw.len());
        for t in raw {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            let parameters = t.get("inputSchema").cloned();
            tools.push(Tool {
                kind: "function".to_string(),
                function: FunctionDef {
                    name,
                    description,
                    parameters,
                },
            });
        }
        Ok(tools)
    }

    /// `tools/call` → concatenate the text of all `type:"text"` content items.
    /// `arguments` is the OpenAI-style JSON-encoded argument string; it is
    /// parsed into an object (defaulting to `{}` on empty/parse failure). If the
    /// result's `isError` is true, the concatenated text is returned as an error.
    pub(crate) async fn call_tool(
        &mut self,
        name: &str,
        arguments: &str,
    ) -> Result<String, KgError> {
        let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
        let params = json!({ "name": name, "arguments": args });
        let result = self.request("tools/call", Some(params)).await?;

        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut text = String::new();
        if let Some(items) = result.get("content").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(s) = item.get("text").and_then(Value::as_str) {
                        text.push_str(s);
                    }
                }
            }
        }
        if is_error {
            return Err(KgError::internal(text));
        }
        Ok(text)
    }
}

/// A real MCP client backed by an external tool-server subprocess, speaking
/// JSON-RPC 2.0 over the child's stdin/stdout. Tools are fetched once at
/// connect time and cached.
pub struct StdioMcpClient {
    name: String,
    tools: Vec<Tool>,
    conn: Mutex<Connection>,
    /// Kept alive so the subprocess isn't reaped; `kill_on_drop` cleans it up
    /// when the client is dropped. `None` for test connections over a pipe.
    _child: Option<Child>,
}

impl StdioMcpClient {
    /// Spawn `command args...` as an MCP tool server, perform the handshake, and
    /// cache its tool list.
    pub async fn connect(
        name: impl Into<String>,
        command: &str,
        args: &[String],
    ) -> Result<Self, KgError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| KgError::internal(format!("mcp: spawn {command}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| KgError::internal("mcp: child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| KgError::internal("mcp: child stdout unavailable"))?;

        let mut conn = Connection::new(Box::new(stdin), Box::new(stdout));
        conn.handshake().await?;
        let tools = conn.list_tools().await?;

        Ok(Self {
            name: name.into(),
            tools,
            conn: Mutex::new(conn),
            _child: Some(child),
        })
    }

    /// Build a client directly from read/write halves (no subprocess). Runs the
    /// handshake and caches tools. Used by the unit tests over a duplex pipe.
    #[cfg(test)]
    pub(crate) async fn from_halves(
        name: impl Into<String>,
        writer: BoxWriter,
        reader: BoxReader,
    ) -> Result<Self, KgError> {
        let mut conn = Connection::new(writer, reader);
        conn.handshake().await?;
        let tools = conn.list_tools().await?;
        Ok(Self {
            name: name.into(),
            tools,
            conn: Mutex::new(conn),
            _child: None,
        })
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_tools(&self) -> Result<Vec<Tool>, KgError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&self, name: &str, arguments: &str) -> Result<String, KgError> {
        let mut conn = self.conn.lock().await;
        conn.call_tool(name, arguments).await
    }
}

#[cfg(test)]
mod stdio_tests {
    use super::*;
    use tokio::io::DuplexStream;
    use tokio::sync::mpsc::UnboundedSender;

    /// A minimal in-memory MCP tool server. Reads request lines from the client
    /// and replies with canned JSON-RPC responses, dispatching on `method`.
    /// `list_response` / `call_response` are full JSON-RPC responses *without*
    /// an id (the id is injected from the request). `recorder`, if set, receives
    /// each `tools/call` request the client sent.
    async fn run_mock_server(
        stream: DuplexStream,
        list_response: Value,
        call_response: Value,
        recorder: Option<UnboundedSender<Value>>,
    ) {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let id = req.get("id").cloned().unwrap_or(Value::Null);

            let mut template = match method {
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "result": { "protocolVersion": "2024-11-05", "capabilities": {} },
                }),
                "tools/list" => list_response.clone(),
                "tools/call" => {
                    if let Some(tx) = &recorder {
                        let _ = tx.send(req.clone());
                    }
                    call_response.clone()
                }
                // Notifications (e.g. notifications/initialized): no response.
                _ => continue,
            };
            template["id"] = id;

            let mut out = serde_json::to_string(&template).unwrap();
            out.push('\n');
            writer.write_all(out.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
        }
    }

    /// Spin up a mock server on one end of a duplex pipe and return a client
    /// connected to the other end (handshake + tools/list already done).
    async fn build_client(
        list_response: Value,
        call_response: Value,
        recorder: Option<UnboundedSender<Value>>,
    ) -> Result<StdioMcpClient, KgError> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_server(
            server_stream,
            list_response,
            call_response,
            recorder,
        ));
        let (reader, writer) = tokio::io::split(client_stream);
        StdioMcpClient::from_halves("mock", Box::new(writer), Box::new(reader)).await
    }

    #[tokio::test]
    async fn handshake_and_list_tools() {
        let schema = json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } },
        });
        let list = json!({
            "jsonrpc": "2.0",
            "result": { "tools": [
                { "name": "echo", "description": "Echoes input", "inputSchema": schema },
            ] },
        });
        let call = json!({ "jsonrpc": "2.0", "result": { "content": [] } });

        let client = build_client(list, call, None).await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].kind, "function");
        assert_eq!(tools[0].function.name, "echo");
        assert_eq!(
            tools[0].function.description.as_deref(),
            Some("Echoes input")
        );
        assert_eq!(tools[0].function.parameters, Some(schema));
        // Default `has_tool` uses list_tools.
        assert!(client.has_tool("echo").await);
        assert!(!client.has_tool("nope").await);
    }

    #[tokio::test]
    async fn call_tool_returns_text_and_sends_parsed_arguments() {
        let list = json!({ "jsonrpc": "2.0", "result": { "tools": [] } });
        let call = json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [{ "type": "text", "text": "result-42" }],
                "isError": false,
            },
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let client = build_client(list, call, Some(tx)).await.unwrap();
        let out = client.call_tool("adder", r#"{"a":1,"b":2}"#).await.unwrap();
        assert_eq!(out, "result-42");

        // Verify the request the client actually sent.
        let req = rx.recv().await.unwrap();
        assert_eq!(req["method"], json!("tools/call"));
        assert_eq!(req["params"]["name"], json!("adder"));
        assert_eq!(req["params"]["arguments"], json!({ "a": 1, "b": 2 }));
    }

    #[tokio::test]
    async fn call_tool_maps_jsonrpc_error_to_err() {
        let list = json!({ "jsonrpc": "2.0", "result": { "tools": [] } });
        let call = json!({
            "jsonrpc": "2.0",
            "error": { "code": -32000, "message": "boom" },
        });

        let client = build_client(list, call, None).await.unwrap();
        let err = client.call_tool("x", "{}").await.unwrap_err();
        assert!(
            err.message.contains("boom"),
            "expected error message to contain 'boom', got: {}",
            err.message
        );
    }
}
