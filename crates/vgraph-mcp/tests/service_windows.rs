//! Windows 실기 시험 — `vgraph-mcp service install/status/uninstall` 이 현재 사용자 시작 항목
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`)에 실제로 등록·해제되고, 등록된 명령을 그대로 실행하면
//! 서버가 창 없이 떠서 HTTP에 응답하는지 확인한다.
//!
//! **기본 `cargo test`에서는 실행되지 않는다**(`#[ignore]`) — 사용자 PC의 레지스트리를 쓰기 때문이다.
//! 수동 실행:
//!
//! ```text
//! cargo test -p vgraph-mcp --test service_windows -- --ignored --nocapture
//! ```
//!
//! 안전 장치:
//! - 제품 기본 이름(`vgraph-mcp`)이 아닌 **시험 전용 등록 이름**(`vgraph-mcp-test-<PID>-<시각>`)만 쓴다.
//! - 데이터 폴더는 임시 폴더, 포트는 `127.0.0.1:0`(운영체제가 고름)이다.
//! - 시험이 실패하거나 패닉해도 `Cleanup`이 drop에서 등록 값 삭제·서버 종료를 반드시 시도한다.
//! - 끝나면 Run 키의 다른 값이 시험 전과 같은지도 확인한다.

#![cfg(windows)]

use serde_json::{json, Value};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const TOKEN: &str = "windows-live-test-token-51c2";
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- 도우미

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{program} 실행 실패: {e}"))
}

fn cli(args: &[&str]) -> Output {
    run(BIN, args)
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// `reg query <Run 키> /v <이름>` 으로 값의 데이터를 읽는다. 없으면 `None`. (제품 코드와 독립된 확인 수단)
fn reg_value(name: &str) -> Option<String> {
    let o = run("reg", &["query", RUN_KEY, "/v", name]);
    if !o.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    text.lines().find_map(|l| {
        let (_, rest) = l.split_once("REG_SZ")?;
        Some(rest.trim().to_string())
    })
}

/// Run 키 전체 목록(값 이름과 데이터). 시험 전후 비교용.
fn reg_snapshot() -> Vec<String> {
    let o = run("reg", &["query", RUN_KEY]);
    let mut lines: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines
}

fn pid_alive(pid: u32) -> bool {
    let o = run(
        "tasklist",
        &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"],
    );
    String::from_utf8_lossy(&o.stdout).contains("vgraph-mcp")
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "시간 초과: {what}");
        std::thread::sleep(POLL);
    }
}

fn server_info(root: &Path) -> Option<(u32, SocketAddr)> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("server.info")).ok()?).ok()?;
    Some((
        u32::try_from(v["pid"].as_u64()?).ok()?,
        v["addr"].as_str()?.parse().ok()?,
    ))
}

fn post(addr: SocketAddr, bearer: Option<&str>) -> u16 {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let mut req = agent
        .post(&format!("http://{addr}/mcp"))
        .set("content-type", "application/json");
    if let Some(t) = bearer {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"service-windows-test","version":"0"}}});
    match req.send_string(&body.to_string()) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("HTTP 전송 실패: {e}"),
    }
}

/// 시험이 만든 것을 어떤 경우에도 치운다: 등록 값 삭제 + 서버 종료.
struct Cleanup {
    entry: String,
    root: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let pid = server_info(&self.root).map(|(p, _)| p);
        let _ = cli(&["service", "uninstall", "--entry-name", &self.entry]);
        let _ = cli(&["stop", "--root", self.root.to_str().unwrap_or_default()]);
        let _ = run("reg", &["delete", RUN_KEY, "/v", &self.entry, "/f"]);
        if let Some(pid) = pid {
            if pid_alive(pid) {
                let _ = run("taskkill", &["/PID", &pid.to_string(), "/F"]);
            }
        }
    }
}

