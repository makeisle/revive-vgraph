//! MCP Streamable HTTP의 내장 TLS(기능 `tls`)와 바인드 안전 규칙 시험 — 실제 서버를 임시 포트로 띄우고
//! `ureq`·`rustls` 클라이언트로 호출한다(모킹 없음). 인증서는 `rcgen`으로 만든 시험용 자체 서명이다.
//!
//! 실제로 같은 PC 밖으로 열리는 주소(`0.0.0.0`)에 바인드하는 시험은 `#[ignore]`다. Windows 방화벽 허용 창이
//! 뜨거나 네트워크에 포트가 잠깐 열릴 수 있어서, 필요할 때 사람이 직접 돌린다:
//!
//! ```text
//! cargo test -p mcp-kit --test http_tls -- --ignored
//! ```
//!
//! 나머지 시험은 루프백에만 바인드하거나, 바인드하기 전에 거부되는 경로만 쓴다.

use mcp_kit::http::{
    evaluate_bind, is_private_ip, BindKind, BindPolicy, BindRefusal, HttpOptions, HttpServer,
    RequestContext, TlsConfig, Unauthorized, MAX_BODY_BYTES,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOOPBACK_EPHEMERAL: &str = "127.0.0.1:0";
const SESSION_HEADER: &str = "mcp-session-id";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
/// 시험용 사용자 토큰. 실제 비밀이 아니다.
const TEST_TOKEN: &str = "test-token-alice";
/// 키 파일 내용이 오류에 새는지 보려고 파일에 심는 표지 문자열.
const KEY_LEAK_MARKER: &str = "TOPSECRETKEYLINEDONOTLEAK";

/// 시험 전용 RSA 2048 개인키(PKCS#1, `openssl genrsa -traditional`). 어디에도 쓰이지 않는 일회용 값이다.
const RSA_PKCS1_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAhWZuVTAdEAb7sYl+mifuYk9eT9N49EYZmmIciiTu0/Hi6DQS
UT5yYaUSD0/bFucl/vOQRk+qhOOuoqPmJNvvoX+8LOHV4MJd5biKJ0Cq7L19d3ua
WkvYJXI54ncmjg/yooFgkNawdwEXbSFvTlWmWdXWfnR2X7rkzl6luDXX82+8Ppuq
caT2OzqmSVvghVKpCEnK/T1M+hgPNuRfYJqNb7lEcjzg8nL5nTD+jjVaASLVtAId
tBsZ6BwNlrfQkEbc8pibNBT0MC4ksmZBr3X9dqrRFmIH0iPUow6aZ7/7qfFM5WbV
fPpwiWvSOrU2ku4DkBt6hgCBJUFXwuwgnR0BoQIDAQABAoIBABh9tcPuEWff4FW3
6IZkzl8+MxJ0wMAYHps5aAP0w+QlhIMdDD/eNyG2LJqjDpYl2dP044gFGElNg8JY
VOhveI8t5F5ZF+srxBFoz/Rk6vyLzX0iNDRfuE8nqXjqZymtw94R4qi0wbEXgjWG
7ehz0RatK7QLra/loBxDRCmNuRf4J7rxqdWmPNOISk88trgGVk7BelRfxFgmHGfc
yX/vr20ISXGXT6dedAeSc+fC4YKxH4JZx7z4Sqf+ACPXVEIMfKywZt1IBSH751dF
ZPa77nflOZf7+BnMzh2XAdFzRpBsE/63A/pEezHtWCi1qqxh3rio6K/34rxIdIWD
mz8FUAECgYEAux3m0JcsIBIkd1fzqZa7++jrhW2ePzQA/wi/c9B7ymSEvDZLsNkw
xIZhwQVIfNKBHei0pAXMNM2J9RzospbqWKulg+tC5jGF6CIAf+SUK5cv9/IVudrs
Qw+DFPXHSgUnPNeboLEUkKfL9dEmCFx+x8qz9PF6+aNsDppbMH4JTw0CgYEAtoI1
bh+TrXEl44x94AXM1uLygQLPjJWG919PccKl7KVA/XiFlkEg8Y6AW52L/GH63NOd
meCwxnWe29dEZD4cizsYzXYe8IFbyGlRaV0+/FhAmpAYN7cc2NgztD51daZvamCf
kxEqLxBr5tYziLjkGoaajpUwtLHvez6FWOqjt+UCgYEAtJn0yQnQHhhEuCfc16fU
Qx0aDcuyvIAeIgo4d7kXck64ysyBG+1Vo6msule2Ejw/QTX0/IdYBPMJn9pKQu1k
vTbW50/wXywXeekvoEFoPfaL5v3yuFM0TLmxJE7hDnGQ4Kmd3WhtV/2M3/MBiXp1
J7NfJCDbOA3i4ula8KAFrhUCgYEAh1Yq9MxpoBXleMV3miXy7e5A0bYZ3jALc2iz
6rHihIdOmCdFOFR3b2iyej1rzu2BLzulBLR/ocsaz/7jcko0tHJQRBuxebsTKpwe
iLliuZFf5r2vl7ErrQk13hTq2SFgQUwBSPdThq7wtK5mbJ5eV+U0u0KaaYQBCC0m
iPcGDJkCgYA22ByNBSdPLA0PzEevWySPF1Brhr/uU3a0SsAePpWl0ITYDjRQL3rk
BafLI5px8y2Ic1SaMUNOQAgf6ew0rhqWAW7DYAKJgXobkTMp9iZTSbmbCz2IeNuS
1GFWSypBHffsEC9kUvy1g+X3H0ir1xjymihv+Xh1+Ro/JMykNXQG7w==
-----END RSA PRIVATE KEY-----
";

// ---------------------------------------------------------------- 인증서·키 준비

struct Material {
    cert_pem: String,
    key_pem: String,
}

struct Files {
    _dir: tempfile::TempDir,
    cert: PathBuf,
    key: PathBuf,
}

fn write_files(cert_pem: &str, key_pem: &str) -> Files {
    let dir = tempfile::tempdir().expect("임시 폴더");
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, cert_pem).expect("cert 쓰기");
    std::fs::write(&key, key_pem).expect("key 쓰기");
    Files {
        _dir: dir,
        cert,
        key,
    }
}

