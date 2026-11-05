//! `vgraph-mcp serve` — 실제 실행 파일을 임시 데이터 폴더·임시 포트로 띄워 HTTP로 부른다.
//! 모킹 없음: 서버는 실제 프로세스이고, 클라이언트는 `ureq`다.
//! 시험이 띄운 서버는 `Server`가 drop될 때 반드시 끝낸다(남은 프로세스 0).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const START_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(50);
const OWNER: &str = "owner";
const GRAPH: &str = "acme/catalog";
const TOOL_COUNT: usize = 20;
const SESSION_HEADER: &str = "mcp-session-id";
const WORKSPACE_HEADER: &str = "X-Vgraph-Workspace";
const USER_HEADER: &str = "X-Vgraph-User";
const THREADS: usize = 4;
const ROUNDS: i64 = 4;

// ---------------------------------------------------------------- 프로세스

/// 떠 있는 `serve` 프로세스. drop되면 반드시 죽인다.
struct Server {
    child: Child,
    addr: SocketAddr,
    log: Arc<Mutex<String>>,
}

fn pump(
    stream: impl Read + Send + 'static,
    log: Arc<Mutex<String>>,
    found: Option<mpsc::Sender<SocketAddr>>,
) {
    std::thread::spawn(move || {
        let mut found = found;
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Some(tx) = found.as_ref() {
                if let Some(idx) = line.find("http://") {
                    let host = line[idx + "http://".len()..]
                        .split('/')
                        .next()
                        .unwrap_or("");
                    if let Ok(addr) = host.parse::<SocketAddr>() {
                        let _ = tx.send(addr);
                        found = None;
                    }
                }
            }
            let mut g = log.lock().unwrap();
            g.push_str(&line);
            g.push('\n');
        }
    });
}

impl Server {
    fn start(root: &Path, extra: &[&str]) -> Server {
        Self::start_at(root, "127.0.0.1:0", extra)
    }

    fn start_at(root: &Path, listen: &str, extra: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", listen, "--user", OWNER])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("vgraph-mcp serve 실행");
        let log = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = mpsc::channel();
        pump(child.stdout.take().unwrap(), Arc::clone(&log), Some(tx));
        pump(child.stderr.take().unwrap(), Arc::clone(&log), None);
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(addr) => Server { child, addr, log },
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let out = log.lock().unwrap().clone();
                panic!("서버가 주소를 알리지 않음({e}). 출력:\n{out}");
            }
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn output(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "서버가 시간 안에 끝나지 않음");
            std::thread::sleep(POLL);
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("UTF-8 경로")
}

/// 하위 명령(또는 인자)을 한 번 실행하고 끝나길 기다린다.
fn run_cli(args: &[&str]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("vgraph-mcp 실행");
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("명령이 시간 안에 끝나지 않음: {args:?}");
        }
        std::thread::sleep(POLL);
    }
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn port_closed(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err()
}

// ---------------------------------------------------------------- HTTP 클라이언트

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!(
                "본문이 JSON이 아님: {e}\n상태={} 본문={:?}",
                self.status, self.body
            )
        })
    }
}

fn send(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&Value>) -> Reply {
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = match body {
        Some(b) => req
            .set("content-type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("전송 실패 {method} {url}: {e}"),
    };
    Reply {
        status: resp.status(),
        session: resp.header(SESSION_HEADER).map(String::from),
        body: resp.into_string().unwrap_or_default(),
    }
}

fn url_of(addr: SocketAddr) -> String {
    format!("http://{addr}/mcp")
}

fn initialize_body() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"serve-http-test","version":"0"}}})
}

/// 초기화를 마친 MCP 세션 하나.
struct Mcp {
    url: String,
    session: String,
    headers: Vec<(String, String)>,
}

