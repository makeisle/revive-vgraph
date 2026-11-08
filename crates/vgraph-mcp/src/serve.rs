//! 서버 모드 — `vgraph-mcp serve`(상주 HTTP 서버), `stop`, `status`. 기능 `server` 뒤에 있다.
//!
//! 중복 기동 차단은 데이터 폴더의 잠금 파일(OS 파일 잠금)로 한다. OS는 프로세스가 어떻게 끝나든
//! 잠금을 풀어 주므로 비정상 종료로 남은 파일은 다음 기동이 그대로 이어받는다.
//! 잠금은 빈 파일 `server.lock`에 걸고, 주소·PID는 별도 파일 `server.info`에 적는다
//! (Windows에서는 잠긴 구간을 다른 핸들이 읽을 수 없어 한 파일에 둘 수 없다).
//! 끄기 요청은 `server.stop` 파일로 전한다 — HTTP 종료 경로는 만들지 않는다.

use clap::{Args, Subcommand};
use fs4::{FileExt, TryLockError};
use mcp_kit::http::{
    check_allow_origins, evaluate_bind, BindKind, BindPolicy, BindRefusal, HttpOptions, HttpServer,
    RequestContext, TlsConfig, Unauthorized,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use vgraph_mcp::{Identity, McpServer, PathPolicy};

use crate::{build_server, service, tokens, CommonArgs, DEFAULT_ROOT};

/// 기본 접속 주소. 널리 쓰이는 개발용 포트(3000·5000·8000·8080 등)와 겹치지 않는 값을 골랐다.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7437";
/// 클라이언트가 접속별 사용자를 알리는 요청 헤더.
pub const USER_HEADER: &str = "X-Vgraph-User";
/// 클라이언트가 접속별 작업 공간을 알리는 요청 헤더.
pub const WORKSPACE_HEADER: &str = "X-Vgraph-Workspace";

const LOCK_FILE: &str = "server.lock";
const INFO_FILE: &str = "server.info";
const INFO_TMP_FILE: &str = "server.info.tmp";
const STOP_FILE: &str = "server.stop";
/// `--detach` 서버의 출력(로그)을 남기는 기본 파일 이름. 데이터 폴더 아래에 둔다.
pub const LOG_FILE: &str = "server.log";
/// `--detach` 가 자식 서버의 기동을 기다려 주는 시간.
const DETACH_WAIT: Duration = Duration::from_secs(30);
/// 서버가 끄기 요청 파일을 살피는 간격.
const STOP_POLL: Duration = Duration::from_millis(250);
/// `stop`이 정상 종료를 기다려 주는 시간. 넘으면 강제 종료한다.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// 강제 종료 뒤 잠금이 풀리길 기다리는 시간.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// `stop`이 상태를 다시 확인하는 간격.
const PROBE_POLL: Duration = Duration::from_millis(100);
/// JSON-RPC "Invalid Request" 코드.
const INVALID_REQUEST: i64 = -32600;

const SERVE_ABOUT: &str = "여러 클라이언트가 함께 쓰는 상주 HTTP 서버를 띄웁니다";
const SERVE_LONG_ABOUT: &str = "\
여러 클라이언트(에이전트)가 한 데이터 폴더를 함께 쓰는 상주 HTTP 서버를 띄웁니다.
같은 데이터 폴더(--root)에는 서버를 하나만 띄울 수 있습니다.
주소는 기본이 루프백(127.0.0.1)이라 이 PC 안에서만 접속됩니다. 같은 PC 밖에서도 접속하게 하려면(--listen 에 루프백이
아닌 주소) TLS(--tls-cert·--tls-key)와 사용자별 토큰(--tokens)을 둘 다 줘야 시작합니다 — 하나라도 없으면 이유를
알리고 시작하지 않습니다. 0.0.0.0·:: 같은 전체 주소도 밖으로 열리는 주소로 봅니다.

접속별 신원은 요청 헤더로 정합니다.
  X-Vgraph-User       사용자 (생략하면 --user)
  X-Vgraph-Workspace  작업 공간 이름 (영문·숫자·-·_). 같은 이름으로 다시 접속하면 이어 씁니다.
                      생략하면 서버가 MCP 세션마다 새 작업 공간을 붙입니다(재접속하면 바뀝니다).

연결 예 (클라이언트 이름은 예시입니다):
  claude mcp add --transport http vgraph http://127.0.0.1:7437/mcp --header \"X-Vgraph-Workspace: my-agent\"

토큰을 쓰면 Authorization: Bearer <토큰> 이 맞는 접속만 받습니다 (--token-file 권장 — 명령줄 인자는 프로세스 목록에 보입니다).
이 토큰은 모든 접속이 함께 쓰는 하나입니다. 사용자마다 따로 주려면 --tokens 를 쓰세요:
  vgraph-mcp token add <사용자> 로 발급하고 vgraph-mcp serve --tokens <파일> 로 띄우면,
  토큰이 사용자를 정하고(X-Vgraph-User 는 무시) 폐기(token revoke)는 재시작 없이 바로 적용됩니다.
  --tokens 는 --token·--token-file 과 함께 쓸 수 없습니다. 주지 않으면 지금까지처럼 동작합니다.

밖으로 여는 예 (인증서·키는 PEM 파일, 개인키는 PKCS#8·PKCS#1·SEC1 형식 모두 읽습니다):
  vgraph-mcp serve --listen 0.0.0.0:7437 --tls-cert server.pem --tls-key server.key --tokens tokens.jsonl
  --allow-origin <출처> 는 브라우저 앱(Origin 헤더를 보내는 클라이언트)을 받을 때만 씁니다(여러 번 가능, 예:
  https://app.example:8443). 밖으로 여는 서버는 목록에 없는 Origin 을 403 으로 거절합니다. Origin 을 보내지 않는
  일반 클라이언트(에이전트)는 영향이 없습니다. 루프백 서버에는 --behind-proxy 와 함께일 때만 쓸 수 있고(프록시의
  공개 출처를 받을 때), 이때도 목록에 없는 Origin 은 루프백 출처까지 403 입니다. 목록이 없으면 루프백 출처만 받습니다.
  리버스 프록시(TLS 종단) 뒤에서는 --behind-proxy 로 평문을 허용합니다: 사설 주소(10.x·172.16~31.x·192.168.x·
  IPv6 fc00::/7)나 루프백에서만 열 수 있고 --tokens 가 필수입니다. 공인 주소·전체 주소는 거부합니다.

끄기와 상태 확인: vgraph-mcp stop --root <폴더> / vgraph-mcp status --root <폴더>";

#[derive(Subcommand)]
pub enum Command {
    #[command(about = SERVE_ABOUT, long_about = SERVE_LONG_ABOUT)]
    Serve(Box<ServeArgs>),
    /// 데이터 폴더에서 떠 있는 서버를 끈다 (정상 종료를 먼저 요청하고, 안 끝나면 강제 종료)
    Stop(RootArg),
    /// 데이터 폴더에서 떠 있는 서버의 상태(주소·PID)를 보인다. 떠 있지 않으면 0이 아닌 코드로 끝난다
    Status(RootArg),
    /// (내부용) 떠 있는 서버의 시작 인자 기록을 줄 단위로 출력한다 — `vgraph update --restart-server`가 읽는다
    #[command(hide = true)]
    StartArgs(RootArg),
    #[command(about = tokens::TOKEN_ABOUT, long_about = tokens::TOKEN_LONG_ABOUT)]
    Token {
        #[command(subcommand)]
        action: tokens::TokenCommand,
    },
    #[command(about = service::SERVICE_ABOUT, long_about = service::SERVICE_LONG_ABOUT)]
    Service {
        #[command(subcommand)]
        action: service::ServiceCommand,
    },
}

/// 서버를 띄우는 데 필요한 설정. `serve`와 `service install`이 함께 쓴다 —
/// 등록 전용 인자 체계를 따로 만들지 않는다.
#[derive(Args)]
pub struct ServeSettings {
    #[command(flatten)]
    common: CommonArgs,
    /// 접속 주소. 루프백이 아니면 --tls-cert·--tls-key 와 --tokens 가 모두 있어야 시작한다.
    /// 포트 0이면 빈 포트를 고르고 실제 주소를 출력한다
    #[arg(long, default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// 접속 토큰. 명령줄 인자는 프로세스 목록에 보이므로 --token-file을 권장한다
    #[arg(long, conflicts_with = "token_file")]
    token: Option<String>,
    /// 접속 토큰을 담은 파일 (앞뒤 공백·줄바꿈은 무시)
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// 사용자별 토큰 파일 (vgraph-mcp token add 로 발급). 주면 토큰이 사용자를 정하고
    /// X-Vgraph-User 헤더는 무시한다. --token·--token-file 과 함께 쓸 수 없다
    #[arg(long, conflicts_with_all = ["token", "token_file"])]
    tokens: Option<PathBuf>,
    /// TLS 서버 인증서 PEM 파일 (--tls-key 와 함께). 주면 HTTPS 로 서비스한다
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// TLS 개인키 PEM 파일 (PKCS#8·PKCS#1·SEC1, --tls-cert 와 함께). 파일 내용은 어디에도 출력하지 않는다
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// 리버스 프록시 뒤에서 도는 서버. 사설 주소·루프백에서만 평문으로 열 수 있고 --tokens 가 필수다
    #[arg(long)]
    behind_proxy: bool,
    /// 밖으로 여는 서버가 받아 줄 브라우저 출처 (Origin 헤더, 여러 번 가능, 예: https://app.example:8443).
    /// 목록에 없는 Origin 은 403 이다. Origin 을 보내지 않는 클라이언트는 영향 없다. 루프백 서버에는 쓸 수 없다
    #[arg(long = "allow-origin", value_name = "출처")]
    allow_origin: Vec<String>,
    /// 내보내기·가져오기(vgraph_export/vgraph_import)의 path가 닿을 수 있는 유일한 폴더
    /// (기본: <데이터 폴더>/exchange). 상대 경로는 이 폴더 기준이고, 밖의 경로는 거부된다.
    /// 데이터 폴더 자신이나 그 상위 폴더는 쓸 수 없다
    #[arg(long, value_name = "폴더")]
    exchange_dir: Option<PathBuf>,
}

/// `ServeSettings::validate`가 확인을 마치고 넘기는 값. 토큰 값·TLS 설정(개인키 포함)이 있어 `Debug`를 파생하지 않는다.
pub struct Checked {
    token: Option<String>,
    tls: Option<TlsConfig>,
}

#[derive(Args)]
pub struct ServeArgs {
    #[command(flatten)]
    settings: ServeSettings,
    /// 창이 없는(콘솔에 붙지 않는) 자식 프로세스로 서버를 다시 띄우고 이 명령은 곧바로 끝낸다.
    /// 서버가 준비되면 주소를 출력한다. 서버의 출력은 로그 파일에 남는다
    #[arg(long)]
    detach: bool,
    /// --detach 일 때 서버 출력을 남기는 로그 파일 (기본: <데이터 폴더>/server.log)
    #[arg(long)]
    log_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct RootArg {
    /// 서버의 데이터 폴더 (serve에 준 --root와 같은 값)
    #[arg(long, default_value = DEFAULT_ROOT)]
    root: PathBuf,
}

pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Serve(args) => serve(*args),
        Command::Stop(a) => stop(&a.root),
        Command::Status(a) => status(&a.root),
        Command::StartArgs(a) => start_args(&a.root),
        Command::Token { action } => tokens::run(action),
        Command::Service { action } => service::run(action),
    }
}

/// 문자열로 쓸 수 있는 경로. UTF-8이 아닌 경로는 등록 항목에 안전하게 옮길 수 없어 거절한다.
pub fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("UTF-8이 아닌 경로는 쓸 수 없습니다: {}", path.display()))
}

