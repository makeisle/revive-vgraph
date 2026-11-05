//! MCP Streamable HTTP 전송 (기능 플래그 `http`).
//!
//! 규약 근거: modelcontextprotocol 사양 `basic/transports`(2025-11-25, 직전판 2025-06-18),
//! 절 이름은 각 항목 주석에 적었다. 이 모듈은 프로토콜 메시지를 해석하지 않고
//! 호출자가 넘긴 처리 함수에 그대로 건넨다 — 사용자·작업 공간 같은 응용 개념은 모른다.
//! 호출자가 지정한 요청 헤더 값(`HttpOptions::forward_header`)과 세션 ID를
//! `RequestContext`로 전달할 뿐이며, 해석은 호출자 몫이다.
//!
//! 지원 범위: POST 단건 → `application/json` 단건 응답(SSE 스트림은 열지 않는다),
//! GET은 405(서버 개시 스트림 없음), DELETE로 세션 종료.
//!
//! 바인드 안전 규칙(`evaluate_bind`): 루프백은 그대로 열고, 같은 PC 밖 주소는 TLS(기능 `tls`)와
//! 인증 검증기가 둘 다 있어야 열며, `behind_proxy`면 사설 주소에서만 평문을 허용한다.
//! 같은 PC 밖 주소(와, `behind_proxy`인 루프백에 `allow_origin`이 주어진 경우)에서는 `Origin` 검사가
//! 루프백 규칙 대신 `allow_origin` 목록을 쓴다.
//!
//! 동기 호출자용이다. `HttpServer`는 자체 tokio 런타임을 들고 있으므로
//! 이미 tokio 런타임 안에서 만들거나 `shutdown`하지 않는다.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// 기본 엔드포인트 경로.
pub const DEFAULT_PATH: &str = "/mcp";
/// 요청 본문 상한(바이트). 넘으면 413.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// 동시에 열어 둘 수 있는 세션 수 상한. 넘으면 initialize가 503.
/// 세션은 DELETE로만 닫히므로(만료 없음) 끊긴 클라이언트가 쌓여도 메모리가 무한히 자라지 않게 막는다.
pub const MAX_SESSIONS: usize = 4096;
/// 종료 시 진행 중 요청을 기다려 주는 시간.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// 세션 ID 난수 바이트 수(128비트).
const SESSION_ID_BYTES: usize = 16;
/// 서버가 아는 프로토콜 버전 (사양 "Protocol Version Header").
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

type Handler = dyn Fn(&Value, &RequestContext) -> Option<Value> + Send + Sync + 'static;
type SessionEndHook = dyn Fn(&str) + Send + Sync + 'static;
type Authenticator =
    dyn Fn(&RequestHeaders<'_>) -> Result<Option<String>, Unauthorized> + Send + Sync + 'static;

/// 처리 함수에 함께 넘기는 요청 문맥.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// 이 요청이 속한 세션 ID. initialize 요청에서는 이번에 발급될 ID가 들어 있다
    /// (처리 함수가 오류 응답을 내면 그 ID는 발급되지 않고 버려진다).
    pub session_id: Option<String>,
    /// `HttpOptions::forward_header`로 지정한 헤더 중 요청에 실려 온 것. 이름은 소문자.
    pub headers: Vec<(String, String)>,
    /// `HttpOptions::authenticator`가 인정한 주체. 검증기를 쓰지 않았거나 검증기가
    /// `Ok(None)`을 돌려줬으면 `None`. 주체가 무엇을 뜻하는지는 호출자만 안다.
    pub principal: Option<String>,
}

/// 인증 검증기가 보는 요청 헤더(읽기 전용). HTTP 라이브러리 타입을 공개 API에 드러내지 않으려고 감쌌다.
pub struct RequestHeaders<'a>(&'a HeaderMap);

impl<'a> RequestHeaders<'a> {
    /// 헤더 값(이름은 대소문자 무관). 없거나 ASCII 문자열이 아니면 `None`.
    pub fn get(&self, name: &str) -> Option<&'a str> {
        self.0.get(name)?.to_str().ok()
    }

    /// `Authorization: Bearer <값>`의 값. 스킴이 다르거나 값이 비어 있으면 `None`.
    pub fn bearer(&self) -> Option<&'a str> {
        let (scheme, value) = self.get(header::AUTHORIZATION.as_str())?.split_once(' ')?;
        (scheme.eq_ignore_ascii_case("bearer") && !value.is_empty()).then_some(value)
    }
}

/// 인증 검증기가 요청을 거부할 때 돌려주는 값 (HTTP 401로 응답한다). 사유는 응답에 싣지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unauthorized;

