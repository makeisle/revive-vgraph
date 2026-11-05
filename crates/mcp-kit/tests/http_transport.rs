//! MCP Streamable HTTP 전송 시험 — 실제 서버를 임시 포트로 띄우고 `ureq`로 호출한다(모킹 없음).
//! 처리 함수는 실제 `mcp_kit::handle_message`를 감싼 것이다.

use mcp_kit::http::{HttpOptions, HttpServer, RequestContext};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

const LOOPBACK_EPHEMERAL: &str = "127.0.0.1:0";
const SESSION_HEADER: &str = "mcp-session-id";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

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

fn send(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Reply {
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
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

/// 실제 `handle_message` 위에 얹은 처리 함수. tools/call은 받은 문맥을 그대로 되돌려 준다.
/// `rendezvous` 도구는 두 호출이 동시에 처리 중일 때만 끝난다(병렬 처리 증명).
fn make_handler(
    barrier: Arc<Barrier>,
) -> impl Fn(&Value, &RequestContext) -> Option<Value> + Send + Sync + 'static {
    move |msg, ctx| {
        mcp_kit::handle_message(
            msg,
            "kit-test",
            "0.0.0",
            "instructions",
            || vec![json!({"name":"echo"}), json!({"name":"rendezvous"})],
            |name, args| {
                if name == "rendezvous" {
                    barrier.wait();
                }
                Ok(json!({
                    "tool": name,
                    "args": args,
                    "session": ctx.session_id,
                    "ident": ctx.header("x-ident"),
                    "other": ctx.header("x-not-forwarded"),
                    "principal": ctx.principal,
                }))
            },
        )
    }
}

fn start(opts: HttpOptions) -> HttpServer {
    HttpServer::start(opts, make_handler(Arc::new(Barrier::new(2)))).expect("서버 기동")
}

fn base_opts() -> HttpOptions {
    HttpOptions::new(LOOPBACK_EPHEMERAL.parse().expect("주소")).forward_header("X-Ident")
}

fn url(s: &HttpServer) -> String {
    format!("http://{}/mcp", s.local_addr())
}

fn initialize(url: &str, extra: &[(&str, &str)]) -> Reply {
    send(
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

fn tool_text(reply: &Reply) -> Value {
    let v = reply.json();
    let text = v
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("tools/call 결과가 아님: {v}"));
    serde_json::from_str(text).expect("도구 결과 JSON")
}

// ---------- 정상 흐름 ----------

#[test]
fn initialize_issues_session_then_list_and_call_work() {
    let server = start(base_opts());
    let u = url(&server);

    let init = initialize(&u, &[]);
    assert_eq!(init.status, 200, "{}", init.body);
    let sid = init.session.clone().expect("initialize 응답에 세션 ID");
    assert!(
        !sid.is_empty() && sid.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "세션 ID는 보이는 ASCII만: {sid:?}"
    );
    assert_eq!(init.json()["result"]["serverInfo"]["name"], "kit-test");

    let hdr = [
        (SESSION_HEADER, sid.as_str()),
        ("MCP-Protocol-Version", "2025-06-18"),
    ];
    let list = send("POST", &u, &hdr, Some(&rpc(2, "tools/list", json!({}))));
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(list.json()["result"]["tools"][0]["name"], "echo");

    let call = send(
        "POST",
        &u,
        &[
            (SESSION_HEADER, sid.as_str()),
            ("X-Ident", "alice"),
            ("X-Not-Forwarded", "zzz"),
        ],
        Some(&rpc(
            3,
            "tools/call",
            json!({"name":"echo","arguments":{"k":1}}),
        )),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    let out = tool_text(&call);
    assert_eq!(out["session"], sid.as_str());
    assert_eq!(out["ident"], "alice", "지정한 헤더만 문맥에 실린다");
    assert_eq!(
        out["other"],
        Value::Null,
        "지정하지 않은 헤더는 넘기지 않는다"
    );
    assert_eq!(out["args"]["k"], 1);
    server.shutdown();
}

#[test]
fn sessions_get_distinct_ids() {
    let server = start(base_opts());
    let u = url(&server);
    let a = initialize(&u, &[]).session.expect("a");
    let b = initialize(&u, &[]).session.expect("b");
    assert_ne!(a, b);
    server.shutdown();
}

#[test]
fn notification_and_response_get_202_without_body() {
    let server = start(base_opts());
    let u = url(&server);
    let sid = initialize(&u, &[]).session.expect("sid");
    let h = [(SESSION_HEADER, sid.as_str())];

    let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
    let r = send("POST", &u, &h, Some(&note));
    assert_eq!((r.status, r.body.as_str()), (202, ""));

    let resp = json!({"jsonrpc":"2.0","id":9,"result":{}}).to_string();
    let r = send("POST", &u, &h, Some(&resp));
    assert_eq!((r.status, r.body.as_str()), (202, ""));
    server.shutdown();
}

#[test]
fn ping_before_and_after_work_with_session() {
    let server = start(base_opts());
    let u = url(&server);
    let sid = initialize(&u, &[]).session.expect("sid");
    let r = send(
        "POST",
        &u,
        &[(SESSION_HEADER, sid.as_str())],
        Some(&rpc(5, "ping", json!({}))),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["result"], json!({}));
    server.shutdown();
}

// ---------- 세션 수명 ----------

#[test]
fn unknown_session_is_404() {
    let server = start(base_opts());
    let r = send(
        "POST",
        &url(&server),
        &[(SESSION_HEADER, "no-such-session")],
        Some(&rpc(2, "tools/list", json!({}))),
    );
    assert_eq!(r.status, 404, "{}", r.body);
    server.shutdown();
}

#[test]
fn missing_session_header_after_initialize_is_400() {
    let server = start(base_opts());
    let r = send(
        "POST",
        &url(&server),
        &[],
        Some(&rpc(2, "tools/list", json!({}))),
    );
    assert_eq!(r.status, 400, "{}", r.body);
    server.shutdown();
}

#[test]
fn delete_ends_session_and_then_404() {
    let ended: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&ended);
    let opts =
        base_opts().on_session_end(move |id| sink.lock().expect("lock").push(id.to_string()));
    let server = start(opts);
    let u = url(&server);
    let sid = initialize(&u, &[]).session.expect("sid");
    let h = [(SESSION_HEADER, sid.as_str())];

    let del = send("DELETE", &u, &h, None);
    assert_eq!(del.status, 204, "{}", del.body);
    assert_eq!(
        *ended.lock().expect("lock"),
        vec![sid.clone()],
        "종료 훅은 정확히 한 번, 그 세션 ID로"
    );

    let after = send("POST", &u, &h, Some(&rpc(2, "tools/list", json!({}))));
    assert_eq!(after.status, 404);
    let again = send("DELETE", &u, &h, None);
    assert_eq!(again.status, 404);
    assert_eq!(
        ended.lock().expect("lock").len(),
        1,
        "두 번째 DELETE는 훅을 다시 부르지 않는다"
    );
    server.shutdown();
}

#[test]
fn delete_without_or_unknown_session() {
    let server = start(base_opts());
    let u = url(&server);
    assert_eq!(send("DELETE", &u, &[], None).status, 400);
    assert_eq!(
        send("DELETE", &u, &[(SESSION_HEADER, "ghost")], None).status,
        404
    );
    server.shutdown();
}

#[test]
fn two_sessions_in_flight_each_get_their_own_session_id() {
    let barrier = Arc::new(Barrier::new(2));
    let server = HttpServer::start(base_opts(), make_handler(barrier)).expect("기동");
    let u = url(&server);
    let sids = [
        initialize(&u, &[]).session.expect("s1"),
        initialize(&u, &[]).session.expect("s2"),
    ];
    let threads: Vec<_> = sids
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, sid)| {
            let u = u.clone();
            std::thread::spawn(move || {
                let ident = format!("user-{i}");
                let r = send(
                    "POST",
                    &u,
                    &[(SESSION_HEADER, sid.as_str()), ("X-Ident", ident.as_str())],
                    Some(&rpc(
                        7,
                        "tools/call",
                        json!({"name":"rendezvous","arguments":{}}),
                    )),
                );
                (sid, ident, r)
            })
        })
        .collect();
    for t in threads {
        let (sid, ident, r) = t.join().expect("스레드");
        assert_eq!(r.status, 200, "{}", r.body);
        let out = tool_text(&r);
        assert_eq!(
            out["session"],
            sid.as_str(),
            "다른 세션의 ID가 섞이면 안 된다"
        );
        assert_eq!(out["ident"], ident.as_str());
    }
    server.shutdown();
}

