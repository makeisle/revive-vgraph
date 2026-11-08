//! 원격 `serve`의 내보내기·가져오기 경로 정책(교환 폴더 한정) 시험.
//! 모킹 없음: 실제 `vgraph-mcp serve` 프로세스를 임시 데이터 폴더·임시 포트로 띄워 HTTP로 부르고,
//! 정책 판정(`PathPolicy::resolve`)과 서버 안 도구 호출(`McpServer::with_path_policy`)도 실제 파일시스템으로 본다.
//! 시험이 띄운 서버는 `Server`가 drop될 때 반드시 끝낸다.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use vgraph_core::{TestClock, TestIdGen};
use vgraph_engine::{Engine, EngineOptions, FsProvider};
use vgraph_mcp::{McpConfig, McpServer, PathPolicy};

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const ALIVE_WINDOW: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
const OWNER: &str = "owner";
const GRAPH: &str = "acme/catalog";
const COPY: &str = "acme/copy";
const SESSION_HEADER: &str = "mcp-session-id";
/// 데이터 폴더에 심어 두는 비밀 문장. 어떤 오류 문구에도 나오면 안 된다.
const SENTINEL: &str = "TOP-SECRET-SENTINEL-4f9a";

// ---------------------------------------------------------------- 서버 프로세스

struct Server {
    child: Child,
    addr: SocketAddr,
}

fn pump(stream: impl Read + Send + 'static, found: Option<mpsc::Sender<SocketAddr>>) {
    std::thread::spawn(move || {
        let mut found = found;
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let (Some(tx), Some(idx)) = (found.as_ref(), line.find("http://")) {
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
    });
}

impl Server {
    fn start(root: &Path, extra: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", "127.0.0.1:0", "--user", OWNER])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("vgraph-mcp serve 실행");
        let (tx, rx) = mpsc::channel();
        pump(child.stdout.take().unwrap(), Some(tx));
        pump(child.stderr.take().unwrap(), None);
        match rx.recv_timeout(START_TIMEOUT) {
            Ok(addr) => Server { child, addr },
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("서버가 주소를 알리지 않음({e})");
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- HTTP 클라이언트

fn send(url: &str, session: Option<&str>, body: &Value) -> (u16, Option<String>, String) {
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let mut req = agent.post(url).set("content-type", "application/json");
    if let Some(s) = session {
        req = req.set(SESSION_HEADER, s);
    }
    let resp = match req.send_string(&body.to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("전송 실패: {e}"),
    };
    (
        resp.status(),
        resp.header(SESSION_HEADER).map(String::from),
        resp.into_string().unwrap_or_default(),
    )
}

struct Mcp {
    url: String,
    session: String,
}

impl Mcp {
    fn connect(addr: SocketAddr) -> Mcp {
        let url = format!("http://{addr}/mcp");
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"serve-path-policy-test","version":"0"}}});
        let (status, session, body) = send(&url, None, &init);
        assert_eq!(status, 200, "initialize: {body}");
        let session = session.expect("세션 ID");
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(send(&url, Some(&session), &note).0, 202);
        Mcp { url, session }
    }

    /// (오류 여부, 본문 — JSON이면 값, 아니면 문자열)
    fn tool(&self, name: &str, args: Value) -> (bool, Value) {
        let req = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":name,"arguments":args}});
        let (status, _, body) = send(&self.url, Some(&self.session), &req);
        assert_eq!(status, 200, "{name}: {body}");
        let resp: Value = serde_json::from_str(&body).expect("JSON 응답");
        let r = &resp["result"];
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        (
            r["isError"].as_bool().unwrap_or(false),
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let (is_err, body) = self.tool(name, args);
        assert!(!is_err, "도구 오류({name}): {body}");
        body
    }

    /// 오류 문구를 돌려준다. 성공하면 시험 실패.
    fn denied(&self, name: &str, args: Value) -> String {
        let (is_err, body) = self.tool(name, args.clone());
        assert!(is_err, "거부되어야 함({name} {args}): {body}");
        body.as_str()
            .map(String::from)
            .unwrap_or_else(|| body.to_string())
    }
}

fn seed(mcp: &Mcp) {
    mcp.call("vgraph_create", json!({"name":GRAPH}));
    mcp.call(
        "vgraph_edit",
        json!({"graph":GRAPH,"ops":[
            {"op":"add_node","labels":["Item"],"slug":"a-1","props":{"name":"첫째"}},
            {"op":"add_node","labels":["Item"],"slug":"b-2","props":{"name":"둘째"}},
        ]}),
    );
    mcp.call("vgraph_commit", json!({"graph":GRAPH,"message":"seed"}));
}

fn pstr(p: &Path) -> String {
    p.to_str().expect("UTF-8 경로").to_string()
}

/// 오류 문구가 비밀 문장·실제 데이터 폴더 경로를 드러내지 않는지 본다.
fn assert_no_leak(msg: &str, root: &Path) {
    assert!(
        !msg.contains(SENTINEL),
        "오류 문구에 파일 내용이 있음: {msg}"
    );
    let root_text = pstr(root);
    assert!(
        !msg.contains(&root_text) && !msg.contains(&root_text.replace('\\', "/")),
        "오류 문구에 실제 경로가 있음: {msg}"
    );
}

/// 폴더 링크(Windows는 정션, 그 밖은 심볼릭 링크)를 만든다. 못 만들면 사유를 돌려준다.
fn make_dir_link(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let out = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string())
    }
}

