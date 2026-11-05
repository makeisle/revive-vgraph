//! 원격 모드 동시 편집 검증 — 실제 실행 파일을 **TLS(자체 서명) + 사용자별 토큰(`--tokens`)** 으로, 이 PC 밖으로
//! 열리는 주소에 띄우고, 서로 다른 토큰의 클라이언트 5개가 동시에 편집·커밋한다. 모킹 없음(서버는 실제 프로세스,
//! 클라이언트는 `ureq` + rustls).
//!
//! 판정
//! - **유실 0:** 반복이 끝난 뒤 새 세션으로 각 사용자의 그래프를 읽어, 보낸 편집이 하나도 빠지지 않고 기대 값으로
//!   남아 있어야 한다. 라운드마다 다른 속성 키를 쓰므로 앞 라운드가 사라지면 드러난다.
//! - **사용자 혼선 0:** 각 그래프의 `vgraph_log`에 있는 모든 커밋의 작성자가 그 토큰의 사용자여야 한다.
//!   또 사용자 목록에는 자기 그래프만 보여야 하고, 다른 사용자의 그래프를 편집하려는 시도는 거부돼야 한다.
//!   (`X-Vgraph-User` 헤더를 일부러 남의 이름으로 실어도 토큰이 이긴다.)
//! - 커밋 결과 중 `conflicts`·오류·알 수 없는 결과가 하나라도 있으면 실패다(각자 자기 그래프만 만진다).
//!
//! **기본 `cargo test`에서는 실행되지 않는다**(`#[ignore]`). 이 PC 밖으로 열리는 주소에 실제로 바인드하므로 Windows
//! 방화벽 허용 창이 뜨거나 네트워크에 포트가 잠깐 열릴 수 있어서 사람이 직접 돌린다:
//!
//! ```text
//! cargo test -p vgraph-mcp --test remote_concurrency -- --ignored --nocapture
//! ```
//!
//! 환경 변수(모두 선택)
//! - `VGRAPH_REMOTE_LISTEN` — 서버가 열 주소(기본 `0.0.0.0:0`, 모든 어댑터). 이 PC의 사설 IP로 확인하려면
//!   `192.168.x.x:0` 처럼 준다. 그 주소는 시험용 인증서에도 들어가고 클라이언트도 그 주소로 접속한다.
//!   `0.0.0.0`·`[::]` 이면 클라이언트는 루프백(`127.0.0.1`)으로 접속한다.
//! - `VGRAPH_REMOTE_RUNS` — 서버를 새로 띄워 반복할 횟수(기본 3).
//! - `VGRAPH_REMOTE_ROUNDS` — 반복마다 클라이언트 하나가 편집·커밋할 횟수(기본 10).
//!
//! 끝에 `SUMMARY` 한 줄로 수치를 출력한다.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const START_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const BARRIER_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);
const SESSION_HEADER: &str = "mcp-session-id";
const USER_HEADER: &str = "X-Vgraph-User";
const DEFAULT_LISTEN: &str = "0.0.0.0:0";
const CLIENTS: usize = 5;
const DEFAULT_RUNS: usize = 3;
const DEFAULT_ROUNDS: usize = 10;
const LISTEN_ENV: &str = "VGRAPH_REMOTE_LISTEN";
const RUNS_ENV: &str = "VGRAPH_REMOTE_RUNS";
const ROUNDS_ENV: &str = "VGRAPH_REMOTE_ROUNDS";
const NODE: &str = "w";

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn user_of(k: usize) -> String {
    format!("user{k}")
}

fn graph_of(k: usize) -> String {
    format!("team/g{k}")
}

// ---------------------------------------------------------------- 인증서·TLS 클라이언트

struct Certs {
    _dir: TempDir,
    cert: PathBuf,
    key: PathBuf,
    cert_pem: String,
}

fn make_certs(extra_host: Option<&str>) -> Certs {
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    names.extend(extra_host.map(String::from));
    let made = rcgen::generate_simple_self_signed(names).expect("자체 서명");
    let dir = TempDir::new().unwrap();
    let cert = dir.path().join("server-cert.pem");
    let key = dir.path().join("server-key.pem");
    let cert_pem = made.cert.pem();
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, made.signing_key.serialize_pem()).unwrap();
    Certs {
        _dir: dir,
        cert,
        key,
        cert_pem,
    }
}

