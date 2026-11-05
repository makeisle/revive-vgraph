//! `vgraph-mcp serve --tokens` — 사용자별 접속 토큰. 실제 실행 파일을 임시 데이터 폴더·임시 포트로 띄우고
//! `ureq`로 부른다(모킹 없음). 시험이 띄운 서버는 `Server`가 drop될 때 반드시 끝낸다.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const START_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(50);
const SESSION_HEADER: &str = "mcp-session-id";
const USER_HEADER: &str = "X-Vgraph-User";
const WORKSPACE_HEADER: &str = "X-Vgraph-Workspace";
const GRAPH: &str = "acme/catalog";
const TOKENS_FILE: &str = "tokens.jsonl";

// ---------------------------------------------------------------- 프로세스

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
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", "127.0.0.1:0", "--user", "owner"])
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

    /// 토큰 파일을 켠 서버.
    fn with_tokens(root: &Path) -> Server {
        let file = root.join(TOKENS_FILE);
        Server::start(root, &["--tokens", path_str(&file)])
    }

    fn output(&self) -> String {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("UTF-8 경로")
}

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

/// 토큰을 발급하고 (id, 원문)을 돌려준다. 원문은 표준 출력 한 줄, id는 `token list`에서 찾는다.
fn add_token(root: &Path, user: &str) -> (String, String) {
    let out = run_cli(&["token", "add", user, "--root", path_str(root)]);
    assert!(out.status.success(), "token add 실패: {}", text(&out));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(
        !token.is_empty() && !token.contains(char::is_whitespace),
        "{token:?}"
    );
    let id = token_rows(root)
        .into_iter()
        .find(|r| r[1] == user && !r[3].contains("폐기"))
        .map(|r| r[0].clone())
        .expect("list에 새 토큰이 보여야 함");
    (id, token)
}

fn revoke(root: &Path, id: &str) -> Output {
    run_cli(&["token", "revoke", id, "--root", path_str(root)])
}

/// `token list` 줄들을 탭으로 나눈 것: id, 사용자, 생성 시각, 상태.
fn token_rows(root: &Path) -> Vec<Vec<String>> {
    let out = run_cli(&["token", "list", "--root", path_str(root)]);
    assert!(out.status.success(), "token list 실패: {}", text(&out));
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect()
}

// ---------------------------------------------------------------- HTTP 클라이언트

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("JSON 아님: {e}\n{} {:?}", self.status, self.body))
    }
}

fn send(url: &str, headers: &[(&str, &str)], body: &Value) -> Reply {
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let mut req = agent.request("POST", url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = req
        .set("content-type", "application/json")
        .send_string(&body.to_string());
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("전송 실패 {url}: {e}"),
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
        "clientInfo":{"name":"serve-tokens-test","version":"0"}}})
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

struct Mcp {
    url: String,
    session: String,
    headers: Vec<(String, String)>,
}