/// ECDSA P-256 자체 서명 인증서 + PKCS#8 키.
fn ec_pkcs8() -> Material {
    let made =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("자체 서명");
    Material {
        cert_pem: made.cert.pem(),
        key_pem: made.signing_key.serialize_pem(),
    }
}

// --- 최소 DER 도구 (키 형식 변환용) ---

fn der_len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![u8::try_from(n).expect("짧은 길이")]
    } else if n < 0x100 {
        vec![0x81, u8::try_from(n).expect("1바이트 길이")]
    } else {
        let n = u16::try_from(n).expect("2바이트 길이");
        vec![0x82, (n >> 8) as u8, (n & 0xff) as u8]
    }
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(der_len(content.len()));
    out.extend_from_slice(content);
    out
}

/// `tag`로 시작하는 DER 항목 하나를 읽어 (내용, 나머지)를 돌려준다.
fn der_take(tag: u8, data: &[u8]) -> (&[u8], &[u8]) {
    assert_eq!(data[0], tag, "DER 태그");
    let (len, header) = match data[1] {
        n if n < 0x80 => (usize::from(n), 2),
        0x81 => (usize::from(data[2]), 3),
        0x82 => (usize::from(data[2]) << 8 | usize::from(data[3]), 4),
        other => panic!("지원하지 않는 DER 길이 {other:#x}"),
    };
    (&data[header..header + len], &data[header + len..])
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> (18 - 6 * i)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn pem(label: &str, der: &[u8]) -> String {
    let body = base64(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// PKCS#8로 감싼 EC 키에서 안쪽 SEC1(`EC PRIVATE KEY`) DER를 꺼낸다.
fn ec_sec1(pkcs8_pem: &str) -> Material {
    let key = PrivatePkcs8KeyDer::from_pem_slice(pkcs8_pem.as_bytes()).expect("PKCS#8");
    let (info, _) = der_take(0x30, key.secret_pkcs8_der());
    let (_version, rest) = der_take(0x02, info);
    let (_algorithm, rest) = der_take(0x30, rest);
    let (sec1, _) = der_take(0x04, rest);
    Material {
        cert_pem: String::new(),
        key_pem: pem("EC PRIVATE KEY", sec1),
    }
}

/// RSA PKCS#1 키(고정 시험 값)와, 그 키로 서명한 자체 서명 인증서.
fn rsa_pkcs1() -> Material {
    let pkcs1 = rustls::pki_types::PrivatePkcs1KeyDer::from_pem_slice(RSA_PKCS1_PEM.as_bytes())
        .expect("PKCS#1");
    // PKCS#8 = SEQUENCE { INTEGER 0, SEQUENCE { OID rsaEncryption, NULL }, OCTET STRING { PKCS#1 } }
    let algorithm = der(
        0x30,
        &[
            &[
                0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
            ][..],
            &[0x05, 0x00][..],
        ]
        .concat(),
    );
    let pkcs8 = der(
        0x30,
        &[
            &[0x02, 0x01, 0x00][..],
            &algorithm[..],
            &der(0x04, pkcs1.secret_pkcs1_der())[..],
        ]
        .concat(),
    );
    let pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(pkcs8),
        &rcgen::PKCS_RSA_SHA256,
    )
    .expect("RSA 키 쌍");
    let cert =
        rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("인증서 인자")
            .self_signed(&pair)
            .expect("RSA 자체 서명");
    Material {
        cert_pem: cert.pem(),
        key_pem: RSA_PKCS1_PEM.to_string(),
    }
}

// ---------------------------------------------------------------- 클라이언트

fn client_config(cert_pem: &str) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(cert_pem.as_bytes()) {
        roots.add(cert.expect("인증서 PEM")).expect("루트 추가");
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("프로토콜")
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

fn tls_agent(cert_pem: &str) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(CLIENT_TIMEOUT)
        .tls_config(client_config(cert_pem))
        .build()
}

fn send(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = match body {
        Some(b) => req.set("content-type", "application/json").send_string(b),
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

fn rpc(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
}

fn bearer() -> String {
    format!("Bearer {TEST_TOKEN}")
}

// ---------------------------------------------------------------- 서버

/// `slow` 도구는 시작을 알리고 풀어 줄 때까지 기다린다(진행 중 요청 시험용).
struct Gate {
    started: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

fn handler(
    gate: Option<Arc<Gate>>,
) -> impl Fn(&Value, &RequestContext) -> Option<Value> + Send + Sync + 'static {
    move |msg, ctx| {
        mcp_kit::handle_message(
            msg,
            "kit-tls-test",
            "0.0.0",
            "instructions",
            || vec![json!({"name":"echo"}), json!({"name":"slow"})],
            |name, args| {
                if name == "slow" {
                    if let Some(gate) = &gate {
                        gate.started
                            .lock()
                            .expect("started")
                            .send(())
                            .expect("알림");
                        gate.release
                            .lock()
                            .expect("release")
                            .recv_timeout(CLIENT_TIMEOUT)
                            .expect("풀어 줌");
                    }
                }
                Ok(json!({"tool": name, "args": args, "principal": ctx.principal}))
            },
        )
    }
}

/// 시험용 검증기: `Bearer TEST_TOKEN`이면 주체 "alice".
fn authenticator(
) -> impl Fn(&mcp_kit::http::RequestHeaders<'_>) -> Result<Option<String>, Unauthorized>
       + Send
       + Sync
       + 'static {
    |headers| match headers.bearer() {
        Some(TEST_TOKEN) => Ok(Some("alice".to_string())),
        _ => Err(Unauthorized),
    }
}

fn tls_options(files: &Files) -> HttpOptions {
    HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소"))
        .tls(TlsConfig::from_pem_files(&files.cert, &files.key).expect("TLS 설정"))
}

fn start_tls(files: &Files) -> HttpServer {
    HttpServer::start(tls_options(files), handler(None)).expect("TLS 서버 기동")
}

fn https_url(server: &HttpServer) -> String {
    format!("https://{}/mcp", server.local_addr())
}

fn initialize(agent: &ureq::Agent, url: &str, extra: &[(&str, &str)]) -> Reply {
    send(
        agent,
        "POST",
        url,
        extra,
        Some(&rpc(
            1,
            "initialize",
            json!({"protocolVersion":"2025-06-18"}),
        )),
    )
}

// ---------------------------------------------------------------- 정상 흐름

#[test]
fn https_initialize_tool_call_and_session_delete_work() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let url = https_url(&server);
    let agent = tls_agent(&material.cert_pem);

    let init = initialize(&agent, &url, &[]);
    assert_eq!(init.status, 200, "{}", init.body);
    let sid = init.session.clone().expect("세션 ID");
    let call = send(
        &agent,
        "POST",
        &url,
        &[(SESSION_HEADER, sid.as_str())],
        Some(&rpc(
            2,
            "tools/call",
            json!({"name":"echo","arguments":{"x":1}}),
        )),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert!(call.body.contains("echo"), "{}", call.body);
    let end = send(
        &agent,
        "DELETE",
        &url,
        &[(SESSION_HEADER, sid.as_str())],
        None,
    );
    assert_eq!(end.status, 204);
    server.shutdown();
}

#[test]
fn plain_http_to_tls_port_fails() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let plain = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = format!("http://{}/mcp", server.local_addr());
    match plain
        .post(&url)
        .set("content-type", "application/json")
        .send_string(&rpc(1, "initialize", json!({})))
    {
        Err(_) => {}
        Ok(r) => panic!(
            "평문 접속이 TLS 포트에서 성공하면 안 됨: 상태 {}",
            r.status()
        ),
    }
    // 평문 시도가 서버를 해치지 않았다.
    let agent = tls_agent(&material.cert_pem);
    assert_eq!(initialize(&agent, &https_url(&server), &[]).status, 200);
    server.shutdown();
}

#[test]
fn client_without_trust_for_the_certificate_cannot_connect() {
    let material = ec_pkcs8();
    let other = ec_pkcs8(); // 다른 인증서만 믿는 클라이언트
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let agent = tls_agent(&other.cert_pem);
    let res = agent
        .post(&https_url(&server))
        .send_string(&rpc(1, "initialize", json!({})));
    assert!(res.is_err(), "믿지 않는 인증서면 접속이 실패해야 함");
    server.shutdown();
}

// ---------------------------------------------------------------- 개인키 형식

#[test]
fn private_key_formats_pkcs8_sec1_and_pkcs1_all_serve_https() {
    let ec = ec_pkcs8();
    let sec1 = ec_sec1(&ec.key_pem);
    let rsa = rsa_pkcs1();
    let cases = [
        ("PKCS#8(EC)", ec.cert_pem.clone(), ec.key_pem.clone()),
        ("SEC1(EC)", ec.cert_pem.clone(), sec1.key_pem.clone()),
        ("PKCS#1(RSA)", rsa.cert_pem.clone(), rsa.key_pem.clone()),
    ];
    for (name, cert_pem, key_pem) in cases {
        assert!(
            key_pem.contains("PRIVATE KEY"),
            "{name}: 시험 준비 오류\n{key_pem}"
        );
        let files = write_files(&cert_pem, &key_pem);
        let config = TlsConfig::from_pem_files(&files.cert, &files.key)
            .unwrap_or_else(|e| panic!("{name} 키를 읽지 못함: {e}"));
        let server = HttpServer::start(
            HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소")).tls(config),
            handler(None),
        )
        .expect("기동");
        let agent = tls_agent(&cert_pem);
        let reply = initialize(&agent, &https_url(&server), &[]);
        assert_eq!(reply.status, 200, "{name}: {}", reply.body);
        server.shutdown();
    }
}

#[test]
fn unusable_pem_files_are_clear_errors_without_key_content() {
    let ec = ec_pkcs8();
    let other = ec_pkcs8();

    let load = |cert_pem: &str, key_pem: &str| {
        let files = write_files(cert_pem, key_pem);
        let err = match TlsConfig::from_pem_files(&files.cert, &files.key) {
            Ok(_) => panic!("실패해야 함"),
            Err(e) => e,
        };
        (err.to_string(), files)
    };

    // 키 줄에 표지를 심은 깨진 PEM — 오류 문구에 그 줄이 나오면 안 된다.
    let broken_key = format!(
        "-----BEGIN PRIVATE KEY-----\n{KEY_LEAK_MARKER}!!not-base64!!\n-----END PRIVATE KEY-----\n"
    );
    let (msg, files) = load(&ec.cert_pem, &broken_key);
    assert!(
        !msg.contains(KEY_LEAK_MARKER),
        "키 내용이 오류에 샜음: {msg}"
    );
    assert!(msg.contains("개인키"), "{msg}");
    assert!(
        msg.contains(&files.key.display().to_string()),
        "경로가 있어야 함: {msg}"
    );

    // 시작 줄이 깨진 경우(파서 오류에 그 줄이 실리는 종류).
    let bad_start = format!("-----BEGIN {KEY_LEAK_MARKER} PRIVATE KEY\n");
    let (msg, _) = load(&ec.cert_pem, &bad_start);
    assert!(
        !msg.contains(KEY_LEAK_MARKER),
        "키 내용이 오류에 샜음: {msg}"
    );

    // PEM이 아닌 파일.
    let (msg, _) = load(&ec.cert_pem, "그냥 글자");
    assert!(msg.contains("PEM"), "{msg}");

    // 인증서와 짝이 아닌 키.
    let (msg, _) = load(&ec.cert_pem, &other.key_pem);
    assert!(msg.contains("인증서와 개인키"), "{msg}");

    // 인증서 파일이 비었다.
    let (msg, _) = load("", &ec.key_pem);
    assert!(msg.contains("인증서"), "{msg}");

    // 파일이 없다 — 경로가 오류에 있어야 한다.
    let missing = Path::new("definitely-missing-dir").join("nope.pem");
    let err = TlsConfig::from_pem_files(&missing, &missing)
        .err()
        .expect("없는 파일은 실패");
    assert!(err.to_string().contains("nope.pem"), "{err}");
}

// ---------------------------------------------------------------- 연결 견고성

#[test]
fn garbage_and_silent_connections_do_not_stop_the_server() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let addr = server.local_addr();

    // 쓰레기를 보내고 끊는 연결, 아무것도 안 보내고 붙들고 있는 연결, 절반만 보내고 붙든 연결.
    let mut garbage = TcpStream::connect(addr).expect("접속");
    garbage
        .write_all(b"\x16\x03\x01\x00\x05garbage-not-a-client-hello")
        .expect("전송");
    let silent = TcpStream::connect(addr).expect("접속");
    let mut partial = TcpStream::connect(addr).expect("접속");
    partial.write_all(b"\x16\x03\x01").expect("전송");

    let agent = tls_agent(&material.cert_pem);
    let url = https_url(&server);
    for _ in 0..3 {
        assert_eq!(initialize(&agent, &url, &[]).status, 200);
    }
    // 쓰레기를 받은 연결은 서버가 닫는다(읽으면 EOF 또는 경고 알림 뒤 EOF).
    garbage
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .expect("타임아웃");
    let mut sink = Vec::new();
    let _ = garbage.read_to_end(&mut sink);
    drop((silent, partial));
    server.shutdown();
}

// ---------------------------------------------------------------- 검사 순서·상한은 TLS에서도 같다

#[test]
fn origin_then_authenticator_order_and_loopback_origin_rule_hold_over_tls() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = HttpServer::start(
        tls_options(&files).authenticator(authenticator()),
        handler(None),
    )
    .expect("기동");
    let url = https_url(&server);
    let agent = tls_agent(&material.cert_pem);
    let body = rpc(1, "initialize", json!({}));

    // Origin이 나쁘면 토큰이 맞아도 403 (Origin 먼저).
    let bad_origin = send(
        &agent,
        "POST",
        &url,
        &[
            ("Origin", "http://evil.example"),
            ("Authorization", &bearer()),
        ],
        Some(&body),
    );
    assert_eq!(bad_origin.status, 403, "{}", bad_origin.body);
    // Origin이 좋아도 토큰이 없으면 401.
    let no_token = send(
        &agent,
        "POST",
        &url,
        &[("Origin", "http://localhost:3000")],
        Some(&body),
    );
    assert_eq!(no_token.status, 401, "{}", no_token.body);
    let wrong = send(
        &agent,
        "POST",
        &url,
        &[("Authorization", "Bearer nope")],
        Some(&body),
    );
    assert_eq!(wrong.status, 401);
    // 둘 다 맞으면 통과하고 주체가 처리 함수에 간다.
    let init = send(
        &agent,
        "POST",
        &url,
        &[
            ("Origin", "http://localhost:3000"),
            ("Authorization", &bearer()),
        ],
        Some(&body),
    );
    assert_eq!(init.status, 200, "{}", init.body);
    let sid = init.session.expect("세션");
    let call = send(
        &agent,
        "POST",
        &url,
        &[(SESSION_HEADER, sid.as_str()), ("Authorization", &bearer())],
        Some(&rpc(2, "tools/call", json!({"name":"echo"}))),
    );
    assert!(
        call.body.contains("alice"),
        "주체가 전달돼야 함: {}",
        call.body
    );
    server.shutdown();
}

#[test]
fn body_over_the_limit_is_413_over_tls() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let agent = tls_agent(&material.cert_pem);
    let big = "x".repeat(MAX_BODY_BYTES + 1);
    let reply = send(&agent, "POST", &https_url(&server), &[], Some(&big));
    assert_eq!(reply.status, 413, "{}", reply.body);
    server.shutdown();
}