/// 등록 값(`"exe" 인자…`)을 시작 항목 실행기가 하듯 문자열 그대로 프로세스로 띄운다.
fn launch_like_run_key(value: &str) -> Output {
    let rest = value
        .strip_prefix('"')
        .expect("등록 값은 인용된 실행 파일 경로로 시작해야 한다");
    let (exe, args) = rest.split_once('"').expect("실행 파일 경로의 닫는 따옴표");
    Command::new(exe)
        .raw_arg(args.trim_start())
        .stdin(Stdio::null())
        .output()
        .expect("등록된 명령 실행")
}

// ---------------------------------------------------------------- 시험

/// 임시 데이터 폴더의 서버를 어떤 경우에도 끈다 (레지스트리는 쓰지 않는 시험용).
struct StopServer(PathBuf);

impl Drop for StopServer {
    fn drop(&mut self) {
        let _ = cli(&["stop", "--root", self.0.to_str().unwrap_or_default()]);
    }
}

/// 레지스트리를 쓰지 않는 시험이라 기본 `cargo test`에서도 돈다: `serve --detach` 가 출력을 캡처하는 쪽
/// (파이프)에서 EOF 를 막지 않는지 본다. 자식이 파이프의 쓰기 끝을 물려받으면 읽는 쪽이 서버가 죽을 때까지 멈춘다.
#[test]
fn detach_does_not_hold_the_callers_output_pipe() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("detach data");
    let _stop = StopServer(root.clone());
    let mut child = Command::new(BIN)
        .args(["serve", "--detach", "--listen", "127.0.0.1:0", "--root"])
        .arg(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        let _ = tx.send(text);
    });
    let text = rx.recv_timeout(WAIT).unwrap_or_else(|_| {
        panic!("serve --detach 의 출력 파이프가 닫히지 않는다 (서버가 파이프를 쥐고 있음)")
    });
    assert!(child.wait().unwrap().success());
    assert!(text.contains("창 없이 시작했습니다"), "{text}");
    let (pid, addr) = server_info(&root).expect("server.info");
    assert!(pid_alive(pid), "서버가 시작기와 함께 죽었다");
    assert_eq!(post(addr, None), 200);
    assert!(root.join("server.log").is_file(), "로그 파일이 없다");
}

#[test]
fn detach_second_start_is_refused_with_the_running_address() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("twice");
    let _stop = StopServer(root.clone());
    let args = ["serve", "--detach", "--listen", "127.0.0.1:0", "--root"];
    let first = run_with_root(&args, &root);
    assert!(first.status.success(), "{}", out(&first));
    let second = run_with_root(&args, &root);
    assert!(!second.status.success());
    assert!(
        out(&second).contains("이미 서버가 떠 있습니다"),
        "{}",
        out(&second)
    );
}

