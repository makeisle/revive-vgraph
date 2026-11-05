//! `vgraph-mcp serve` 동시 편집 검증 — 실제 실행 파일 하나에 클라이언트 5개가 같은 그래프·같은 브랜치를
//! 동시에 편집·커밋해도 커밋된 편집이 하나도 유실되지 않고, 그동안 이 데이터 폴더를 쓰는 엔진 프로세스가
//! 하나뿐임을 확인한다. 모킹 없음(서버는 실제 프로세스, 클라이언트는 `ureq`).
//!
//! **기본 `cargo test`에서는 실행되지 않는다**(`#[ignore]`, 반복이 길다). 수동 실행:
//!
//! ```text
//! cargo test -p vgraph-mcp --test http_concurrency -- --ignored --nocapture
//! ```
//!
//! 반복 횟수는 환경 변수 `VGRAPH_CONC_RUNS`(기본 50)로 줄이거나 늘릴 수 있다.
//!
//! 설계
//! - 반복마다 **새 서버·새 임시 데이터 폴더·`127.0.0.1:0`**. 반복이 서로 상태를 물려받지 않아 독립 표본이
//!   되고(2026-10-03 수정 검증이 반복마다 새 폴더를 쓴 것과 같다), 기동·종료 경로도 매번 함께 시험된다.
//! - 클라이언트 5개 = 별개 MCP 세션 5개: 헤더 없는 자동 작업 공간 3개 + `X-Vgraph-Workspace` 지정 2개.
//!   모두 같은 사용자·같은 그래프·같은 브랜치(`main`)다.
//! - 각자 자기 노드 `w<k>`에 **라운드마다 다른 속성 키** `r<j>`를 쓰고 커밋한다(8라운드). 같은 키를 덮어쓰면
//!   앞 라운드가 유실돼도 마지막 값만 보여 감지 못 하기 때문이다. 라운드는 배리어로 동시 출발한다.
//! - 판정: 반복이 끝난 뒤 새 세션으로 모든 `w<k>.r<j>`가 기대 값으로 남아 있어야 한다(유실 0). 커밋 결과 중
//!   `conflicts`·오류·알 수 없는 결과는 따로 집계하고, 하나라도 있으면 실패다(각자 자기 노드만 만지므로 충돌이
//!   나면 안 된다).
//! - 엔진 프로세스 수: 운영체제의 프로세스 목록에서 **명령줄에 `vgraph-mcp`와 이 반복의 임시 폴더 경로가 함께
//!   있는 프로세스**를 센다. 다른 세션이 띄운 같은 이름의 프로세스는 폴더가 달라 섞이지 않는다. 판정은
//!   "그 집합이 시험 서버 PID 하나뿐"이다. 편집이 진행되는 동안 표본을 계속 뽑아(최소·최대 기록) 마지막에
//!   한 번 더 확인하고, `server.info`의 PID가 시험 서버와 같은지도 본다.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const BARRIER_TIMEOUT: Duration = Duration::from_secs(60);
const OWNER: &str = "owner";
const GRAPH: &str = "acme/catalog";
const SESSION_HEADER: &str = "mcp-session-id";
const WORKSPACE_HEADER: &str = "X-Vgraph-Workspace";
/// 헤더 없는(자동 작업 공간) 클라이언트 수.
const AUTO_CLIENTS: usize = 3;
/// `X-Vgraph-Workspace`로 이름을 지정하는 클라이언트의 작업 공간 이름.
const NAMED_WORKSPACES: [&str; 2] = ["conc-a", "conc-b"];
const CLIENTS: usize = AUTO_CLIENTS + NAMED_WORKSPACES.len();
const ROUNDS: usize = 8;
const DEFAULT_RUNS: usize = 50;

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
    fn start(root: &Path) -> Server {
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", "127.0.0.1:0", "--user", OWNER])
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
}

/// 시험이 띄운 서버는 어떤 경로로 끝나도(패닉 포함) 반드시 죽인다.
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- 프로세스 수 판정