/// 파일 심볼릭 링크. Windows는 권한이 없으면 실패한다.
fn make_file_link(link: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| e.to_string())
    }
}

/// 데이터 폴더 루트에 비밀 내용이 든 파일들을 심는다.
fn plant_root_files(root: &Path) {
    for name in ["secret.txt", "victim.db", "victim.db-wal"] {
        std::fs::write(root.join(name), SENTINEL).unwrap();
    }
}

// ---------------------------------------------------------------- 정책 단위 (실제 파일시스템)

fn policy_fixture() -> (TempDir, PathBuf, PathPolicy) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let exchange = root.join("exchange");
    std::fs::create_dir_all(exchange.join("sub")).unwrap();
    plant_root_files(&root);
    (tmp, root, PathPolicy::ExchangeDir(exchange))
}

#[test]
fn policy_unrestricted_passes_any_path_through() {
    let p = PathPolicy::Unrestricted
        .resolve("C:/anywhere/x.json")
        .unwrap();
    assert_eq!(p, PathBuf::from("C:/anywhere/x.json"));
}

#[test]
fn policy_allows_new_and_existing_files_inside_exchange() {
    let (_tmp, root, policy) = policy_fixture();
    let exchange = std::fs::canonicalize(root.join("exchange")).unwrap();
    // 상대 경로(새 파일, 하위 폴더)
    assert_eq!(
        policy.resolve("out.json").unwrap(),
        exchange.join("out.json")
    );
    assert_eq!(
        policy.resolve("sub/out.json").unwrap(),
        exchange.join("sub").join("out.json")
    );
    // 이미 있는 파일 + 교환 폴더 안의 절대 경로
    std::fs::write(exchange.join("have.json"), "{}").unwrap();
    let abs = pstr(&exchange.join("have.json"));
    assert_eq!(policy.resolve(&abs).unwrap(), exchange.join("have.json"));
}

#[test]
fn policy_rejects_escapes_and_root_files() {
    let (_tmp, root, policy) = policy_fixture();
    let slashed = pstr(&root).replace('\\', "/");
    let mut cases: Vec<String> = vec![
        String::new(),
        ".".into(),
        "..".into(),
        "../secret.txt".into(),
        "..\\secret.txt".into(),
        "sub/../../secret.txt".into(),
        "sub\\..\\../server.stop".into(),
        "./../server.info".into(),
        "/server.stop".into(),
        "\\server.stop".into(),
        "missing-dir/x.json".into(),
        pstr(&root),
        pstr(&root.join("secret.txt")),
        pstr(&root.join("server.stop")),
        pstr(&root.join("registry.jsonl")),
        pstr(&root.join("victim.db-wal")),
        pstr(&root.join("exchange")),
        format!("{slashed}/server.stop"),
        format!("{slashed}/exchange/../server.stop"),
    ];
    #[cfg(windows)]
    cases.extend(
        [
            "Z:\\nope\\x.json",
            "Z:/nope/x.json",
            "\\\\server\\share\\x.json",
            "//server/share/x.json",
            "\\\\?\\C:\\Windows\\win.ini",
            "C:x.json",
        ]
        .map(String::from),
    );
    for raw in &cases {
        let msg = policy
            .resolve(raw)
            .expect_err(&format!("거부되어야 함: {raw:?}"));
        assert_no_leak(&msg, &root);
    }
}