impl RequestContext {
    /// 지정해 둔 헤더 값을 이름(대소문자 무관)으로 찾는다.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 서버 설정. 토큰이 들어 있으므로 `Debug`를 파생하지 않는다.
pub struct HttpOptions {
    addr: SocketAddr,
    path: String,
    token: Option<String>,
    authenticator: Option<Arc<Authenticator>>,
    forward_headers: Vec<String>,
    on_session_end: Option<Arc<SessionEndHook>>,
    behind_proxy: bool,
    allow_origins: Vec<String>,
    #[cfg(feature = "tls")]
    tls: Option<TlsConfig>,
}

impl HttpOptions {
    /// `addr`이 루프백이 아니면 `HttpServer::start`가 `evaluate_bind` 규칙으로 판정한다
    /// (TLS와 인증 검증기가 없으면 거부).
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            path: DEFAULT_PATH.to_string(),
            token: None,
            authenticator: None,
            forward_headers: Vec::new(),
            on_session_end: None,
            behind_proxy: false,
            allow_origins: Vec::new(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// 엔드포인트 경로(기본 `/mcp`). 앞에 `/`가 없으면 붙인다.
    pub fn path(mut self, path: &str) -> Self {
        self.path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        self
    }

    /// 설정하면 `Authorization: Bearer <토큰>`이 맞는 요청만 받는다(아니면 401).
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// 요청마다 불리는 인증 검증기. `Ok(주체)`면 받아들이고(주체는 `RequestContext::principal`로
    /// 처리 함수에 전달), `Err(Unauthorized)`면 401로 거부한다. Origin 검사 뒤에 불린다.
    /// 검증기는 파일을 읽을 수도 있으므로 블로킹 풀에서 불린다.
    /// `token`과는 함께 쓸 수 없다(`HttpServer::start`가 `InvalidInput`으로 거부한다) —
    /// 어느 쪽이 이기는지 모호한 설정을 허용하지 않는다.
    pub fn authenticator(
        mut self,
        check: impl Fn(&RequestHeaders<'_>) -> Result<Option<String>, Unauthorized>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.authenticator = Some(Arc::new(check));
        self
    }

    /// 리버스 프록시 뒤에서 도는 서버임을 알린다. 사설 주소(`is_private_ip`)·루프백에서만 평문으로 열 수 있고
    /// 인증 검증기가 반드시 있어야 한다(`evaluate_bind`). TLS는 프록시가 맡는다고 본다.
    pub fn behind_proxy(mut self, yes: bool) -> Self {
        self.behind_proxy = yes;
        self
    }

    /// 같은 PC 밖 주소에서 `Origin` 헤더가 실려 온 요청을 받아 줄 출처(`https://app.example:8443` 꼴, 여러 번
    /// 호출 가능). 정확히 같은 값(대소문자 무관)만 통과한다. 루프백 서버에는 `behind_proxy`와 함께일 때만
    /// 쓸 수 있다(아니면 `HttpServer::start`가 `InvalidInput`으로 거부한다 — 루프백 서버는 루프백 출처만 받는다).
    pub fn allow_origin(mut self, origin: &str) -> Self {
        self.allow_origins.push(origin.to_string());
        self
    }

    /// TLS로 서비스한다(HTTPS). 인증서·키는 `TlsConfig::from_pem_files`로 미리 읽어 둔다.
    #[cfg(feature = "tls")]
    pub fn tls(mut self, config: TlsConfig) -> Self {
        self.tls = Some(config);
        self
    }

    fn has_tls(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            self.tls.is_some()
        }
        #[cfg(not(feature = "tls"))]
        {
            false
        }
    }

    /// 처리 함수에 넘길 요청 헤더 이름을 더한다(여러 번 호출 가능).
    pub fn forward_header(mut self, name: &str) -> Self {
        self.forward_headers.push(name.to_ascii_lowercase());
        self
    }

    /// 클라이언트가 DELETE로 세션을 끝냈을 때 한 번 불리는 훅(인자: 세션 ID).
    /// 훅은 요청을 처리하는 비동기 작업 안에서 불리므로 오래 막지 않는다.
    pub fn on_session_end(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_session_end = Some(Arc::new(hook));
        self
    }
}

struct Shared {
    handler: Arc<Handler>,
    token: Option<String>,
    authenticator: Option<Arc<Authenticator>>,
    forward_headers: Vec<String>,
    on_session_end: Option<Arc<SessionEndHook>>,
    origin_policy: OriginPolicy,
    sessions: Mutex<HashSet<String>>,
}

fn build_router(shared: Arc<Shared>, path: &str) -> Router {
    Router::new()
        .route(path, any(endpoint))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(shared)
}

impl Shared {
    // 가드는 이 파일의 동기 함수 안에서만 쓰고 .await 너머로 넘기지 않는다.
    // 중간에 패닉해도 HashSet 자체는 온전하므로 중독된 잠금도 그대로 쓴다.
    fn sessions(&self) -> MutexGuard<'_, HashSet<String>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 떠 있는 HTTP 서버. `shutdown`(또는 drop)으로 멈춘다.
pub struct HttpServer {
    addr: SocketAddr,
    stop: Arc<Notify>,
    runtime: Option<Runtime>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl HttpServer {
    /// 서버를 띄운다. 바인드 주소가 `evaluate_bind` 규칙을 통과하지 못하면 바인드하기 전에
    /// `InvalidInput`으로 거부한다(사양 "Security Warning": 로컬 실행 시 localhost에만 바인드,
    /// 밖으로 열려면 TLS·인증).
    pub fn start<H>(opts: HttpOptions, handler: H) -> io::Result<Self>
    where
        H: Fn(&Value, &RequestContext) -> Option<Value> + Send + Sync + 'static,
    {
        let kind = evaluate_bind(
            opts.addr.ip(),
            BindPolicy {
                tls: opts.has_tls(),
                authenticated: opts.authenticator.is_some(),
                behind_proxy: opts.behind_proxy,
            },
        )
        .map_err(|refusal| io::Error::new(io::ErrorKind::InvalidInput, refusal.to_string()))?;
        if opts.token.is_some() && opts.authenticator.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "token과 authenticator는 함께 쓸 수 없습니다",
            ));
        }
        // 프록시 뒤의 루프백 서버는 허용 출처가 주어졌을 때만 허용 목록 규칙을 쓴다
        // (프록시의 공개 출처가 Origin으로 오기 때문). 목록이 없으면 루프백 규칙 그대로.
        let origin_kind =
            if kind == BindKind::Loopback && opts.behind_proxy && !opts.allow_origins.is_empty() {
                BindKind::Remote
            } else {
                kind
            };
        let origin_policy = OriginPolicy::new(origin_kind, &opts.allow_origins)?;
        let std_listener = std::net::TcpListener::bind(opts.addr)?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;

        let runtime = Builder::new_multi_thread().enable_all().build()?;
        let shared = Arc::new(Shared {
            handler: Arc::new(handler),
            token: opts.token,
            authenticator: opts.authenticator,
            forward_headers: opts.forward_headers,
            on_session_end: opts.on_session_end,
            origin_policy,
            sessions: Mutex::new(HashSet::new()),
        });
        let app = build_router(shared, &opts.path);

