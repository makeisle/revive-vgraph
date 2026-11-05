//! 원격 접속 안전 규칙 — `serve`의 `--tls-cert`·`--tls-key`·`--behind-proxy`·`--allow-origin`과
//! `service install`의 같은 옵션. 실제 실행 파일을 임시 데이터 폴더에서 돌리고 `ureq`로 부른다(모킹 없음).
//! 인증서는 `rcgen`으로 만든 시험용 자체 서명이다. 시험이 띄운 서버는 `Server`가 drop될 때 반드시 끝낸다.
//!
//! 기본 시험은 같은 PC 밖 주소를 실제로 열지 않는다 — 기동을 **바인드 전에** 거부하는 경로와 루프백 TLS만 쓴다.
//! 이 PC의 밖으로 열리는 주소(`0.0.0.0`)에 실제로 바인드하는 시험은 `#[ignore]`다. Windows 방화벽 허용 창이 뜨거나
//! 네트워크에 포트가 잠깐 열릴 수 있어서 사람이 직접 돌린다:
//!
//! ```text
//! cargo test -p vgraph-mcp --test serve_remote -- --ignored
//! ```
//!
//! (이 PC의 사설 IP로 확인하려면 시험 안의 `REMOTE_LISTEN`을 `192.168.x.x:0` 같은 값으로 바꿔 돌린다.)

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
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
/// 수동 시험이 여는 주소. 모든 어댑터에 열리므로 방화벽 창이 뜰 수 있다.
const REMOTE_LISTEN: &str = "0.0.0.0:0";
/// 키 내용이 새는지 보려고 쓰는 표지(오류 시험용 깨진 키 파일에 심는다).
const KEY_LEAK_MARKER: &str = "TOPSECRETKEYLINEDONOTLEAK";

// ---------------------------------------------------------------- 재료

struct Certs {
    _dir: TempDir,
    cert: PathBuf,
    key: PathBuf,
    cert_pem: String,
    key_pem: String,
}

fn make_certs() -> Certs {
    let made =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("자체 서명");
    let dir = TempDir::new().unwrap();
    let cert = dir.path().join("server cert.pem"); // 공백이 든 경로도 같이 시험한다
    let key = dir.path().join("server key.pem");
    let (cert_pem, key_pem) = (made.cert.pem(), made.signing_key.serialize_pem());
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, &key_pem).unwrap();
    Certs {
        _dir: dir,
        cert,
        key,
        cert_pem,
        key_pem,
    }
}