#[test]
fn policy_rejects_links_that_leave_exchange() {
    let (_tmp, root, policy) = policy_fixture();
    let exchange = root.join("exchange");
    let link = exchange.join("out-link");
    match make_dir_link(&link, &root) {
        Ok(()) => {
            for raw in ["out-link/secret.txt", "out-link/new.json", "out-link"] {
                let msg = policy.resolve(raw).expect_err(raw);
                assert_no_leak(&msg, &root);
            }
        }
        Err(why) => eprintln!("SKIP 폴더 링크 케이스 — 링크를 만들 수 없음: {why}"),
    }
    let flink = exchange.join("file-link.json");
    match make_file_link(&flink, &root.join("secret.txt")) {
        Ok(()) => {
            let msg = policy.resolve("file-link.json").expect_err("파일 링크");
            assert_no_leak(&msg, &root);
        }
        Err(why) => eprintln!("SKIP 파일 링크 케이스 — 링크를 만들 수 없음: {why}"),
    }
}

#[test]
fn policy_rejects_dangling_link_without_following() {
    let (_tmp, root, policy) = policy_fixture();
    let flink = root.join("exchange").join("dangling.json");
    match make_file_link(&flink, &root.join("not-yet.txt")) {
        Ok(()) => {
            assert!(policy.resolve("dangling.json").is_err());
            assert!(
                !root.join("not-yet.txt").exists(),
                "끊어진 링크를 따라 만들면 안 됨"
            );
        }
        Err(why) => eprintln!("SKIP 끊어진 링크 케이스 — 링크를 만들 수 없음: {why}"),
    }
}

#[test]
fn policy_fails_closed_when_exchange_dir_is_missing() {
    let tmp = TempDir::new().unwrap();
    let policy = PathPolicy::ExchangeDir(tmp.path().join("gone"));
    let msg = policy.resolve("x.json").expect_err("교환 폴더 없음");
    assert_no_leak(&msg, tmp.path());
}

// ---------------------------------------------------------------- 서버 안 도구 호출 (stdio와 같은 경로, 정책만 다름)

fn local_server(dir: &Path, policy: PathPolicy) -> McpServer {
    let engine = Engine::open(
        Box::new(FsProvider::new(dir).unwrap()),
        Arc::new(TestIdGen::new()),
        Arc::new(TestClock::default()),
        EngineOptions::default(),
    )
    .unwrap();
    let cfg = McpConfig {
        root: dir.to_path_buf(),
        user: OWNER.into(),
        embedder: None,
        embed: Default::default(),
    };
    McpServer::with_engine(engine, cfg).with_path_policy(policy)
}