fn client_agent(cert_pem: &str) -> ureq::Agent {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(cert_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    ureq::AgentBuilder::new()
        .timeout(CLIENT_TIMEOUT)
        .tls_config(Arc::new(config))
        .build()
}

// ---------------------------------------------------------------- 서버 프로세스

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
                if let Some(idx) = line.find("https://") {
                    let host = line[idx + "https://".len()..]
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
    fn start(root: &Path, listen: &str, certs: &Certs, tokens: &Path) -> Server {
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", listen, "--user", "owner"])
            .arg("--tls-cert")
            .arg(&certs.cert)
            .arg("--tls-key")
            .arg(&certs.key)
            .arg("--tokens")
            .arg(tokens)
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

    /// 서버가 알린 주소로 클라이언트가 접속할 URL. 와일드카드 주소는 루프백으로 바꾼다.
    fn url(&self) -> String {
        let host = if self.addr.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            match self.addr.ip() {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            }
        };
        format!("https://{host}:{}/mcp", self.addr.port())
    }

    fn output(&self) -> String {
        self.log.lock().unwrap().clone()
    }
}

/// 시험이 띄운 서버는 어떤 경로로 끝나도(패닉 포함) 반드시 죽인다.
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("UTF-8 경로")
}

/// 사용자 토큰을 발급해 원문을 돌려준다(`<root>/tokens.jsonl`에 기록된다).
fn add_token(root: &Path, user: &str) -> String {
    let mut child = Command::new(BIN)
        .args(["token", "add", user, "--root", path_str(root)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("vgraph-mcp token add 실행");
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "token add가 끝나지 않음");
        std::thread::sleep(POLL);
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "token add 실패: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

// ---------------------------------------------------------------- HTTP 클라이언트

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

fn send(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
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

struct Mcp {
    agent: ureq::Agent,
    url: String,
    session: String,
    auth: String,
    headers: Vec<(String, String)>,
}

impl Mcp {
    /// 토큰으로 인증된 MCP 세션을 연다. `extra`는 토큰 외에 실을 헤더다.
    fn connect(agent: &ureq::Agent, url: &str, token: &str, extra: &[(&str, &str)]) -> Mcp {
        let auth = format!("Bearer {token}");
        let mut headers = vec![("Authorization".to_string(), auth.clone())];
        headers.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"remote-concurrency-test","version":"0"}}});
        let hs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let r = send(agent, "POST", url, &hs, Some(&init));
        assert_eq!(r.status, 200, "initialize: {}", r.body);
        let session = r
            .session
            .unwrap_or_else(|| panic!("세션 ID 없음: {}", r.body));
        let mcp = Mcp {
            agent: agent.clone(),
            url: url.to_string(),
            session,
            auth,
            headers,
        };
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(mcp.post(&note).status, 202);
        mcp
    }

    fn post(&self, body: &Value) -> Reply {
        let mut h: Vec<(&str, &str)> = vec![(SESSION_HEADER, self.session.as_str())];
        h.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        send(&self.agent, "POST", &self.url, &h, Some(body))
    }

    /// 도구를 부른다. 반환: (도구 오류 여부, 본문 JSON — 아니면 문자열).
    fn tool(&self, name: &str, args: &Value) -> (bool, Value) {
        let r = self.post(&json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":name,"arguments":args}}));
        assert_eq!(r.status, 200, "{name}: {}", r.body);
        let resp: Value =
            serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("JSON 아님({e}): {}", r.body));
        let res = &resp["result"];
        let txt = res["content"][0]["text"].as_str().unwrap_or("").to_string();
        let is_err = res["isError"].as_bool().unwrap_or(false);
        (
            is_err,
            serde_json::from_str(&txt).unwrap_or(Value::String(txt)),
        )
    }

    fn call(&self, name: &str, args: &Value) -> Value {
        let (is_err, body) = self.tool(name, args);
        assert!(!is_err, "도구 오류({name}): {body}");
        body
    }

    fn close(self) {
        let r = send(
            &self.agent,
            "DELETE",
            &self.url,
            &[
                (SESSION_HEADER, self.session.as_str()),
                ("Authorization", self.auth.as_str()),
            ],
            None,
        );
        assert_eq!(r.status, 204, "DELETE: {}", r.body);
    }
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
        let deadline = Instant::now() + BARRIER_TIMEOUT;
        while self.arrived.load(Ordering::SeqCst) < self.parties * round {
            assert!(
                Instant::now() < deadline,
                "다른 스레드가 집결점에 오지 않음(앞선 실패)"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

// ---------------------------------------------------------------- 한 반복

#[derive(Default, Debug)]
struct RunResult {
    committed: usize,
    rebased: usize,
    conflicts: usize,
    other: Vec<String>,
    lost: Vec<String>,
    wrong_author: Vec<String>,
    leaks: Vec<String>,
    commits_seen: usize,
    millis: u128,
}

fn create_graph(me: &Mcp, graph: &str) {
    me.call("vgraph_create", &json!({"name":graph}));
    me.call(
        "vgraph_edit",
        &json!({"graph":graph,"ops":[{"op":"add_node","labels":["Item"],"slug":NODE}]}),
    );
    me.call("vgraph_commit", &json!({"graph":graph,"message":"seed"}));
}

/// 한 클라이언트(= 한 사용자)의 편집 루프. 라운드마다 집결점에서 동시에 출발한다.
fn client_loop(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    k: usize,
    rounds: usize,
    barrier: &TimedBarrier,
    tally: &Mutex<RunResult>,
) {
    // 헤더로 남의 이름을 대도 토큰이 이긴다는 것까지 함께 시험한다.
    let other = user_of((k + 1) % CLIENTS);
    let me = Mcp::connect(agent, url, token, &[(USER_HEADER, other.as_str())]);
    let graph = graph_of(k);
    create_graph(&me, &graph);
    for j in 0..rounds {
        barrier.wait(j + 1);
        me.call(
            "vgraph_edit",
            &json!({"graph":graph,"ops":[{"op":"set_prop","node":NODE,
                "key":format!("r{j}"),"value":j}]}),
        );
        let (is_err, out) = me.tool(
            "vgraph_commit",
            &json!({"graph":graph,"message":format!("c{k} r{j}")}),
        );
        let mut t = tally.lock().unwrap();
        match (is_err, out["outcome"].as_str()) {
            (false, Some("committed")) => t.committed += 1,
            (false, Some("rebased")) => t.rebased += 1,
            (false, Some("conflicts")) => t.conflicts += 1,
            _ => t.other.push(format!("c{k} r{j}: {out}")),
        }
    }
    me.close();
}

/// 편집이 끝난 뒤 새 세션으로 한 사용자의 결과를 판정해 `res`에 쌓는다.
fn judge_user(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    k: usize,
    rounds: usize,
    res: &mut RunResult,
) {
    let user = user_of(k);
    let graph = graph_of(k);
    let me = Mcp::connect(agent, url, token, &[]);
    let shown = me.call("vgraph_show", &json!({"addr":format!("{graph}#{NODE}")}));
    for j in 0..rounds {
        if shown["props"][format!("r{j}")] != json!(j) {
            res.lost.push(format!("{user}.r{j}"));
        }
    }
    let log = me.call("vgraph_log", &json!({"graph":graph}));
    let commits = log["commits"].as_array().cloned().unwrap_or_default();
    res.commits_seen += commits.len();
    for c in &commits {
        if c["author"] != json!(user) {
            res.wrong_author
                .push(format!("{graph}: {} != {user}", c["author"]));
        }
    }
    if commits.len() < rounds {
        res.lost
            .push(format!("{user}: 커밋 {}개 < {rounds}", commits.len()));
    }
    // 사용자 목록에는 자기 그래프만, 남의 그래프 편집은 거부.
    let list = me.call("vgraph_list", &json!({}));
    let names: Vec<String> = list["graphs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|g| g["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if names != vec![graph.clone()] {
        res.leaks.push(format!("{user}의 목록: {names:?}"));
    }
    let foreign = graph_of((k + 1) % CLIENTS);
    let (is_err, body) = me.tool(
        "vgraph_edit",
        &json!({"graph":foreign,"ops":[{"op":"set_prop","node":NODE,"key":"intrude","value":1}]}),
    );
    if !is_err {
        res.leaks
            .push(format!("{user}가 {foreign}를 편집함: {body}"));
    }
    me.close();
}

fn one_run(listen: &str, rounds: usize) -> RunResult {
    let listen_ip = listen.parse::<SocketAddr>().ok().map(|a| a.ip());
    let extra_host = listen_ip
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| ip.to_string());
    let certs = make_certs(extra_host.as_deref());
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    let tokens: Vec<String> = (0..CLIENTS)
        .map(|k| add_token(&root, &user_of(k)))
        .collect();
    let tokens_file = root.join("tokens.jsonl");
    let srv = Server::start(&root, listen, &certs, &tokens_file);
    let url = srv.url();
    let agent = client_agent(&certs.cert_pem);

    let started = Instant::now();
    let barrier = TimedBarrier::new(CLIENTS);
    let tally = Mutex::new(RunResult::default());

    std::thread::scope(|scope| {
        let workers: Vec<_> = tokens
            .iter()
            .enumerate()
            .map(|(k, token)| {
                let (agent, url, barrier, tally) = (&agent, &url, &barrier, &tally);
                scope.spawn(move || client_loop(agent, url, token, k, rounds, barrier, tally))
            })
            .collect();
        let joined: Vec<_> = workers.into_iter().map(|w| w.join()).collect();
        assert!(joined.iter().all(Result::is_ok), "클라이언트 스레드 실패");
    });

    let mut res = tally.into_inner().unwrap();
    res.millis = started.elapsed().as_millis();
    for (k, token) in tokens.iter().enumerate() {
        judge_user(&agent, &url, token, k, rounds, &mut res);
    }
    assert!(
        !srv.output().to_lowercase().contains("panic"),
        "서버 출력에 panic: {}",
        srv.output()
    );
    res
}

impl RunResult {
    fn failed(&self) -> bool {
        !self.lost.is_empty()
            || self.conflicts > 0
            || !self.other.is_empty()
            || !self.wrong_author.is_empty()
            || !self.leaks.is_empty()
    }
}

#[test]
#[ignore = "이 PC 밖으로 열리는 주소에 바인드한다(방화벽 허용 창이 뜰 수 있음) — 파일 머리 주석 참조"]
fn five_token_users_over_tls_lose_no_edit_and_never_mix_users() {
    let listen = std::env::var(LISTEN_ENV).unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let runs = env_usize(RUNS_ENV, DEFAULT_RUNS);
    let rounds = env_usize(ROUNDS_ENV, DEFAULT_ROUNDS);
    let started = Instant::now();
    let mut total = RunResult::default();
    let mut failures = Vec::new();

    for i in 1..=runs {
        let r = one_run(&listen, rounds);
        println!(
            "run {i:>3}/{runs}: committed={} rebased={} conflicts={} other={} lost={} wrong_author={} leaks={} commits_in_logs={} {}ms",
            r.committed,
            r.rebased,
            r.conflicts,
            r.other.len(),
            r.lost.len(),
            r.wrong_author.len(),
            r.leaks.len(),
            r.commits_seen,
            r.millis
        );
        if r.failed() {
            failures.push(format!(
                "run {i}: lost={:?} conflicts={} other={:?} wrong_author={:?} leaks={:?}",
                r.lost, r.conflicts, r.other, r.wrong_author, r.leaks
            ));
        }
        total.committed += r.committed;
        total.rebased += r.rebased;
        total.conflicts += r.conflicts;
        total.lost.extend(r.lost);
        total.other.extend(r.other);
        total.wrong_author.extend(r.wrong_author);
        total.leaks.extend(r.leaks);
        total.commits_seen += r.commits_seen;
        total.millis += r.millis;
    }

    println!(
        "SUMMARY listen={listen} runs={runs} clients={CLIENTS} rounds={rounds} \
         edits+commits={} committed={} rebased={} conflicts={} other={} lost={} \
         wrong_author={} leaks={} commits_in_logs={} edit-phase={:.1}s wall={:.1}s",
        runs * CLIENTS * rounds,
        total.committed,
        total.rebased,
        total.conflicts,
        total.other.len(),
        total.lost.len(),
        total.wrong_author.len(),
        total.leaks.len(),
        total.commits_seen,
        total.millis as f64 / 1000.0,
        started.elapsed().as_secs_f64()
    );
    assert!(
        failures.is_empty(),
        "원격 동시성 실패:\n{}",
        failures.join("\n")
    );
    assert_eq!(
        total.committed + total.rebased,
        runs * CLIENTS * rounds,
        "커밋 결과 합이 편집 수와 다름"
    );
}