impl Mcp {
    fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Mcp {
        let r = send("POST", &url_of(addr), headers, Some(&initialize_body()));
        assert_eq!(r.status, 200, "initialize: {}", r.body);
        let session = r
            .session
            .clone()
            .unwrap_or_else(|| panic!("세션 ID 없음: {}", r.body));
        assert_eq!(
            r.json()["result"]["serverInfo"]["name"],
            json!("vgraph-mcp")
        );
        let mcp = Mcp {
            url: url_of(addr),
            session,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(mcp.post(&note).status, 202);
        mcp
    }

    fn post(&self, body: &Value) -> Reply {
        let mut h: Vec<(&str, &str)> = vec![(SESSION_HEADER, self.session.as_str())];
        h.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        send("POST", &self.url, &h, Some(body))
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let r = self.post(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}));
        assert_eq!(r.status, 200, "{method}: {}", r.body);
        r.json()
    }

    fn tool(&self, name: &str, args: Value) -> (bool, Value) {
        let resp = self.rpc("tools/call", json!({"name":name,"arguments":args}));
        let r = &resp["result"];
        let txt = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        let is_err = r["isError"].as_bool().unwrap_or(false);
        (
            is_err,
            serde_json::from_str(&txt).unwrap_or(Value::String(txt)),
        )
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let (is_err, body) = self.tool(name, args);
        assert!(!is_err, "도구 오류({name}): {body}");
        body
    }

    fn set_prop(&self, node: &str, v: i64) {
        self.call(
            "vgraph_edit",
            json!({"graph":GRAPH,"ops":[{"op":"set_prop","node":node,"key":"v","value":v}]}),
        );
    }

    fn commit(&self, msg: &str) -> Value {
        self.call("vgraph_commit", json!({"graph":GRAPH,"message":msg}))
    }

    fn prop(&self, slug: &str) -> Value {
        self.call("vgraph_show", json!({"addr":format!("{GRAPH}#{slug}")}))["props"]["v"].clone()
    }

    fn pending(&self) -> Value {
        self.call("vgraph_pending", json!({"graph":GRAPH}))
    }

    fn close(self) {
        let r = send(
            "DELETE",
            &self.url,
            &[(SESSION_HEADER, self.session.as_str())],
            None,
        );
        assert_eq!(r.status, 204, "DELETE: {}", r.body);
    }
}

/// 그래프를 만들고 노드 n0..n{count-1}(v=0)을 커밋해 둔다.
fn seed(addr: SocketAddr, count: usize) {
    let m = Mcp::connect(addr, &[(WORKSPACE_HEADER, "seed")]);
    m.call("vgraph_create", json!({"name":GRAPH}));
    let ops: Vec<Value> = (0..count)
        .map(|i| json!({"op":"add_node","labels":["Item"],"slug":format!("n{i}"),"props":{"v":0}}))
        .collect();
    m.call("vgraph_edit", json!({"graph":GRAPH,"ops":ops}));
    m.commit("seed");
    m.close();
}

/// 시간 제한이 있는 집결점. 한 스레드가 실패해 빠져도 나머지가 영원히 기다리지 않고 실패한다.
struct TimedBarrier {
    arrived: AtomicUsize,
    parties: usize,
}

impl TimedBarrier {
    fn new(parties: usize) -> Self {
        TimedBarrier {
            arrived: AtomicUsize::new(0),
            parties,
        }
    }