/// 운영체제 프로세스 목록에서 명령줄에 `vgraph-mcp`와 `root` 경로가 함께 있는 프로세스의 PID.
/// 목록을 얻지 못하면 조용히 넘기지 않고 패닉한다(판정 불능을 "1개"로 오인하지 않도록).
fn engine_pids(root: &Path) -> Vec<u32> {
    let listing = if cfg!(windows) {
        let ps = "Get-CimInstance Win32_Process | ForEach-Object { $_.ProcessId.ToString() + ' ' + $_.CommandLine }";
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", ps])
            .stdin(Stdio::null())
            .output()
            .expect("powershell 실행")
    } else {
        Command::new("ps")
            .args(["-eo", "pid=,args="])
            .stdin(Stdio::null())
            .output()
            .expect("ps 실행")
    };
    assert!(
        listing.status.success(),
        "프로세스 목록을 얻지 못함: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let root = root.to_string_lossy().to_lowercase();
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let lower = line.to_lowercase();
            if !lower.contains("vgraph-mcp") || !lower.contains(&root) {
                return None;
            }
            line.split_whitespace().next()?.parse::<u32>().ok()
        })
        .collect()
}

// ---------------------------------------------------------------- HTTP 클라이언트

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
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

struct Mcp {
    url: String,
    session: String,
    headers: Vec<(String, String)>,
}

impl Mcp {
    fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Mcp {
        let url = format!("http://{addr}/mcp");
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"http-concurrency-test","version":"0"}}});
        let r = send("POST", &url, headers, Some(&init));
        assert_eq!(r.status, 200, "initialize: {}", r.body);
        let session = r
            .session
            .unwrap_or_else(|| panic!("세션 ID 없음: {}", r.body));
        let mcp = Mcp {
            url,
            session,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
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
            "DELETE",
            &self.url,
            &[(SESSION_HEADER, self.session.as_str())],
            None,
        );
        assert_eq!(r.status, 204, "DELETE: {}", r.body);
    }
}

fn seed(addr: SocketAddr) {
    let m = Mcp::connect(addr, &[(WORKSPACE_HEADER, "seed")]);
    m.call("vgraph_create", &json!({"name":GRAPH}));
    let ops: Vec<Value> = (0..CLIENTS)
        .map(|k| json!({"op":"add_node","labels":["Item"],"slug":format!("w{k}")}))
        .collect();
    m.call("vgraph_edit", &json!({"graph":GRAPH,"ops":ops}));
    m.call("vgraph_commit", &json!({"graph":GRAPH,"message":"seed"}));
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
    min_procs: usize,
    max_procs: usize,
    samples: usize,
    millis: u128,
}

fn client_headers(k: usize) -> Vec<(&'static str, &'static str)> {
    match k.checked_sub(AUTO_CLIENTS) {
        Some(i) => vec![(WORKSPACE_HEADER, NAMED_WORKSPACES[i])],
        None => Vec::new(),
    }
}

fn one_run() -> RunResult {
    let dir = TempDir::new().unwrap();
    let srv = Server::start(dir.path());
    let server_pid = srv.pid();
    seed(srv.addr);

    // 편집 시작 직전 표본(프로세스 목록 조회가 한 번에 ~1초라 편집 중 표본이 적을 수 있어 앞뒤로도 뽑는다).
    let before = engine_pids(dir.path());
    assert_eq!(before, vec![server_pid], "시작 전 확인: {before:?}");

    let started = Instant::now();
    let barrier = TimedBarrier::new(CLIENTS);
    let done = AtomicBool::new(false);
    let tally = Mutex::new(RunResult {
        min_procs: usize::MAX,
        ..RunResult::default()
    });

    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for k in 0..CLIENTS {
            let (addr, barrier, tally) = (srv.addr, &barrier, &tally);
            workers.push(scope.spawn(move || {
                let me = Mcp::connect(addr, &client_headers(k));
                let node = format!("w{k}");
                for j in 0..ROUNDS {
                    barrier.wait(j + 1);
                    me.call(
                        "vgraph_edit",
                        &json!({"graph":GRAPH,"ops":[{"op":"set_prop","node":node,
                            "key":format!("r{j}"),"value":j}]}),
                    );
                    let (is_err, out) = me.tool(
                        "vgraph_commit",
                        &json!({"graph":GRAPH,"message":format!("c{k} r{j}")}),
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
            }));
        }
        // 편집이 진행되는 동안 엔진 프로세스 수를 표본으로 뽑는다.
        let sampler = scope.spawn(|| {
            let mut counts = Vec::new();
            while !done.load(Ordering::SeqCst) {
                counts.push(engine_pids(dir.path()));
            }
            counts
        });
        // 클라이언트가 패닉해도 표본 스레드가 영원히 돌지 않도록, 먼저 모두 거둔 뒤 종료 표시를 한다.
        let joined: Vec<_> = workers.into_iter().map(|w| w.join()).collect();
        done.store(true, Ordering::SeqCst);
        assert!(joined.iter().all(Result::is_ok), "클라이언트 스레드 실패");
        let samples = sampler.join().expect("표본 스레드 실패");
        let mut t = tally.lock().unwrap();
        for pids in &samples {
            assert!(
                pids.iter().all(|p| *p == server_pid),
                "시험 서버 {server_pid} 외의 엔진 프로세스가 이 데이터 폴더를 씀: {pids:?}"
            );
            t.min_procs = t.min_procs.min(pids.len());
            t.max_procs = t.max_procs.max(pids.len());
        }
        t.samples = samples.len();
    });

    let mut res = tally.into_inner().unwrap();
    res.samples += 1; // 시작 전 표본
    res.min_procs = res.min_procs.min(before.len());
    res.max_procs = res.max_procs.max(before.len());
    res.millis = started.elapsed().as_millis();

    // 종료 직전 한 번 더: 서버만 있어야 하고, server.info의 PID가 시험 서버여야 한다.
    let last = engine_pids(dir.path());
    assert_eq!(last, vec![server_pid], "마지막 확인: {last:?}");
    res.samples += 1;
    res.min_procs = res.min_procs.min(last.len());
    res.max_procs = res.max_procs.max(last.len());
    let info: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("server.info")).unwrap())
            .unwrap();
    assert_eq!(info["pid"], json!(server_pid), "server.info: {info}");

    // 유실 판정 — 새 세션(자동 작업 공간)에서 커밋된 상태를 읽는다.
    let check = Mcp::connect(srv.addr, &[]);
    for k in 0..CLIENTS {
        let shown = check.call("vgraph_show", &json!({"addr":format!("{GRAPH}#w{k}")}));
        for j in 0..ROUNDS {
            if shown["props"][format!("r{j}")] != json!(j) {
                res.lost.push(format!("w{k}.r{j}"));
            }
        }
    }
    check.close();
    assert!(
        !srv.output().to_lowercase().contains("panic"),
        "서버 출력에 panic: {}",
        srv.output()
    );
    res
}