impl Mcp {
    fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Mcp {
        let r = send(&url_of(addr), headers, &initialize_body());
        assert_eq!(r.status, 200, "initialize: {}", r.body);
        let mcp = Mcp {
            url: url_of(addr),
            session: r.session.clone().expect("세션 ID"),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(mcp.post(&note).status, 202);
        mcp
    }

    /// 사용자 토큰으로 붙는다. `extra`는 토큰과 함께 보내는 다른 헤더.
    fn as_token(addr: SocketAddr, token: &str, extra: &[(&str, &str)]) -> Mcp {
        let auth = bearer(token);
        let mut h = vec![("Authorization", auth.as_str())];
        h.extend_from_slice(extra);
        Mcp::connect(addr, &h)
    }

    fn post(&self, body: &Value) -> Reply {
        let mut h: Vec<(&str, &str)> = vec![(SESSION_HEADER, self.session.as_str())];
        h.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        send(&self.url, &h, body)
    }

    fn tool(&self, name: &str, args: Value) -> (bool, Value) {
        let r = self.post(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":name,"arguments":args}}));
        assert_eq!(r.status, 200, "{name}: {}", r.body);
        let v = r.json();
        let txt = v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let is_err = v["result"]["isError"].as_bool().unwrap_or(false);
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

    fn edit_prop(&self, v: i64) -> (bool, Value) {
        self.tool(
            "vgraph_edit",
            json!({"graph":GRAPH,"ops":[{"op":"set_prop","node":"n0","key":"v","value":v}]}),
        )
    }
}

/// 토큰의 사용자 이름으로 그래프를 만들고 노드 n0을 커밋해 둔다.
fn seed_as(addr: SocketAddr, token: &str) {
    let m = Mcp::as_token(addr, token, &[(WORKSPACE_HEADER, "seed")]);
    m.call("vgraph_create", json!({"name":GRAPH}));
    m.call(
        "vgraph_edit",
        json!({"graph":GRAPH,"ops":[{"op":"add_node","labels":["Item"],"slug":"n0","props":{"v":0}}]}),
    );
    m.call("vgraph_commit", json!({"graph":GRAPH,"message":"seed"}));
}

/// `Authorization` 값을 그대로 실어 initialize를 보내고 상태 코드를 돌려준다.
fn status_with(addr: SocketAddr, auth: Option<&str>) -> u16 {
    let mut h: Vec<(&str, &str)> = Vec::new();
    if let Some(v) = auth {
        h.push(("Authorization", v));
    }
    send(&url_of(addr), &h, &initialize_body()).status
}

fn tokens_path(root: &Path) -> PathBuf {
    root.join(TOKENS_FILE)
}

// ---------------------------------------------------------------- 시험

#[test]
fn token_decides_user_and_user_header_is_ignored() {
    let dir = TempDir::new().unwrap();
    let (_, alice) = add_token(dir.path(), "alice");
    let (_, bob) = add_token(dir.path(), "bob");
    let srv = Server::with_tokens(dir.path());
    seed_as(srv.addr, &alice);

    // 토큰 A + 헤더 B → 여전히 A: 자기 그래프를 편집할 수 있다.
    let a = Mcp::as_token(srv.addr, &alice, &[(USER_HEADER, "bob")]);
    let list = a.call("vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["name"], json!(GRAPH), "{list}");
    let (is_err, body) = a.edit_prop(1);
    assert!(!is_err, "A의 편집이 막힘: {body}");

    // 토큰 B + 헤더 A → 여전히 B: A의 그래프가 안 보이고 편집도 거부된다.
    let b = Mcp::as_token(srv.addr, &bob, &[(USER_HEADER, "alice")]);
    assert_eq!(b.call("vgraph_list", json!({}))["graphs"], json!([]));
    let (is_err, body) = b.edit_prop(9);
    assert!(is_err, "B가 A로 행세해 편집함: {body}");

    // 서버 기본 사용자(--user)로도 행세할 수 없다.
    let c = Mcp::as_token(srv.addr, &bob, &[(USER_HEADER, "owner")]);
    assert_eq!(c.call("vgraph_list", json!({}))["graphs"], json!([]));
}

#[test]
fn missing_unknown_and_malformed_credentials_are_401() {
    let dir = TempDir::new().unwrap();
    let (_, alice) = add_token(dir.path(), "alice");
    let srv = Server::with_tokens(dir.path());

    assert_eq!(status_with(srv.addr, None), 401, "헤더 없음");
    assert_eq!(status_with(srv.addr, Some("Bearer nope")), 401, "없는 토큰");
    assert_eq!(status_with(srv.addr, Some("Bearer")), 401, "값 없음");
    assert_eq!(status_with(srv.addr, Some("Bearer ")), 401, "빈 값");
    assert_eq!(status_with(srv.addr, Some(&alice)), 401, "스킴 없음");
    assert_eq!(
        status_with(srv.addr, Some(&format!("Basic {alice}"))),
        401,
        "다른 스킴"
    );
    assert_eq!(
        status_with(srv.addr, Some(&format!("Bearer {alice}x"))),
        401,
        "한 글자 더 붙음"
    );
    let short = &alice[..alice.len() - 1];
    assert_eq!(
        status_with(srv.addr, Some(&format!("Bearer {short}"))),
        401,
        "한 글자 모자람"
    );
    assert_eq!(status_with(srv.addr, Some(&bearer(&alice))), 200);
}

#[test]
fn revoke_takes_effect_on_running_server_without_restart() {
    let dir = TempDir::new().unwrap();
    let (id, alice) = add_token(dir.path(), "alice");
    let (_, bob) = add_token(dir.path(), "bob");
    let srv = Server::with_tokens(dir.path());

    let a = Mcp::as_token(srv.addr, &alice, &[]);
    a.call("vgraph_list", json!({}));

    let out = revoke(dir.path(), &id);
    assert!(out.status.success(), "{}", text(&out));

    // 이미 열려 있던 세션도, 새 접속도 곧바로 401.
    let r = a.post(&json!({"jsonrpc":"2.0","id":9,"method":"tools/list"}));
    assert_eq!(r.status, 401, "폐기 뒤에도 기존 세션이 통과함: {}", r.body);
    assert_eq!(status_with(srv.addr, Some(&bearer(&alice))), 401);
    // 폐기하지 않은 다른 토큰은 영향이 없다.
    assert_eq!(status_with(srv.addr, Some(&bearer(&bob))), 200);

    // 실행 중에 새로 발급한 토큰도 재시작 없이 통한다.
    let (_, carol) = add_token(dir.path(), "carol");
    assert_eq!(status_with(srv.addr, Some(&bearer(&carol))), 200);
}

#[test]
fn token_secret_appears_nowhere_but_the_add_output() {
    let dir = TempDir::new().unwrap();
    let out = run_cli(&["token", "add", "alice", "--root", path_str(dir.path())]);
    assert!(out.status.success(), "{}", text(&out));
    let token = String::from_utf8(out.stdout.clone())
        .unwrap()
        .trim()
        .to_string();
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(&token),
        "원문이 표준 오류에도 나옴"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).matches(&token).count(),
        1,
        "원문은 한 번만 출력된다"
    );

    let srv = Server::with_tokens(dir.path());
    // 맞는 토큰·틀린 토큰으로 이것저것 시도해 로그·오류에 새는지 본다.
    let m = Mcp::as_token(srv.addr, &token, &[]);
    m.call("vgraph_list", json!({}));
    let wrong = format!("{token}0");
    let bad = send(
        &url_of(srv.addr),
        &[("Authorization", &bearer(&wrong))],
        &initialize_body(),
    );
    assert_eq!(bad.status, 401);
    assert!(
        !bad.body.contains(&token) && !bad.body.contains(&wrong),
        "{}",
        bad.body
    );

    let info = std::fs::read_to_string(dir.path().join("server.info")).unwrap();
    let file = std::fs::read_to_string(tokens_path(dir.path())).unwrap();
    let list = text(&run_cli(&["token", "list", "--root", path_str(dir.path())]));
    // 원문을 id로 잘못 넣은 폐기 시도의 오류 문구에도 원문이 되풀이되지 않아야 한다.
    let revoke_unknown = text(&revoke(dir.path(), &token));
    let server_output = srv.output();
    for (name, haystack) in [
        ("server.info", &info),
        ("tokens.jsonl", &file),
        ("token list", &list),
        ("token revoke 오류", &revoke_unknown),
        ("서버 출력", &server_output),
    ] {
        assert!(
            !haystack.contains(&token),
            "{name}에 토큰 원문이 있음:\n{haystack}"
        );
    }
    // 저장은 해시이고 줄마다 필드가 정해져 있다.
    let line: Value = serde_json::from_str(file.lines().next().unwrap()).unwrap();
    assert_eq!(line["user"], json!("alice"));
    assert!(line["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(line["created"].as_str().is_some());
    let hash = line["hash"].as_str().unwrap_or("");
    assert_eq!(hash.len(), 64, "{line}");
    assert!(!list.contains(hash), "list에 해시가 나옴");
}

#[test]
fn tokens_option_conflicts_with_single_token_options() {
    let dir = TempDir::new().unwrap();
    add_token(dir.path(), "alice");
    let file = tokens_path(dir.path());
    let secret = dir.path().join("one.txt");
    std::fs::write(&secret, "abc").unwrap();
    for extra in [
        vec!["--tokens", path_str(&file), "--token", "abc"],
        vec![
            "--tokens",
            path_str(&file),
            "--token-file",
            path_str(&secret),
        ],
    ] {
        let mut args = vec![
            "serve",
            "--root",
            path_str(dir.path()),
            "--listen",
            "127.0.0.1:0",
        ];
        args.extend(extra);
        let out = run_cli(&args);
        assert!(!out.status.success(), "함께 줬는데 떠 버림: {}", text(&out));
        assert!(text(&out).contains("cannot be used with"), "{}", text(&out));
    }
}

#[test]
fn tokens_file_must_exist_and_parse_at_startup() {
    let dir = TempDir::new().unwrap();
    let file = tokens_path(dir.path());
    let args = [
        "serve",
        "--root",
        path_str(dir.path()),
        "--listen",
        "127.0.0.1:0",
        "--tokens",
        path_str(&file),
    ];
    let out = run_cli(&args);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("토큰 파일"), "{}", text(&out));

    std::fs::write(&file, "이건 JSON이 아님\n").unwrap();
    let out = run_cli(&args);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("토큰 파일"), "{}", text(&out));
}

#[test]
fn uploads_are_isolated_per_token_user() {
    let dir = TempDir::new().unwrap();
    let (_, alice) = add_token(dir.path(), "alice");
    let (_, bob) = add_token(dir.path(), "bob");
    let srv = Server::with_tokens(dir.path());
    seed_as(srv.addr, &alice);

    let a = Mcp::as_token(srv.addr, &alice, &[(WORKSPACE_HEADER, "up")]);
    let begin = a.call("vgraph_import", json!({"graph":GRAPH,"upload":"begin"}));
    let up = begin["upload_id"].as_str().expect("upload_id").to_string();

    // 같은 작업 공간 이름·헤더로 A 행세를 해도 B는 A의 업로드를 이어 쓸 수 없다.
    let b = Mcp::as_token(
        srv.addr,
        &bob,
        &[(WORKSPACE_HEADER, "up"), (USER_HEADER, "alice")],
    );
    let (is_err, body) = b.tool(
        "vgraph_import",
        json!({"graph":GRAPH,"upload":"append","upload_id":up,"seq":0,"chunk":"{}"}),
    );
    assert!(is_err, "다른 사용자가 업로드를 이어 씀: {body}");
    // A는 이어 쓸 수 있다.
    let (is_err, body) = a.tool(
        "vgraph_import",
        json!({"graph":GRAPH,"upload":"append","upload_id":up,"seq":0,"chunk":"{"}),
    );
    assert!(!is_err, "{body}");
}

#[test]
fn without_tokens_option_user_header_is_still_honoured() {
    let dir = TempDir::new().unwrap();
    // 토큰 파일이 데이터 폴더에 있어도 --tokens 를 주지 않으면 인증이 켜지지 않는다.
    add_token(dir.path(), "alice");
    let srv = Server::start(dir.path(), &[]);
    assert_eq!(status_with(srv.addr, None), 200);

    let owner = Mcp::connect(srv.addr, &[]);
    owner.call("vgraph_create", json!({"name":GRAPH}));
    let stranger = Mcp::connect(srv.addr, &[(USER_HEADER, "stranger")]);
    assert_eq!(stranger.call("vgraph_list", json!({}))["graphs"], json!([]));
    let named = Mcp::connect(srv.addr, &[(USER_HEADER, "owner")]);
    assert_eq!(
        named.call("vgraph_list", json!({}))["graphs"][0]["name"],
        json!(GRAPH)
    );
}

#[test]
fn token_cli_list_revoke_and_input_errors() {
    let dir = TempDir::new().unwrap();
    let root = path_str(dir.path()).to_string();
    let (id, _) = add_token(dir.path(), "alice");

    let rows = token_rows(dir.path());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].len(), 4, "{rows:?}");
    assert_eq!(rows[0][0], id);
    assert_eq!(rows[0][1], "alice");
    assert!(
        rows[0][2].ends_with('Z') && rows[0][2].contains('T'),
        "{:?}",
        rows[0]
    );
    assert!(!rows[0][3].contains("폐기"), "{:?}", rows[0]);

    let out = revoke(dir.path(), &id);
    assert!(out.status.success(), "{}", text(&out));
    assert!(token_rows(dir.path())[0][3].contains("폐기"));

    // 같은 토큰을 또 폐기하거나, 없는 id를 폐기하면 오류.
    assert!(!revoke(dir.path(), &id).status.success());
    assert!(!revoke(dir.path(), "nope").status.success());

    // 잘못된 사용자 이름은 발급을 거부하고 파일에 흔적을 남기지 않는다.
    let before = std::fs::read_to_string(tokens_path(dir.path())).unwrap();
    for bad in ["", "  ", "a b\tc", "line\nbreak"] {
        let out = run_cli(&["token", "add", bad, "--root", &root]);
        assert!(!out.status.success(), "{bad:?}: {}", text(&out));
    }
    assert_eq!(
        std::fs::read_to_string(tokens_path(dir.path())).unwrap(),
        before
    );

    // 파일이 없으면 오류가 아니라 빈 목록.
    let empty = TempDir::new().unwrap();
    assert!(token_rows(empty.path()).is_empty());
}