    /// `round`는 1부터 센다. 모든 참가자가 그 라운드에 도착해야 통과한다.
    fn wait(&self, round: usize) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while self.arrived.load(Ordering::SeqCst) < self.parties * round {
            assert!(
                Instant::now() < deadline,
                "다른 스레드가 집결점에 오지 않음(앞선 실패)"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

// ---------------------------------------------------------------- 시험

#[test]
fn http_roundtrip_initialize_list_edit_commit_query() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let m = Mcp::connect(srv.addr, &[]);

    let tools = m.rpc("tools/list", json!({}));
    assert_eq!(
        tools["result"]["tools"].as_array().unwrap().len(),
        TOOL_COUNT
    );

    m.call("vgraph_create", json!({"name":GRAPH}));
    m.call(
        "vgraph_edit",
        json!({"graph":GRAPH,"ops":[{"op":"add_node","labels":["Item"],"slug":"a","props":{"v":1}}]}),
    );
    let out = m.commit("first");
    assert!(
        matches!(out["outcome"].as_str(), Some("committed" | "rebased")),
        "{out}"
    );
    assert_eq!(m.prop("a"), json!(1));
    let list = m.call("vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["name"], json!(GRAPH), "{list}");
    m.close();
}

#[test]
fn second_serve_on_same_root_is_refused_and_first_keeps_answering() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let first = Mcp::connect(srv.addr, &[]);

    let out = run_cli(&[
        "serve",
        "--root",
        path_str(dir.path()),
        "--listen",
        "127.0.0.1:0",
    ]);
    assert!(
        !out.status.success(),
        "두 번째 서버가 떠 버림: {}",
        text(&out)
    );
    let t = text(&out);
    assert!(t.contains("이미 서버가 떠 있습니다"), "{t}");
    assert!(
        t.contains(&srv.addr.to_string()),
        "기존 서버 주소가 안 보임: {t}"
    );

    // 첫 서버는 그대로 응답한다
    assert_eq!(
        first.rpc("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        TOOL_COUNT
    );
    Mcp::connect(srv.addr, &[]).close();
}

#[test]
fn different_roots_run_side_by_side() {
    let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let sa = Server::start(a.path(), &[]);
    let sb = Server::start(b.path(), &[]);
    assert_ne!(sa.addr, sb.addr);
    Mcp::connect(sa.addr, &[]).close();
    Mcp::connect(sb.addr, &[]).close();
}

#[test]
fn headerless_sessions_get_separate_workspaces_and_lose_no_edit() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    seed(srv.addr, THREADS);

    let start = TimedBarrier::new(THREADS);
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let (addr, start) = (srv.addr, &start);
            scope.spawn(move || {
                let me = Mcp::connect(addr, &[]); // 헤더 없음 -> 서버가 세션마다 작업 공간을 붙인다
                let node = format!("n{t}");
                for round in 1..=ROUNDS {
                    start.wait(usize::try_from(round).unwrap());
                    me.set_prop(&node, round);
                    let p = me.pending();
                    assert_eq!(p["pending"]["props_set"], json!(1), "t{t} r{round}: {p}");
                    let out = me.commit(&format!("t{t} r{round}"));
                    assert!(
                        matches!(out["outcome"].as_str(), Some("committed" | "rebased")),
                        "t{t} r{round}: {out}"
                    );
                }
                me.close();
            });
        }
    });
    let check = Mcp::connect(srv.addr, &[]);
    for t in 0..THREADS {
        assert_eq!(check.prop(&format!("n{t}")), json!(ROUNDS), "n{t} 유실");
    }
}

#[test]
fn headerless_session_does_not_see_another_sessions_pending_edit() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    seed(srv.addr, 2);
    let a = Mcp::connect(srv.addr, &[]);
    let b = Mcp::connect(srv.addr, &[]);
    a.set_prop("n0", 9);
    assert_eq!(a.pending()["pending"]["props_set"], json!(1));
    let pb = b.pending();
    assert!(
        pb["working"].is_null() || pb["pending"]["props_set"] == json!(0),
        "다른 세션의 편집이 보임: {pb}"
    );
    // 같은 세션은 요청이 여러 번이어도 같은 작업 공간을 쓴다
    a.set_prop("n1", 3);
    assert_eq!(a.pending()["pending"]["props_set"], json!(2));
}

#[test]
fn same_workspace_header_resumes_same_workspace_across_reconnects() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    seed(srv.addr, 2);
    let h = [(WORKSPACE_HEADER, "keep")];

    let first = Mcp::connect(srv.addr, &h);
    first.set_prop("n0", 5);
    first.close();

    // 다른 이름으로는 보이지 않는다
    let other = Mcp::connect(srv.addr, &[(WORKSPACE_HEADER, "other")]);
    let po = other.pending();
    assert!(
        po["working"].is_null() || po["pending"]["props_set"] == json!(0),
        "{po}"
    );

    // 같은 이름으로 다시 붙으면 이어 쓴다
    let again = Mcp::connect(srv.addr, &h);
    assert_eq!(again.pending()["pending"]["props_set"], json!(1));
    again.commit("resumed");
    let plain = Mcp::connect(srv.addr, &[]);
    assert_eq!(plain.prop("n0"), json!(5));
}

