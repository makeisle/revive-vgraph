//! mcp-kit — 미니멀 MCP stdio 서버 골격 (newline-delimited JSON-RPC 2.0).
//!
//! vgraph-mcp에서 라이브 검증된 프로토콜 계층을 추출한 공용 크레이트 —
//! vgraph-mcp와, 이 계층으로 MCP 서버를 만드는 다른 크레이트가 함께 쓴다.
//! 규약: initialize(서버 전역 instructions 주입)/ping/tools/list/tools/call,
//! id 없는 메시지(notification)는 무응답, 알 수 없는 메서드는 -32601,
//! 툴 오류는 JSON-RPC error가 아니라 tools/call 결과의 `isError` + 사람 문장.

#[cfg(feature = "http")]
pub mod http;

use serde_json::{json, Value};

/// 한 메시지 처리 — 순수 함수라 stdio 없이 결정적으로 테스트된다.
/// `tools`는 tools/list 응답, `call`은 tools/call 디스패치(Ok=결과 JSON, Err=사람 문장).
pub fn handle_message(
    msg: &Value,
    server_name: &str,
    server_version: &str,
    instructions: &str,
    tools: impl FnOnce() -> Vec<Value>,
    call: impl FnOnce(&str, &Value) -> Result<Value, String>,
) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str())?;
    let id = msg.get("id").cloned()?; // id 없음 = notification → 무응답
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": msg
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": server_name, "version": server_version },
            "instructions": instructions,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            match call(name, &args) {
                Ok(v) => Ok(json!({
                    "content": [{ "type": "text",
                                  "text": serde_json::to_string_pretty(&v).unwrap() }],
                })),
                Err(e) => Ok(json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true,
                })),
            }
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

/// stdio 서빙 루프 — stdout은 JSON-RPC 응답 전용 채널 (로그는 stderr 소관).
pub fn serve_stdio(handle_line: impl Fn(&str) -> Option<String>) -> std::io::Result<()> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_line(&line) {
            let mut out = stdout.lock();
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}