// ---------------------------------------------------------------- 그레이스풀 종료

#[test]
fn shutdown_closes_idle_keep_alive_connections_promptly() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = start_tls(&files);
    let agent = tls_agent(&material.cert_pem);
    // 같은 agent가 연결을 풀에 붙들고 있다(keep-alive).
    assert_eq!(initialize(&agent, &https_url(&server), &[]).status, 200);

    let started = Instant::now();
    server.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "유휴 연결 때문에 종료가 지연됨: {elapsed:?}"
    );
}

#[test]
fn in_flight_request_finishes_before_shutdown_completes() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = Arc::new(Gate {
        started: Mutex::new(started_tx),
        release: Mutex::new(release_rx),
    });
    let server = HttpServer::start(tls_options(&files), handler(Some(gate))).expect("기동");
    let url = https_url(&server);
    let agent = tls_agent(&material.cert_pem);
    let sid = initialize(&agent, &url, &[]).session.expect("세션");

    let slow = {
        let agent = agent.clone();
        let url = url.clone();
        std::thread::spawn(move || {
            send(
                &agent,
                "POST",
                &url,
                &[(SESSION_HEADER, sid.as_str())],
                Some(&rpc(2, "tools/call", json!({"name":"slow"}))),
            )
        })
    };
    started_rx.recv_timeout(CLIENT_TIMEOUT).expect("처리 시작");
    let stopper = std::thread::spawn(move || {
        let t = Instant::now();
        server.shutdown();
        t.elapsed()
    });
    // 요청이 처리 중인 동안 종료는 끝나지 않는다.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !stopper.is_finished(),
        "진행 중 요청을 기다리지 않고 종료됨"
    );
    release_tx.send(()).expect("풀어 줌");

    let reply = slow.join().expect("요청 스레드");
    assert_eq!(reply.status, 200, "진행 중이던 요청이 잘림: {}", reply.body);
    let elapsed = stopper.join().expect("종료 스레드");
    assert!(
        elapsed < Duration::from_secs(9),
        "종료가 너무 오래 걸림: {elapsed:?}"
    );
}

