//! "Test" for an MCP server in Settings › AI Assist: connect, initialize and
//! list its tools, so a bad command, URL or header shows up before a call and
//! the allowlist can be picked from the real tool names.
//!
//! This is only the settings check. The research lane itself connects from
//! the Pipecat sidecar through Pipecat's `MCPClient` (stdio, SSE and
//! streamable HTTP); this probe speaks the same JSON-RPC handshake for stdio
//! and streamable HTTP. Legacy SSE servers are not probed.

use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::settings::{McpServer, McpTransport};

/// A local server may first fetch a token (mcp-connector reads Bitwarden),
/// so this is generous.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

pub async fn list_tools(server: &McpServer) -> Result<Vec<ToolInfo>, String> {
    let timed_out = || format!("{} did not answer within {} s", server.name, PROBE_TIMEOUT.as_secs());
    match server.transport {
        McpTransport::Stdio => stdio(server).await,
        McpTransport::Http => tokio::time::timeout(PROBE_TIMEOUT, http(server)).await.map_err(|_| timed_out())?,
        McpTransport::Sse => Err("Test only covers local and streamable-HTTP servers; an SSE server is \
                                  checked when the first lookup runs"
            .to_string()),
    }
}

fn initialize() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "meetily-ai-assist", "version": env!("CARGO_PKG_VERSION") },
        },
    })
}

fn initialized() -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

fn tools_list() -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} })
}