/// 키 PEM의 본문 첫 줄(표지 줄 다음) — 이 줄이 어디에도 나오면 키가 샌 것이다.
fn key_body_line(key_pem: &str) -> String {
    key_pem.lines().nth(1).expect("키 본문").to_string()
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

// ---------------------------------------------------------------- 프로세스

struct Server {
    child: Child,
    addr: SocketAddr,
    https: bool,
    log: Arc<Mutex<String>>,
    root: PathBuf,
}

fn pump(
    stream: impl Read + Send + 'static,
    log: Arc<Mutex<String>>,
    found: Option<mpsc::Sender<(SocketAddr, bool)>>,
) {
    std::thread::spawn(move || {
        let mut found = found;
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Some(tx) = found.as_ref() {
                for (scheme, https) in [("https://", true), ("http://", false)] {
                    if let Some(idx) = line.find(scheme) {
                        let host = line[idx + scheme.len()..].split('/').next().unwrap_or("");
                        if let Ok(addr) = host.parse::<SocketAddr>() {
                            let _ = tx.send((addr, https));
                            found = None;
                            break;
                        }
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
    fn start(root: &Path, listen: &str, extra: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(["serve", "--root"])
            .arg(root)
            .args(["--listen", listen, "--user", "owner"])
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
            Ok((addr, https)) => Server {
                child,
                addr,
                https,
                log,
                root: root.to_path_buf(),
            },
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let out = log.lock().unwrap().clone();
                panic!("서버가 주소를 알리지 않음({e}). 출력:\n{out}");
            }
        }
    }

    fn output(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn info_file(&self) -> String {
        std::fs::read_to_string(self.root.join("server.info")).unwrap_or_default()
    }

    fn url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}/mcp", self.addr)
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

/// 사용자 토큰을 발급해 원문을 돌려준다(`<root>/tokens.jsonl`에 기록된다).
fn add_token(root: &Path, user: &str) -> String {
    let out = run_cli(&["token", "add", user, "--root", path_str(root)]);
    assert!(out.status.success(), "token add 실패: {}", text(&out));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn tokens_file(root: &Path) -> PathBuf {
    root.join("tokens.jsonl")
}

/// `serve`를 실행해 **바로 거부되어야 하는** 경우. 거부 출력을 돌려준다.
/// 거부는 바인드 전에 일어나므로 데이터 폴더도 만들어지지 않아야 한다.
fn serve_refused(root: &Path, listen: &str, extra: &[&str]) -> String {
    let mut args = vec!["serve", "--root", path_str(root), "--listen", listen];
    args.extend_from_slice(extra);
    let out = run_cli(&args);
    assert!(
        !out.status.success(),
        "기동이 거부되어야 함: {listen} {extra:?}\n{}",
        text(&out)
    );
    assert!(
        !root.exists(),
        "거부된 기동이 데이터 폴더를 만들었다: {listen} {extra:?}"
    );
    text(&out)
}

// ---------------------------------------------------------------- HTTP 클라이언트

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

fn post(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)], body: &Value) -> Reply {
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

fn initialize_body() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"serve-remote-test","version":"0"}}})
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

// ---------------------------------------------------------------- 바인드 전 거부 [핵심]

#[test]
fn outside_addresses_without_tls_and_tokens_are_refused_before_binding() {
    let tmp = TempDir::new().unwrap();
    for listen in [
        "0.0.0.0:0",
        "[::]:0",
        "192.168.0.10:0",
        "8.8.8.8:0",
        "[fd00::1]:0",
    ] {
        let root = tmp.path().join("none");
        let out = serve_refused(&root, listen, &[]);
        assert!(
            out.contains("TLS") && out.contains("사용자별 토큰"),
            "{listen}: {out}"
        );
    }
}

#[test]
fn outside_addresses_need_both_tls_and_a_per_user_tokens_file() {
    let tmp = TempDir::new().unwrap();
    let certs = make_certs();
    let (cert, key) = (path_str(&certs.cert), path_str(&certs.key));
    let tokens = tokens_file(&tmp.path().join("t")); // 파일 존재 여부와 무관하게 거부는 먼저 일어난다
    let tokens = path_str(&tokens);

    // TLS만 있고 사용자별 토큰이 없다.
    let root = tmp.path().join("a");
    let out = serve_refused(&root, "0.0.0.0:0", &["--tls-cert", cert, "--tls-key", key]);
    assert!(
        out.contains("사용자별 토큰") && out.contains("--tokens"),
        "{out}"
    );
    // 사용자별 토큰만 있고 TLS가 없다.
    let root = tmp.path().join("b");
    let out = serve_refused(&root, "0.0.0.0:0", &["--tokens", tokens]);
    assert!(out.contains("TLS"), "{out}");
    // 모두가 함께 쓰는 토큰(--token-file)은 사용자별 토큰이 아니다 — TLS가 있어도 거부하고 --tokens를 안내한다.
    let shared = tmp.path().join("shared.txt");
    std::fs::write(&shared, "shared-secret\n").unwrap();
    let root = tmp.path().join("c");
    let out = serve_refused(
        &root,
        "0.0.0.0:0",
        &[
            "--tls-cert",
            cert,
            "--tls-key",
            key,
            "--token-file",
            path_str(&shared),
        ],
    );
    assert!(out.contains("--tokens"), "{out}");
    assert!(
        !out.contains("shared-secret"),
        "토큰 값이 출력에 나왔다: {out}"
    );
}

#[test]
fn behind_proxy_is_refused_on_public_and_wildcard_addresses_and_without_tokens() {
    let tmp = TempDir::new().unwrap();
    let tokens = tokens_file(&tmp.path().join("t"));
    let tokens = path_str(&tokens);
    for (listen, needle) in [
        ("8.8.8.8:0", "사설 주소"),
        ("[2001:db8::1]:0", "사설 주소"),
        ("0.0.0.0:0", "전체 주소"),
        ("[::]:0", "전체 주소"),
    ] {
        let root = tmp.path().join("p");
        let out = serve_refused(&root, listen, &["--behind-proxy", "--tokens", tokens]);
        assert!(out.contains(needle), "{listen}: {out}");
    }
    // 사설 주소라도 사용자별 토큰이 없으면 거부 (루프백도 같다).
    for listen in ["192.168.0.10:0", "10.0.0.5:0", "127.0.0.1:0"] {
        let root = tmp.path().join("q");
        let out = serve_refused(&root, listen, &["--behind-proxy"]);
        assert!(out.contains("사용자별 토큰"), "{listen}: {out}");
    }
}

#[test]
fn allow_origin_is_refused_on_loopback_and_with_a_bad_value() {
    let tmp = TempDir::new().unwrap();
    let out = serve_refused(
        &tmp.path().join("r"),
        "127.0.0.1:0",
        &["--allow-origin", "https://app.example"],
    );
    assert!(out.contains("allow-origin"), "{out}");
    assert!(out.contains("--behind-proxy"), "안내 문구 없음: {out}");
}

#[test]
fn behind_proxy_on_loopback_with_allow_origin_uses_the_allow_list_rule() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    let alice = add_token(&root, "alice");
    let tokens = tokens_file(&root);
    let server = Server::start(
        &root,
        "127.0.0.1:0",
        &[
            "--behind-proxy",
            "--tokens",
            path_str(&tokens),
            "--allow-origin",
            "https://app.example",
        ],
    );
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = server.url();
    let body = initialize_body();
    let auth = bearer(&alice);
    let status = |origin: Option<&str>| {
        let mut headers = vec![("Authorization", auth.as_str())];
        if let Some(origin) = origin {
            headers.push(("Origin", origin));
        }
        post(&agent, &url, &headers, &body).status
    };
    assert_eq!(status(Some("https://app.example")), 200);
    assert_eq!(status(None), 200);
    assert_eq!(status(Some("http://localhost:3000")), 403);
    assert_eq!(status(Some("https://evil.example")), 403);
}