// ---------------------------------------------------------------- 바인드 정책 (바인드 전 거부)

fn ip(text: &str) -> IpAddr {
    text.parse().expect("IP")
}

fn policy(tls: bool, authenticated: bool, behind_proxy: bool) -> BindPolicy {
    BindPolicy {
        tls,
        authenticated,
        behind_proxy,
    }
}

#[test]
fn private_address_table() {
    for private in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.10",
        "fc00::1",
        "fd12:3456::1",
        "::ffff:10.1.2.3",
        "::ffff:192.168.1.1",
    ] {
        assert!(is_private_ip(ip(private)), "{private}");
    }
    for not_private in [
        "127.0.0.1",
        "::1",
        "0.0.0.0",
        "::",
        "8.8.8.8",
        "172.15.255.255",
        "172.32.0.0",
        "192.169.0.1",
        "11.0.0.1",
        "169.254.1.1",
        "fe80::1",
        "2001:db8::1",
        "::ffff:8.8.8.8",
        "::ffff:127.0.0.1",
    ] {
        assert!(!is_private_ip(ip(not_private)), "{not_private}");
    }
}

#[test]
fn bind_policy_table() {
    // 루프백: 늘 허용 (TLS·인증은 선택). 프록시 뒤 모드면 인증만 필요.
    for lo in ["127.0.0.1", "::1"] {
        for tls in [false, true] {
            for auth in [false, true] {
                assert_eq!(
                    evaluate_bind(ip(lo), policy(tls, auth, false)),
                    Ok(BindKind::Loopback),
                    "{lo} tls={tls} auth={auth}"
                );
            }
        }
        assert_eq!(
            evaluate_bind(ip(lo), policy(false, false, true)),
            Err(BindRefusal::ProxyNeedsAuth)
        );
        assert_eq!(
            evaluate_bind(ip(lo), policy(false, true, true)),
            Ok(BindKind::Loopback)
        );
    }
    // 밖으로 열리는 주소(사설·공인·전체·IPv6): TLS와 인증이 둘 다 있어야 한다.
    for remote in [
        "192.168.0.10",
        "10.0.0.1",
        "8.8.8.8",
        "0.0.0.0",
        "::",
        "fd00::1",
        "2001:db8::1",
    ] {
        assert_eq!(
            evaluate_bind(ip(remote), policy(false, false, false)),
            Err(BindRefusal::RemoteNeedsTlsAndAuth {
                missing_tls: true,
                missing_auth: true
            }),
            "{remote}"
        );
        assert_eq!(
            evaluate_bind(ip(remote), policy(true, false, false)),
            Err(BindRefusal::RemoteNeedsTlsAndAuth {
                missing_tls: false,
                missing_auth: true
            }),
            "{remote}"
        );
        assert_eq!(
            evaluate_bind(ip(remote), policy(false, true, false)),
            Err(BindRefusal::RemoteNeedsTlsAndAuth {
                missing_tls: true,
                missing_auth: false
            }),
            "{remote}"
        );
        assert_eq!(
            evaluate_bind(ip(remote), policy(true, true, false)),
            Ok(BindKind::Remote),
            "{remote}"
        );
    }
    // 프록시 뒤 모드: 사설 주소에서만, 인증과 함께만.
    for private in ["10.0.0.1", "172.16.5.5", "192.168.1.1", "fd00::1"] {
        assert_eq!(
            evaluate_bind(ip(private), policy(false, true, true)),
            Ok(BindKind::Remote),
            "{private}"
        );
        assert_eq!(
            evaluate_bind(ip(private), policy(false, false, true)),
            Err(BindRefusal::ProxyNeedsAuth),
            "{private}"
        );
    }
    for public in ["8.8.8.8", "2001:db8::1", "172.32.0.1", "169.254.0.1"] {
        assert_eq!(
            evaluate_bind(ip(public), policy(true, true, true)),
            Err(BindRefusal::ProxyNeedsPrivateAddress { unspecified: false }),
            "공인 주소는 인증이 있어도 거부: {public}"
        );
    }
    // 전체 주소는 사설이 아니다 — 프록시 뒤 모드로도 거부, TLS 필수.
    for any in ["0.0.0.0", "::"] {
        assert_eq!(
            evaluate_bind(ip(any), policy(true, true, true)),
            Err(BindRefusal::ProxyNeedsPrivateAddress { unspecified: true }),
            "{any}"
        );
    }
}