/// 상대 경로를 지금 작업 폴더 기준 절대 경로로 바꾼다 (심볼릭 링크는 따라가지 않는다).
pub fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path)
        .map_err(|e| anyhow::anyhow!("절대 경로로 바꿀 수 없습니다: {}: {e}", path.display()))
}

/// 교환 폴더 기본 이름 (데이터 폴더 바로 아래).
const EXCHANGE_DIR_NAME: &str = "exchange";

impl ServeSettings {
    /// 원격 호출자의 `path`를 가둘 교환 폴더를 만들고 경로 정책으로 돌려준다.
    /// 데이터 폴더가 교환 폴더와 같거나 그 안이면 가두는 의미가 없어 거절한다.
    fn exchange_policy(&self) -> anyhow::Result<PathPolicy> {
        let root = fs::canonicalize(&self.common.root).map_err(|e| {
            anyhow::anyhow!(
                "데이터 폴더를 확인할 수 없습니다: {}: {e}",
                self.common.root.display()
            )
        })?;
        let dir = match &self.exchange_dir {
            Some(d) => absolute(d)?,
            None => root.join(EXCHANGE_DIR_NAME),
        };
        fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("교환 폴더를 만들 수 없습니다: {}: {e}", dir.display()))?;
        let dir = fs::canonicalize(&dir).map_err(|e| {
            anyhow::anyhow!("교환 폴더를 확인할 수 없습니다: {}: {e}", dir.display())
        })?;
        if root.starts_with(&dir) {
            anyhow::bail!(
                "교환 폴더({})는 데이터 폴더와 같거나 그 상위일 수 없습니다 — 데이터 폴더의 파일이 노출됩니다",
                dir.display()
            );
        }
        Ok(PathPolicy::ExchangeDir(dir))
    }

    pub fn has_inline_token(&self) -> bool {
        self.token.is_some()
    }

    /// 데이터 폴더의 절대 경로.
    pub fn root_abs(&self) -> anyhow::Result<PathBuf> {
        absolute(&self.common.root)
    }

    /// 접속 설정 검사: 바인드 정책(루프백이 아니면 TLS와 사용자별 토큰 필수), 허용 출처 형식,
    /// 토큰·TLS 파일 읽기. 파일 내용과 토큰 값은 오류 문구에 넣지 않는다(파일 경로만).
    /// 정책 거부는 바인드는 물론 데이터 폴더를 만들기 전에 일어난다.
    pub fn validate(&self) -> anyhow::Result<Checked> {
        let kind = evaluate_bind(
            self.listen.ip(),
            BindPolicy {
                tls: self.tls_cert.is_some(),
                authenticated: self.tokens.is_some(),
                behind_proxy: self.behind_proxy,
            },
        )
        .map_err(|refusal| self.refusal_error(&refusal))?;
        if kind == BindKind::Loopback && !self.behind_proxy {
            anyhow::ensure!(
                self.allow_origin.is_empty(),
                "--allow-origin 은 루프백이 아닌 주소로 여는 서버이거나 --behind-proxy 와 함께일 때만 쓸 수 있습니다 (루프백 서버는 루프백 출처만 받습니다)"
            );
        }
        check_allow_origins(&self.allow_origin)?;
        let token = self.resolve_token()?;
        let tls = match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => Some(TlsConfig::from_pem_files(cert, key)?),
            _ => None,
        };
        Ok(Checked { token, tls })
    }

    /// 정책 거부 문구에 이 설정에 맞는 해결 방법을 덧붙인다.
    fn refusal_error(&self, refusal: &BindRefusal) -> anyhow::Error {
        let needs_tokens = matches!(
            refusal,
            BindRefusal::ProxyNeedsAuth
                | BindRefusal::RemoteNeedsTlsAndAuth {
                    missing_auth: true,
                    ..
                }
        );
        let mut message = format!("--listen {}: {refusal}", self.listen);
        if needs_tokens {
            message.push_str(
                ". 사용자별 토큰은 vgraph-mcp token add <사용자> 로 발급하고 --tokens <파일> 로 지정하세요",
            );
            if self.token.is_some() || self.token_file.is_some() {
                message.push_str(
                    " (--token·--token-file 은 모두가 함께 쓰는 토큰 하나라 사용자별 토큰으로 인정되지 않습니다)",
                );
            }
        }
        anyhow::anyhow!(message)
    }

    /// `serve` 하위 명령 뒤에 그대로 붙일 인자 목록 (`--detach`·`--log-file` 제외).
    /// 경로는 모두 절대 경로로 바꿔, 어느 작업 폴더에서 다시 실행해도 같은 서버가 뜨게 한다.
    pub fn to_args(&self) -> anyhow::Result<Vec<String>> {
        let c = &self.common;
        let mut v = vec![
            "--root".to_string(),
            path_text(&self.root_abs()?)?.to_string(),
            "--user".to_string(),
            c.user.clone(),
            "--embedder".to_string(),
            c.embedder.clone(),
        ];
        if let Some(fields) = &c.embed_fields {
            v.extend(["--embed-fields".to_string(), fields.clone()]);
        }
        v.extend(["--chunker".to_string(), c.chunker.clone()]);
        if c.no_embed_fallback {
            v.push("--no-embed-fallback".to_string());
        }
        v.extend(["--listen".to_string(), self.listen.to_string()]);
        if let Some(dir) = &self.exchange_dir {
            v.extend([
                "--exchange-dir".to_string(),
                path_text(&absolute(dir)?)?.to_string(),
            ]);
        }
        if let Some(token) = &self.token {
            v.extend(["--token".to_string(), token.clone()]);
        }
        if let Some(file) = &self.token_file {
            v.extend([
                "--token-file".to_string(),
                path_text(&absolute(file)?)?.to_string(),
            ]);
        }
        if let Some(file) = &self.tokens {
            v.extend([
                "--tokens".to_string(),
                path_text(&absolute(file)?)?.to_string(),
            ]);
        }
        // 인증서·키는 경로만 넘긴다(내용은 파일에 있고 여기엔 없다).
        if let (Some(cert), Some(key)) = (&self.tls_cert, &self.tls_key) {
            for (flag, file) in [("--tls-cert", cert), ("--tls-key", key)] {
                v.extend([flag.to_string(), path_text(&absolute(file)?)?.to_string()]);
            }
        }
        if self.behind_proxy {
            v.push("--behind-proxy".to_string());
        }
        for origin in &self.allow_origin {
            v.extend(["--allow-origin".to_string(), origin.clone()]);
        }
        Ok(v)
    }

    /// `server.info`에 남길 시작 인자 기록. 인라인 `--token`이면 값을 남기지 않고 그 사실만 표시한다.
    /// 인자를 만들 수 없거나(UTF-8이 아닌 경로) 줄바꿈이 든 인자가 있으면 기록 없음으로 둔다 —
    /// 그래도 다시 시작은 `--root`만으로 되돌아가 동작하므로 서버 기동을 막지 않는다.
    fn start_record(&self) -> StartRecord {
        if self.has_inline_token() {
            return StartRecord::InlineToken;
        }
        match self.to_args() {
            Ok(args) if !args.iter().any(|a| a.contains(['\r', '\n'])) => StartRecord::Args(args),
            _ => StartRecord::Unrecorded,
        }
    }

    /// 토큰을 인자 또는 파일에서 읽는다. 오류 문구에 토큰 값은 넣지 않는다.
    fn resolve_token(&self) -> anyhow::Result<Option<String>> {
        let raw = match (&self.token, &self.token_file) {
            (Some(t), _) => t.clone(), // 인자 구조체를 빌려 읽으므로 값을 복제해 서버 설정에 넘긴다
            (None, Some(path)) => fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("토큰 파일을 읽을 수 없습니다: {}: {e}", path.display())
            })?,
            (None, None) => return Ok(None),
        };
        let token = raw.trim();
        anyhow::ensure!(
            !token.is_empty(),
            "토큰이 비어 있습니다 (--token 또는 --token-file)"
        );
        Ok(Some(token.to_string()))
    }
}