        let stop = Arc::new(Notify::new());
        let stop_for_task = Arc::clone(&stop);
        #[cfg(feature = "tls")]
        let tls = opts.tls;
        let task = runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener)?;
            #[cfg(feature = "tls")]
            if let Some(tls) = tls {
                tls::serve(
                    listener,
                    app,
                    &tls,
                    stop_for_task,
                    tls::TlsLimits::default(),
                )
                .await;
                return Ok(());
            }
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop_for_task.notified().await })
                .await
        });
        Ok(Self {
            addr,
            stop,
            runtime: Some(runtime),
            task: Some(task),
        })
    }

    /// 실제로 바인드된 주소(포트 0을 줬다면 배정된 포트).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// 진행 중 요청을 마저 처리한 뒤 멈춘다.
    pub fn shutdown(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        self.stop.notify_one();
        if let Some(task) = self.task.take() {
            match runtime.block_on(async { tokio::time::timeout(DRAIN_TIMEOUT, task).await }) {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => eprintln!("mcp-kit http: 서버 종료 중 오류: {e}"),
                Ok(Err(e)) => eprintln!("mcp-kit http: 서버 작업이 비정상 종료: {e}"),
                Err(_) => eprintln!("mcp-kit http: 진행 중 요청을 기다리다 시간 초과, 강제 종료"),
            }
        }
        runtime.shutdown_timeout(DRAIN_TIMEOUT);
    }

    /// 서버가 끝날 때까지(Ctrl-C·SIGTERM 포함) 기다린다.
    fn wait_for_signal(mut self) -> io::Result<()> {
        if let Some(runtime) = self.runtime.as_ref() {
            let stop = Arc::clone(&self.stop);
            runtime.spawn(async move {
                wait_termination_signal().await;
                stop.notify_one();
            });
        }
        let result = match (self.runtime.as_ref(), self.task.take()) {
            (Some(rt), Some(task)) => rt
                .block_on(task)
                .map_err(|e| io::Error::other(e.to_string()))
                .and_then(|r| r),
            _ => Ok(()),
        };
        self.stop_now();
        result
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// 서버를 띄우고 종료 신호(Ctrl-C, unix에서는 SIGTERM)가 올 때까지 막는다.
pub fn serve_http<H>(opts: HttpOptions, handler: H) -> io::Result<()>
where
    H: Fn(&Value, &RequestContext) -> Option<Value> + Send + Sync + 'static,
{
    HttpServer::start(opts, handler)?.wait_for_signal()
}

async fn wait_termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        // 신호 핸들러를 못 걸면 곧바로 끝내지 않고 영원히 기다린다(서버는 계속 뜬다).
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

// ---------------------------------------------------------------------------
// 요청 처리
// ---------------------------------------------------------------------------

/// 엔드포인트가 아닌 경로: 404. 요청 본문을 상한(`MAX_BODY_BYTES`) 안에서 끝까지 받은 뒤 답한다.
/// 본문을 읽지 않고 답하면 응답 뒤 연결을 닫을 때 안 읽은 수신 데이터 때문에 RST가 나가
/// (Windows) 클라이언트가 상태 줄을 읽기 전에 연결 중단 오류를 볼 수 있다.
/// 검사 수준은 이전과 같다 — 없는 경로는 Origin·토큰을 보지 않고 늘 404다.
async fn not_found(_body: Bytes) -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn endpoint(
    State(shared): State<Arc<Shared>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 사양 "Security Warning" 1항: Origin이 있고 유효하지 않으면 403 (모든 연결에 적용).
    // 유효 기준은 서버가 열린 범위에 따른다: 루프백 서버는 루프백 출처, 밖으로 열린 서버는 허용 목록.
    if let Some(origin) = headers.get(header::ORIGIN) {
        if !origin
            .to_str()
            .is_ok_and(|o| shared.origin_policy.allows(o))
        {
            return rpc_error(StatusCode::FORBIDDEN, -32600, "forbidden origin");
        }
    }
    // 사양 "Security Warning" 3항(SHOULD authentication): 선택 토큰.
    if let Some(expected) = shared.token.as_deref() {
        if !bearer_matches(&headers, expected) {
            return unauthorized();
        }
    }
    let principal = match authenticate(&shared, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // 사양 "Protocol Version Header": 지원하지 않는 값이면 400, 헤더가 없으면 받아들인다(2025-03-26 가정).
    if let Some(v) = headers.get(PROTOCOL_HEADER) {
        if !v
            .to_str()
            .is_ok_and(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(&v))
        {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                -32600,
                "unsupported MCP-Protocol-Version",
            );
        }
    }
    match method {
        Method::POST => post(shared, headers, body, principal).await,
        Method::DELETE => delete(&shared, &headers),
        // 사양 "Listening for Messages from the Server": GET을 지원하지 않으면 405.
        _ => {
            let mut resp = StatusCode::METHOD_NOT_ALLOWED.into_response();
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            resp
        }
    }
}

fn unauthorized() -> Response {
    let mut resp = rpc_error(StatusCode::UNAUTHORIZED, -32600, "unauthorized");
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

/// 인증 검증기가 있으면 부른다. 거부(또는 검증기 패닉)는 곧바로 보낼 응답으로 돌려준다.
async fn authenticate(shared: &Shared, headers: &HeaderMap) -> Result<Option<String>, Response> {
    let Some(check) = shared.authenticator.as_ref().map(Arc::clone) else {
        return Ok(None);
    };
    let headers = headers.clone(); // 블로킹 작업으로 넘기려면 소유가 필요하다(요청 헤더는 작다)
    match tokio::task::spawn_blocking(move || check(&RequestHeaders(&headers))).await {
        Ok(Ok(principal)) => Ok(principal),
        Ok(Err(Unauthorized)) => Err(unauthorized()),
        Err(_join_error) => Err(rpc_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            -32603,
            "internal error",
        )),
    }
}

async fn post(
    shared: Arc<Shared>,
    headers: HeaderMap,
    body: Bytes,
    principal: Option<String>,
) -> Response {
    // 사양 "Sending Messages to the Server" 3항: 본문은 단건 JSON-RPC 메시지(배치 아님).
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(StatusCode::BAD_REQUEST, -32700, "parse error"),
    };
    if !msg.is_object() {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            -32600,
            "expected a single JSON-RPC message object",
        );
    }
    let method = msg.get("method").and_then(Value::as_str);
    let is_request = method.is_some() && msg.get("id").is_some();

    // 응답(클라이언트가 서버 요청에 답한 것)은 받아 두기만 한다 — 4항: 202, 본문 없음.
    let Some(method) = method else {
        return StatusCode::ACCEPTED.into_response();
    };

    let is_initialize = is_request && method == "initialize";
    let session_id = if is_initialize {
        // 사양 "Session Management" 1항: initialize 응답에 세션 ID를 싣는다.
        if shared.sessions().len() >= MAX_SESSIONS {
            return rpc_error(StatusCode::SERVICE_UNAVAILABLE, -32000, "too many sessions");
        }
        match new_session_id() {
            Ok(id) => id,
            Err(_) => {
                return rpc_error(StatusCode::INTERNAL_SERVER_ERROR, -32603, "internal error")
            }
        }
    } else {
        // 2항: initialize 외 요청에 세션 헤더가 없으면 400, 3항: 모르는 세션은 404.
        let Some(id) = session_header(&headers) else {
            return rpc_error(StatusCode::BAD_REQUEST, -32600, "missing Mcp-Session-Id");
        };
        if !shared.sessions().contains(id) {
            return rpc_error(StatusCode::NOT_FOUND, -32001, "unknown session");
        }
        id.to_string()
    };

    let ctx = RequestContext {
        session_id: Some(session_id.clone()),
        headers: forwarded_headers(&shared.forward_headers, &headers),
        principal,
    };
    let handler = Arc::clone(&shared.handler);
    // 처리 함수는 동기(엔진 호출 등 블로킹 가능)이므로 블로킹 풀에서 부른다.
    let outcome = tokio::task::spawn_blocking(move || handler(&msg, &ctx)).await;
    let reply = match outcome {
        Ok(reply) => reply,
        Err(_join_error) => {
            // 패닉 상세는 응답에 싣지 않는다(패닉 훅이 이미 stderr에 남긴다).
            return rpc_error(StatusCode::INTERNAL_SERVER_ERROR, -32603, "internal error");
        }
    };

    // 4항: notification은 처리 결과와 무관하게 202. 요청인데 응답이 없으면 역시 202.
    let Some(reply) = reply.filter(|_| is_request) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let mut resp = json_response(StatusCode::OK, &reply);
    if is_initialize && reply.get("result").is_some() {
        shared.sessions().insert(session_id.clone());
        if let Ok(v) = HeaderValue::from_str(&session_id) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
    }
    resp
}