#[test]
fn service_install_carries_allow_origin_with_behind_proxy_on_loopback() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    let tokens = tmp.path().join("tokens.jsonl");
    let out = run_cli(&print_args(
        path_str(&root),
        "windows",
        &[
            "--behind-proxy",
            "--tokens",
            path_str(&tokens),
            "--allow-origin",
            "https://app.example",
        ],
    ));
    assert!(out.status.success(), "{}", text(&out));
    let printed = text(&out);
    assert!(printed.contains("--allow-origin"), "{printed}");
    assert!(printed.contains("https://app.example"), "{printed}");
}

#[test]
fn tls_cert_and_tls_key_must_come_together() {
    let tmp = TempDir::new().unwrap();
    let certs = make_certs();
    let root = tmp.path().join("s");
    let only_cert = run_cli(&[
        "serve",
        "--root",
        path_str(&root),
        "--tls-cert",
        path_str(&certs.cert),
    ]);
    assert!(!only_cert.status.success());
    assert!(
        text(&only_cert).contains("--tls-key"),
        "{}",
        text(&only_cert)
    );
    let only_key = run_cli(&[
        "serve",
        "--root",
        path_str(&root),
        "--tls-key",
        path_str(&certs.key),
    ]);
    assert!(!only_key.status.success());
    assert!(
        text(&only_key).contains("--tls-cert"),
        "{}",
        text(&only_key)
    );
    assert!(!root.exists());
}

#[test]
fn unreadable_key_file_is_an_error_with_the_path_but_never_the_content() {
    let tmp = TempDir::new().unwrap();
    let certs = make_certs();
    let broken = tmp.path().join("broken.key");
    std::fs::write(
        &broken,
        format!("-----BEGIN PRIVATE KEY-----\n{KEY_LEAK_MARKER}!!\n-----END PRIVATE KEY-----\n"),
    )
    .unwrap();
    let root = tmp.path().join("k");
    let out = serve_refused(
        &root,
        "127.0.0.1:0",
        &[
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&broken),
        ],
    );
    assert!(out.contains("broken.key"), "경로가 오류에 있어야 함: {out}");
    assert!(
        !out.contains(KEY_LEAK_MARKER),
        "키 내용이 오류에 샜음: {out}"
    );

    let missing = tmp.path().join("missing.key");
    let out = serve_refused(
        &root,
        "127.0.0.1:0",
        &[
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&missing),
        ],
    );
    assert!(out.contains("missing.key"), "{out}");
}

// ---------------------------------------------------------------- 루프백 TLS와 프록시 뒤 모드(실제 기동)