fn local_call(srv: &McpServer, name: &str, args: Value) -> (bool, Value) {
    let msg = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":args}});
    let resp = srv.handle_message(&msg).expect("response");
    let r = &resp["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    (
        r["isError"].as_bool().unwrap_or(false),
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn local_seed(srv: &McpServer) {
    for (name, args) in [
        ("vgraph_create", json!({"name":GRAPH})),
        (
            "vgraph_edit",
            json!({"graph":GRAPH,"ops":[
                {"op":"add_node","labels":["Item"],"slug":"a-1","props":{"name":"첫째"}}]}),
        ),
        ("vgraph_commit", json!({"graph":GRAPH,"message":"seed"})),
    ] {
        let (e, b) = local_call(srv, name, args);
        assert!(!e, "{name}: {b}");
    }
}

#[test]
fn stdio_default_policy_exports_and_imports_any_path() {
    let dir = TempDir::new().unwrap();
    let srv = local_server(dir.path(), PathPolicy::Unrestricted);
    local_seed(&srv);
    let elsewhere = TempDir::new().unwrap();
    let file = pstr(&elsewhere.path().join("any.json"));
    let (e, b) = local_call(&srv, "vgraph_export", json!({"graph":GRAPH,"path":file}));
    assert!(!e, "{b}");
    assert!(Path::new(&file).is_file());
    let (e, b) = local_call(&srv, "vgraph_create", json!({"name":COPY}));
    assert!(!e, "{b}");
    let (e, b) = local_call(&srv, "vgraph_import", json!({"graph":COPY,"path":file}));
    assert!(!e, "{b}");
}

#[test]
fn restricted_server_rejects_outside_paths_but_serves_inside() {
    let dir = TempDir::new().unwrap();
    let exchange = dir.path().join("exchange");
    std::fs::create_dir_all(&exchange).unwrap();
    plant_root_files(dir.path());
    let srv = local_server(dir.path(), PathPolicy::ExchangeDir(exchange.clone()));
    local_seed(&srv);
    let outside = pstr(&dir.path().join("secret.txt"));
    let (e, b) = local_call(&srv, "vgraph_export", json!({"graph":GRAPH,"path":outside}));
    assert!(e, "{b}");
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        SENTINEL,
        "내용이 바뀌면 안 됨"
    );
    let (e, b) = local_call(&srv, "vgraph_import", json!({"graph":COPY,"path":outside}));
    assert!(e, "{b}");
    assert_no_leak(&b.to_string(), dir.path());
    // 교환 폴더 안은 된다
    let (e, b) = local_call(
        &srv,
        "vgraph_export",
        json!({"graph":GRAPH,"path":"ok.json"}),
    );
    assert!(!e, "{b}");
    assert!(exchange.join("ok.json").is_file());
    // path 없는 호출은 그대로 (응답에 document)
    let (e, b) = local_call(&srv, "vgraph_export", json!({"graph":GRAPH}));
    assert!(!e && b["document"].is_object(), "{b}");
}

// ---------------------------------------------------------------- 실제 serve 프로세스

fn start_remote() -> (TempDir, PathBuf, Server, Mcp) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    plant_root_files(&root);
    let server = Server::start(&root, &[]);
    let mcp = Mcp::connect(server.addr);
    seed(&mcp);
    (tmp, root, server, mcp)
}

#[test]
fn a2_remote_export_to_server_stop_is_rejected_and_server_stays_up() {
    let (_tmp, root, mut server, mcp) = start_remote();
    let stop = root.join("server.stop");
    for target in [pstr(&stop), pstr(&stop).replace('\\', "/")] {
        let msg = mcp.denied("vgraph_export", json!({"graph":GRAPH,"path":target}));
        assert_no_leak(&msg, &root);
    }
    assert!(!stop.exists(), "server.stop이 만들어지면 안 됨");
    let deadline = Instant::now() + ALIVE_WINDOW;
    while Instant::now() < deadline {
        assert!(server.is_alive(), "서버가 죽음");
        std::thread::sleep(POLL);
    }
    // 아직 도구가 응답한다
    mcp.call("vgraph_pending", json!({"graph":GRAPH}));
}

#[test]
fn a3_remote_cannot_write_or_read_root_files() {
    let (_tmp, root, _server, mcp) = start_remote();
    let before_registry = std::fs::read(root.join("registry.jsonl")).ok();
    let before_info = std::fs::read(root.join("server.info")).unwrap();
    for name in [
        "registry.jsonl",
        "server.info",
        "victim.db-wal",
        "victim.db",
    ] {
        let msg = mcp.denied(
            "vgraph_export",
            json!({"graph":GRAPH,"path":pstr(&root.join(name))}),
        );
        assert_no_leak(&msg, &root);
    }
    assert_eq!(
        std::fs::read(root.join("registry.jsonl")).ok(),
        before_registry
    );
    assert_eq!(
        std::fs::read(root.join("server.info")).unwrap(),
        before_info
    );
    assert_eq!(
        std::fs::read_to_string(root.join("victim.db-wal")).unwrap(),
        SENTINEL
    );
    // 루트 파일 읽기(가져오기)
    for name in ["secret.txt", "server.info", "victim.db-wal"] {
        let msg = mcp.denied(
            "vgraph_import",
            json!({"graph":COPY,"path":pstr(&root.join(name))}),
        );
        assert_no_leak(&msg, &root);
    }
    // 데이터 폴더 자체
    mcp.denied("vgraph_export", json!({"graph":GRAPH,"path":pstr(&root)}));
}