#[test]
fn refusal_messages_name_the_missing_pieces() {
    let msg = BindRefusal::RemoteNeedsTlsAndAuth {
        missing_tls: true,
        missing_auth: false,
    }
    .to_string();
    assert!(
        msg.contains("TLS 인증서") && !msg.contains("TLS 인증서와 사용자별"),
        "{msg}"
    );
    let msg = BindRefusal::RemoteNeedsTlsAndAuth {
        missing_tls: false,
        missing_auth: true,
    }
    .to_string();
    assert!(msg.contains("사용자별 토큰"), "{msg}");
}

fn refused(opts: HttpOptions) -> std::io::Error {
    match HttpServer::start(opts, handler(None)) {
        Ok(_) => panic!("기동이 거부되어야 함"),
        Err(e) => e,
    }
}

#[test]
fn start_refuses_remote_addresses_before_binding() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let addr: SocketAddr = "0.0.0.0:0".parse().expect("주소");
    // TLS만 있고 검증기가 없다.
    let err = refused(
        HttpOptions::new(addr)
            .tls(TlsConfig::from_pem_files(&files.cert, &files.key).expect("TLS")),
    );
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("사용자별 토큰"), "{err}");
    // 검증기만 있고 TLS가 없다.
    let err = refused(HttpOptions::new(addr).authenticator(authenticator()));
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("TLS"), "{err}");
    // 공유 토큰 하나는 사용자별 검증기가 아니다.
    let err = refused(
        HttpOptions::new(addr)
            .tls(TlsConfig::from_pem_files(&files.cert, &files.key).expect("TLS"))
            .token("shared"),
    );
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn start_refuses_proxy_mode_on_public_or_wildcard_addresses_and_without_auth() {
    for (addr, needle) in [
        ("8.8.8.8:0", "사설 주소"),
        ("0.0.0.0:0", "전체 주소"),
        ("[::]:0", "전체 주소"),
    ] {
        let a: SocketAddr = addr.parse().expect("주소");
        let err = refused(
            HttpOptions::new(a)
                .behind_proxy(true)
                .authenticator(authenticator()),
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{addr}");
        assert!(err.to_string().contains(needle), "{addr}: {err}");
    }
    // 사설 주소라도 검증기가 없으면 거부 (바인드 전에 거부되므로 실제로 열리지 않는다).
    let err = refused(HttpOptions::new("192.168.0.10:0".parse().expect("주소")).behind_proxy(true));
    assert!(err.to_string().contains("사용자별 토큰"), "{err}");
    let err =
        refused(HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소")).behind_proxy(true));
    assert!(err.to_string().contains("사용자별 토큰"), "{err}");
}

#[test]
fn proxy_mode_on_loopback_with_authenticator_starts_plain() {
    let server = HttpServer::start(
        HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소"))
            .behind_proxy(true)
            .authenticator(authenticator()),
        handler(None),
    )
    .expect("루프백 + 프록시 뒤 + 검증기는 평문으로 열린다");
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = format!("http://{}/mcp", server.local_addr());
    let body = rpc(1, "initialize", json!({}));
    assert_eq!(send(&agent, "POST", &url, &[], Some(&body)).status, 401);
    assert_eq!(
        send(
            &agent,
            "POST",
            &url,
            &[("Authorization", &bearer())],
            Some(&body)
        )
        .status,
        200
    );
    server.shutdown();
}

#[test]
fn proxy_mode_on_loopback_accepts_allow_origin_and_uses_the_allow_list_rule() {
    let server = HttpServer::start(
        HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소"))
            .behind_proxy(true)
            .authenticator(authenticator())
            .allow_origin("https://app.example"),
        handler(None),
    )
    .expect("루프백 + 프록시 뒤 + 허용 출처는 열린다");
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = format!("http://{}/mcp", server.local_addr());
    let body = rpc(1, "initialize", json!({}));
    let auth = bearer();
    let status = |origin: Option<&str>| {
        let mut headers = vec![("Authorization", auth.as_str())];
        if let Some(origin) = origin {
            headers.push(("Origin", origin));
        }
        send(&agent, "POST", &url, &headers, Some(&body)).status
    };
    assert_eq!(status(Some("https://app.example")), 200);
    assert_eq!(status(None), 200);
    // 목록 밖은 루프백 출처여도 403 — 원격 모드와 같은 규칙.
    assert_eq!(status(Some("http://localhost:3000")), 403);
    assert_eq!(status(Some("https://evil.example")), 403);
    server.shutdown();
}

#[test]
fn proxy_mode_on_loopback_without_allow_origin_keeps_the_loopback_origin_rule() {
    let server = HttpServer::start(
        HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소"))
            .behind_proxy(true)
            .authenticator(authenticator()),
        handler(None),
    )
    .expect("기동");
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let url = format!("http://{}/mcp", server.local_addr());
    let body = rpc(1, "initialize", json!({}));
    let auth = bearer();
    let with_origin = |origin: &str| {
        send(
            &agent,
            "POST",
            &url,
            &[("Authorization", auth.as_str()), ("Origin", origin)],
            Some(&body),
        )
        .status
    };
    assert_eq!(with_origin("http://localhost:3000"), 200);
    assert_eq!(with_origin("https://app.example"), 403);
    server.shutdown();
}

#[test]
fn allow_origin_on_loopback_without_proxy_mode_is_refused_with_guidance() {
    let err = refused(
        HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소"))
            .authenticator(authenticator())
            .allow_origin("https://app.example"),
    );
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("behind_proxy"), "{err}");
}

#[test]
fn allow_origin_is_refused_on_loopback_and_malformed_entries_are_refused() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    // 루프백 서버에는 허용 출처를 줄 수 없다 — 루프백 Origin 규칙은 그대로다.
    let err = refused(tls_options(&files).allow_origin("https://app.example"));
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("allow_origin"), "{err}");

    // 밖으로 여는 서버에서도 형식이 틀린 출처는 바인드하기 전에 거부된다(와일드카드 포함).
    for bad in ["*", "app.example", "https://app.example/path"] {
        let err = refused(
            HttpOptions::new("0.0.0.0:0".parse().expect("주소"))
                .tls(TlsConfig::from_pem_files(&files.cert, &files.key).expect("TLS"))
                .authenticator(authenticator())
                .allow_origin(bad),
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
        assert!(err.to_string().contains("허용 출처"), "{bad}: {err}");
    }
}