fn run_with_root(args: &[&str], root: &Path) -> Output {
    Command::new(BIN)
        .args(args)
        .arg(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
#[ignore = "사용자 PC 레지스트리(HKCU Run)를 쓴다 — 수동 실행 전용: cargo test -p vgraph-mcp --test service_windows -- --ignored"]
fn install_run_status_uninstall_roundtrip() {
    let tmp = TempDir::new().unwrap();
    // 공백이 든 경로: 레지스트리 값의 인용이 실제로 통하는지 본다.
    let base = tmp.path().join("live data");
    let root = base.join("root");
    std::fs::create_dir_all(&base).unwrap();
    let token_file = base.join("token.txt");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();

    let entry = format!(
        "vgraph-mcp-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let before = reg_snapshot();
    assert!(
        !before.iter().any(|l| l.contains(&entry)),
        "시험 전용 이름이 이미 있다"
    );
    let _cleanup = Cleanup {
        entry: entry.clone(),
        root: root.clone(),
    };

    // 1) install (--no-start: 값만 만든다. 실행은 아래에서 등록된 명령을 그대로 돌려 확인한다)
    let install_args = [
        "service",
        "install",
        "--entry-name",
        &entry,
        "--root",
        root.to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
        "--user",
        "live",
        "--token-file",
        token_file.to_str().unwrap(),
        "--no-start",
    ];
    let o = cli(&install_args);
    assert!(o.status.success(), "install 실패: {}", out(&o));

    // 2) 레지스트리 값 생성 확인
    let value = reg_value(&entry).unwrap_or_else(|| panic!("Run 키에 {entry} 값이 없다"));
    println!("등록된 값: {value}");
    assert!(value.contains("serve --detach"), "{value}");
    assert!(value.contains(root.to_str().unwrap()), "{value}");
    assert!(
        value.contains("live data"),
        "공백 경로가 값에 있어야 함: {value}"
    );
    assert!(value.contains("--token-file"), "{value}");
    assert!(
        !value.contains(TOKEN),
        "토큰 값이 레지스트리에 평문으로 있다"
    );
    assert!(value.contains("--log-file"), "{value}");

    // 3) 덮어쓰기 알림
    let o = cli(&install_args);
    assert!(o.status.success(), "재 install 실패: {}", out(&o));
    assert!(
        out(&o).contains("덮어"),
        "덮어쓴다는 알림이 없다: {}",
        out(&o)
    );
    assert_eq!(reg_value(&entry).as_deref(), Some(value.as_str()));

    // 4) 등록된 명령을 그대로 실행 → 시작기는 끝나고 서버는 남는다
    let launcher = launch_like_run_key(&value);
    assert!(
        launcher.status.success(),
        "등록된 명령 실행 실패: {}",
        out(&launcher)
    );
    println!("시작기 출력: {}", out(&launcher));
    wait_until("server.info 생성", || server_info(&root).is_some());
    let (pid, addr) = server_info(&root).unwrap();
    assert!(
        pid_alive(pid),
        "시작기가 끝난 뒤 서버(PID {pid})가 살아 있어야 한다"
    );

    // 5) HTTP 응답 (토큰 파일이 실제로 적용됐는지도 본다)
    assert_eq!(post(addr, None), 401, "토큰 없이 접속이 허용됨");
    assert_eq!(post(addr, Some(TOKEN)), 200, "토큰으로 접속 실패");

    // 6) 창 없음: 콘솔이 없어 conhost 자식이 없고, 보이는 창도 없다
    let ps = run(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-Process -Id {pid}).MainWindowHandle.ToInt64(); \
                 @(Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | \
                 Where-Object {{ $_.Name -eq 'conhost.exe' }}).Count"
            ),
        ],
    );
    let lines: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines,
        ["0", "0"],
        "창 핸들 0, conhost 자식 0 이어야 한다: {}",
        out(&ps)
    );

    // 7) status
    let st = cli(&["service", "status", "--entry-name", &entry]);
    assert!(st.status.success(), "status 비정상: {}", out(&st));
    assert!(out(&st).contains("실행 중"), "{}", out(&st));
    assert!(out(&st).contains(&addr.to_string()), "{}", out(&st));

    // 8) uninstall → 서버 꺼짐 · 값 삭제
    let un = cli(&["service", "uninstall", "--entry-name", &entry]);
    assert!(un.status.success(), "uninstall 실패: {}", out(&un));
    assert!(reg_value(&entry).is_none(), "uninstall 뒤에도 값이 남았다");
    wait_until("서버 프로세스 종료", || !pid_alive(pid));
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
        "uninstall 뒤에도 포트가 열려 있다"
    );
    let st = cli(&["service", "status", "--entry-name", &entry]);
    assert!(!st.status.success(), "등록 해제 뒤 status 가 정상이다");
    assert!(out(&st).contains("등록되어 있지 않"), "{}", out(&st));

    // 9) 다른 시작 항목은 그대로
    assert_eq!(reg_snapshot(), before, "Run 키의 다른 값이 바뀌었다");
}

#[test]
#[ignore = "사용자 PC 레지스트리(HKCU Run)를 쓴다 — 수동 실행 전용: cargo test -p vgraph-mcp --test service_windows -- --ignored"]
fn uninstall_when_not_registered_changes_nothing() {
    let entry = format!("vgraph-mcp-test-none-{}", std::process::id());
    let before = reg_snapshot();
    let o = cli(&["service", "uninstall", "--entry-name", &entry]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("등록되어 있지 않"), "{}", out(&o));
    assert_eq!(reg_snapshot(), before);
}