fn delete(shared: &Shared, headers: &HeaderMap) -> Response {
    // 사양 "Session Management" 5항: DELETE로 세션을 끝낸다.
    let Some(id) = session_header(headers) else {
        return rpc_error(StatusCode::BAD_REQUEST, -32600, "missing Mcp-Session-Id");
    };
    if !shared.sessions().remove(id) {
        return rpc_error(StatusCode::NOT_FOUND, -32001, "unknown session");
    }
    if let Some(hook) = &shared.on_session_end {
        hook(id);
    }
    StatusCode::NO_CONTENT.into_response()
}

fn session_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(SESSION_HEADER)?
        .to_str()
        .ok()
        .filter(|s| !s.is_empty())
}

fn forwarded_headers(names: &[String], headers: &HeaderMap) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|name| {
            let value = headers.get(name.as_str())?.to_str().ok()?;
            Some((name.clone(), value.to_string()))
        })
        .collect()
}

fn new_session_id() -> Result<String, getrandom::Error> {
    use std::fmt::Write;
    let mut bytes = [0u8; SESSION_ID_BYTES];
    getrandom::fill(&mut bytes)?;
    let mut id = String::with_capacity(SESSION_ID_BYTES * 2);
    for b in bytes {
        // String에 쓰는 write!는 실패하지 않는다.
        let _ = write!(id, "{b:02x}");
    }
    Ok(id)
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// id 없는 JSON-RPC 오류 본문 (사양: 오류 본문은 id 없이 줄 수 있다).
fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    json_response(
        status,
        &json!({"jsonrpc":"2.0","id":null,"error":{"code":code,"message":message}}),
    )
}

// ---------------------------------------------------------------------------
// 검사 함수
// ---------------------------------------------------------------------------

/// `Authorization: Bearer <토큰>` 일치 여부. 비교 시간이 일치한 길이에 좌우되지 않게 한다.
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, presented)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer")
        && constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

/// 길이가 달라도 두 입력 중 긴 쪽 전체를 훑는다(길이 차이는 결과에만 반영).
/// 토큰·해시처럼 비밀과 비교할 때 쓰는 공용 비교 함수다.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// `Origin`(`scheme://host[:port]`)을 호스트와 포트로 가른다. `http`·`https`가 아니거나 경로·사용자 정보가
/// 붙었거나 호스트가 비었거나 포트가 숫자가 아니면 `None`.
fn origin_authority(origin: &str) -> Option<(&str, Option<&str>)> {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    // Origin에는 경로·사용자 정보가 없다.
    if rest.is_empty() || rest.contains(['/', '@', '?', '#']) {
        return None;
    }
    let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        let (inner, tail) = bracketed.split_once(']')?;
        (inner, tail.strip_prefix(':'))
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        }
    };
    if host.is_empty()
        || port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some((host, port))
}

/// `Origin`이 루프백(`localhost`·`127.x.x.x`·`[::1]`)인지. `null`·그 밖의 값은 거부.
fn is_loopback_origin(origin: &str) -> bool {
    let Some((host, _port)) = origin_authority(origin) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `HttpOptions::allow_origin`에 줄 출처 목록의 형식을 서버를 띄우기 전에 미리 검사한다
/// (`https://app.example:8443` 꼴만 통과, 경로·와일드카드는 `InvalidInput`).
pub fn check_allow_origins(origins: &[String]) -> io::Result<()> {
    OriginPolicy::new(BindKind::Remote, origins).map(|_policy| ())
}

/// 어떤 `Origin`을 받아 줄지. 서버가 열린 범위(`BindKind`)로 정한다.
enum OriginPolicy {
    /// 루프백 서버: 루프백 출처만(`is_loopback_origin`).
    Loopback,
    /// 같은 PC 밖으로 열린 서버: 허용 목록에 정확히 있는 출처만(소문자로 맞춰 둔 값). 목록이 비면 모두 거부.
    Allowed(Vec<String>),
}

impl OriginPolicy {
    fn new(kind: BindKind, allowed: &[String]) -> io::Result<Self> {
        match kind {
            BindKind::Loopback if !allowed.is_empty() => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "allow_origin 은 같은 PC 밖으로 여는 서버이거나 behind_proxy 와 함께일 때만 쓸 수 있습니다 (루프백 서버는 루프백 출처만 받습니다)",
            )),
            BindKind::Loopback => Ok(Self::Loopback),
            BindKind::Remote => {
                let mut list = Vec::with_capacity(allowed.len());
                for entry in allowed {
                    // 끝의 `/`는 흔한 실수라 눈감아 준다. 그 밖의 경로·와일드카드는 거부한다.
                    let normalized = entry.trim_end_matches('/').to_ascii_lowercase();
                    if origin_authority(&normalized).is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "허용 출처 형식이 올바르지 않습니다: {entry:?} (예: https://app.example:8443)"
                            ),
                        ));
                    }
                    list.push(normalized);
                }
                Ok(Self::Allowed(list))
            }
        }
    }

    fn allows(&self, origin: &str) -> bool {
        match self {
            Self::Loopback => is_loopback_origin(origin),
            Self::Allowed(list) => list.iter().any(|a| a.eq_ignore_ascii_case(origin)),
        }
    }
}