#[test]
fn a4_remote_rejects_traversal_mixed_separators_and_foreign_prefixes() {
    let (_tmp, root, _server, mcp) = start_remote();
    let mut paths = vec![
        "../server.stop".to_string(),
        "..\\server.stop".to_string(),
        "sub/../../secret.txt".to_string(),
        format!("{}/exchange/../secret.txt", pstr(&root).replace('\\', "/")),
        format!(
            "{}\\exchange\\..\\secret.txt",
            pstr(&root).replace('/', "\\")
        ),
    ];
    #[cfg(windows)]
    paths.extend(
        [
            "Z:\\nope\\x.json",
            "\\\\server\\share\\x.json",
            "//server/share/x.json",
        ]
        .map(String::from),
    );
    for raw in &paths {
        let msg = mcp.denied("vgraph_export", json!({"graph":GRAPH,"path":raw}));
        assert_no_leak(&msg, &root);
        let msg = mcp.denied("vgraph_import", json!({"graph":COPY,"path":raw}));
        assert_no_leak(&msg, &root);
    }
    assert!(!root.join("server.stop").exists());
}

#[test]
fn a4_remote_rejects_link_inside_exchange_pointing_out() {
    let (_tmp, root, _server, mcp) = start_remote();
    let link = root.join("exchange").join("out-link");
    match make_dir_link(&link, &root) {
        Ok(()) => {
            let msg = mcp.denied(
                "vgraph_export",
                json!({"graph":GRAPH,"path":"out-link/server.stop"}),
            );
            assert_no_leak(&msg, &root);
            let msg = mcp.denied(
                "vgraph_import",
                json!({"graph":COPY,"path":"out-link/secret.txt"}),
            );
            assert_no_leak(&msg, &root);
            assert!(!root.join("server.stop").exists());
        }
        Err(why) => eprintln!("SKIP 링크 케이스 — 링크를 만들 수 없음: {why}"),
    }
}

#[test]
fn a5_remote_export_and_import_inside_exchange_work() {
    let (_tmp, root, _server, mcp) = start_remote();
    let exchange = root.join("exchange");
    assert!(exchange.is_dir(), "교환 폴더 기본값은 <root>/exchange");
    // 상대 경로
    let out = mcp.call("vgraph_export", json!({"graph":GRAPH,"path":"cat.json"}));
    assert_eq!(out["nodes"], json!(2), "{out}");
    assert!(exchange.join("cat.json").is_file());
    mcp.call("vgraph_create", json!({"name":COPY}));
    let done = mcp.call("vgraph_import", json!({"graph":COPY,"path":"cat.json"}));
    assert!(done.is_object(), "{done}");
    // 교환 폴더 안의 절대 경로
    let abs = pstr(&exchange.join("abs.json"));
    mcp.call("vgraph_export", json!({"graph":COPY,"path":abs}));
    assert_eq!(
        std::fs::read_to_string(exchange.join("abs.json")).unwrap(),
        std::fs::read_to_string(exchange.join("cat.json")).unwrap(),
        "가져온 사본을 다시 내보내면 같은 문서"
    );
    // path 없는 내보내기·분할 내보내기는 그대로
    let doc = mcp.call("vgraph_export", json!({"graph":GRAPH}));
    assert!(doc["document"].is_object(), "{doc}");
    let part = mcp.call("vgraph_export", json!({"graph":GRAPH,"max_bytes":1024}));
    assert!(part["chunk"].is_string(), "{part}");
}

#[test]
fn custom_exchange_dir_option_moves_the_boundary() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    let custom = tmp.path().join("handoff");
    std::fs::create_dir_all(&root).unwrap();
    let server = Server::start(&root, &["--exchange-dir", &pstr(&custom)]);
    let mcp = Mcp::connect(server.addr);
    seed(&mcp);
    mcp.call("vgraph_export", json!({"graph":GRAPH,"path":"c.json"}));
    assert!(custom.join("c.json").is_file());
    assert!(!root.join("exchange").join("c.json").exists());
    mcp.denied(
        "vgraph_export",
        json!({"graph":GRAPH,"path":pstr(&root.join("escape.json"))}),
    );
}

#[test]
fn exchange_dir_that_contains_the_data_root_is_refused_at_startup() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    for dir in [pstr(&root), pstr(tmp.path())] {
        let out = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(&root)
            .args(["--listen", "127.0.0.1:0", "--exchange-dir", &dir])
            .stdin(Stdio::null())
            .output()
            .expect("실행");
        assert!(!out.status.success(), "시작되면 안 됨: {dir}");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("교환 폴더"), "사유가 없음: {text}");
    }
}