#[test]
#[ignore = "수동 실행: 50회 반복(수 분). 파일 머리 주석 참조"]
fn five_clients_on_one_server_same_branch_lose_no_commit() {
    let runs = std::env::var("VGRAPH_CONC_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_RUNS);
    let started = Instant::now();
    let mut total = RunResult {
        min_procs: usize::MAX,
        ..RunResult::default()
    };
    let mut failures = Vec::new();

    for i in 1..=runs {
        let r = one_run();
        println!(
            "run {i:>3}/{runs}: committed={} rebased={} conflicts={} other={} lost={} procs(min/max)={}/{} samples={} {}ms",
            r.committed,
            r.rebased,
            r.conflicts,
            r.other.len(),
            r.lost.len(),
            r.min_procs,
            r.max_procs,
            r.samples,
            r.millis
        );
        if !r.lost.is_empty() || r.conflicts > 0 || !r.other.is_empty() {
            failures.push(format!(
                "run {i}: lost={:?} conflicts={} other={:?}",
                r.lost, r.conflicts, r.other
            ));
        }
        total.committed += r.committed;
        total.rebased += r.rebased;
        total.conflicts += r.conflicts;
        total.lost.extend(r.lost);
        total.other.extend(r.other);
        total.min_procs = total.min_procs.min(r.min_procs);
        total.max_procs = total.max_procs.max(r.max_procs);
        total.samples += r.samples;
        total.millis += r.millis;
    }

    println!(
        "SUMMARY runs={runs} clients={CLIENTS}({AUTO_CLIENTS} auto + {} named) rounds={ROUNDS} \
         edits+commits={} committed={} rebased={} conflicts={} other={} lost={} \
         engine_procs(min/max over {} samples)={}/{} edit-phase={:.1}s wall={:.1}s",
        NAMED_WORKSPACES.len(),
        runs * CLIENTS * ROUNDS,
        total.committed,
        total.rebased,
        total.conflicts,
        total.other.len(),
        total.lost.len(),
        total.samples,
        total.min_procs,
        total.max_procs,
        total.millis as f64 / 1000.0,
        started.elapsed().as_secs_f64()
    );
    assert!(failures.is_empty(), "동시성 실패:\n{}", failures.join("\n"));
    assert_eq!(total.max_procs, 1, "엔진 프로세스가 둘 이상 관측됨");
    assert_eq!(
        total.min_procs, 1,
        "엔진 프로세스가 관측되지 않은 표본이 있음"
    );
}