// ---------------------------------------------------------------------------
// 바인드 정책
// ---------------------------------------------------------------------------

/// 바인드 정책 판정에 쓰는 서버 설정 요약.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindPolicy {
    /// TLS 인증서가 설정되어 있다.
    pub tls: bool,
    /// 인증 검증기가 설정되어 있다(공유 토큰 하나는 해당하지 않는다).
    pub authenticated: bool,
    /// 리버스 프록시 뒤에서 도는 서버다.
    pub behind_proxy: bool,
}

/// 바인드가 허용된 경우 서버가 열린 범위.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    /// 같은 PC 안에서만 접속된다.
    Loopback,
    /// 같은 PC 밖에서도 접속될 수 있다.
    Remote,
}

/// 바인드를 허용하지 않는 이유. `Display`가 사용자에게 보일 문구다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindRefusal {
    /// 같은 PC 밖 주소인데 TLS 또는 인증 검증기가 없다.
    RemoteNeedsTlsAndAuth {
        /// TLS 인증서가 없다.
        missing_tls: bool,
        /// 인증 검증기가 없다.
        missing_auth: bool,
    },
    /// 프록시 뒤 모드인데 인증 검증기가 없다.
    ProxyNeedsAuth,
    /// 프록시 뒤 모드인데 사설 주소도 루프백도 아니다.
    ProxyNeedsPrivateAddress {
        /// `0.0.0.0`·`::` 같은 전체 주소다.
        unspecified: bool,
    },
}

impl std::fmt::Display for BindRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::RemoteNeedsTlsAndAuth {
                missing_tls,
                missing_auth,
            } => {
                let missing = match (missing_tls, missing_auth) {
                    (true, true) => "TLS 인증서와 사용자별 토큰",
                    (true, false) => "TLS 인증서",
                    _ => "사용자별 토큰",
                };
                write!(
                    f,
                    "같은 PC 밖에서 접속할 수 있는 주소는 TLS와 사용자별 토큰이 모두 있어야 열 수 있습니다 (없는 것: {missing}). \
                     루프백(127.0.0.1)에만 열거나, 리버스 프록시 뒤라면 프록시 뒤 모드를 쓰세요"
                )
            }
            Self::ProxyNeedsAuth => f.write_str(
                "프록시 뒤 모드에서도 사용자별 토큰은 필요합니다 (프록시를 거치지 않고 들어오는 접속을 막을 수단이 토큰뿐입니다)",
            ),
            Self::ProxyNeedsPrivateAddress { unspecified } => {
                if unspecified {
                    f.write_str(
                        "프록시 뒤 모드는 0.0.0.0·:: 같은 전체 주소에 열 수 없습니다 (모든 네트워크 어댑터, 곧 공인 주소에도 열립니다). \
                         프록시가 접속하는 사설 주소(10.x·172.16~31.x·192.168.x) 하나를 지정하세요",
                    )
                } else {
                    f.write_str(
                        "프록시 뒤 모드는 사설 주소(10.x·172.16~31.x·192.168.x·IPv6 fc00::/7)나 루프백에서만 평문으로 열 수 있습니다. \
                         공인 주소로 평문을 열 수는 없습니다 — TLS를 직접 쓰세요",
                    )
                }
            }
        }
    }
}

impl std::error::Error for BindRefusal {}

/// 사설 주소인지: IPv4 `10/8`·`172.16/12`·`192.168/16`, IPv6 고유 지역 `fc00::/7`
/// (IPv4 매핑 IPv6는 IPv4로 보고 판정). 루프백·링크 로컬·전체 주소(`0.0.0.0`·`::`)는 사설이 아니다.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => v6.is_unique_local(),
    }
}

/// 바인드 정책 판정 (순수 함수).
///
/// - 프록시 뒤 모드: 루프백·사설 주소만, 인증 검증기 필수(TLS 무관). 전체 주소·공인 주소는 거부.
/// - 그 밖에 루프백: 늘 허용(TLS·인증은 선택).
/// - 그 밖의 모든 주소(`0.0.0.0`·`::` 포함): TLS와 인증 검증기가 둘 다 있어야 허용.
pub fn evaluate_bind(ip: IpAddr, policy: BindPolicy) -> Result<BindKind, BindRefusal> {
    let kind = if ip.is_loopback() {
        BindKind::Loopback
    } else {
        BindKind::Remote
    };
    if policy.behind_proxy {
        if kind == BindKind::Remote && !is_private_ip(ip) {
            return Err(BindRefusal::ProxyNeedsPrivateAddress {
                unspecified: ip.is_unspecified(),
            });
        }
        if !policy.authenticated {
            return Err(BindRefusal::ProxyNeedsAuth);
        }
        return Ok(kind);
    }
    if kind == BindKind::Remote && !(policy.tls && policy.authenticated) {
        return Err(BindRefusal::RemoteNeedsTlsAndAuth {
            missing_tls: !policy.tls,
            missing_auth: !policy.authenticated,
        });
    }
    Ok(kind)
}

// ---------------------------------------------------------------------------
// TLS (기능 `tls`)
// ---------------------------------------------------------------------------

#[cfg(feature = "tls")]
pub use tls::TlsConfig;