// ---------- 규약: 메서드·버전·본문 ----------

#[test]
fn get_is_405_with_allow() {
    let server = start(base_opts());
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    let res = agent
        .get(&url(&server))
        .set("Accept", "text/event-stream")
        .call();
    match res {
        Err(ureq::Error::Status(405, r)) => {
            let allow = r.header("allow").unwrap_or("").to_string();
            assert!(
                allow.contains("POST") && allow.contains("DELETE"),
                "Allow={allow:?}"
            );
        }
        other => panic!("GET은 405여야 함: {other:?}"),
    }
    server.shutdown();
}

#[test]
fn unsupported_protocol_version_is_400_and_known_versions_pass() {
    let server = start(base_opts());
    let u = url(&server);
    let sid = initialize(&u, &[]).session.expect("sid");
    let call = |v: &str| {
        send(
            "POST",
            &u,
            &[(SESSION_HEADER, sid.as_str()), ("MCP-Protocol-Version", v)],
            Some(&rpc(2, "ping", json!({}))),
        )
        .status
    };
    assert_eq!(call("1999-01-01"), 400);
    for ok in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
        assert_eq!(call(ok), 200, "{ok}");
    }
    // 헤더가 아예 없으면 하위 호환 규정에 따라 받아들인다.
    let none = send(
        "POST",
        &u,
        &[(SESSION_HEADER, sid.as_str())],
        Some(&rpc(3, "ping", json!({}))),
    );
    assert_eq!(none.status, 200);
    server.shutdown();
}