#[test]
fn invalid_workspace_header_is_rejected_with_existing_message() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let url = url_of(srv.addr);

    // 초기화부터 거부: 오류 응답이고 세션이 발급되지 않는다
    let r = send(
        "POST",
        &url,
        &[(WORKSPACE_HEADER, "a b")],
        Some(&initialize_body()),
    );
    let body = r.json();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
        "{body}"
    );
    assert!(r.session.is_none(), "거부했는데 세션이 발급됨");

    // 정상 초기화 뒤에 요청이 나쁜 헤더를 실어 오면 그 요청만 거부
    let m = Mcp::connect(srv.addr, &[]);
    let bad = Mcp {
        url: m.url.clone(),
        session: m.session.clone(),
        headers: vec![(WORKSPACE_HEADER.into(), "../x".into())],
    };
    let resp = bad.rpc("tools/list", json!({}));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("작업 공간 이름은"),
        "{resp}"
    );
    assert_eq!(
        m.rpc("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        TOOL_COUNT
    );
}

#[test]
fn acl_follows_user_header() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    seed(srv.addr, 1);

    let stranger = Mcp::connect(srv.addr, &[(USER_HEADER, "stranger")]);
    let list = stranger.call("vgraph_list", json!({}));
    assert_eq!(list["graphs"], json!([]), "{list}");
    let (is_err, body) = stranger.tool(
        "vgraph_edit",
        json!({"graph":GRAPH,"ops":[{"op":"set_prop","node":"n0","key":"v","value":9}]}),
    );
    assert!(is_err, "다른 사용자의 편집이 통과함: {body}");

    // 헤더가 없으면 --user(소유자)
    let owner = Mcp::connect(srv.addr, &[]);
    let list = owner.call("vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["name"], json!(GRAPH), "{list}");
    // 헤더로 소유자를 직접 지정해도 같다
    let named = Mcp::connect(srv.addr, &[(USER_HEADER, OWNER)]);
    assert_eq!(
        named.call("vgraph_list", json!({}))["graphs"][0]["name"],
        json!(GRAPH)
    );
}

#[test]
fn status_then_stop_closes_port_and_releases_lock() {
    let dir = TempDir::new().unwrap();
    let root = path_str(dir.path()).to_string();
    let mut srv = Server::start(dir.path(), &[]);
    let addr = srv.addr;

    let st = run_cli(&["status", "--root", &root]);
    let t = text(&st);
    assert!(st.status.success(), "{t}");
    assert!(t.contains(&addr.to_string()), "{t}");
    assert!(t.contains(&srv.pid().to_string()), "{t}");

    let stop = run_cli(&["stop", "--root", &root]);
    let t = text(&stop);
    assert!(stop.status.success(), "{t}");
    assert!(!t.contains("강제"), "정상 종료가 아니라 강제 종료됨: {t}");
    srv.wait_exit();
    assert!(port_closed(addr), "stop 뒤에도 포트가 열려 있음");

    let st = run_cli(&["status", "--root", &root]);
    assert!(!st.status.success(), "{}", text(&st));
    assert!(text(&st).contains("떠 있지 않습니다"), "{}", text(&st));

    // 잠금이 풀렸으니 같은 폴더로 다시 띄울 수 있다
    let again = Server::start(dir.path(), &[]);
    Mcp::connect(again.addr, &[]).close();
}

#[test]
fn stop_without_server_is_a_noop_success() {
    let dir = TempDir::new().unwrap();
    let out = run_cli(&["stop", "--root", path_str(dir.path())]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("떠 있지 않습니다"), "{}", text(&out));
}

#[test]
fn restart_after_hard_kill_takes_over_stale_lock() {
    let dir = TempDir::new().unwrap();
    let mut first = Server::start(dir.path(), &[]);
    first.kill(); // 비정상 종료: 정리 코드가 돌지 않는다
    let second = Server::start(dir.path(), &[]);
    let st = run_cli(&["status", "--root", path_str(dir.path())]);
    let t = text(&st);
    assert!(st.status.success(), "{t}");
    assert!(
        t.contains(&second.pid().to_string()),
        "새 서버의 PID가 아님: {t}"
    );
    Mcp::connect(second.addr, &[]).close();
}