/// The tools in a `tools/list` result, or the server's JSON-RPC error.
fn tools_from(response: &serde_json::Value) -> Result<Vec<ToolInfo>, String> {
    if let Some(err) = response.get("error") {
        return Err(format!("the server refused tools/list: {err}"));
    }
    let tools = response
        .pointer("/result/tools")
        .and_then(|t| t.as_array())
        .ok_or("the server's tools/list reply held no tools array")?;
    Ok(tools
        .iter()
        .filter_map(|t| {
            Some(ToolInfo {
                name: t.get("name")?.as_str()?.to_string(),
                description: t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect())
}

async fn stdio(server: &McpServer) -> Result<Vec<ToolInfo>, String> {
    let mut cmd = tokio::process::Command::new(server.command.trim());
    cmd.args(&server.args)
        // DO NOT pass the app's whole environment: inside the AppImage it
        // carries PYTHONHOME / LD_LIBRARY_PATH for the bundled runtime, which
        // breaks a Python server (the WORK-559 sidecar trap).
        .env_clear()
        .envs(crate::audio::pipecat::inherited_env(std::env::vars()))
        .envs(server.env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Its own process group, so the whole tree goes at the end: a shell,
    // `npx` or relay often starts the real server as a child, which killing
    // the direct child alone would leave running. Unix only (tokio has no
    // process groups elsewhere); other platforms kill the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", server.command.trim()))?;
    let group = child.id();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut lines = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();

    let talk = async {
        let send = |msg: serde_json::Value| format!("{msg}\n");
        stdin.write_all(send(initialize()).as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
        read_reply(&mut lines, 1).await?;
        stdin.write_all(send(initialized()).as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
        stdin.write_all(send(tools_list()).as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
        read_reply(&mut lines, 2).await
    };
    let reply = tokio::time::timeout(PROBE_TIMEOUT, talk)
        .await
        .map_err(|_| format!("{} did not answer within {} s", server.name, PROBE_TIMEOUT.as_secs()));
    drop(stdin);
    #[cfg(unix)]
    if let Some(pgid) = group {
        kill_group(pgid);
    }
    #[cfg(not(unix))]
    let _ = group;
    let _ = child.kill().await;
    tools_from(&reply??)
}

/// SIGKILL a process group (tokio has no group kill; `kill(1)` does).
#[cfg(unix)]
fn kill_group(pgid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The JSON-RPC response with `id`, skipping notifications and log lines.
async fn read_reply(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Result<serde_json::Value, String> {
    while let Some(line) = lines.next_line().await.map_err(|e| format!("read failed: {e}"))? {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            if id == 1 {
                if let Some(err) = value.get("error") {
                    return Err(format!("the server refused initialize: {err}"));
                }
            }
            return Ok(value);
        }
    }
    Err("the server exited before it answered".into())
}

async fn http(server: &McpServer) -> Result<Vec<ToolInfo>, String> {
    let client = reqwest::Client::new();
    let url = server.url.trim();
    let post = |body: serde_json::Value, session: Option<&str>| {
        let mut req = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&body);
        for (k, v) in &server.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }
        req.send()
    };
    let resp = post(initialize(), None).await.map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = resp.status();
    let session = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = resp.text().await.map_err(|e| format!("could not read the reply: {e}"))?;
    if !status.is_success() {
        return Err(format!("initialize returned HTTP {status}: {}", body.chars().take(300).collect::<String>()));
    }
    let init = reply_with_id(&body, 1).ok_or("the initialize reply held no JSON-RPC response")?;
    if let Some(err) = init.get("error") {
        return Err(format!("the server refused initialize: {err}"));
    }
    let _ = post(initialized(), session.as_deref()).await;
    let resp = post(tools_list(), session.as_deref()).await.map_err(|e| format!("tools/list failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("could not read the reply: {e}"))?;
    if !status.is_success() {
        return Err(format!("tools/list returned HTTP {status}: {}", body.chars().take(300).collect::<String>()));
    }
    tools_from(&reply_with_id(&body, 2).ok_or("the tools/list reply held no JSON-RPC response")?)
}

/// The JSON-RPC response with `id` in a streamable-HTTP body: plain JSON, or
/// an SSE stream whose `data:` lines carry the messages.
fn reply_with_id(body: &str, id: u64) -> Option<serde_json::Value> {
    let matches = |v: &serde_json::Value| v.get("id").and_then(|i| i.as_u64()) == Some(id);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) {
        return match v {
            serde_json::Value::Array(items) => items.into_iter().find(matches),
            v if matches(&v) => Some(v),
            _ => None,
        };
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
        .find(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_found_in_json_and_sse_bodies() {
        let json = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search_knowledge","description":"d"}]}}"#;
        let got = tools_from(&reply_with_id(json, 2).unwrap()).unwrap();
        assert_eq!(got, vec![ToolInfo { name: "search_knowledge".into(), description: "d".into() }]);
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n";
        assert_eq!(tools_from(&reply_with_id(sse, 2).unwrap()).unwrap(), vec![]);
        assert!(reply_with_id("{\"jsonrpc\":\"2.0\",\"id\":1}", 2).is_none());
        let err = serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"no"}});
        assert!(tools_from(&err).is_err());
    }

    /// A tiny stdio MCP server in shell: answers initialize and tools/list.
    #[tokio::test]
    async fn a_stdio_server_lists_its_tools() {
        let script = r#"while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t","version":"1"}}}' ;;
    *'"id":2'*) echo 'log noise'; echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search_knowledge","description":"Search"}]}}' ;;
  esac
done"#;
        let server = McpServer {
            id: "t".into(),
            name: "t".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            allowed_tools: vec![],
        };
        let tools = list_tools(&server).await.unwrap();
        assert_eq!(tools[0].name, "search_knowledge");

        let missing = McpServer { command: "/nonexistent/mcp".into(), ..server };
        assert!(list_tools(&missing).await.unwrap_err().contains("could not start"));
    }

    /// A server that starts its real worker as a child and then answers:
    /// the worker must not outlive the Test.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_whole_process_tree_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild.pid");
        let script = format!(
            r#"sleep 300 & echo $! > {m}
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}' ;;
    *'"id":2'*) echo '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[]}}}}' ;;
  esac
done"#,
            m = marker.display()
        );
        let server = McpServer {
            id: "t".into(),
            name: "t".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            allowed_tools: vec![],
        };
        assert_eq!(list_tools(&server).await.unwrap(), vec![]);
        let pid = std::fs::read_to_string(&marker).unwrap().trim().to_string();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "grandchild {pid} survived the Test");
    }
}