#[test]
fn malformed_body_is_400_with_jsonrpc_error() {
    let server = start(base_opts());
    let u = url(&server);
    let bad = send("POST", &u, &[], Some("{not json"));
    assert_eq!(bad.status, 400);
    assert_eq!(bad.json()["error"]["code"], -32700);

    let batch = send(
        "POST",
        &u,
        &[],
        Some("[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]"),
    );
    assert_eq!(
        batch.status, 400,
        "배치는 받지 않는다(2025-06-18 이후 규약)"
    );
    assert_eq!(batch.json()["error"]["code"], -32600);

    let scalar = send("POST", &u, &[], Some("42"));
    assert_eq!(scalar.status, 400);
    server.shutdown();
}

#[test]
fn oversized_body_is_rejected() {
    let server = start(base_opts());
    let u = url(&server);
    let big = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"pad\":\"{}\"}}",
        "x".repeat(mcp_kit::http::MAX_BODY_BYTES + 1)
    );
    let r = send("POST", &u, &[], Some(&big));
    assert_eq!(r.status, 413, "{}", r.body);
    server.shutdown();
}

#[test]
fn unknown_path_is_404() {
    let server = start(base_opts());
    let r = send(
        "POST",
        &format!("http://{}/other", server.local_addr()),
        &[],
        Some("{}"),
    );
    assert_eq!(r.status, 404);
    server.shutdown();
}

/// 없는 경로로 본문이 큰 요청을 거듭 보내도 응답 상태 줄을 항상 읽을 수 있어야 한다.
/// 서버가 본문을 읽지 않은 채 연결을 닫으면 Windows에서 RST가 나가 클라이언트가 10053을 본다.
#[test]
fn unknown_path_with_large_body_is_always_404() {
    const ROUNDS: usize = 50;
    const BODY_BYTES: usize = 512 * 1024;
    let server = start(base_opts());
    let u = format!("http://{}/other", server.local_addr());
    let body = format!("{{\"pad\":\"{}\"}}", "x".repeat(BODY_BYTES));
    for round in 0..ROUNDS {
        let r = send("POST", &u, &[], Some(&body));
        assert_eq!(r.status, 404, "round {round}: {}", r.body);
    }
    server.shutdown();
}

/// 대조군: 엔드포인트 경로는 본문을 읽으므로 같은 큰 본문에도 항상 응답을 받는다(파싱 실패 400).
#[test]
fn large_body_to_known_path_is_always_answered() {
    const ROUNDS: usize = 50;
    const BODY_BYTES: usize = 512 * 1024;
    let server = start(base_opts());
    let u = url(&server);
    let body = format!("not json {}", "x".repeat(BODY_BYTES));
    for round in 0..ROUNDS {
        let r = send("POST", &u, &[], Some(&body));
        assert_eq!(r.status, 400, "round {round}: {}", r.body);
    }
    server.shutdown();
}

#[test]
fn error_response_from_initialize_does_not_create_session() {
    // 처리 함수가 initialize에 오류 응답을 내면 세션이 만들어지지 않아야 한다.
    let server = HttpServer::start(base_opts(), |msg: &Value, _ctx: &RequestContext| {
        Some(json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32000,"message":"no"}}))
    })
    .expect("기동");
    let r = initialize(&url(&server), &[]);
    assert_eq!(r.status, 200);
    assert!(r.session.is_none(), "오류 응답에는 세션 ID를 붙이지 않는다");
    server.shutdown();
}