#[cfg(feature = "tls")]
mod tls {
    use super::{io, Arc, Duration, Notify, Router};
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    use rustls_pki_types::pem::{Error as PemError, PemObject};
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::path::Path;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::watch;
    use tokio::task::JoinSet;
    use tokio_rustls::rustls::crypto::ring;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    /// TLS 핸드셰이크를 기다려 주는 시간. 넘으면 그 연결만 끊는다(서버는 계속 돈다).
    /// 정상 클라이언트의 핸드셰이크는 네트워크 왕복 몇 번이면 끝나므로 느린 회선을 감안해도 넉넉한 값이다.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
    /// 종료 신호 뒤 진행 중 요청을 기다려 주는 시간. 바깥의 `DRAIN_TIMEOUT`(10초)보다 짧아야
    /// 안쪽이 먼저 끝나 정상 종료로 보고된다. 요청 하나는 단건 JSON 응답이라 보통 이보다 훨씬 빨리 끝난다.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(8);
    /// 수락 오류(파일 핸들 고갈 등) 뒤 다시 시도하기까지의 간격(바쁜 루프 방지).
    const ACCEPT_RETRY: Duration = Duration::from_millis(100);

    /// 연결 수명 상한. 시험에서 짧게 줄여 쓸 수 있도록 값으로 분리했다.
    #[derive(Clone, Copy)]
    pub(super) struct TlsLimits {
        pub(super) handshake: Duration,
        pub(super) drain: Duration,
    }

    impl Default for TlsLimits {
        fn default() -> Self {
            Self {
                handshake: HANDSHAKE_TIMEOUT,
                drain: DRAIN_TIMEOUT,
            }
        }
    }

    /// 서버 인증서와 개인키. 개인키가 들어 있으므로 `Debug`를 파생하지 않는다.
    pub struct TlsConfig(Arc<ServerConfig>);

    impl TlsConfig {
        /// PEM 파일에서 읽는다. 개인키는 PKCS#8·PKCS#1(RSA)·SEC1(EC) 어느 것이든 되고, 인증서 파일에는
        /// 서버 인증서 다음에 중간 인증서를 이어 붙일 수 있다. 암호 공급자는 `ring`으로 못 박는다
        /// (프로세스 전역 기본 공급자에 기대지 않는다).
        ///
        /// 오류에는 파일 경로만 싣고 파일 내용은 싣지 않는다(PEM 파서의 오류 문구에 키 줄이 섞일 수 있어 쓰지 않는다).
        pub fn from_pem_files(cert: &Path, key: &Path) -> io::Result<Self> {
            let certs = CertificateDer::pem_file_iter(cert)
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .map_err(|e| pem_error("인증서", cert, &e))?;
            if certs.is_empty() {
                return Err(invalid(format!(
                    "인증서 파일에 인증서가 없습니다: {}",
                    cert.display()
                )));
            }
            let key =
                PrivateKeyDer::from_pem_file(key).map_err(|e| pem_error("개인키", key, &e))?;
            let mut config =
                ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
                    .with_safe_default_protocol_versions()
                    .map_err(|e| invalid(format!("TLS 프로토콜 버전을 정할 수 없습니다: {e}")))?
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    // rustls 오류 문구는 종류만 말한다(키 내용 없음). 예: 인증서와 키가 짝이 아님.
                    .map_err(|e| invalid(format!("인증서와 개인키를 쓸 수 없습니다: {e}")))?;
            // 이 서버는 HTTP/1.1만 한다.
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(Self(Arc::new(config)))
        }
    }

    fn invalid(message: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }

    fn pem_error(what: &str, path: &Path, error: &PemError) -> io::Error {
        let path = path.display();
        match error {
            PemError::Io(e) => io::Error::new(
                e.kind(),
                format!("{what} 파일을 읽을 수 없습니다: {path}: {e}"),
            ),
            PemError::NoItemsFound => invalid(format!(
                "{what} 파일에서 PEM 항목을 찾지 못했습니다: {path} (개인키는 PKCS#8·PKCS#1·SEC1 PEM이어야 합니다)"
            )),
            // 그 밖의 오류 문구에는 파일 줄이 그대로 들어갈 수 있어 싣지 않는다.
            _ => invalid(format!(
                "{what} 파일의 PEM 형식이 올바르지 않습니다: {path}"
            )),
        }
    }