#[test]
fn token_file_requires_bearer() {
    let dir = TempDir::new().unwrap();
    let token = "t0ken-for-test-only";
    let tf = dir.path().join("token.txt");
    std::fs::write(&tf, format!("{token}\n")).unwrap();
    let root = TempDir::new().unwrap();
    let srv = Server::start(root.path(), &["--token-file", path_str(&tf)]);
    let url = url_of(srv.addr);

    let none = send("POST", &url, &[], Some(&initialize_body()));
    assert_eq!(none.status, 401, "{}", none.body);
    let wrong = send(
        "POST",
        &url,
        &[("Authorization", "Bearer nope")],
        Some(&initialize_body()),
    );
    assert_eq!(wrong.status, 401);

    let auth = format!("Bearer {token}");
    let m = Mcp::connect(srv.addr, &[("Authorization", auth.as_str())]);
    assert_eq!(
        m.rpc("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        TOOL_COUNT
    );
    assert!(
        !srv.output().contains(token),
        "토큰이 서버 출력에 나옴:\n{}",
        srv.output()
    );
}

// ---------------------------------------------------------------- 시작 인자 기록 (server.info)

fn read_info(root: &Path) -> String {
    std::fs::read_to_string(root.join("server.info")).expect("server.info")
}

/// 내부 명령 `start-args`의 출력: (`INLINE_TOKEN` 표시 여부, `ARG <값>` 줄들).
fn start_args(root: &Path) -> (bool, Vec<String>) {
    let out = run_cli(&["start-args", "--root", path_str(root)]);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let inline = stdout.lines().any(|l| l == "INLINE_TOKEN");
    let args = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("ARG "))
        .map(str::to_owned)
        .collect();
    (inline, args)
}

#[test]
fn server_info_records_the_start_args_with_token_file_path_only() {
    let tokens = TempDir::new().unwrap();
    let token = "t0ken-file-secret-value";
    let tf = tokens.path().join("token.txt");
    std::fs::write(&tf, token).unwrap();
    let root = TempDir::new().unwrap();
    let _srv = Server::start(root.path(), &["--token-file", path_str(&tf)]);

    let info: Value = serde_json::from_str(&read_info(root.path())).unwrap();
    let args: Vec<&str> = info["args"]
        .as_array()
        .expect("args 배열")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let at = args
        .iter()
        .position(|a| *a == "--token-file")
        .expect("--token-file");
    assert_eq!(args.get(at + 1).copied(), Some(path_str(&tf)));
    assert!(args.contains(&"--listen"), "{args:?}");
    assert!(info.get("args_unavailable").is_none(), "{info}");
    assert!(
        !read_info(root.path()).contains(token),
        "토큰 값이 파일에 있다"
    );

    let (inline, recorded) = start_args(root.path());
    assert!(!inline);
    assert_eq!(recorded, args);
}

#[test]
fn inline_token_is_never_written_to_server_info() {
    let secret = "inline-secret-token-value-123";
    let root = TempDir::new().unwrap();
    let srv = Server::start(root.path(), &["--token", secret]);

    let raw = read_info(root.path());
    assert!(!raw.contains(secret), "토큰 값이 server.info에 있다: {raw}");
    let info: Value = serde_json::from_str(&raw).unwrap();
    assert!(
        info.get("args").is_none(),
        "인라인 토큰이면 args를 쓰지 않는다: {raw}"
    );
    assert_eq!(info["args_unavailable"], json!("inline_token"));
    // 기존 필드는 그대로
    assert_eq!(info["pid"], json!(srv.pid()));
    assert_eq!(info["addr"], json!(srv.addr.to_string()));

    let out = run_cli(&["start-args", "--root", path_str(root.path())]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(secret), "{}", text(&out));
    let (inline, recorded) = start_args(root.path());
    assert!(inline);
    assert!(recorded.is_empty(), "{recorded:?}");
    assert!(!srv.output().contains(secret), "{}", srv.output());
}