#[test]
fn handler_panic_is_500_without_internal_detail_and_server_survives() {
    let server = HttpServer::start(base_opts(), |msg: &Value, _ctx: &RequestContext| {
        assert!(msg["method"] != "boom", "SECRET-INTERNAL-DETAIL");
        Some(json!({"jsonrpc":"2.0","id":msg["id"],"result":{}}))
    })
    .expect("기동");
    let u = url(&server);
    let sid = initialize(&u, &[]).session.expect("sid");
    let r = send(
        "POST",
        &u,
        &[(SESSION_HEADER, sid.as_str())],
        Some(&rpc(1, "boom", json!({}))),
    );
    assert_eq!(r.status, 500);
    assert!(
        !r.body.contains("SECRET-INTERNAL-DETAIL"),
        "5xx에 내부 상세 노출: {}",
        r.body
    );
    let ok = send(
        "POST",
        &u,
        &[(SESSION_HEADER, sid.as_str())],
        Some(&rpc(2, "ping", json!({}))),
    );
    assert_eq!(ok.status, 200, "패닉 뒤에도 서버와 세션이 산다");
    server.shutdown();
}

// ---------- 보안 ----------

#[test]
fn token_required_when_configured() {
    let server = start(base_opts().token("s3cret"));
    let u = url(&server);
    let body = rpc(1, "initialize", json!({}));

    assert_eq!(send("POST", &u, &[], Some(&body)).status, 401, "헤더 없음");
    assert_eq!(
        send(
            "POST",
            &u,
            &[("Authorization", "Bearer wrong")],
            Some(&body)
        )
        .status,
        401
    );
    assert_eq!(
        send(
            "POST",
            &u,
            &[("Authorization", "Bearer s3cret-longer")],
            Some(&body)
        )
        .status,
        401
    );
    assert_eq!(
        send(
            "POST",
            &u,
            &[("Authorization", "Bearer s3cre")],
            Some(&body)
        )
        .status,
        401
    );
    assert_eq!(
        send(
            "POST",
            &u,
            &[("Authorization", "Basic s3cret")],
            Some(&body)
        )
        .status,
        401
    );
    let ok = send(
        "POST",
        &u,
        &[("Authorization", "Bearer s3cret")],
        Some(&body),
    );
    assert_eq!(ok.status, 200, "{}", ok.body);
    // 토큰은 DELETE·GET에도 적용된다.
    assert_eq!(send("DELETE", &u, &[], None).status, 401);
    server.shutdown();
}

#[test]
fn no_token_means_no_auth_check() {
    let server = start(base_opts());
    let r = initialize(&url(&server), &[]);
    assert_eq!(r.status, 200);
    server.shutdown();
}

#[test]
fn non_loopback_bind_is_refused() {
    for addr in ["0.0.0.0:0", "[::]:0", "192.0.2.1:0"] {
        let a: SocketAddr = addr.parse().expect("주소");
        let err = HttpServer::start(HttpOptions::new(a), |_m: &Value, _c: &RequestContext| None)
            .err()
            .unwrap_or_else(|| panic!("{addr} 바인드가 거부되어야 함"));
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{addr}: {err}"
        );
    }
}

#[test]
fn non_loopback_origin_is_403_and_loopback_origin_passes() {
    let server = start(base_opts());
    let u = url(&server);
    let body = rpc(1, "initialize", json!({}));
    for bad in [
        "http://evil.example",
        "https://127.0.0.1.evil.example",
        "null",
        "http://localhost.evil.example:3000",
        "garbage",
    ] {
        let r = send("POST", &u, &[("Origin", bad)], Some(&body));
        assert_eq!(r.status, 403, "Origin={bad}");
    }
    for good in [
        "http://127.0.0.1:3000",
        "http://localhost",
        "https://localhost:8443",
        "http://[::1]:9000",
    ] {
        let r = send("POST", &u, &[("Origin", good)], Some(&body));
        assert_eq!(r.status, 200, "Origin={good}: {}", r.body);
    }
    // Origin 검사는 다른 메서드에도 적용된다.
    assert_eq!(
        send("DELETE", &u, &[("Origin", "http://evil.example")], None).status,
        403
    );
    server.shutdown();
}

#[test]
fn origin_is_checked_before_token() {
    let server = start(base_opts().token("t"));
    let r = send(
        "POST",
        &url(&server),
        &[("Origin", "http://evil.example")],
        Some("{}"),
    );
    assert_eq!(r.status, 403);
    server.shutdown();
}