// ---------------------------------------------------------------- 잠금·상태 파일

struct Paths {
    lock: PathBuf,
    info: PathBuf,
    info_tmp: PathBuf,
    stop: PathBuf,
}

impl Paths {
    fn of(root: &Path) -> Self {
        Paths {
            lock: root.join(LOCK_FILE),
            info: root.join(INFO_FILE),
            info_tmp: root.join(INFO_TMP_FILE),
            stop: root.join(STOP_FILE),
        }
    }
}

/// 잠금 파일에 배타 잠금을 시도한다. 이미 다른 프로세스가 쥐고 있으면 `None`.
/// (fs4 트레이트를 경로로 직접 불러 Rust 1.89+ 표준 라이브러리의 같은 이름 메서드와 섞이지 않게 한다.)
fn try_lock(path: &Path, create: bool) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(create)
        .write(true)
        .truncate(false)
        .open(path)?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// 서버가 어떤 인자로 시작했는지에 대한 기록. 업데이트가 같은 설정으로 다시 띄우는 데 쓴다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRecord {
    /// 기록이 없다(옛 버전이 띄운 서버, 또는 기록할 수 없는 인자).
    Unrecorded,
    /// 토큰을 `--token` 값으로 받아 시작해서 인자를 기록하지 않았다(토큰 값은 파일에 남기지 않는다).
    InlineToken,
    /// `serve` 뒤에 그대로 붙일 수 있는 인자(경로는 절대 경로, 비밀 값 없음).
    Args(Vec<String>),
}