#[test]
fn loopback_tls_serves_https_and_never_prints_the_key() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    let certs = make_certs();
    let server = Server::start(
        &root,
        "127.0.0.1:0",
        &[
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&certs.key),
        ],
    );
    assert!(
        server.https,
        "출력이 https 주소를 알려야 함: {}",
        server.output()
    );

    let agent = client_agent(&certs.cert_pem);
    let r = post(&agent, &server.url(), &[], &initialize_body());
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.session.is_some());

    // 같은 포트에 평문으로 접속하면 실패한다.
    let plain = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let plain_url = server.url().replacen("https://", "http://", 1);
    assert!(
        plain
            .post(&plain_url)
            .send_string(&initialize_body().to_string())
            .is_err(),
        "평문 접속이 성공하면 안 됨"
    );

    // 비밀 비노출: 키 내용은 출력·server.info·status 어디에도 없다. 인증서·키 경로도 info에 싣지 않는다.
    let status = run_cli(&["status", "--root", path_str(&root)]);
    assert!(status.status.success(), "{}", text(&status));
    assert!(
        text(&status).contains("https://"),
        "status가 https를 알려야 함: {}",
        text(&status)
    );
    let body_line = key_body_line(&certs.key_pem);
    for (what, content) in [
        ("서버 출력", server.output()),
        ("server.info", server.info_file()),
        ("status 출력", text(&status)),
    ] {
        assert!(
            !content.contains(&body_line),
            "{what}에 키 내용이 있다: {content}"
        );
        assert!(
            !content.contains("PRIVATE KEY"),
            "{what}에 키 표지가 있다: {content}"
        );
    }
    assert!(
        server.info_file().contains("\"tls\":true"),
        "{}",
        server.info_file()
    );

    let stop = run_cli(&["stop", "--root", path_str(&root)]);
    assert!(stop.status.success(), "{}", text(&stop));
}

#[test]
fn behind_proxy_on_loopback_with_tokens_serves_plain_and_keeps_loopback_origin_rule() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    let alice = add_token(&root, "alice");
    let tokens = tokens_file(&root);
    let server = Server::start(
        &root,
        "127.0.0.1:0",
        &["--behind-proxy", "--tokens", path_str(&tokens)],
    );
    assert!(!server.https);
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = server.url();
    let body = initialize_body();
    assert_eq!(post(&agent, &url, &[], &body).status, 401);
    let auth = bearer(&alice);
    assert_eq!(
        post(&agent, &url, &[("Authorization", &auth)], &body).status,
        200
    );
    // 루프백 서버의 Origin 규칙은 그대로 — 루프백 출처는 통과, 그 밖은 403.
    let local = post(
        &agent,
        &url,
        &[
            ("Authorization", &auth),
            ("Origin", "http://localhost:3000"),
        ],
        &body,
    );
    assert_eq!(local.status, 200, "{}", local.body);
    let other = post(
        &agent,
        &url,
        &[("Authorization", &auth), ("Origin", "https://app.example")],
        &body,
    );
    assert_eq!(other.status, 403);
}

// ---------------------------------------------------------------- service install --print

fn print_args<'a>(root: &'a str, target: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "service", "install", "--print", target, "--root", root, "--user", "alice",
    ];
    args.extend_from_slice(extra);
    args
}

#[test]
fn service_install_carries_the_new_options_as_absolute_paths_for_every_target() {
    let tmp = TempDir::new().unwrap();
    let certs = make_certs();
    let root = tmp.path().join("root");
    let tokens = tmp.path().join("tokens.jsonl");
    let extra = [
        "--listen",
        "0.0.0.0:7437",
        "--tls-cert",
        path_str(&certs.cert),
        "--tls-key",
        path_str(&certs.key),
        "--tokens",
        path_str(&tokens),
        "--allow-origin",
        "https://app.example:8443",
        "--allow-origin",
        "https://other.example",
    ];
    for target in ["windows", "macos", "linux"] {
        let out = run_cli(&print_args(path_str(&root), target, &extra));
        assert!(out.status.success(), "{target}: {}", text(&out));
        let printed = text(&out).replace('\\', "/");
        for needle in [
            "--tls-cert",
            "--tls-key",
            "--tokens",
            "--allow-origin",
            "https://app.example:8443",
            "https://other.example",
            "0.0.0.0:7437",
            "server cert.pem",
            "server key.pem",
        ] {
            assert!(
                printed.contains(needle),
                "{target}: {needle:?} 없음:\n{printed}"
            );
        }
        assert!(
            !printed.contains("--behind-proxy"),
            "{target}: 주지 않은 옵션이 나옴"
        );
        // 키 내용은 등록 내용에 없다 — 경로만 있다.
        assert!(
            !printed.contains(&key_body_line(&certs.key_pem)),
            "{target}: 키 내용이 있다"
        );
        assert!(!printed.contains("PRIVATE KEY"), "{target}");
    }
}