#[test]
fn shutdown_stops_accepting() {
    let server = start(base_opts());
    let addr = server.local_addr();
    server.shutdown();
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    assert!(agent
        .post(&format!("http://{addr}/mcp"))
        .send_string("{}")
        .is_err());
}

// ---------- 인증 검증기 ----------

use mcp_kit::http::Unauthorized;

/// `Authorization: Bearer <이름>-tok` 이면 그 이름을 주체로 인정하는 검증기. 나머지는 거부.
fn named_tokens(
) -> impl Fn(&mcp_kit::http::RequestHeaders<'_>) -> Result<Option<String>, Unauthorized>
       + Send
       + Sync
       + 'static {
    |headers| {
        let token = headers.bearer().ok_or(Unauthorized)?;
        token
            .strip_suffix("-tok")
            .map(|name| Some(name.to_string()))
            .ok_or(Unauthorized)
    }
}

fn echo_principal(u: &str, bearer: &str) -> Value {
    let auth = format!("Bearer {bearer}");
    let init = send(
        "POST",
        u,
        &[("Authorization", auth.as_str())],
        Some(&rpc(
            1,
            "initialize",
            json!({"protocolVersion":"2025-06-18"}),
        )),
    );
    assert_eq!(init.status, 200, "{}", init.body);
    let sid = init.session.clone().expect("세션 ID");
    let call = send(
        "POST",
        u,
        &[
            ("Authorization", auth.as_str()),
            (SESSION_HEADER, sid.as_str()),
        ],
        Some(&rpc(2, "tools/call", json!({"name":"echo","arguments":{}}))),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    tool_text(&call)["principal"].clone()
}

#[test]
fn authenticator_principal_reaches_handler_exactly() {
    let server = start(base_opts().authenticator(named_tokens()));
    let u = url(&server);
    assert_eq!(echo_principal(&u, "alice-tok"), json!("alice"));
    assert_eq!(echo_principal(&u, "bob-tok"), json!("bob"));
    server.shutdown();
}

#[test]
fn authenticator_ok_none_passes_without_principal() {
    let server = start(base_opts().authenticator(|_h| Ok(None)));
    let init = initialize(&url(&server), &[]);
    assert_eq!(init.status, 200, "{}", init.body);
    server.shutdown();
}

#[test]
fn no_authenticator_means_no_principal() {
    let server = start(base_opts());
    assert_eq!(echo_principal(&url(&server), "alice-tok"), Value::Null);
    server.shutdown();
}

#[test]
fn authenticator_rejection_is_401_on_every_method() {
    let server = start(base_opts().authenticator(named_tokens()));
    let u = url(&server);
    let body = rpc(1, "initialize", json!({}));
    for headers in [
        vec![],
        vec![("Authorization", "Bearer nope")],
        vec![("Authorization", "Basic alice-tok")],
        vec![("Authorization", "Bearer")],
    ] {
        let r = send("POST", &u, &headers, Some(&body));
        assert_eq!(r.status, 401, "{headers:?}: {}", r.body);
    }
    assert_eq!(send("DELETE", &u, &[], None).status, 401);
    assert_eq!(send("GET", &u, &[], None).status, 401);
    // 거부 응답은 Bearer 도전을 싣고, 본문은 단일 토큰일 때와 같은 JSON-RPC 오류다.
    let agent = ureq::AgentBuilder::new().timeout(CLIENT_TIMEOUT).build();
    match agent.post(&u).send_string(&body) {
        Err(ureq::Error::Status(401, r)) => {
            assert_eq!(r.header("www-authenticate"), Some("Bearer"));
            let v: Value = serde_json::from_str(&r.into_string().expect("본문")).expect("JSON");
            assert_eq!(v["error"]["message"], "unauthorized");
        }
        other => panic!("401 이어야 함: {:?}", other.map(|r| r.status())),
    }
    server.shutdown();
}

#[test]
fn origin_is_checked_before_authenticator() {
    let server = start(base_opts().authenticator(|_h| Err(Unauthorized)));
    let r = send(
        "POST",
        &url(&server),
        &[("Origin", "http://evil.example")],
        Some("{}"),
    );
    assert_eq!(r.status, 403);
    server.shutdown();
}

#[test]
fn token_and_authenticator_together_are_refused_at_start() {
    let err = HttpServer::start(
        base_opts().token("t").authenticator(|_h| Ok(None)),
        |_m: &Value, _c: &RequestContext| None,
    )
    .err()
    .expect("둘을 함께 주면 거부되어야 함");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
}