/// 상태 파일의 `args_unavailable` 에 적는 값.
const INLINE_TOKEN_MARK: &str = "inline_token";

struct Info {
    pid: u32,
    addr: SocketAddr,
    /// HTTPS 로 서비스 중이다(상태 파일에는 켜졌을 때만 적는다 — 평문 서버의 파일 내용은 예전과 같다).
    tls: bool,
    start: StartRecord,
}

/// 접속 주소(URL). 인증서·키 경로 같은 설정은 싣지 않는다.
pub fn mcp_url(addr: SocketAddr, tls: bool) -> String {
    let scheme = if tls { "https" } else { "http" };
    format!("{scheme}://{addr}/mcp")
}

impl Info {
    fn read(paths: &Paths) -> Option<Info> {
        let v: Value = serde_json::from_str(&fs::read_to_string(&paths.info).ok()?).ok()?;
        Some(Info {
            pid: u32::try_from(v["pid"].as_u64()?).ok()?,
            addr: v["addr"].as_str()?.parse().ok()?,
            tls: v["tls"].as_bool().unwrap_or(false),
            start: start_record_of(&v),
        })
    }

    /// 임시 파일에 쓰고 바꿔치기해서, 읽는 쪽이 쓰다 만 내용을 보지 않게 한다.
    fn write(&self, paths: &Paths) -> io::Result<()> {
        let mut body = json!({
            "pid": self.pid,
            "addr": self.addr.to_string(),
            "version": env!("CARGO_PKG_VERSION"),
        });
        if self.tls {
            body["tls"] = json!(true);
        }
        match &self.start {
            StartRecord::Unrecorded => {}
            StartRecord::InlineToken => body["args_unavailable"] = json!(INLINE_TOKEN_MARK),
            StartRecord::Args(args) => body["args"] = json!(args),
        }
        fs::write(&paths.info_tmp, body.to_string())?;
        fs::rename(&paths.info_tmp, &paths.info)
    }
}