    /// TLS 수락 루프. `stop`이 알려지면 새 연결을 받지 않고, 진행 중 요청을 `limits.drain` 안에서
    /// 마저 처리한 뒤 남은 연결을 끊고 돌아온다.
    pub(super) async fn serve(
        listener: TcpListener,
        app: Router,
        config: &TlsConfig,
        stop: Arc<Notify>,
        limits: TlsLimits,
    ) {
        let acceptor = TlsAcceptor::from(Arc::clone(&config.0));
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut connections = JoinSet::new();
        // select!의 세 분기(수락·태스크 수거·종료 알림)는 모두 취소해도 잃는 상태가 없다.
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((tcp, _peer)) => {
                        connections.spawn(connection(
                            tcp,
                            acceptor.clone(),
                            app.clone(),
                            stop_rx.clone(),
                            limits.handshake,
                        ));
                    }
                    Err(e) => {
                        eprintln!("mcp-kit http: 연결 수락 오류: {e}");
                        tokio::time::sleep(ACCEPT_RETRY).await;
                    }
                },
                Some(finished) = connections.join_next() => report(finished),
                () = stop.notified() => break,
            }
        }
        drop(listener); // 새 연결을 더 받지 않는다
                        // 모든 연결 태스크에 알린다. 수신자가 없으면(연결 없음) 알릴 곳도 없다.
        let _ = stop_tx.send(true);
        let drain = async {
            while let Some(finished) = connections.join_next().await {
                report(finished);
            }
        };
        if tokio::time::timeout(limits.drain, drain).await.is_err() {
            // 남은 연결은 `connections`를 놓을 때 중단된다.
            eprintln!("mcp-kit http: 진행 중 요청을 기다리다 시간 초과, 남은 연결을 끊습니다");
        }
    }

    fn report(finished: Result<(), tokio::task::JoinError>) {
        if let Err(e) = finished {
            eprintln!("mcp-kit http: 연결 처리 작업이 비정상 종료: {e}");
        }
    }

    /// 연결 하나: 핸드셰이크(시간 상한) → HTTP/1.1 서비스. 종료 알림이 오면 핸드셰이크 중인 연결은 버리고,
    /// 서비스 중인 연결은 진행 중 요청을 끝낸 뒤 닫는다.
    async fn connection(
        tcp: TcpStream,
        acceptor: TlsAcceptor,
        app: Router,
        mut stop: watch::Receiver<bool>,
        handshake: Duration,
    ) {
        let accepted = tokio::select! {
            result = tokio::time::timeout(handshake, acceptor.accept(tcp)) => result,
            _ = stop.changed() => return,
        };
        // 핸드셰이크 실패(평문 접속·스캐너·시간 초과)는 클라이언트 쪽 일이고 흔하다 —
        // 접속마다 로그를 남기면 스캔 한 번에 로그가 넘치므로 조용히 연결만 닫는다.
        let Ok(Ok(tls)) = accepted else {
            return;
        };
        let connection = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(tls), TowerToHyperService::new(app));
        tokio::pin!(connection);
        tokio::select! {
            // 끊긴 연결 같은 클라이언트 쪽 오류는 요청 처리 결과가 아니므로 기록하지 않는다.
            _ = connection.as_mut() => {}
            _ = stop.changed() => {
                connection.as_mut().graceful_shutdown();
                let _ = connection.await; // 같은 이유로 결과는 보지 않는다
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_parser_cases() {
        for ok in [
            "http://localhost",
            "http://LOCALHOST:3000",
            "https://127.0.0.1:8443",
            "http://127.1.2.3",
            "http://[::1]",
            "http://[::1]:80",
        ] {
            assert!(is_loopback_origin(ok), "{ok}");
        }
        for bad in [
            "",
            "null",
            "localhost",
            "ftp://localhost",
            "http://",
            "http://localhost:",
            "http://localhost:abc",
            "http://localhost@evil.example",
            "http://localhost/",
            "http://127.0.0.1.evil.example",
            "http://[::1",
            "http://[::2]",
            "http://0.0.0.0",
            "http://example.com",
        ] {
            assert!(!is_loopback_origin(bad), "{bad}");
        }
    }

    #[test]
    fn constant_time_eq_cases() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn session_ids_are_32_hex_chars_and_unique() {
        let a = new_session_id().expect("a");
        let b = new_session_id().expect("b");
        assert_eq!(a.len(), SESSION_ID_BYTES * 2);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn request_context_header_lookup_is_case_insensitive() {
        let ctx = RequestContext {
            session_id: None,
            headers: vec![("x-ident".into(), "v".into())],
            principal: None,
        };
        assert_eq!(ctx.header("X-Ident"), Some("v"));
        assert_eq!(ctx.header("x-other"), None);
    }

    // ---- 원격 모드 Origin 규칙 (같은 PC 밖 주소에 바인드하지 않고 실제 처리 경로를 직접 호출한다) ----

    fn remote_policy(allowed: &[&str]) -> OriginPolicy {
        let list: Vec<String> = allowed.iter().map(ToString::to_string).collect();
        OriginPolicy::new(BindKind::Remote, &list).expect("허용 목록")
    }

    #[test]
    fn remote_origin_policy_allows_only_listed_origins() {
        let policy = remote_policy(&["https://App.Example:8443/", "http://tool.internal"]);
        for ok in [
            "https://app.example:8443",
            "HTTPS://APP.EXAMPLE:8443",
            "http://tool.internal",
        ] {
            assert!(policy.allows(ok), "{ok}");
        }
        // 목록 밖은 루프백이어도 거부, 포트·스킴이 다르면 다른 출처다.
        for bad in [
            "http://localhost",
            "http://127.0.0.1:3000",
            "https://app.example",
            "http://app.example:8443",
            "https://app.example:8443.evil.example",
            "null",
            "",
        ] {
            assert!(!policy.allows(bad), "{bad}");
        }
        // 목록이 비면 Origin이 있는 요청은 전부 거부한다.
        let empty = remote_policy(&[]);
        assert!(!empty.allows("http://localhost"));
        assert!(!empty.allows("https://app.example"));
        // 루프백 규칙은 그대로다.
        assert!(OriginPolicy::Loopback.allows("http://localhost:3000"));
        assert!(!OriginPolicy::Loopback.allows("https://app.example"));
    }

    #[test]
    fn origin_policy_rejects_bad_entries_and_loopback_lists() {
        for bad in [
            "*",
            "app.example",
            "https://app.example/path",
            "",
            "https://",
            "ftp://x",
            "https://a@b",
        ] {
            let err = OriginPolicy::new(BindKind::Remote, &[bad.to_string()])
                .err()
                .unwrap_or_else(|| panic!("{bad:?} 는 거부되어야 함"));
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        let err = OriginPolicy::new(BindKind::Loopback, &["https://app.example".to_string()])
            .err()
            .expect("루프백에는 허용 목록을 줄 수 없음");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(OriginPolicy::new(BindKind::Loopback, &[]).is_ok());
    }

    fn shared_with(policy: OriginPolicy) -> Arc<Shared> {
        Arc::new(Shared {
            handler: Arc::new(|_msg, _ctx| Some(json!({"jsonrpc":"2.0","id":1,"result":{}}))),
            token: None,
            authenticator: None,
            forward_headers: Vec::new(),
            on_session_end: None,
            origin_policy: policy,
            sessions: Mutex::new(HashSet::new()),
        })
    }

    async fn post_initialize(shared: &Arc<Shared>, origin: Option<&str>) -> StatusCode {
        let mut headers = HeaderMap::new();
        if let Some(origin) = origin {
            headers.insert(
                header::ORIGIN,
                HeaderValue::from_str(origin).expect("헤더 값"),
            );
        }
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        endpoint(State(Arc::clone(shared)), Method::POST, headers, body)
            .await
            .status()
    }

    #[tokio::test]
    async fn endpoint_applies_remote_origin_list_and_keeps_originless_clients() {
        let remote = shared_with(remote_policy(&["https://app.example:8443"]));
        assert_eq!(
            post_initialize(&remote, None).await,
            StatusCode::OK,
            "Origin 없음은 통과"
        );
        assert_eq!(
            post_initialize(&remote, Some("https://app.example:8443")).await,
            StatusCode::OK
        );
        for bad in [
            "http://localhost:3000",
            "http://127.0.0.1",
            "https://evil.example",
        ] {
            assert_eq!(
                post_initialize(&remote, Some(bad)).await,
                StatusCode::FORBIDDEN,
                "원격 서버는 루프백 Origin도 목록 밖이면 거부: {bad}"
            );
        }
        let empty = shared_with(remote_policy(&[]));
        assert_eq!(
            post_initialize(&empty, Some("http://localhost")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(post_initialize(&empty, None).await, StatusCode::OK);

        // 루프백 서버는 지금과 같다.
        let local = shared_with(OriginPolicy::Loopback);
        assert_eq!(
            post_initialize(&local, Some("http://localhost:3000")).await,
            StatusCode::OK
        );
        assert_eq!(
            post_initialize(&local, Some("https://app.example:8443")).await,
            StatusCode::FORBIDDEN
        );
    }

    // ---- TLS 수락 루프의 시간 상한 (짧은 상한으로 실제 연결을 건다) ----

    #[cfg(feature = "tls")]
    mod tls_limits {
        use super::*;
        use std::io::{Read, Write};
        use std::time::Instant;

        const SHORT_HANDSHAKE: Duration = Duration::from_millis(300);
        const SHORT_DRAIN: Duration = Duration::from_millis(400);
        const WAIT: Duration = Duration::from_secs(5);

        struct Running {
            addr: SocketAddr,
            stop: Arc<Notify>,
            served: tokio::task::JoinHandle<()>,
            client: Arc<rustls::ClientConfig>,
            _dir: tempfile::TempDir,
        }

        async fn start_serve() -> Running {
            let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
                .expect("자체 서명");
            let dir = tempfile::tempdir().expect("임시 폴더");
            let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
            std::fs::write(&cert, made.cert.pem()).expect("cert");
            std::fs::write(&key, made.signing_key.serialize_pem()).expect("key");
            let config = TlsConfig::from_pem_files(&cert, &key).expect("TLS 설정");

            let mut roots = rustls::RootCertStore::empty();
            roots.add(made.cert.der().clone()).expect("루트");
            let client = Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .expect("프로토콜")
                .with_root_certificates(roots)
                .with_no_client_auth(),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("바인드");
            let addr = listener.local_addr().expect("주소");
            let app = build_router(shared_with(OriginPolicy::Loopback), DEFAULT_PATH);
            let stop = Arc::new(Notify::new());
            let limits = tls::TlsLimits {
                handshake: SHORT_HANDSHAKE,
                drain: SHORT_DRAIN,
            };
            let served = tokio::spawn({
                let stop = Arc::clone(&stop);
                async move { tls::serve(listener, app, &config, stop, limits).await }
            });
            Running {
                addr,
                stop,
                served,
                client,
                _dir: dir,
            }
        }

        fn connect_tls(
            addr: SocketAddr,
            config: &Arc<rustls::ClientConfig>,
        ) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
            let name = rustls::pki_types::ServerName::try_from("127.0.0.1").expect("서버 이름");
            let conn = rustls::ClientConnection::new(Arc::clone(config), name).expect("클라이언트");
            let tcp = std::net::TcpStream::connect(addr).expect("접속");
            tcp.set_read_timeout(Some(WAIT)).expect("읽기 제한");
            rustls::StreamOwned::new(conn, tcp)
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stalled_handshake_is_dropped_after_the_limit_and_does_not_block_others() {
            let running = start_serve().await;
            let (addr, client) = (running.addr, Arc::clone(&running.client));

            let outcome = tokio::task::spawn_blocking(move || {
                // 아무것도 보내지 않고 붙들고 있는 연결.
                let mut silent = std::net::TcpStream::connect(addr).expect("접속");
                silent.set_read_timeout(Some(WAIT)).expect("읽기 제한");

                // 그 사이에 정상 클라이언트는 곧바로 서비스를 받는다.
                let mut good = connect_tls(addr, &client);
                let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
                let request = format!(
                    "POST /mcp HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                good.write_all(request.as_bytes()).expect("요청");
                let mut head = [0u8; 12];
                good.read_exact(&mut head).expect("응답 첫 줄");

                // 붙들린 연결은 상한 뒤 서버가 닫는다.
                let started = Instant::now();
                let mut buf = [0u8; 8];
                let closed = silent.read(&mut buf);
                (head, closed, started.elapsed())
            })
            .await
            .expect("시험 스레드");

            assert_eq!(&outcome.0, b"HTTP/1.1 200", "정상 클라이언트가 막힘");
            let closed_by_server = match &outcome.1 {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => !matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ),
            };
            assert!(
                closed_by_server,
                "핸드셰이크 상한 뒤 서버가 연결을 닫아야 함: {:?}",
                outcome.1
            );
            assert!(
                outcome.2 < Duration::from_secs(3),
                "연결이 너무 늦게 닫힘: {:?}",
                outcome.2
            );

            running.stop.notify_one();
            tokio::time::timeout(WAIT, running.served)
                .await
                .expect("종료")
                .expect("태스크");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn drain_is_bounded_when_a_client_holds_a_request_open() {
            let running = start_serve().await;
            let (addr, client) = (running.addr, Arc::clone(&running.client));

            // 본문을 100바이트라 해 놓고 보내지 않는 요청 — 서버는 이 요청을 처리 중으로 본다.
            let held = tokio::task::spawn_blocking(move || {
                let mut stream = connect_tls(addr, &client);
                stream
                    .write_all(b"POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{")
                    .expect("요청 머리");
                stream.flush().expect("flush");
                stream
            })
            .await
            .expect("시험 스레드");
            // 서버가 연결을 받아 처리 중으로 돌려놓을 시간을 준다(상한 시험이라 짧은 실제 대기면 충분하다).
            tokio::time::sleep(Duration::from_millis(300)).await;

            let started = Instant::now();
            running.stop.notify_one();
            tokio::time::timeout(WAIT, running.served)
                .await
                .expect("종료가 상한 안에 끝나야 함")
                .expect("태스크");
            let elapsed = started.elapsed();
            assert!(
                elapsed >= SHORT_DRAIN.mul_f32(0.6),
                "붙잡힌 요청을 기다려 주지 않음: {elapsed:?}"
            );
            assert!(
                elapsed < Duration::from_secs(3),
                "드레인 상한을 넘김: {elapsed:?}"
            );
            drop(held);
        }
    }
}