// ---------------------------------------------------------------- 같은 PC 밖 주소에 실제로 바인드 (수동 실행)

fn remote_server(files: &Files, allow: &[&str]) -> HttpServer {
    let mut opts = HttpOptions::new("0.0.0.0:0".parse().expect("주소"))
        .tls(TlsConfig::from_pem_files(&files.cert, &files.key).expect("TLS"))
        .authenticator(authenticator());
    for origin in allow {
        opts = opts.allow_origin(origin);
    }
    HttpServer::start(opts, handler(None)).expect("원격 서버 기동")
}

#[test]
#[ignore = "0.0.0.0에 실제로 바인드한다(방화벽 허용 창이 뜰 수 있음) — 수동: cargo test -p mcp-kit --test http_tls -- --ignored"]
fn remote_bind_with_tls_and_auth_serves_https_and_applies_the_origin_allow_list() {
    let material = ec_pkcs8();
    let files = write_files(&material.cert_pem, &material.key_pem);
    let server = remote_server(
        &files,
        &["https://app.example:8443", "https://other.example/"],
    );
    let port = server.local_addr().port();
    let url = format!("https://127.0.0.1:{port}/mcp");
    let agent = tls_agent(&material.cert_pem);
    let body = rpc(1, "initialize", json!({}));
    let auth = bearer();

    // 토큰 없음 401, 있으면 200 — Origin 없는 비브라우저 클라이언트는 통과.
    assert_eq!(send(&agent, "POST", &url, &[], Some(&body)).status, 401);
    assert_eq!(
        send(
            &agent,
            "POST",
            &url,
            &[("Authorization", &auth)],
            Some(&body)
        )
        .status,
        200
    );
    // 허용 목록의 출처만 통과(대소문자 무관, 목록 끝의 `/`는 무시).
    for ok in [
        "https://app.example:8443",
        "HTTPS://APP.EXAMPLE:8443",
        "https://other.example",
    ] {
        let r = send(
            &agent,
            "POST",
            &url,
            &[("Authorization", &auth), ("Origin", ok)],
            Some(&body),
        );
        assert_eq!(r.status, 200, "Origin={ok}: {}", r.body);
    }
    // 목록 밖은 루프백 출처여도 403.
    for bad in [
        "http://localhost:3000",
        "http://127.0.0.1",
        "https://evil.example",
        "https://app.example",
        "null",
    ] {
        let r = send(
            &agent,
            "POST",
            &url,
            &[("Authorization", &auth), ("Origin", bad)],
            Some(&body),
        );
        assert_eq!(r.status, 403, "Origin={bad}");
    }
    server.shutdown();

    // 허용 목록이 비어 있으면 Origin이 있는 요청은 전부 403, 없는 요청은 통과.
    let server = remote_server(&files, &[]);
    let url = format!("https://127.0.0.1:{}/mcp", server.local_addr().port());
    let r = send(
        &agent,
        "POST",
        &url,
        &[("Authorization", &auth), ("Origin", "http://localhost")],
        Some(&body),
    );
    assert_eq!(r.status, 403);
    let r = send(
        &agent,
        "POST",
        &url,
        &[("Authorization", &auth)],
        Some(&body),
    );
    assert_eq!(r.status, 200);
    server.shutdown();
}
