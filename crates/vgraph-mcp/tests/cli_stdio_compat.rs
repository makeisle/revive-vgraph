//! 하위 명령 없이 실행하면 예전과 똑같이 stdio MCP 서버로 동작한다(기존 사용 방식 보존).
//! 실제 실행 파일을 기존 평면 인자로 띄우고 stdin/stdout으로 JSON-RPC 줄을 주고받는다. 모킹 없음.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const TOOL_COUNT: usize = 20;

/// 요청 줄들을 보내고 stdin을 닫은 뒤 응답 줄들을 모두 읽는다. 종료 코드도 돌려준다.
fn run_stdio(args: &[&str], requests: &[Value]) -> (Vec<Value>, std::process::ExitStatus, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("vgraph-mcp 실행");
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
    } // stdin 닫힘 -> 서버가 끝난다
    let out = BufReader::new(child.stdout.take().unwrap());
    let replies: Vec<Value> = out
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).unwrap_or_else(|e| panic!("JSON 아님: {e}: {l}")))
        .collect();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    let status = child.wait().unwrap();
    (replies, status, err)
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn tool(id: u64, name: &str, args: Value) -> Value {
    rpc(id, "tools/call", json!({"name":name,"arguments":args}))
}

fn payload(reply: &Value) -> Value {
    let txt = reply["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(txt).unwrap_or(Value::String(txt.to_string()))
}

#[test]
fn legacy_flat_args_still_serve_stdio() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let (replies, status, err) = run_stdio(
        &[
            "--root",
            root,
            "--user",
            "alice",
            "--embedder",
            "toy",
            "--workspace",
            "w1",
        ],
        &[
            rpc(
                1,
                "initialize",
                json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}),
            ),
            rpc(2, "tools/list", json!({})),
            tool(3, "vgraph_create", json!({"name":"acme/catalog"})),
            tool(4, "vgraph_list", json!({})),
        ],
    );
    assert!(status.success(), "종료 코드 {status:?}: {err}");
    assert_eq!(replies.len(), 4, "{replies:?}");
    assert_eq!(
        replies[0]["result"]["serverInfo"]["name"],
        json!("vgraph-mcp")
    );
    assert_eq!(
        replies[1]["result"]["tools"].as_array().unwrap().len(),
        TOOL_COUNT
    );
    assert!(
        replies[2]["result"]["isError"] != json!(true),
        "{}",
        replies[2]
    );
    // --user alice 가 적용됐다: 목록에 방금 만든 그래프가 보인다
    assert_eq!(
        payload(&replies[3])["graphs"][0]["name"],
        json!("acme/catalog"),
        "{}",
        replies[3]
    );
}

#[test]
fn stdio_mode_defaults_are_unchanged() {
    // 인자 없이도 뜬다(기본 --root ./vgraph-data 는 현재 폴더 기준이라 임시 폴더에서 실행)
    let dir = TempDir::new().unwrap();
    let mut child = Command::new(BIN)
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", rpc(1, "tools/list", json!({}))).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), TOOL_COUNT);
    assert!(
        dir.path().join("vgraph-data").is_dir(),
        "기본 데이터 폴더가 ./vgraph-data 가 아님"
    );
}

#[test]
fn stdio_mode_leaves_no_server_files_and_ignores_running_state() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    let (_r, status, err) = run_stdio(&["--root", root], &[rpc(1, "tools/list", json!({}))]);
    assert!(status.success(), "{err}");
    assert!(!err.contains("서버"), "stdio 모드에 새 경고가 생김: {err}");
    for name in ["server.lock", "server.info", "server.stop"] {
        assert!(!dir.path().join(name).exists(), "stdio 가 {name} 을 만듦");
    }
}

#[test]
fn invalid_workspace_still_fails_startup_with_existing_message() {
    let dir = TempDir::new().unwrap();
    let (_r, status, err) = run_stdio(
        &["--root", dir.path().to_str().unwrap(), "--workspace", "a b"],
        &[],
    );
    assert!(!status.success());
    assert!(
        err.contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
        "{err}"
    );
}

#[test]
fn help_keeps_existing_workspace_warning_and_lists_every_old_option() {
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(out.status.success());
    let t = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "--root",
        "--user",
        "--embedder",
        "--embed-fields",
        "--chunker",
        "--no-embed-fallback",
        "--workspace",
        "각자 다른 --workspace 이름",
        "사라질 수 있습니다",
        "동시에 쓰는 프로세스마다 고유",
        "같은 이름을 동시에 쓰면",
        "자동으로 합쳐지고",
        "충돌로 보고",
        "./vgraph-data",
        "저장소 루트 디렉터리",
    ] {
        assert!(t.contains(needle), "--help 에 없음: {needle}\n{t}");
    }
    // 서버 모드 안내는 더해진 것이다
    for needle in ["serve", "stop", "status"] {
        assert!(
            t.contains(needle),
            "--help 에 하위 명령 안내가 없음: {needle}\n{t}"
        );
    }
}

#[test]
fn serve_does_not_accept_workspace_option() {
    let dir = TempDir::new().unwrap();
    let out = Command::new(BIN)
        .args([
            "serve",
            "--root",
            dir.path().to_str().unwrap(),
            "--workspace",
            "x",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--workspace"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