/// 상태 파일에서 시작 인자 기록을 읽는다. `args`가 없는 옛 형식은 `Unrecorded`.
/// 배열이 문자열만으로 이루어지지 않았으면 일부만 믿지 않고 기록 없음으로 본다.
fn start_record_of(v: &Value) -> StartRecord {
    if let Some(items) = v["args"].as_array() {
        let args: Option<Vec<String>> = items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect();
        return args.map_or(StartRecord::Unrecorded, StartRecord::Args);
    }
    if v["args_unavailable"].as_str() == Some(INLINE_TOKEN_MARK) {
        return StartRecord::InlineToken;
    }
    StartRecord::Unrecorded
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

enum Probe {
    Stopped,
    /// 서버가 잠금을 쥐고 있다. 주소·PID는 막 기동 중이면 아직 없을 수 있다.
    Running(Option<Info>),
}

/// 데이터 폴더의 서버 상태를 본다. 아무것도 만들지 않는다.
fn probe(paths: &Paths) -> io::Result<Probe> {
    match try_lock(&paths.lock, false) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Probe::Stopped),
        Err(e) => Err(e),
        Ok(Some(_released_at_drop)) => Ok(Probe::Stopped),
        Ok(None) => Ok(Probe::Running(Info::read(paths))),
    }
}

// ---------------------------------------------------------------- serve