#[test]
fn old_format_server_info_without_args_is_still_read() {
    let root = TempDir::new().unwrap();
    let srv = Server::start(root.path(), &[]);
    // 옛 버전 서버가 쓴 형식: args·args_unavailable 가 없다.
    std::fs::write(
        root.path().join("server.info"),
        json!({"pid": srv.pid(), "addr": srv.addr.to_string(), "version": "0.0.1"}).to_string(),
    )
    .unwrap();

    let st = run_cli(&["status", "--root", path_str(root.path())]);
    assert!(st.status.success(), "{}", text(&st));
    assert!(text(&st).contains(&srv.addr.to_string()), "{}", text(&st));
    assert!(text(&st).contains(&srv.pid().to_string()), "{}", text(&st));
    let (inline, recorded) = start_args(root.path());
    assert!(!inline && recorded.is_empty());
}

#[test]
fn start_args_of_a_stopped_server_is_empty_success() {
    let root = TempDir::new().unwrap();
    let (inline, recorded) = start_args(root.path());
    assert!(!inline && recorded.is_empty());
}

#[test]
fn token_and_token_file_together_are_refused() {
    let dir = TempDir::new().unwrap();
    let out = run_cli(&[
        "serve",
        "--root",
        path_str(dir.path()),
        "--listen",
        "127.0.0.1:0",
        "--token",
        "x",
        "--token-file",
        "y.txt",
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("--token-file") && !t.contains("unexpected argument 'serve'"),
        "{t}"
    );
}

#[test]
fn missing_token_file_is_reported_without_leaking_anything() {
    let dir = TempDir::new().unwrap();
    let out = run_cli(&[
        "serve",
        "--root",
        path_str(dir.path()),
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        path_str(&dir.path().join("none.txt")),
    ]);
    assert!(!out.status.success());
    assert!(text(&out).contains("토큰 파일"), "{}", text(&out));
}

#[test]
fn busy_port_is_reported_and_refused() {
    let dir = TempDir::new().unwrap();
    let blocker = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = blocker.local_addr().unwrap().to_string();
    let out = run_cli(&["serve", "--root", path_str(dir.path()), "--listen", &busy]);
    assert!(!out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("이미 사용 중"), "{t}");
    assert!(t.contains(&busy), "{t}");
    // 실패한 기동이 잠금을 쥔 채 남지 않는다
    let ok = Server::start(dir.path(), &[]);
    Mcp::connect(ok.addr, &[]).close();
}

#[test]
fn non_loopback_listen_is_refused() {
    let dir = TempDir::new().unwrap();
    let out = run_cli(&[
        "serve",
        "--root",
        path_str(dir.path()),
        "--listen",
        "0.0.0.0:0",
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("루프백"), "{}", text(&out));
}

#[test]
fn serve_help_shows_connection_example_and_headers() {
    let out = run_cli(&["serve", "--help"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    for needle in [
        "--listen",
        "--token-file",
        "claude mcp add --transport http",
        "--header",
        WORKSPACE_HEADER,
        USER_HEADER,
    ] {
        assert!(t.contains(needle), "serve --help 에 없음: {needle}\n{t}");
    }
    assert!(
        !t.contains("--workspace"),
        "serve 는 --workspace 를 받지 않는다:\n{t}"
    );
}

// ---------------------------------------------------------------- 다른 프로세스가 만든 그래프

/// 별도 stdio 프로세스(`vgraph-mcp`, 같은 폴더)가 도구 호출을 차례로 처리하고 끝나길 기다린다.
/// 돌려주는 값은 각 호출의 오류 여부와 본문이다.
fn other_process_calls(root: &Path, calls: &[(&str, Value)]) -> Vec<(bool, Value)> {
    use std::io::Write as _;
    let mut child = Command::new(BIN)
        .args(["--root"])
        .arg(root)
        .args(["--user", OWNER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("vgraph-mcp stdio 실행");
    {
        let mut stdin = child.stdin.take().unwrap();
        for (i, (name, args)) in calls.iter().enumerate() {
            let req = json!({"jsonrpc":"2.0","id":i + 1,"method":"tools/call",
                "params":{"name":name,"arguments":args}});
            writeln!(stdin, "{req}").unwrap();
        }
    } // stdin 닫힘 -> 프로세스가 끝난다
    let out = BufReader::new(child.stdout.take().unwrap());
    let replies: Vec<Value> = out
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).unwrap_or_else(|e| panic!("JSON 아님: {e}: {l}")))
        .collect();
    let status = child.wait().unwrap();
    assert!(status.success(), "stdio 프로세스 비정상 종료: {status:?}");
    assert_eq!(replies.len(), calls.len(), "{replies:?}");
    replies
        .iter()
        .map(|r| {
            let txt = r["result"]["content"][0]["text"].as_str().unwrap_or("");
            (
                r["result"]["isError"].as_bool().unwrap_or(false),
                serde_json::from_str(txt).unwrap_or(Value::String(txt.to_string())),
            )
        })
        .collect()
}

#[test]
fn running_server_sees_graph_created_by_another_process_without_restart() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let m = Mcp::connect(srv.addr, &[]);

    // 서버가 떠 있는 동안, 별도 프로세스가 같은 폴더에 그래프를 만들고 노드를 커밋한다.
    let done = other_process_calls(
        dir.path(),
        &[
            ("vgraph_create", json!({"name":GRAPH})),
            (
                "vgraph_edit",
                json!({"graph":GRAPH,"ops":[{"op":"add_node","labels":["Item"],"slug":"a","props":{"v":1}}]}),
            ),
            ("vgraph_commit", json!({"graph":GRAPH,"message":"other"})),
        ],
    );
    for (is_err, body) in &done {
        assert!(!is_err, "다른 프로세스의 호출 실패: {body}");
    }

    // 서버(재시작 없음): 목록·조회·편집·커밋이 된다
    let list = m.call("vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["name"], json!(GRAPH), "{list}");
    assert_eq!(m.prop("a"), json!(1));
    m.set_prop("a", 5);
    let out = m.commit("from server");
    assert!(
        matches!(out["outcome"].as_str(), Some("committed" | "rebased")),
        "{out}"
    );
    assert_eq!(m.prop("a"), json!(5));

    // 다른 프로세스가 이미 만든 이름을 서버에서 만들면 거부된다
    let (is_err, body) = m.tool("vgraph_create", json!({"name":GRAPH}));
    assert!(is_err, "중복 만들기가 통과함: {body}");
    assert!(body.to_string().contains("graph exists"), "{body}");
    m.close();
}

#[test]
fn server_created_name_is_refused_for_another_process() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let m = Mcp::connect(srv.addr, &[]);
    m.call("vgraph_create", json!({"name":GRAPH}));
    let r = other_process_calls(dir.path(), &[("vgraph_create", json!({"name":GRAPH}))]);
    assert!(r[0].0, "중복 만들기가 통과함: {}", r[0].1);
    assert!(r[0].1.to_string().contains("graph exists"), "{}", r[0].1);
    m.close();
}

/// 서버에는 `vgraph_list` 를 부르지 않는다. 다른 프로세스가 만든 그래프를 주소 하나만으로
/// 찾아야 한다 — "모를 때 다시 읽기"(주소 해석 경로)가 실제로 일하는지 따로 가린다.
#[test]
fn running_server_resolves_other_process_graph_by_address_without_list() {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path(), &[]);
    let m = Mcp::connect(srv.addr, &[]);

    let done = other_process_calls(
        dir.path(),
        &[
            ("vgraph_create", json!({"name":GRAPH})),
            (
                "vgraph_edit",
                json!({"graph":GRAPH,"ops":[{"op":"add_node","labels":["Item"],"slug":"a","props":{"v":3}}]}),
            ),
            ("vgraph_commit", json!({"graph":GRAPH,"message":"other"})),
        ],
    );
    for (is_err, body) in &done {
        assert!(!is_err, "다른 프로세스의 호출 실패: {body}");
    }

    // list·create 등 다른 도구를 거치지 않고 곧바로 주소로 조회한다.
    assert_eq!(m.prop("a"), json!(3));
    m.close();
}