#[test]
fn service_install_carries_behind_proxy() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    let tokens = tmp.path().join("tokens.jsonl");
    let out = run_cli(&print_args(
        path_str(&root),
        "windows",
        &[
            "--listen",
            "192.168.0.10:7437",
            "--behind-proxy",
            "--tokens",
            path_str(&tokens),
        ],
    ));
    assert!(out.status.success(), "{}", text(&out));
    let printed = text(&out);
    assert!(printed.contains("--behind-proxy"), "{printed}");
    assert!(printed.contains("192.168.0.10:7437"), "{printed}");
}

#[test]
fn service_install_refuses_unsafe_remote_settings_and_still_refuses_inline_token() {
    let tmp = TempDir::new().unwrap();
    let certs = make_certs();
    let root = tmp.path().join("root");
    // TLS·토큰 없이 밖으로 여는 등록은 등록 단계에서 거부한다.
    let out = run_cli(&print_args(
        path_str(&root),
        "windows",
        &["--listen", "0.0.0.0:7437"],
    ));
    assert!(!out.status.success());
    assert!(text(&out).contains("TLS"), "{}", text(&out));
    // 공인 주소 + 프록시 뒤 모드도 거부.
    let tokens = tmp.path().join("tokens.jsonl");
    let out = run_cli(&print_args(
        path_str(&root),
        "linux",
        &[
            "--listen",
            "8.8.8.8:7437",
            "--behind-proxy",
            "--tokens",
            path_str(&tokens),
        ],
    ));
    assert!(!out.status.success());
    assert!(text(&out).contains("사설 주소"), "{}", text(&out));
    // 기존 규칙: 등록에는 --token(평문)을 넣을 수 없다.
    let out = run_cli(&print_args(
        path_str(&root),
        "windows",
        &[
            "--token",
            "inline-secret",
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&certs.key),
        ],
    ));
    assert!(!out.status.success());
    assert!(
        !text(&out).contains("inline-secret") || text(&out).contains("--token-file"),
        "{}",
        text(&out)
    );
    // 키 파일을 읽을 수 없으면 등록 단계에서 알린다(경로만, 내용 없음).
    let missing = tmp.path().join("nokey.pem");
    let out = run_cli(&print_args(
        path_str(&root),
        "windows",
        &[
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&missing),
        ],
    ));
    assert!(!out.status.success());
    assert!(text(&out).contains("nokey.pem"), "{}", text(&out));
}

// ---------------------------------------------------------------- 같은 PC 밖 주소에 실제로 바인드 (수동 실행)

#[test]
#[ignore = "0.0.0.0에 실제로 바인드한다(방화벽 허용 창이 뜰 수 있음) — 수동: cargo test -p vgraph-mcp --test serve_remote -- --ignored"]
fn remote_bind_with_tls_and_tokens_serves_https_and_applies_the_origin_allow_list() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("data");
    let certs = make_certs();
    let alice = add_token(&root, "alice");
    let tokens = tokens_file(&root);
    let server = Server::start(
        &root,
        REMOTE_LISTEN,
        &[
            "--tls-cert",
            path_str(&certs.cert),
            "--tls-key",
            path_str(&certs.key),
            "--tokens",
            path_str(&tokens),
            "--allow-origin",
            "https://app.example:8443",
        ],
    );
    assert!(server.https);
    // 와일드카드 주소로 열렸으므로 시험은 루프백 주소로 접속한다(인증서에 127.0.0.1이 들어 있다).
    let url = format!("https://127.0.0.1:{}/mcp", server.addr.port());
    let agent = client_agent(&certs.cert_pem);
    let body = initialize_body();
    let auth = bearer(&alice);

    assert_eq!(post(&agent, &url, &[], &body).status, 401, "토큰 없음");
    assert_eq!(
        post(&agent, &url, &[("Authorization", &auth)], &body).status,
        200
    );
    let ok = post(
        &agent,
        &url,
        &[
            ("Authorization", &auth),
            ("Origin", "https://app.example:8443"),
        ],
        &body,
    );
    assert_eq!(ok.status, 200, "{}", ok.body);
    for bad in [
        "http://localhost:3000",
        "http://127.0.0.1",
        "https://evil.example",
    ] {
        let r = post(
            &agent,
            &url,
            &[("Authorization", &auth), ("Origin", bad)],
            &body,
        );
        assert_eq!(r.status, 403, "Origin={bad}");
    }
    let body_line = key_body_line(&certs.key_pem);
    assert!(!server.output().contains(&body_line));
    assert!(!server.info_file().contains(&body_line));
}