fn serve(args: ServeArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.detach || args.log_file.is_none(),
        "--log-file 은 --detach 와 함께만 쓸 수 있습니다 (서버를 직접 띄울 때 출력은 이 터미널로 나옵니다)"
    );
    let checked = args.settings.validate()?;
    // 서버를 띄우기 전에 토큰 파일을 확인한다(없거나 깨졌으면 여기서 멈춘다). detach 부모도 같은 검사를 거친다.
    let verifier = match &args.settings.tokens {
        Some(path) => Some(Arc::new(tokens::Verifier::open(path)?)),
        None => None,
    };
    if args.detach {
        return detach(&args);
    }
    let ServeArgs { settings, .. } = args;
    let start = settings.start_record();
    let listen = settings.listen;
    let root = settings.common.root.clone();
    let default_user = settings.common.user.clone();
    let Checked { token, tls } = checked;
    let tls_on = tls.is_some();

    fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("데이터 폴더를 만들 수 없습니다: {}: {e}", root.display()))?;
    let paths = Paths::of(&root);
    let Some(lock) = try_lock(&paths.lock, true).map_err(|e| {
        anyhow::anyhow!("잠금 파일을 열 수 없습니다: {}: {e}", paths.lock.display())
    })?
    else {
        return Err(already_running(&paths, &root));
    };
    // 이 아래로는 우리가 이 폴더의 유일한 서버다. 이전 서버가 남긴 흔적을 치운다.
    remove_if_present(&paths.info)?;
    remove_if_present(&paths.stop)?;

    let path_policy = settings.exchange_policy()?;
    let server = Arc::new(build_server(settings.common)?.with_path_policy(path_policy));
    let router = Arc::new(Router::new(Arc::clone(&server), default_user));
    let mut opts = HttpOptions::new(listen)
        .forward_header(USER_HEADER)
        .forward_header(WORKSPACE_HEADER);
    if let Some(t) = token {
        opts = opts.token(t);
    }
    if let Some(tls) = tls {
        opts = opts.tls(tls);
    }
    opts = opts.behind_proxy(settings.behind_proxy);
    for origin in &settings.allow_origin {
        opts = opts.allow_origin(origin);
    }
    if let Some(verifier) = verifier {
        // 거부 사유(없는 토큰·폐기·형식 오류)는 응답으로 구분해 알리지 않는다.
        opts = opts.authenticator(move |headers| {
            let presented = headers.bearer().ok_or(Unauthorized)?;
            verifier
                .authenticate(presented)
                .map(Some)
                .ok_or(Unauthorized)
        });
    }
    let on_end = Arc::clone(&router);
    opts = opts.on_session_end(move |session| on_end.forget(session));

    let http = HttpServer::start(opts, move |msg, ctx| router.handle(msg, ctx)).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse {
            anyhow::anyhow!(
                "{} 포트는 이미 사용 중입니다. 다른 프로그램이 쓰고 있으니 --listen 으로 다른 포트를 주세요.",
                listen
            )
        } else {
            anyhow::anyhow!("서버를 시작하지 못했습니다({listen}): {e}")
        }
    })?;

    let addr = http.local_addr();
    let pid = std::process::id();
    let info = Info {
        pid,
        addr,
        tls: tls_on,
        start,
    };
    info.write(&paths).map_err(|e| {
        anyhow::anyhow!("상태 파일을 쓸 수 없습니다: {}: {e}", paths.info.display())
    })?;
    println!(
        "vgraph-mcp 서버 시작: {} (PID {pid}, 데이터 폴더 {})",
        mcp_url(addr, tls_on),
        root.display()
    );

    wait_for_shutdown(&paths.stop)?;

    http.shutdown();
    remove_if_present(&paths.info)?;
    remove_if_present(&paths.stop)?;
    drop(lock); // 잠금을 푼다 (프로세스가 끝나도 풀리지만 명시한다)
    println!("vgraph-mcp 서버를 종료했습니다.");
    Ok(())
}

fn already_running(paths: &Paths, root: &Path) -> anyhow::Error {
    match Info::read(paths) {
        Some(i) => anyhow::anyhow!(
            "이 데이터 폴더에는 이미 서버가 떠 있습니다(주소 {}, PID {}): {}",
            mcp_url(i.addr, i.tls),
            i.pid,
            root.display()
        ),
        None => anyhow::anyhow!(
            "이 데이터 폴더에는 이미 서버가 떠 있습니다(주소는 아직 알 수 없음): {}",
            root.display()
        ),
    }
}

/// `--detach`: 창 없는 자식 프로세스로 같은 서버를 다시 띄우고, 준비되면 주소를 알린 뒤 끝난다.
/// 자식에게는 `--detach`·`--log-file` 없이 같은 설정을 넘기므로 자식은 일반 `serve`와 똑같이 동작한다.
fn detach(args: &ServeArgs) -> anyhow::Result<()> {
    let root = args.settings.root_abs()?;
    fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("데이터 폴더를 만들 수 없습니다: {}: {e}", root.display()))?;
    let paths = Paths::of(&root);
    if matches!(probe(&paths)?, Probe::Running(_)) {
        return Err(already_running(&paths, &root));
    }
    let log_path = match &args.log_file {
        Some(p) => absolute(p)?,
        None => root.join(LOG_FILE),
    };
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("로그 폴더를 만들 수 없습니다: {}: {e}", dir.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("로그 파일을 열 수 없습니다: {}: {e}", log_path.display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = Process::new(std::env::current_exe()?);
    cmd.arg("serve")
        .args(args.settings.to_args()?)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    detach_flags(&mut cmd);
    keep_own_stdio_from_child();
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("서버 프로세스를 띄울 수 없습니다: {e}"))?;

    // 자식이 잠금을 잡고 상태 파일에 자기 PID를 적을 때까지 기다린다. 예전 서버가 남긴 파일과
    // 섞이지 않도록 PID가 자식과 같은 것만 인정한다.
    let child_pid = child.id();
    let deadline = Instant::now() + DETACH_WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "서버가 바로 종료되었습니다({status}). 이유는 로그에 있습니다: {}",
                log_path.display()
            );
        }
        if let Some(info) = Info::read(&paths).filter(|i| i.pid == child_pid) {
            println!(
                "vgraph-mcp 서버를 창 없이 시작했습니다: {} (PID {child_pid}, 로그 {})",
                mcp_url(info.addr, info.tls),
                log_path.display()
            );
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "서버가 {}초 안에 준비되지 않았습니다 (PID {child_pid}). 로그를 확인하세요: {}",
            DETACH_WAIT.as_secs(),
            log_path.display()
        );
        std::thread::sleep(PROBE_POLL);
    }
}

/// 자식이 이 명령의 콘솔·프로세스 그룹에서 떨어져 나가도록 한다 (창이 뜨지 않고, 부모가 끝나도 남는다).
#[cfg(windows)]
fn detach_flags(cmd: &mut Process) {
    use std::os::windows::process::CommandExt;
    /// 콘솔을 아예 붙이지 않는다 (콘솔 창도, conhost 프로세스도 생기지 않는다).
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Windows 는 자식을 만들 때 **상속 가능한 핸들을 전부** 물려준다. 이 명령의 표준 입출력이 파이프
/// (`$(vgraph-mcp serve --detach)`, 출력을 캡처하는 도구)이면 상주 서버가 그 파이프의 쓰기 끝을 쥐고 있어
/// 읽는 쪽이 서버가 끝날 때까지 EOF 를 보지 못한다. 그래서 자식을 만들기 전에 이 프로세스의 표준 핸들을
/// 상속 불가로 바꾼다 (자식의 표준 입출력은 로그 파일·NUL 로 따로 지정되어 영향이 없다).
#[cfg(windows)]
fn keep_own_stdio_from_child() {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    for handle in [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ] {
        if !handle.is_null() {
            // SAFETY: 이 프로세스가 열어 둔 표준 핸들에 플래그만 바꾼다. 유효하지 않은 핸들이면 호출이 0 을
            // 돌려줄 뿐이고(결과는 보지 않는다), 메모리를 읽거나 쓰지 않는다.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// unix 는 자식의 표준 입출력을 지정하면 원래 파이프가 자식에게 남지 않는다 (할 일 없음).
#[cfg(unix)]
fn keep_own_stdio_from_child() {}

#[cfg(unix)]
fn detach_flags(cmd: &mut Process) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// 데이터 폴더에서 떠 있는 서버의 주소·PID (기동 중이면 아직 없을 수 있다).
pub struct RunningServer {
    pub pid: Option<u32>,
    pub addr: Option<SocketAddr>,
    /// HTTPS 로 서비스 중이다.
    pub tls: bool,
    /// 서버가 시작할 때 받은 인자의 기록(상태 파일이 없거나 옛 형식이면 `Unrecorded`).
    pub start: StartRecord,
}

/// 떠 있는 서버가 없으면 `None`. 아무것도 만들지 않는다.
pub fn running_server(root: &Path) -> io::Result<Option<RunningServer>> {
    Ok(match probe(&Paths::of(root))? {
        Probe::Stopped => None,
        Probe::Running(info) => Some(RunningServer {
            pid: info.as_ref().map(|i| i.pid),
            tls: info.as_ref().is_some_and(|i| i.tls),
            start: info
                .as_ref()
                .map_or(StartRecord::Unrecorded, |i| i.start.clone()),
            addr: info.map(|i| i.addr),
        }),
    })
}

/// 내부 명령 `start-args`: 떠 있는 서버의 시작 인자 기록을 줄 단위로 출력한다. 업데이트 도구(`vgraph update
/// --restart-server`)가 읽는다. 떠 있지 않거나 기록이 없으면 아무것도 출력하지 않고 0으로 끝난다.
///   `INLINE_TOKEN`  토큰을 `--token` 값으로 받아 인자를 기록하지 않았다
///   `ARG <값>`      `serve` 뒤에 붙일 인자 하나 (값 뒤의 줄바꿈 전까지가 전부)
fn start_args(root: &Path) -> anyhow::Result<()> {
    let record = running_server(root)?.map(|s| s.start);
    let mut out = String::new();
    match record {
        Some(StartRecord::InlineToken) => out.push_str("INLINE_TOKEN\n"),
        Some(StartRecord::Args(args)) => {
            for arg in args {
                out.push_str("ARG ");
                out.push_str(&arg);
                out.push('\n');
            }
        }
        Some(StartRecord::Unrecorded) | None => {}
    }
    print!("{out}");
    Ok(())
}

/// 종료 신호(Ctrl-C, unix에서는 SIGTERM) 또는 끄기 요청 파일이 나타날 때까지 기다린다.
fn wait_for_shutdown(stop_file: &Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("종료 감시를 시작하지 못했습니다: {e}"))?;
    // select!의 세 분기는 모두 취소해도 잃는 상태가 없다(신호 대기·타이머 폴링).
    runtime.block_on(async {
        tokio::select! {
            () = termination_signal() => {}
            () = stop_requested(stop_file) => {}
        }
    });
    Ok(())
}

async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    // 신호 핸들러를 못 걸면 곧바로 끝내지 않고 영원히 기다린다(서버는 계속 뜬다).
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn stop_requested(stop_file: &Path) {
    // 250ms마다 파일 존재만 확인하는 짧은 시스템 호출이라 이 전용 런타임에서 막아도 무방하다.
    while !stop_file.exists() {
        tokio::time::sleep(STOP_POLL).await;
    }
}

// ---------------------------------------------------------------- 접속별 신원

/// HTTP 요청 문맥(헤더·세션)을 MCP 신원으로 바꿔 서버에 넘긴다.
struct Router {
    server: Arc<McpServer>,
    default_user: String,
    /// 작업 공간 헤더가 없는 MCP 세션에 서버가 붙인 이름 (세션 ID -> `s-<ULID>`).
    /// 세션이 DELETE로 끝나면 `forget`이 지운다.
    auto_workspaces: Mutex<HashMap<String, String>>,
}

impl Router {
    fn new(server: Arc<McpServer>, default_user: String) -> Self {
        Router {
            server,
            default_user,
            auto_workspaces: Mutex::new(HashMap::new()),
        }
    }

    // 가드는 이 파일의 동기 함수 안에서만 쓰고 .await 너머로 넘기지 않는다.
    // 맵 자체는 패닉으로 깨지지 않으므로 중독된 잠금도 그대로 쓴다.
    fn map(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.auto_workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn forget(&self, session: &str) {
        self.map().remove(session);
    }

    fn handle(&self, msg: &Value, ctx: &RequestContext) -> Option<Value> {
        match self.identity(msg, ctx) {
            Ok(id) => self.server.handle_message_as(msg, &id),
            // 헤더가 잘못됐다: 요청이면 JSON-RPC 오류로 거절하고, 알림이면 답할 곳이 없다.
            Err(e) => msg.get("id").map(|id| {
                json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": INVALID_REQUEST, "message": e.to_string() } })
            }),
        }
    }

    fn identity(&self, msg: &Value, ctx: &RequestContext) -> anyhow::Result<Identity> {
        // 인증된 주체(토큰의 사용자)가 있으면 그것이 사용자다 — 자기 신고 헤더는 보지 않는다.
        let user = match ctx.principal.as_deref() {
            Some(authenticated) => authenticated,
            None => ctx
                .header(USER_HEADER)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .unwrap_or(&self.default_user),
        };
        if let Some(name) = ctx.header(WORKSPACE_HEADER) {
            return Identity::new(user, Some(name.trim().to_string()));
        }
        // initialize는 작업 공간을 쓰지 않는다. 거절될 수도 있는 세션에 이름을 미리 붙이지 않는다.
        let is_initialize = msg.get("method").and_then(Value::as_str) == Some("initialize");
        match (&ctx.session_id, is_initialize) {
            (Some(session), false) => {
                let name = self
                    .map()
                    .entry(session.clone()) // 맵의 키로 소유권이 필요해서 복제
                    .or_insert_with(|| format!("s-{}", ulid::Ulid::new()))
                    .clone(); // 가드를 놓은 뒤 Identity로 넘기려면 값을 가져와야 한다
                Identity::new(user, Some(name))
            }
            _ => Identity::new(user, None),
        }
    }
}

// ---------------------------------------------------------------- stop · status

fn status(root: &Path) -> anyhow::Result<()> {
    let paths = Paths::of(root);
    match probe(&paths)? {
        Probe::Stopped => {
            println!("서버가 떠 있지 않습니다 (데이터 폴더 {})", root.display());
            std::process::exit(1);
        }
        Probe::Running(info) => {
            println!("상태: 실행 중");
            match info {
                Some(i) => {
                    println!("주소: {}", mcp_url(i.addr, i.tls));
                    println!("PID: {}", i.pid);
                }
                None => println!("주소: 아직 알 수 없음 (기동 중)"),
            }
            println!("데이터 폴더: {}", root.display());
            Ok(())
        }
    }
}

pub fn stop(root: &Path) -> anyhow::Result<()> {
    let paths = Paths::of(root);
    let pid = match probe(&paths)? {
        Probe::Stopped => {
            println!("서버가 떠 있지 않습니다 (데이터 폴더 {})", root.display());
            return Ok(());
        }
        Probe::Running(info) => info.map(|i| i.pid),
    };

    // 1) 정상 종료 요청: 서버가 이 파일을 보고 스스로 내려간다. Windows에는 SIGTERM이 없고
    //    콘솔 제어 이벤트는 대상 프로세스의 콘솔에 붙어야 보낼 수 있어, 두 OS 모두 같은 방식을 쓴다.
    fs::write(&paths.stop, b"").map_err(|e| {
        anyhow::anyhow!(
            "끄기 요청 파일을 쓸 수 없습니다: {}: {e}",
            paths.stop.display()
        )
    })?;
    if wait_until_stopped(&paths, STOP_GRACE)? {
        remove_if_present(&paths.stop)?;
        println!("서버를 종료했습니다(정상 종료).");
        return Ok(());
    }

    // 2) 정해진 시간 안에 끝나지 않았다: 강제 종료. 잠금을 쥐고 있는 쪽이 이 PID의 주인이다.
    let Some(pid) = pid else {
        remove_if_present(&paths.stop)?;
        anyhow::bail!("서버가 종료 요청에 응답하지 않고 PID도 알 수 없어 강제 종료할 수 없습니다");
    };
    force_kill(pid)?;
    let stopped = wait_until_stopped(&paths, KILL_WAIT)?;
    remove_if_present(&paths.stop)?;
    anyhow::ensure!(
        stopped,
        "PID {pid}를 강제 종료했지만 서버가 여전히 잠금을 쥐고 있습니다"
    );
    println!("서버가 정상 종료 요청에 응답하지 않아 강제 종료했습니다(PID {pid}).");
    Ok(())
}

fn wait_until_stopped(paths: &Paths, within: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + within;
    loop {
        if matches!(probe(paths)?, Probe::Stopped) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(PROBE_POLL);
    }
}

fn force_kill(pid: u32) -> anyhow::Result<()> {
    let pid_arg = pid.to_string();
    #[cfg(windows)]
    let status = Process::new("taskkill")
        .args(["/PID", &pid_arg, "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(windows))]
    let status = Process::new("kill")
        .args(["-KILL", &pid_arg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = status
        .map_err(|e| anyhow::anyhow!("프로세스를 강제 종료하지 못했습니다(PID {pid}): {e}"))?;
    anyhow::ensure!(
        status.success(),
        "프로세스를 강제 종료하지 못했습니다(PID {pid}): {status}"
    );
    Ok(())
}
