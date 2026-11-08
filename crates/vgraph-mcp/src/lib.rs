//! vgraph-mcp — 에이전트용 MCP 창구 (stdio, newline-delimited JSON-RPC 2.0).
//!
//! 아키텍처 결정: 기존 Rust API의 **대체가 아니라 별도 어댑터 레이어** —
//! 엔진 코어 불가침, 도구는 과업 단위로 큐레이션 (1:1 미러링 금지).
//! - 핸들 없음: 모든 파라미터는 주소 문자열(`graph://…` 또는 `ns/slug[@branch][#node]`).
//! - 무상태 호출: working copy는 WAL로 지속 → 호출 간 resume (CLI와 동일 패턴).
//! - 충돌·위반은 오류가 아니라 구조화된 다음-행동 옵션으로 반환 (인터페이스 §6).
//! - SDK 미사용: 전송 표면이 작아(newline JSON-RPC) 자체 구현 — `handle_message`가
//!   순수 함수라 프로토콜 계층까지 결정적으로 테스트된다.

use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use vgraph_core::canon::{value_from_json, write_value};
use vgraph_core::{
    diff, GraphAddr, GraphId, Hash256, IdGen, NodeSel, Props, SystemIdGen, Uuid, Value, WorkingId,
};
use vgraph_engine::interchange::{self, InterchangeError, ProblemCode};
use vgraph_engine::view::{FindHit, FindOptions, FindOptionsError, TextFilter};
use vgraph_engine::{
    Choice, CommitOutcome, ConflictItem, ConflictReport, EditSession, EditStartError, Engine,
    EngineError, EngineOptions, EngineResult, FindQuery, FsProvider, MergeOpts, MergeOutcome,
    Resolution, Schema, SchemaMode, Session, SideRef, View, WalkQuery,
};
use vgraph_vec::providers::embedder_from_spec;
use vgraph_vec::{embed_pending, Chunker, EmbedSpec, Embedder, VecIndex};

// ---- 명령줄 진입점(`vgraph-mcp` 바이너리가 쓰는 부분) ----
// 바이너리의 본체가 이 라이브러리로 옮겨 왔다. 상주 서버 하위 명령 모듈(`serve`·`service`·`tokens`)도 함께 온다.
use clap::{Args, Parser};
use std::process::ExitCode;

#[cfg(feature = "server")]
mod serve;
#[cfg(feature = "server")]
mod service;
#[cfg(feature = "server")]
mod tokens;

// 하위 모듈(`serve.rs`)이 바이너리 시절처럼 `vgraph_mcp::…` 경로로 이 크레이트의 공개 항목을 가리킨다.
extern crate self as vgraph_mcp;

/// initialize 응답의 서버 전역 안내 — 도구 설명에 반복하기 어려운 규약을 한 곳에.
/// pub: 이 서버를 다른 MCP 서버에 내장할 때
/// 호스트가 자기 instructions에 이어 붙일 수 있도록.
pub const SERVER_INSTRUCTIONS: &str = "\
vgraph = 버저닝·브랜칭 그래프 데이터베이스. 규약:\n\
- 주소: \"ns/slug[@branch][.commit][#node]\" (graph:// 접두 생략 가능, 기본 브랜치 main). \
읽기 응답의 epoch = 그 버전의 commit hash — 재현 가능한 버전 식별자.\n\
- 편집 흐름: vgraph_edit로 op를 쌓고(호출 간 유지됨) vgraph_commit으로 확정. \
충돌은 오류가 아니라 report.items[index] — vgraph_merge의 theirs:[index]로 해소 가능.\n\
- external link: prop 값 {\"$ref\":\"graph://ns/slug#node\"} (축약 ns/slug#node도 허용 — 자동 보정). \
커밋 시 lockfile에 대상 버전이 핀 고정되고, 대상 전진은 vgraph_links(drifts)로 감지, \
upgrade의 to에는 drifts 응답의 target_head를 사용.\n\
- 유사도 검색: 노드 props의 embed_target(문자열|배열)로 임베딩 텍스트를 지정 가능 \
(미지정 시 전체 텍스트 폴백). similar 검색 전 pending_embeddings>0이면 vgraph_embed_pending 먼저.\n\
- slug가 __로 시작하는 노드(__schema, __lockfile)는 시스템 노드 — 직접 수정 금지.\n\
- 조회 패턴(도구 호출 2번 — 시작 노드 찾기 + 이웃 확장): ① vgraph_find로 시작 노드를 찾는다 \
(예: {\"graph\":\"ns/slug\",\"labels\":[\"item\"],\"text\":\"배송\",\"limit\":5}) → \
② vgraph_neighbors에 그 slug를 addr로 주고 \
{\"addr\":\"ns/slug#<slug>\",\"depth\":2,\"expand_labels\":[\"Category\",\"Brand\"],\
\"include\":[\"labels\",\"props\"]} 를 준다 (1단계는 모든 이웃, 2단계는 expand_labels 노드에서만 뻗음). \
전체 나열은 vgraph_scan(kind=nodes|edges, next_cursor로 이어 받기).
- 동시 편집: 여러 프로세스·에이전트가 같은 사용자로 같은 그래프·같은 브랜치를 동시에 편집할 때는 각자 다른 --workspace 이름으로 서버를 띄워야 한다. 이름 없이(기본 작업 공간) 동시에 편집하면 편집 내용이 사라질 수 있다. 작업 공간 이름은 동시에 쓰는 프로세스마다 고유해야 하며, 같은 이름을 동시에 쓰면 이름 없이 쓸 때와 같이 편집이 사라질 수 있다. 이름이 다르면 커밋할 때 자동으로 합쳐지고, 같은 값을 다르게 바꿨으면 충돌로 보고된다.";

pub struct McpConfig {
    pub root: PathBuf,
    pub user: String,
    /// 임베더 spec: "toy" | "toy:<dim>" | "openai:<model>" | "ollama:<model>[@url]" | "http:<url>".
    /// None = similar 검색 비활성.
    pub embedder: Option<String>,
    /// 임베딩 정책 (필드 선택 + 청킹).
    pub embed: EmbedSpec,
}

/// `vgraph_export`·`vgraph_import`의 `path`가 닿을 수 있는 곳.
/// stdio 서버는 호출자가 곧 로컬 사용자이므로 제한이 없고(기본), 원격 `serve`는 교환 폴더 안으로 가둔다 —
/// 안 그러면 원격 호출자가 데이터 폴더의 `server.stop`·`registry.jsonl` 등을 쓰고 읽을 수 있다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PathPolicy {
    /// 어떤 경로든 그대로 쓴다 (stdio 기본).
    #[default]
    Unrestricted,
    /// 이 폴더 안의 파일만 허용한다. 상대 경로는 이 폴더 기준으로 푼다.
    ExchangeDir(PathBuf),
}

impl PathPolicy {
    /// 호출자가 준 `path`를 실제로 열 경로로 바꾼다. 거부 문구에는 파일 내용이나 실제 경로를 싣지 않는다.
    ///
    /// 정규화(링크를 따라간 실제 위치)한 결과가 교환 폴더 안일 때만 통과한다. 없는 파일은 부모 폴더를
    /// 정규화해 비교한다. 검사와 실제 열기 사이에 링크를 바꿔치기하는 경합은 막지 못한다(교환 폴더에
    /// 쓸 수 있는 쪽이 없다는 전제).
    pub fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        let PathPolicy::ExchangeDir(dir) = self else {
            return Ok(PathBuf::from(raw));
        };
        const DENIED: &str =
            "path는 서버의 교환 폴더 안의 파일만 쓸 수 있습니다 (상대 경로는 교환 폴더 기준)";
        if raw.is_empty() || raw.contains('\u{0}') {
            return Err(DENIED.into());
        }
        let base = std::fs::canonicalize(dir)
            .map_err(|_| "서버의 교환 폴더를 열 수 없습니다".to_string())?;
        let joined = base.join(raw);
        let resolved = match std::fs::canonicalize(&joined) {
            Ok(real) => real,
            Err(_) => {
                // 끊어진 링크는 정규화에 실패하지만 이미 무언가가 있는 자리다 — 따라가지 않고 거부한다.
                if std::fs::symlink_metadata(&joined).is_ok() {
                    return Err(DENIED.into());
                }
                let name = joined.file_name().ok_or_else(|| DENIED.to_string())?;
                let parent = joined.parent().ok_or_else(|| DENIED.to_string())?;
                let parent = std::fs::canonicalize(parent).map_err(|_| DENIED.to_string())?;
                parent.join(name)
            }
        };
        if resolved != base && resolved.starts_with(&base) {
            Ok(resolved)
        } else {
            Err(DENIED.into())
        }
    }
}

pub struct McpServer {
    engine: Engine,
    cfg: McpConfig,
    /// `path` 인자 정책. 기본은 제한 없음 — `McpConfig`는 구조체 리터럴로 만들어지므로 필드를 더하지 않는다.
    path_policy: PathPolicy,
    embedder: Option<Box<dyn Embedder>>,
    /// 신원을 지정하지 않은 호출(`handle_message`·`handle_line`)이 쓰는 서버 기본 신원.
    /// `McpConfig`는 외부 구조체 리터럴이라 필드를 더하지 않고 여기에 둔다.
    identity: Identity,
    /// 나눠 올리는 중인 가져오기 문서 — 서버 프로세스 메모리에만 둔다(디스크 흔적 없음).
    /// 락은 짧은 동기 구간에서만 잡고 `.await`를 넘기지 않으며, 가져오기 실행 중에는 쥐지 않는다.
    uploads: Mutex<Uploads>,
}

/// 요청 하나가 따르는 신원 — 사용자와 (선택) 이름 있는 작업 공간.
/// 작업 공간 이름은 만들 때 한 번 검증하므로, 가진 값은 항상 유효하다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    user: String,
    workspace: Option<String>,
}

/// 작업 공간 이름 규칙: 영문·숫자·-·_ (비어 있으면 안 됨).
fn check_workspace_name(workspace: &Option<String>) -> anyhow::Result<()> {
    if let Some(w) = workspace {
        let ok = !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            anyhow::bail!("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다: \"{w}\"");
        }
    }
    Ok(())
}

impl Identity {
    /// `workspace`가 `None`이면 기본 작업 공간. 이름 규칙을 어기면 오류.
    pub fn new(user: impl Into<String>, workspace: Option<String>) -> anyhow::Result<Self> {
        check_workspace_name(&workspace)?;
        Ok(Identity {
            user: user.into(),
            workspace,
        })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }
}

/// 나눠 올리기 상한.
///
/// 기본값의 근거: 문서 하나 64MiB(수 MB급 그래프의 십여 배이고, 동시 업로드 수와 곱해도 서버
/// 메모리가 수백 MB 안쪽), 동시 8개(사람·에이전트 몇 명이 각자 올리는 규모), 유휴 10분
/// (조각 사이 네트워크 지연·재시도에는 넉넉하고 버려진 업로드는 금방 치워짐).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimits {
    /// 업로드 하나가 모을 수 있는 문서의 최대 바이트.
    pub max_bytes: usize,
    /// 동시에 열어 둘 수 있는 업로드 수(서버 전체).
    pub max_open: usize,
    /// 마지막으로 쓴 때부터 이 시간이 지나면 만료되어 폐기된다.
    pub idle: Duration,
}

impl Default for UploadLimits {
    fn default() -> Self {
        UploadLimits {
            max_bytes: 64 * 1024 * 1024,
            max_open: 8,
            idle: Duration::from_secs(600),
        }
    }
}

/// 업로드를 못 찾은 모든 경우(없음·만료·다른 신원)에 같은 문장 — 존재 여부를 흘리지 않는다.
const UPLOAD_GONE: &str = "없는 업로드이거나 만료되었습니다 — upload=begin부터 다시 시작하세요";

/// `begin`에서 정해 두는 가져오기 인자 — `commit`이 한 번에 가져오기와 같은 규칙으로 쓴다.
struct ImportPlan {
    graph: String,
    via: Option<String>,
    message: String,
}

impl ImportPlan {
    /// 대상·모드·via_branch·message 인자를 검증해 읽는다(한 번에 가져오기와 같은 규칙·같은 문장).
    fn from_args(a: &J) -> Result<Self, String> {
        let graph = s(a, "graph")?;
        let mode = opt_str(a, "mode")?.unwrap_or("replace");
        let via = opt_str(a, "via_branch")?;
        match (mode, via) {
            ("replace", None) | ("merge", Some(_)) => {}
            ("replace", Some(_)) => {
                return Err("via_branch는 mode=merge에서만 씁니다 (replace에서는 빼세요)".into())
            }
            ("merge", None) => return Err(
                "mode=merge에는 via_branch(가져오기 전용 브랜치 이름)가 필요합니다 — 기본값 없음"
                    .into(),
            ),
            (other, _) => {
                return Err(format!(
                    "mode는 replace 또는 merge여야 합니다 (받은 값: {other})"
                ))
            }
        }
        Ok(ImportPlan {
            graph: graph.to_string(),
            via: via.map(str::to_string),
            message: opt_str(a, "message")?
                .unwrap_or("import (vgraph-json/1)")
                .to_string(),
        })
    }
}

struct Upload {
    owner: Identity,
    plan: ImportPlan,
    text: String,
    next_seq: u64,
    touched: Instant,
}

/// 열려 있는 업로드 모음. 시각은 호출자가 넘긴다(만료 규칙을 시계 없이 시험하기 위해).
/// 만료된 업로드는 다음 접근(begin·append·commit·abort) 때 한꺼번에 폐기한다.
struct Uploads {
    limits: UploadLimits,
    open: HashMap<String, Upload>,
}

impl Uploads {
    fn new(limits: UploadLimits) -> Self {
        Uploads {
            limits,
            open: HashMap::new(),
        }
    }

    fn sweep(&mut self, now: Instant) {
        let idle = self.limits.idle;
        self.open
            .retain(|_, u| now.saturating_duration_since(u.touched) < idle);
    }

    fn begin(
        &mut self,
        now: Instant,
        owner: &Identity,
        plan: ImportPlan,
    ) -> Result<String, String> {
        self.sweep(now);
        if self.open.len() >= self.limits.max_open {
            return Err(format!(
                "동시에 열어 둘 수 있는 업로드는 {}개입니다 — 끝난 업로드는 commit, 필요 없는 것은 abort 하세요",
                self.limits.max_open
            ));
        }
        // 추측할 수 없게 ULID 둘(각 80비트 난수)을 잇는다. 신원 검사가 따로 있어 id는 이중 방어선이다.
        let id = format!("up-{}-{}", SystemIdGen.next_uuid(), SystemIdGen.next_uuid());
        self.open.insert(
            id.clone(),
            Upload {
                owner: owner.clone(), // 업로드가 신원을 소유해 이후 접근자와 대조한다
                plan,
                text: String::new(),
                next_seq: 0,
                touched: now,
            },
        );
        Ok(id)
    }

    /// 시작한 신원의 업로드만 돌려준다 — 없음·만료·남의 것은 같은 오류.
    /// `graph`가 begin 때와 다르면(내 업로드일 때만 판정) 거부한다.
    fn get_mut(
        &mut self,
        now: Instant,
        id: &str,
        owner: &Identity,
        graph: &str,
    ) -> Result<&mut Upload, String> {
        self.sweep(now);
        let up = self
            .open
            .get_mut(id)
            .filter(|u| u.owner == *owner)
            .ok_or_else(|| UPLOAD_GONE.to_string())?;
        if up.plan.graph != graph {
            return Err("graph는 begin에서 준 값과 같아야 합니다".into());
        }
        Ok(up)
    }

    fn append(
        &mut self,
        now: Instant,
        id: &str,
        owner: &Identity,
        graph: &str,
        seq: u64,
        chunk: &str,
    ) -> Result<(usize, u64), String> {
        let max = self.limits.max_bytes;
        let up = self.get_mut(now, id, owner, graph)?;
        if seq != up.next_seq {
            return Err(format!(
                "seq가 어긋났습니다: 기대 {} (받은 값 {seq}) — 조각 번호는 0부터 하나씩, 이미 받은 조각은 다시 보내지 마세요",
                up.next_seq
            ));
        }
        let new_len = up.text.len().saturating_add(chunk.len());
        if new_len > max {
            return Err(format!(
                "업로드 문서가 상한 {max}바이트를 넘습니다 (받은 {}바이트 + 이번 조각 {}바이트) — 업로드는 그대로 남아 있습니다",
                up.text.len(),
                chunk.len()
            ));
        }
        up.text.push_str(chunk);
        up.next_seq = up.next_seq.saturating_add(1);
        up.touched = now;
        Ok((up.text.len(), up.next_seq))
    }

    /// 업로드를 꺼내 지운다(commit·abort). 검사를 통과한 경우에만 지운다.
    fn take(
        &mut self,
        now: Instant,
        id: &str,
        owner: &Identity,
        graph: &str,
    ) -> Result<Upload, String> {
        self.get_mut(now, id, owner, graph)?;
        self.open.remove(id).ok_or_else(|| UPLOAD_GONE.to_string())
    }
}

fn default_identity(cfg: &McpConfig) -> Identity {
    Identity {
        user: cfg.user.clone(), // cfg는 서버가 계속 소유(root·embed 사용)하므로 사용자 이름만 복제
        workspace: None,
    }
}

fn resolve_embedder(spec: &Option<String>) -> anyhow::Result<Option<Box<dyn Embedder>>> {
    match spec {
        None => Ok(None),
        Some(s) => Ok(Some(
            embedder_from_spec(s).map_err(|e| anyhow::anyhow!("{e}"))?,
        )),
    }
}

impl McpServer {
    pub fn new(cfg: McpConfig) -> anyhow::Result<Self> {
        let engine = Engine::open(
            Box::new(FsProvider::new(&cfg.root).map_err(|e| anyhow::anyhow!("{e}"))?),
            Arc::new(vgraph_core::SystemIdGen),
            Arc::new(vgraph_core::SystemClock),
            EngineOptions::default(),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let embedder = resolve_embedder(&cfg.embedder)?;
        let identity = default_identity(&cfg);
        Ok(McpServer {
            engine,
            cfg,
            embedder,
            identity,
            path_policy: PathPolicy::default(),
            uploads: Mutex::new(Uploads::new(UploadLimits::default())),
        })
    }

    /// 테스트·임베딩 주입용 (TestIdGen/TestClock 엔진 등). 잘못된 spec은 즉시 panic.
    pub fn with_engine(engine: Engine, cfg: McpConfig) -> Self {
        let embedder = resolve_embedder(&cfg.embedder).expect("embedder spec");
        let identity = default_identity(&cfg);
        McpServer {
            engine,
            cfg,
            embedder,
            identity,
            path_policy: PathPolicy::default(),
            uploads: Mutex::new(Uploads::new(UploadLimits::default())),
        }
    }

    /// 내보내기·가져오기 `path`의 허용 범위를 정한다. 원격 `serve`가 교환 폴더로 가둘 때 쓴다.
    #[must_use]
    pub fn with_path_policy(mut self, policy: PathPolicy) -> Self {
        self.path_policy = policy;
        self
    }

    /// 나눠 올리기(`vgraph_import`의 `upload`) 상한을 바꾼다. 기본값은 [`UploadLimits::default`].
    pub fn with_upload_limits(self, limits: UploadLimits) -> Self {
        let mut uploads = self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
        uploads.limits = limits;
        drop(uploads);
        self
    }

    /// 이 서버가 쓸 작업 공간 이름을 정한다 (`None` = 기본). 이름 규칙: 영문·숫자·-·_ (비어 있으면 안 됨).
    pub fn with_workspace(mut self, workspace: Option<String>) -> anyhow::Result<Self> {
        check_workspace_name(&workspace)?;
        self.identity.workspace = workspace;
        Ok(self)
    }

    // ---------- JSON-RPC ----------

    pub fn handle_line(&self, line: &str) -> Option<String> {
        let msg: J = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => return None, // 프레이밍 오류는 무시 (stderr 로깅은 bin 소관)
        };
        self.handle_message(&msg).map(|r| r.to_string())
    }

    /// 서버 기본 신원으로 처리한다.
    pub fn handle_message(&self, msg: &J) -> Option<J> {
        self.handle_message_as(msg, &self.identity)
    }

    /// 주어진 신원으로 요청 하나를 처리한다. `&self`만 받으므로 여러 스레드가 같은 서버를
    /// 서로 다른 신원으로 동시에 불러도 되고, 신원은 이 호출 안에서만 쓰인다.
    pub fn handle_message_as(&self, msg: &J, id: &Identity) -> Option<J> {
        // 프로토콜 골격은 mcp-kit 공용 계층 (P5에서 추출 — 다른 MCP 서버와 공유).
        mcp_kit::handle_message(
            msg,
            "vgraph-mcp",
            env!("CARGO_PKG_VERSION"),
            SERVER_INSTRUCTIONS,
            tool_defs,
            |name, args| self.call_tool(id, name, args),
        )
    }

    // ---------- 도구 디스패치 ----------

    fn sess(&self, id: &Identity) -> Session {
        self.engine.session(id.user())
    }

    fn call_tool(&self, id: &Identity, name: &str, a: &J) -> Result<J, String> {
        match name {
            "vgraph_list" => self.t_list(id),
            "vgraph_create" => self.t_create(id, a),
            "vgraph_show" => self.t_show(id, a),
            "vgraph_search" => self.t_search(id, a),
            "vgraph_neighbors" => self.t_neighbors(id, a),
            "vgraph_find" => self.t_find(id, a),
            "vgraph_scan" => self.t_scan(id, a),
            "vgraph_export" => self.t_export(id, a),
            "vgraph_import" => self.t_import(id, a),
            "vgraph_edit" => self.t_edit(id, a),
            "vgraph_pending" => self.t_pending(id, a),
            "vgraph_commit" => self.t_commit(id, a),
            "vgraph_discard" => self.t_discard(id, a),
            "vgraph_branch" => self.t_branch(id, a),
            "vgraph_merge" => self.t_merge(id, a),
            "vgraph_log" => self.t_log(id, a),
            "vgraph_diff" => self.t_diff(id, a),
            "vgraph_links" => self.t_links(id, a),
            "vgraph_admin" => self.t_admin(id, a),
            "vgraph_embed_pending" => self.t_embed_pending(id, a),
            other => Err(format!("unknown tool: {other}")),
        }
    }

    fn t_list(&self, id: &Identity) -> Result<J, String> {
        let sess = self.sess(id);
        let mut out = Vec::new();
        for (ns, slug, id) in sess.graphs() {
            let head = sess
                .resolve(&format!("graph://{ns}/{slug}"))
                .map(|(_, h)| J::String(h.to_string()))
                .unwrap_or(J::Null);
            out.push(json!({ "name": format!("{ns}/{slug}"), "id": id.to_string(), "head": head }));
        }
        Ok(json!({ "graphs": out }))
    }

    fn t_create(&self, id: &Identity, a: &J) -> Result<J, String> {
        let name = s(a, "name")?;
        let (ns, slug) = name.split_once('/').ok_or("형식: ns/slug")?;
        let id = self.sess(id).create_graph(ns, slug).map_err(ee)?;
        Ok(json!({ "created": name, "id": id.to_string() }))
    }

    fn view_of(&self, sess: &Session, addr: &str) -> Result<(GraphAddr, Arc<View>), String> {
        let parsed: GraphAddr = full_addr(addr)
            .parse()
            .map_err(|e| format!("주소 형식 오류: {e}"))?;
        let (g, c) = sess.resolve(&full_addr(addr)).map_err(|e| e.to_string())?;
        let v = sess.checkout(g, c).map_err(ee)?;
        Ok((parsed, v))
    }

    fn t_show(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (parsed, v) = self.view_of(&sess, s(a, "addr")?)?;
        match &parsed.node {
            Some(sel) => {
                let u = node_in_view(&v, sel)?;
                Ok(node_j(&v, u))
            }
            None => {
                let state = v.to_state();
                let is_sys = |slug: &Option<vgraph_core::Key>| {
                    slug.as_deref().is_some_and(|s| s.starts_with("__"))
                };
                let sys_count = state.nodes().filter(|n| is_sys(&n.slug)).count();
                let nodes: Vec<J> = state
                    .nodes()
                    .take(200)
                    .map(|n| {
                        let mut o = json!({
                            "uuid": n.uuid.to_string(),
                            "slug": n.slug.as_deref(),
                            "labels": n.labels.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
                        });
                        if is_sys(&n.slug) {
                            o["system"] = json!(true); // UX-4: 시스템 노드 명시
                        }
                        o
                    })
                    .collect();
                let edges: Vec<J> = state
                    .edges()
                    .take(200)
                    .map(|e| {
                        json!({
                            "uuid": e.uuid.to_string(), "type": e.etype.to_string(),
                            "from": e.from.to_string(), "to": e.to.to_string(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "epoch": v.epoch().to_string(),
                    "node_count": v.node_count() - sys_count, // 사용자 노드 수
                    "system_nodes": sys_count,
                    "edge_count": v.edge_count(),
                    "nodes": nodes, "edges": edges,
                    "truncated": v.node_count() > 200 || v.edge_count() > 200,
                }))
            }
        }
    }

    fn t_search(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let compact = compact_of(a)?;
        let (_, v) = self.view_of(&sess, s(a, "graph")?)?;
        let mode = a.get("mode").and_then(|m| m.as_str()).unwrap_or("prop");
        match mode {
            "prop" => {
                let key = s(a, "key")?;
                let val = j2v(a.get("value").ok_or("필수 인자 누락: value")?)?;
                let hits: Vec<J> = v
                    .find_by_prop(key, &val)
                    .into_iter()
                    .map(|u| id_cols(&v, u, compact))
                    .collect();
                let mut out = json!({ "mode": "prop", "hits": hits });
                put_epoch(&mut out, &v, compact);
                Ok(out)
            }
            "similar" => {
                let embedder = self
                    .embedder
                    .as_ref()
                    .ok_or("임베더 미설정 — 서버를 --embedder toy|openai:<model>|ollama:<model> 로 실행하세요")?;
                let query = s(a, "query")?;
                let k = a.get("k").and_then(|k| k.as_u64()).unwrap_or(5) as usize;
                let idx = VecIndex::for_graph(&self.cfg.root, v.graph(), &embedder.model_id())
                    .map_err(|e| e.to_string())?;
                let pending = idx
                    .pending(&v, &self.cfg.embed)
                    .map_err(|e| e.to_string())?
                    .len();
                let qv = embedder.embed(query).map_err(|e| e.to_string())?;
                let hits: Vec<J> = idx
                    .search(&v, &qv, k)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|h| {
                        let mut o = id_cols(&v, h.node, compact);
                        o["score"] = json!(h.score);
                        o["part"] = json!(h.part); // 최고 점수 청크 (provenance)
                        o
                    })
                    .collect();
                let mut out = json!({
                    "mode": "similar",
                    "model": embedder.model_id(),
                    "hits": hits,
                    "pending_embeddings": pending, // >0이면 vgraph_embed_pending 권장
                });
                put_epoch(&mut out, &v, compact);
                Ok(out)
            }
            other => Err(format!("mode는 prop|similar: {other}")),
        }
    }

    fn t_neighbors(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let compact = compact_of(a)?;
        let (parsed, v) = self.view_of(&sess, s(a, "addr")?)?;
        let sel = parsed.node.as_ref().ok_or("시작 노드 필요: …#slug")?;
        let start = node_in_view(&v, sel)?;
        let dir = match a.get("dir").and_then(|d| d.as_str()).unwrap_or("out") {
            "in" => vgraph_core::Dir::In,
            "both" => vgraph_core::Dir::Both,
            _ => vgraph_core::Dir::Out,
        };
        let depth = a.get("depth").and_then(|d| d.as_u64()).unwrap_or(1) as u32;
        if has_walk_args(a) {
            return walk_response(&v, start, dir, depth, a, compact);
        }
        let etype = a.get("etype").and_then(|t| t.as_str());
        let tv;
        let types = match etype {
            Some(t) => {
                tv = vec![t];
                Some(tv.as_slice())
            }
            None => None,
        };
        let hits: Vec<J> = v
            .neighbors(start, dir, types, None, 1, depth, None)
            .into_iter()
            .map(|h| {
                let mut o = id_cols(&v, h.node, compact);
                o["depth"] = json!(h.depth);
                if !compact {
                    o["via"] = json!(h.via.to_string());
                }
                o
            })
            .collect();
        let mut out = json!({ "hits": hits });
        put_epoch(&mut out, &v, compact);
        Ok(out)
    }

    fn t_edit(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        let (es, warnings) = open_work(&sess, g, &branch, id.workspace())?;
        let ops = a
            .get("ops")
            .and_then(|o| o.as_array())
            .ok_or("필수 인자 누락: ops[]")?;
        let mut results = Vec::new();
        for op in ops {
            let kind = op
                .get("op")
                .and_then(|k| k.as_str())
                .ok_or("op 항목에 op 필드 필요")?;
            let r = self.apply_op(id, &es, kind, op)?;
            results.push(r);
        }
        let d = es.pending();
        let mut out = json!({
            "applied": results.len(), "results": results,
            "pending": delta_summary(&d),
            "hint": "vgraph_commit으로 확정 (working copy는 호출 간 유지됨)",
        });
        attach_recovery_warnings(&mut out, warnings);
        Ok(out)
    }

    /// Ref 값 정규화 + 조기 검증 (UX-1·UX-2): 축약 "ns/slug#node" → graph:// 보정,
    /// 대상 그래프 미등록·노드 미지정은 **edit 시점**에 즉시 명확한 오류.
    fn normalize_ref(&self, id: &Identity, raw: &str) -> Result<String, String> {
        let full = if raw.starts_with("graph://") {
            raw.to_string()
        } else {
            format!("graph://{raw}")
        };
        let a: GraphAddr = full
            .parse()
            .map_err(|e| format!("링크 주소 형식 오류 ({raw}): {e}"))?;
        if self.sess(id).graph_id(&a.ns, &a.slug).is_none() {
            return Err(format!("링크 대상 그래프 미등록: {}/{}", a.ns, a.slug));
        }
        if a.node.is_none() {
            return Err(format!(
                "링크는 노드까지 지정해야 합니다: graph://{}/{}#<node>",
                a.ns, a.slug
            ));
        }
        Ok(full)
    }

    fn fix_refs(&self, id: &Identity, p: &mut Props) -> Result<(), String> {
        let keys: Vec<_> = p
            .iter()
            .filter(|(_, v)| matches!(v, Value::Ref(_)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if let Some(Value::Ref(r)) = p.get(&k) {
                let fixed = self.normalize_ref(id, r)?;
                p.insert(k, Value::Ref(fixed.into()));
            }
        }
        Ok(())
    }

    fn apply_op(&self, id: &Identity, es: &EditSession, kind: &str, op: &J) -> Result<J, String> {
        match kind {
            "add_node" => {
                let labels: Vec<String> = op
                    .get("labels")
                    .and_then(|l| l.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let labels_ref: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                let mut props = match op.get("props") {
                    Some(p) => props_from_json_obj(p)?,
                    None => Props::new(),
                };
                self.fix_refs(id, &mut props)?;
                let u = es
                    .add_node(&labels_ref, op.get("slug").and_then(|s| s.as_str()), props)
                    .map_err(ee)?;
                Ok(json!({ "op": "add_node", "uuid": u.to_string() }))
            }
            "add_edge" => {
                let from = work_node(es, s(op, "from")?)?;
                let to = work_node(es, s(op, "to")?)?;
                let mut props = match op.get("props") {
                    Some(p) => props_from_json_obj(p)?,
                    None => Props::new(),
                };
                self.fix_refs(id, &mut props)?;
                let u = es.add_edge(s(op, "etype")?, from, to, props).map_err(ee)?;
                Ok(json!({ "op": "add_edge", "uuid": u.to_string() }))
            }
            "set_prop" => {
                let node = work_node(es, s(op, "node")?)?;
                let mut value = j2v(op.get("value").ok_or("value 필요")?)?;
                if let Value::Ref(r) = &value {
                    value = Value::Ref(self.normalize_ref(id, r)?.into());
                }
                es.set_prop(node, s(op, "key")?, value).map_err(ee)?;
                Ok(json!({ "op": "set_prop" }))
            }
            "unset_prop" => {
                let node = work_node(es, s(op, "node")?)?;
                es.unset_prop(node, s(op, "key")?).map_err(ee)?;
                Ok(json!({ "op": "unset_prop" }))
            }
            "remove_node" => {
                let node = work_node(es, s(op, "node")?)?;
                let cascade = op.get("cascade").and_then(|c| c.as_bool()).unwrap_or(true);
                es.remove_node(node, cascade).map_err(ee)?;
                Ok(json!({ "op": "remove_node" }))
            }
            "remove_edge" => {
                let e: Uuid = s(op, "edge")?.parse().map_err(|e| format!("{e:?}"))?;
                es.remove_edge(e).map_err(ee)?;
                Ok(json!({ "op": "remove_edge" }))
            }
            "set_slug" => {
                let node = work_node(es, s(op, "node")?)?;
                es.set_slug(node, op.get("slug").and_then(|s| s.as_str()))
                    .map_err(ee)?;
                Ok(json!({ "op": "set_slug" }))
            }
            "add_label" => {
                let node = work_node(es, s(op, "node")?)?;
                es.add_label(node, s(op, "label")?).map_err(ee)?;
                Ok(json!({ "op": "add_label" }))
            }
            "remove_label" => {
                let node = work_node(es, s(op, "node")?)?;
                es.remove_label(node, s(op, "label")?).map_err(ee)?;
                Ok(json!({ "op": "remove_label" }))
            }
            other => Err(format!("unknown op: {other}")),
        }
    }

    fn t_pending(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        let (found, warnings) = find_work(&sess, g, &branch, id.workspace()).map_err(ee)?;
        let mut out = match found {
            None => json!({ "working": null }),
            Some(es) => {
                let d = es.pending();
                json!({
                    "working": es.wid().to_string(),
                    "base": es.base().map(|h| h.to_string()),
                    "pending": delta_summary(&d),
                })
            }
        };
        attach_recovery_warnings(&mut out, warnings);
        Ok(out)
    }

    fn t_commit(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        let (found, warnings) = find_work(&sess, g, &branch, id.workspace()).map_err(ee)?;
        let es = found.ok_or("working copy 없음 — 먼저 vgraph_edit로 변경을 쌓으세요")?;
        let mut out = commit_outcome_j(sess.commit(&es, s(a, "message")?).map_err(ee)?);
        attach_recovery_warnings(&mut out, warnings);
        Ok(out)
    }

    fn t_discard(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        let (found, warnings) = find_work(&sess, g, &branch, id.workspace()).map_err(ee)?;
        let mut out = match found {
            None => json!({ "discarded": false }),
            Some(es) => {
                sess.discard(es).map_err(ee)?;
                json!({ "discarded": true })
            }
        };
        attach_recovery_warnings(&mut out, warnings);
        Ok(out)
    }

    fn t_branch(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let graph = s(a, "graph")?;
        let (g, _, _) = target_of(&sess, graph)?;
        let (_, from) = sess.resolve(&full_addr(graph)).map_err(|e| e.to_string())?;
        sess.branch(g, from, s(a, "name")?).map_err(ee)?;
        Ok(json!({ "branch": s(a, "name")?, "at": from.to_string() }))
    }

    fn t_merge(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, _, _) = target_of(&sess, s(a, "graph")?)?;
        let resolutions_of = |key: &str, choose: Choice| -> Vec<Resolution> {
            a.get(key)
                .and_then(|t| t.as_array())
                .map(|xs| {
                    xs.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|i| Resolution {
                            item: i as usize,
                            choose: choose.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut resolutions = resolutions_of("theirs", Choice::Theirs);
        resolutions.extend(resolutions_of("ours", Choice::Ours));
        let opts = MergeOpts { resolutions };
        Ok(merge_outcome_j(
            sess.merge(g, s(a, "src")?, s(a, "dst")?, &opts)
                .map_err(ee)?,
        ))
    }

    fn t_log(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        let mut out = Vec::new();
        for h in sess.log(g, &branch).map_err(ee)? {
            let c = sess.commit_meta(g, h).map_err(ee)?;
            out.push(json!({
                "commit": h.to_string(), "ts": c.ts, "author": c.author, "msg": c.msg,
                "merge": c.parents.len() > 1,
                "parents": c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            }));
        }
        Ok(json!({ "commits": out }))
    }

    fn t_diff(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (_, v_from) = self.view_of(&sess, s(a, "from")?)?;
        let (_, v_to) = self.view_of(&sess, s(a, "to")?)?;
        if v_from.graph() != v_to.graph() {
            return Err("같은 그래프의 두 버전이어야 합니다".into());
        }
        let d = diff(&v_from.to_state(), &v_to.to_state());
        Ok(json!({
            "from": v_from.epoch().to_string(), "to": v_to.epoch().to_string(),
            "delta": delta_summary(&d),
            "props_set": d.props_set.iter().map(|p| json!({
                "elem": p.elem.to_string(), "key": p.key.to_string(),
                "old": p.old.as_ref().map(v2j), "new": p.new.as_ref().map(v2j),
            })).collect::<Vec<_>>(),
        }))
    }

    fn t_links(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        match s(a, "action")? {
            "lockfile" => {
                let lock = sess.lockfile(g, &branch).map_err(ee)?;
                Ok(json!({
                    "lockfile": lock.iter().map(|(k, e)| json!({
                        "target": k, "branch": e.branch, "pin": e.pin.to_string(),
                    })).collect::<Vec<_>>(),
                }))
            }
            "drifts" => {
                let items = sess.drifts(g, &branch).map_err(ee)?;
                Ok(json!({
                    "drifts": items.iter().map(|d| json!({
                        "target": d.target, "node": d.node.to_string(),
                        "pin": d.pin.to_string(),
                        "target_head": d.current_head.to_string(), // UX-3: upgrade의 to로 사용
                    })).collect::<Vec<_>>(),
                    "hint": if items.is_empty() { "" } else {
                        "vgraph_links action=upgrade, to=<target_head 값>으로 핀 갱신 가능"
                    },
                }))
            }
            "upgrade" => {
                let to: Hash256 = s(a, "to")?.parse().map_err(|e| format!("{e}"))?;
                Ok(commit_outcome_j(
                    sess.link_upgrade(g, &branch, s(a, "target")?, to)
                        .map_err(ee)?,
                ))
            }
            other => Err(format!("action은 lockfile|drifts|upgrade: {other}")),
        }
    }

    fn t_admin(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let (g, branch, _) = target_of(&sess, s(a, "graph")?)?;
        match s(a, "action")? {
            "gc_plan" => {
                let p = sess.gc_plan(g).map_err(ee)?;
                Ok(json!({
                    "roots": p.roots.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
                    "orphan_blobs": p.orphan_blobs.len(),
                    "orphan_payloads": p.orphan_payloads.len(),
                }))
            }
            "gc_run" => {
                let r = sess.gc_run(g).map_err(ee)?;
                Ok(
                    json!({ "removed_blobs": r.removed_blobs, "removed_payloads": r.removed_payloads }),
                )
            }
            "validate" => {
                let (_, head) = sess
                    .resolve(&full_addr(s(a, "graph")?))
                    .map_err(|e| e.to_string())?;
                let schema = sess
                    .schema(g, head)
                    .map_err(ee)?
                    .ok_or("이 버전에 스키마 없음 — set_schema 먼저")?;
                let issues = sess.validate(g, head, &schema).map_err(ee)?;
                Ok(json!({ "issues": issues.iter().map(issue_str).collect::<Vec<_>>() }))
            }
            "set_schema" => {
                let schema = schema_from_json(a.get("schema").ok_or("schema 필요")?)?;
                Ok(commit_outcome_j(
                    sess.set_schema(g, &branch, &schema).map_err(ee)?,
                ))
            }
            other => Err(format!(
                "action은 gc_plan|gc_run|validate|set_schema: {other}"
            )),
        }
    }

    fn t_embed_pending(&self, id: &Identity, a: &J) -> Result<J, String> {
        let embedder = self.embedder.as_ref().ok_or(
            "임베더 미설정 — 서버를 --embedder toy|openai:<model>|ollama:<model> 로 실행하세요",
        )?;
        let sess = self.sess(id);
        let (_, v) = self.view_of(&sess, s(a, "graph")?)?;
        let idx = VecIndex::for_graph(&self.cfg.root, v.graph(), &embedder.model_id())
            .map_err(|e| e.to_string())?;
        let n = embed_pending(&idx, &v, embedder.as_ref(), &self.cfg.embed)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "embedded": n, "model": embedder.model_id(), "epoch": v.epoch().to_string() }))
    }
}

// ---------- 헬퍼 ----------

fn s<'a>(a: &'a J, k: &str) -> Result<&'a str, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .ok_or(format!("필수 인자 누락: {k}"))
}

fn ee(e: EngineError) -> String {
    e.to_string()
}

fn rejected_with(e: &InterchangeError, code: ProblemCode) -> bool {
    match e {
        InterchangeError::Rejected(ps) => ps.iter().any(|p| p.code == code),
        _ => false,
    }
}

/// 가져오기·내보내기 오류 → 도구 오류 문장 (스키마 위반은 사람 친화 문장으로, 고치는 방법 덧붙임).
fn interchange_err(e: InterchangeError) -> String {
    if let InterchangeError::SchemaViolation(issues) = &e {
        return format!(
            "스키마 위반으로 거부됨 — head 불변: {}",
            issues.iter().map(issue_str).collect::<Vec<_>>().join("; ")
        );
    }
    if rejected_with(&e, ProblemCode::SluglessNode) {
        return format!("{e} — vgraph_edit의 set_slug로 slug를 부여한 뒤 다시 내보내세요");
    }
    if rejected_with(&e, ProblemCode::LockfileEdge) {
        return format!("{e} — __lockfile(시스템 노드)에 닿은 엣지를 지운 뒤 다시 내보내세요");
    }
    e.to_string()
}

/// merge 가져오기 충돌 시 해소 안내문.
fn conflict_resolve_hint(ns: &str, slug: &str, via: &str, dst: &str) -> String {
    format!(
        "대상 브랜치({dst})는 바뀌지 않았고 가져오기 전용 브랜치({via})에는 문서가 반영돼 있습니다. \
         해소: vgraph_merge를 {{\"graph\":\"{ns}/{slug}\",\"src\":\"{via}\",\"dst\":\"{dst}\",\"theirs\":[번호]}} 로 \
         부르면 문서 값(theirs)을, \"ours\":[번호]로 부르면 대상 브랜치 값(ours)을 채택합니다. \
         번호로 풀 수 있는 것은 kind=prop_both_set(같은 속성을 양쪽이 다르게 바꿈)뿐입니다. \
         removed_vs_edit(삭제 대 수정)·dangling_edge(끝이 사라진 엣지)·slug_collision(slug 겹침)·\
         schema_clash(스키마 충돌)는 번호로 풀 수 없습니다 — 대상 브랜치를 고치거나 문서를 고쳐 \
         다시 가져오세요(다시 가져오면 {via}가 새 문서에 맞춰지고 충돌을 새로 계산합니다)."
    )
}

/// SchemaIssue → 사람 친화 문장 (UX-5: Rust Debug 포맷 누출 방지).
fn issue_str(i: &vgraph_engine::SchemaIssue) -> String {
    use vgraph_engine::SchemaIssue as S;
    let labels = |xs: &[vgraph_core::Key]| {
        xs.iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match i {
        S::RequiredMissing { class, elem, key } => {
            format!("{class} 노드({elem})에 필수 속성 '{key}' 없음")
        }
        S::DomainViolation {
            edge_class,
            edge,
            end,
            expected,
        } => {
            format!(
                "{edge_class} 엣지({edge})의 from({end}) 라벨이 허용 범위[{}] 밖",
                labels(expected)
            )
        }
        S::RangeViolation {
            edge_class,
            edge,
            end,
            expected,
        } => {
            format!(
                "{edge_class} 엣지({edge})의 to({end}) 라벨이 허용 범위[{}] 밖",
                labels(expected)
            )
        }
        S::Cardinality {
            edge_class,
            elem,
            max,
            found,
        } => {
            format!("노드({elem})의 {edge_class} 엣지 {found}개 — 최대 {max}개 허용")
        }
    }
}

fn full_addr(x: &str) -> String {
    if x.starts_with("graph://") {
        x.to_string()
    } else {
        format!("graph://{x}")
    }
}

fn target_of(sess: &Session, x: &str) -> Result<(GraphId, String, Option<NodeSel>), String> {
    let a: GraphAddr = full_addr(x)
        .parse()
        .map_err(|e| format!("주소 형식 오류: {e}"))?;
    let g = sess.graph_id(&a.ns, &a.slug).ok_or(format!(
        "미등록 그래프: {}/{} (vgraph_create 먼저)",
        a.ns, a.slug
    ))?;
    Ok((g, a.branch.as_deref().unwrap_or("main").to_string(), a.node))
}

/// 되살릴 수 없어 격리한 작업본 하나에 대한 경고 (응답의 `recovery_warnings` 항목).
fn quarantine_warning(wid: WorkingId, kind: &str, quarantined: bool) -> J {
    let message = if quarantined {
        "되살릴 수 없는 작업본을 격리했습니다(내용은 보존, 이후 목록에서 제외). 새 작업본으로 이어 씁니다."
    } else {
        "되살릴 수 없는 작업본을 만났으나 격리하지 못했습니다. 새 작업본으로 이어 씁니다."
    };
    json!({
        "code": "working_quarantined",
        "working": wid.to_string(),
        "error": kind,
        "quarantined": quarantined,
        "message": message,
    })
}

/// 되살리기 오류를 "작업본 자체가 되살릴 수 없는 상태"로 볼지. 저장소 입출력 오류(`Store`)처럼
/// 일시적일 수 있는 것은 격리하지 않는다(멀쩡한 작업본을 잃지 않도록).
fn resume_error_kind(e: &EngineError) -> Option<&'static str> {
    match e {
        EngineError::Corrupt(_) => Some("corrupt"),
        EngineError::State(_) => Some("state"),
        EngineError::Resolve(_) => Some("resolve"),
        EngineError::Store(_) | EngineError::BranchExists | EngineError::BadBranchName(_) => None,
        EngineError::Link(_) | EngineError::UnknownWorking => None,
    }
}

/// 같은 브랜치·작업 공간의 내 작업본을 찾는다. 되살릴 수 없는 작업본은 삼키지 않고
/// 표준 오류에 경고 한 줄 + 격리 + 응답 경고로 드러낸다. 반환 = (찾은 작업본, 경고들).
///
/// `UnknownWorking`은 `resume`이 "이미 head 체인에 반영된 남은 작업본"을 스스로 정리했거나
/// 비어 있다는 뜻이다(engine.rs `resume`) — 정상 정리이므로 격리도 경고도 하지 않고 다음 후보로 간다.
fn find_work(
    sess: &Session,
    g: GraphId,
    branch: &str,
    ws: Option<&str>,
) -> EngineResult<(Option<EditSession>, Vec<J>)> {
    let mut warnings = Vec::new();
    for wid in sess.list_working(g)? {
        match sess.resume(g, wid) {
            Ok(es) => {
                // 브랜치와 작업 공간 이름이 모두 같은 것만 (미지정 = 이름 없는 것만)
                if es.branch().as_deref() == Some(branch) && es.workspace().as_deref() == ws {
                    return Ok((Some(es), warnings));
                }
            }
            Err(EngineError::UnknownWorking) => {}
            Err(e) => {
                let Some(kind) = resume_error_kind(&e) else {
                    eprintln!("vgraph: warn: 작업본 {wid} 되살리기 실패({e}) — 일시적 오류일 수 있어 격리하지 않고 건너뜀");
                    continue;
                };
                let quarantined = match sess.quarantine_working(g, wid) {
                    Ok(done) => done,
                    Err(qe) => {
                        eprintln!("vgraph: warn: 작업본 {wid} 격리 실패({qe})");
                        false
                    }
                };
                eprintln!(
                    "vgraph: warn: 작업본 {wid} 되살리기 실패({e}) — {}",
                    if quarantined {
                        "격리함"
                    } else {
                        "격리하지 못함"
                    }
                );
                warnings.push(quarantine_warning(wid, kind, quarantined));
            }
        }
    }
    Ok((None, warnings))
}

/// 응답 객체에 격리 경고를 싣는다. 경고가 없으면 아무 필드도 더하지 않는다(기존 응답 구조 불변).
fn attach_recovery_warnings(out: &mut J, warnings: Vec<J>) {
    if warnings.is_empty() {
        return;
    }
    if let Some(o) = out.as_object_mut() {
        o.insert("recovery_warnings".into(), J::Array(warnings));
    }
}

fn open_work(
    sess: &Session,
    g: GraphId,
    branch: &str,
    ws: Option<&str>,
) -> Result<(EditSession, Vec<J>), String> {
    let (found, warnings) = find_work(sess, g, branch, ws).map_err(ee)?;
    if let Some(es) = found {
        return Ok((es, warnings));
    }
    match sess.edit_in(g, branch, ws) {
        Ok(es) => Ok((es, warnings)),
        Err(EditStartError::Engine(e)) => Err(ee(e)),
        Err(EditStartError::NotHead { .. }) => Err("과거 commit 편집은 MCP v0 미지원".into()),
    }
}

fn work_node(es: &EditSession, x: &str) -> Result<Uuid, String> {
    if let Ok(u) = x.parse::<Uuid>() {
        return Ok(u);
    }
    es.by_slug(x).ok_or(format!("미존재 노드: {x}"))
}

fn node_in_view(v: &View, sel: &NodeSel) -> Result<Uuid, String> {
    match sel {
        NodeSel::Uuid(u) => Ok(*u),
        NodeSel::Slug(sl) => v.by_slug(sl).ok_or(format!("미존재 노드: {sl}")),
    }
}

fn v2j(v: &Value) -> J {
    let mut out = String::new();
    write_value(v, &mut out);
    serde_json::from_str(&out).unwrap_or(J::Null)
}

fn j2v(j: &J) -> Result<Value, String> {
    value_from_json(j).map_err(|e| e.to_string())
}

fn props_from_json_obj(j: &J) -> Result<Props, String> {
    let obj = j.as_object().ok_or("props는 객체여야 합니다")?;
    let mut p = Props::new();
    for (k, v) in obj {
        p.insert(smol(k), j2v(v)?);
    }
    Ok(p)
}

fn smol(s: &str) -> vgraph_core::Key {
    vgraph_core::Key::new(s)
}

fn node_j(v: &View, u: Uuid) -> J {
    let props: J = v
        .props_of(u)
        .map(|p| J::Object(p.iter().map(|(k, val)| (k.to_string(), v2j(val))).collect()))
        .unwrap_or(J::Null);
    json!({
        "uuid": u.to_string(),
        "slug": v.node_slug(u).flatten().as_deref(),
        "labels": v.node_labels(u).unwrap_or_default().iter().map(|l| l.to_string()).collect::<Vec<_>>(),
        "props": props,
        "epoch": v.epoch().to_string(),
    })
}

fn delta_summary(d: &vgraph_core::Delta) -> J {
    json!({
        "nodes_added": d.nodes_added.len(), "nodes_removed": d.nodes_removed.len(),
        "edges_added": d.edges_added.len(), "edges_removed": d.edges_removed.len(),
        "props_set": d.props_set.len(), "slugs_set": d.slugs_set.len(),
        "empty": d.is_empty(),
    })
}

fn conflict_items_j(items: &[ConflictItem]) -> Vec<J> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            ConflictItem::PropBothSet {
                elem,
                key,
                base,
                ours,
                theirs,
            } => json!({
                "index": i, "kind": "prop_both_set",
                "elem": elem.to_string(), "key": key.to_string(),
                "base": base.as_ref().map(v2j), "ours": ours.as_ref().map(v2j),
                "theirs": theirs.as_ref().map(v2j),
            }),
            ConflictItem::RemovedVsEdit { elem, removed_by } => json!({
                "index": i, "kind": "removed_vs_edit",
                "elem": elem.to_string(), "removed_by": format!("{removed_by:?}"),
            }),
            ConflictItem::DanglingEdge {
                edge,
                missing_end,
                removed_by,
            } => json!({
                "index": i, "kind": "dangling_edge",
                "edge": edge.to_string(), "missing_end": missing_end.to_string(),
                "removed_by": format!("{removed_by:?}"),
            }),
            ConflictItem::SlugCollision { slug, ours, theirs } => json!({
                "index": i, "kind": "slug_collision", "slug": slug.to_string(),
                "ours": ours.to_string(), "theirs": theirs.to_string(),
            }),
            ConflictItem::SchemaClash { issue } => json!({
                "index": i, "kind": "schema_clash", "issue": issue_str(issue),
            }),
            ConflictItem::ApplyFailed { reason } => json!({
                "index": i, "kind": "apply_failed", "reason": reason,
            }),
        })
        .collect()
}

fn report_j(r: &ConflictReport) -> J {
    let side = |s: &SideRef| match s {
        SideRef::Commit(h) => json!({ "commit": h.to_string() }),
        SideRef::Working(w) => json!({ "working": w.to_string() }),
    };
    json!({
        "base": r.base.map(|h| h.to_string()),
        "ours": side(&r.ours), "theirs": side(&r.theirs),
        "items": conflict_items_j(&r.items),
        "hint": "prop_both_set 항목은 vgraph_merge의 theirs:[index]로 해소 가능. 그 외는 review_queue로.",
    })
}

fn commit_outcome_j(o: CommitOutcome) -> J {
    match o {
        CommitOutcome::Committed { commit, warnings } => json!({
            "outcome": "committed", "commit": commit.to_string(),
            "warnings": warnings.iter().map(issue_str).collect::<Vec<_>>(),
        }),
        CommitOutcome::Rebased { onto, committed } => json!({
            "outcome": "rebased", "onto": onto.to_string(), "commit": committed.to_string(),
        }),
        CommitOutcome::Empty => json!({ "outcome": "empty" }),
        CommitOutcome::Conflicts(r) => json!({ "outcome": "conflicts", "report": report_j(&r) }),
        CommitOutcome::SchemaViolation(issues) => json!({
            "outcome": "schema_violation",
            "issues": issues.iter().map(issue_str).collect::<Vec<_>>(),
        }),
    }
}

fn merge_outcome_j(o: MergeOutcome) -> J {
    match o {
        MergeOutcome::FastForward(h) => {
            json!({ "outcome": "fast_forward", "commit": h.to_string() })
        }
        MergeOutcome::Merged(h) => json!({ "outcome": "merged", "commit": h.to_string() }),
        MergeOutcome::UpToDate(h) => json!({ "outcome": "up_to_date", "commit": h.to_string() }),
        MergeOutcome::Conflicts(r) => json!({ "outcome": "conflicts", "report": report_j(&r) }),
    }
}

fn schema_from_json(j: &J) -> Result<Schema, String> {
    let mode = match j.get("mode").and_then(|m| m.as_str()).unwrap_or("strict") {
        "strict" => SchemaMode::Strict,
        "warn" => SchemaMode::Warn,
        _ => SchemaMode::Off,
    };
    let mut node_classes = std::collections::BTreeMap::new();
    if let Some(ncs) = j.get("node_classes").and_then(|n| n.as_object()) {
        for (name, def) in ncs {
            let required = def
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(smol)).collect())
                .unwrap_or_default();
            node_classes.insert(smol(name), vgraph_engine::NodeClassDef { required });
        }
    }
    let mut edge_classes = std::collections::BTreeMap::new();
    if let Some(ecs) = j.get("edge_classes").and_then(|n| n.as_object()) {
        for (name, def) in ecs {
            let list = |k: &str| -> Vec<vgraph_core::Key> {
                def.get(k)
                    .and_then(|r| r.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(smol)).collect())
                    .unwrap_or_default()
            };
            edge_classes.insert(
                smol(name),
                vgraph_engine::EdgeClassDef {
                    domain: list("domain"),
                    range: list("range"),
                    max_out: def
                        .get("max_out")
                        .and_then(|m| m.as_u64())
                        .map(|m| m as u32),
                },
            );
        }
    }
    Ok(Schema {
        mode,
        node_classes,
        edge_classes,
    })
}

// ---------- 조회 면: vgraph_find · vgraph_scan · neighbors 확장 ----------

/// vgraph_find limit 기본값·상한.
const FIND_LIMIT_DEFAULT: usize = 50;
const FIND_LIMIT_MAX: usize = 500;
/// vgraph_scan limit 기본값·상한.
const SCAN_LIMIT_DEFAULT: usize = 200;
const SCAN_LIMIT_MAX: usize = 1000;
/// 커서 문자열의 epoch·uuid 구분자. epoch("sha256:<hex>")와 uuid(ULID)에는 나오지 않는 문자.
const CURSOR_SEP: char = '|';
/// vgraph_neighbors의 새 선택 인자 — 하나라도 있으면 View::walk 경로, 없으면 기존 경로.
const WALK_ARGS: [&str; 7] = [
    "etypes",
    "emit_labels",
    "expand_labels",
    "include",
    "prop_keys",
    "limit",
    "max_bytes",
];

/// include 인자 해석 결과.
#[derive(Clone, Copy, Default)]
struct Include {
    labels: bool,
    props: bool,
}

/// 선택 문자열 인자. 있는데 문자열이 아니면 오류.
fn opt_str<'a>(a: &'a J, k: &str) -> Result<Option<&'a str>, String> {
    match a.get(k) {
        None | Some(J::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or(format!("{k}는 문자열이어야 합니다")),
    }
}

/// 선택 불리언 인자. 있는데 불리언이 아니면 오류.
fn opt_bool(a: &J, k: &str) -> Result<Option<bool>, String> {
    match a.get(k) {
        None | Some(J::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or(format!("{k}는 true/false여야 합니다")),
    }
}

/// 선택 문자열 배열 인자. 있는데 문자열 배열이 아니면 오류.
fn opt_str_list<'a>(a: &'a J, k: &str) -> Result<Option<Vec<&'a str>>, String> {
    match a.get(k) {
        None | Some(J::Null) => Ok(None),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or(format!("{k}는 문자열 배열이어야 합니다"))?;
            arr.iter()
                .map(|x| {
                    x.as_str()
                        .ok_or(format!("{k}의 항목은 문자열이어야 합니다"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        }
    }
}

/// 빈 배열은 "필터 없음"과 같다.
fn non_empty_list(l: Option<Vec<&str>>) -> Option<Vec<&str>> {
    l.filter(|v| !v.is_empty())
}

/// 주어진 키 중 값이 있는(null 아님) 첫 키.
fn present_key<'a>(a: &J, keys: &[&'a str]) -> Option<&'a str> {
    keys.iter()
        .copied()
        .find(|k| a.get(*k).is_some_and(|v| !v.is_null()))
}

/// 선택 인자 `max_bytes`(나눠 받기 조각 크기). 범위 밖·비정수는 오류.
fn export_part_size(a: &J) -> Result<Option<usize>, String> {
    let (lo, hi) = (interchange::PART_MIN_BYTES, interchange::PART_MAX_BYTES);
    match a.get("max_bytes") {
        None | Some(J::Null) => Ok(None),
        Some(v) => match v.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
            _ => Err(format!(
                "max_bytes는 {lo} 이상 {hi} 이하의 정수여야 합니다 (받은 값: {v})"
            )),
        },
    }
}

/// 선택 양의 정수 인자 (0·음수·비정수는 오류).
fn opt_count(a: &J, k: &str) -> Result<Option<usize>, String> {
    match a.get(k) {
        None | Some(J::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            _ => Err(format!("{k}는 1 이상의 정수여야 합니다")),
        },
    }
}

fn include_of(a: &J) -> Result<Include, String> {
    let mut inc = Include::default();
    for item in opt_str_list(a, "include")?.unwrap_or_default() {
        match item {
            "labels" => inc.labels = true,
            "props" => inc.props = true,
            other => return Err(format!("include 항목은 \"labels\"|\"props\": {other}")),
        }
    }
    Ok(inc)
}

/// 노드의 라벨·속성(`prop_keys`가 있으면 그 키만)을 응답 객체에 싣는다.
fn put_content(o: &mut J, v: &View, u: Uuid, inc: Include, prop_keys: Option<&[&str]>) {
    if inc.labels {
        o["labels"] = json!(v
            .node_labels(u)
            .unwrap_or_default()
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>());
    }
    if inc.props {
        o["props"] = v
            .props_of(u)
            .map(|p| {
                J::Object(
                    p.iter()
                        .filter(|(k, _)| {
                            prop_keys.is_none_or(|ks| ks.iter().any(|x| *x == k.as_str()))
                        })
                        .map(|(k, val)| (k.to_string(), v2j(val)))
                        .collect(),
                )
            })
            .unwrap_or(J::Null);
    }
}

/// 이어 받기 커서 — "<epoch>|<마지막 uuid>". epoch를 담아 같은 버전에서만 이어 읽게 한다.
fn cursor_encode(epoch: vgraph_core::Hash256, last: Uuid) -> String {
    format!("{epoch}{CURSOR_SEP}{last}")
}

fn cursor_decode(c: &str) -> Result<(vgraph_core::Hash256, Uuid), String> {
    let bad = || format!("커서 형식 오류: {c} (이전 응답의 next_cursor를 그대로 쓰세요)");
    let (e, u) = c.split_once(CURSOR_SEP).ok_or_else(bad)?;
    let epoch = e.parse::<vgraph_core::Hash256>().map_err(|_| bad())?;
    let last = u.parse::<Uuid>().map_err(|_| bad())?;
    Ok((epoch, last))
}

/// 커서가 담은 epoch로 고정한 주소. 주소가 명시적으로 다른 `.commit`을 가리키면 오류.
fn pinned_addr(addr: &str, epoch: vgraph_core::Hash256) -> Result<String, String> {
    let parsed: GraphAddr = full_addr(addr)
        .parse()
        .map_err(|e| format!("주소 형식 오류: {e}"))?;
    if let Some(explicit) = parsed.commit {
        if explicit != epoch {
            return Err(format!(
                "다른 epoch의 커서입니다: 커서 {epoch} ≠ 주소 {explicit} — 첫 호출부터 다시 시작하세요"
            ));
        }
    }
    Ok(format!("graph://{}/{}.{}", parsed.ns, parsed.slug, epoch))
}

/// 내보내기 나눠 받기 커서 — "ex1|<epoch>|<시작 바이트>|<문서 전체 바이트>".
/// 서버는 상태를 갖지 않는다: epoch(커밋)에서 같은 문서를 다시 만들어 그 위치부터 자른다.
/// 내보내기는 같은 커밋이면 같은 바이트이므로 판이 고정된다. 전체 바이트는 어긋남 검사용.
const EXPORT_CURSOR_TAG: &str = "ex1";

fn export_cursor_encode(epoch: vgraph_core::Hash256, offset: usize, total: usize) -> String {
    format!("{EXPORT_CURSOR_TAG}{CURSOR_SEP}{epoch}{CURSOR_SEP}{offset}{CURSOR_SEP}{total}")
}

fn export_cursor_decode(c: &str) -> Result<(vgraph_core::Hash256, usize, usize), String> {
    let bad =
        || format!("커서 형식 오류: {c} (vgraph_export의 이전 응답 next_cursor를 그대로 쓰세요)");
    let mut it = c.split(CURSOR_SEP);
    if it.next() != Some(EXPORT_CURSOR_TAG) {
        return Err(bad());
    }
    let epoch = it
        .next()
        .and_then(|e| e.parse::<vgraph_core::Hash256>().ok())
        .ok_or_else(bad)?;
    let offset = it
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(bad)?;
    let total = it
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(bad)?;
    if it.next().is_some() {
        return Err(bad());
    }
    Ok((epoch, offset, total))
}

fn is_system_slug(slug: Option<&str>) -> bool {
    slug.is_some_and(|s| s.starts_with("__"))
}

/// 선택 인자 `compact`(생략·null=false). 있는데 불리언이 아니면 오류.
fn compact_of(a: &J) -> Result<bool, String> {
    Ok(opt_bool(a, "compact")?.unwrap_or(false))
}

/// 노드 식별 칸. 기본은 {uuid, slug}. compact 이고 slug 가 있으면 {slug}만,
/// slug 가 없으면 {uuid}만 낸다(식별자는 하나).
fn id_cols(v: &View, u: Uuid, compact: bool) -> J {
    let slug = v.node_slug(u).flatten();
    match (compact, slug.as_deref()) {
        (true, Some(s)) => json!({ "slug": s }),
        (true, None) => json!({ "uuid": u.to_string() }),
        (false, s) => json!({ "uuid": u.to_string(), "slug": s }),
    }
}

/// compact 가 아닐 때만 응답에 epoch 를 싣는다(compact 에서는 커서가 epoch 를 담고 있다).
fn put_epoch(out: &mut J, v: &View, compact: bool) {
    if !compact {
        out["epoch"] = json!(v.epoch().to_string());
    }
}

/// 노드 기본 항목 {uuid, slug} (+ 시스템 노드면 system:true). compact 는 `id_cols` 규칙.
fn node_stub(v: &View, u: Uuid, compact: bool) -> J {
    let mut o = id_cols(v, u, compact);
    if is_system_slug(v.node_slug(u).flatten().as_deref()) {
        o["system"] = json!(true);
    }
    o
}

/// vgraph_neighbors에 새 선택 인자가 하나라도 있는가.
fn has_walk_args(a: &J) -> bool {
    WALK_ARGS
        .iter()
        .any(|k| a.get(k).is_some_and(|v| !v.is_null()))
}

/// 새 인자 경로의 vgraph_neighbors 응답 — View::walk.
/// limit: 결과 개수 상한. max_bytes: hits 배열을 JSON으로 직렬화한 바이트 수 상한
/// (`[` `]` 2바이트 + 각 항목 + 항목 사이 쉼표). 어느 쪽이든 걸리면 truncated:true.
fn walk_response(
    v: &View,
    start: Uuid,
    dir: vgraph_core::Dir,
    depth: u32,
    a: &J,
    compact: bool,
) -> Result<J, String> {
    let etypes = match non_empty_list(opt_str_list(a, "etypes")?) {
        Some(l) => Some(l),
        None => a.get("etype").and_then(|t| t.as_str()).map(|t| vec![t]),
    };
    let emit = non_empty_list(opt_str_list(a, "emit_labels")?);
    let expand = non_empty_list(opt_str_list(a, "expand_labels")?);
    let inc = include_of(a)?;
    let prop_keys = non_empty_list(opt_str_list(a, "prop_keys")?);
    let limit = opt_count(a, "limit")?;
    let max_bytes = opt_count(a, "max_bytes")?;

    // limit+1개를 받아 "더 있음"을 판정한다.
    let mut hits = v.walk(&WalkQuery {
        start,
        dir,
        etypes: etypes.as_deref(),
        max_depth: depth,
        emit_labels: emit.as_deref(),
        expand_labels: expand.as_deref(),
        limit: limit.map(|l| l.saturating_add(1)),
    });
    let mut truncated = false;
    if let Some(l) = limit {
        if hits.len() > l {
            hits.truncate(l);
            truncated = true;
        }
    }
    let mut out: Vec<J> = Vec::new();
    let mut bytes: usize = 2;
    for h in &hits {
        let mut item = id_cols(v, h.node, compact);
        item["depth"] = json!(h.depth);
        item["via_type"] = json!(h.via_type.as_str());
        if !compact {
            item["via"] = json!(h.via.to_string());
        }
        put_content(&mut item, v, h.node, inc, prop_keys.as_deref());
        let add = item
            .to_string()
            .len()
            .saturating_add(usize::from(!out.is_empty()));
        if max_bytes.is_some_and(|m| bytes.saturating_add(add) > m) {
            truncated = true;
            break;
        }
        bytes = bytes.saturating_add(add);
        out.push(item);
    }
    let mut res = json!({ "hits": out, "truncated": truncated });
    put_epoch(&mut res, v, compact);
    Ok(res)
}

impl McpServer {
    /// 읽기 뷰. 커서가 있으면 커서가 담은 epoch(commit)의 뷰를 고정해 이어 읽는다 —
    /// 브랜치 head가 그 사이 움직여도 첫 호출과 같은 버전이다.
    /// 주소가 명시적으로 다른 `.commit`을 가리키면 오류.
    fn view_pinned(
        &self,
        sess: &Session,
        addr: &str,
        cursor: Option<&str>,
    ) -> Result<(Arc<View>, Option<Uuid>), String> {
        let Some(c) = cursor else {
            let (_, v) = self.view_of(sess, addr)?;
            return Ok((v, None));
        };
        let (epoch, last) = cursor_decode(c)?;
        let pinned = pinned_addr(addr, epoch)?;
        let (_, v) = self.view_of(sess, &pinned)?;
        Ok((v, Some(last)))
    }

    fn t_find(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        // 선택 인자(order·normalize·any) — 하나도 없으면 아래 응답은 기존과 바이트 단위로 같다.
        let relevance = match opt_str(a, "order")? {
            None => false,
            Some("relevance") => true,
            Some(other) => {
                return Err(format!(
                    "order는 \"relevance\"만 쓸 수 있습니다 (생략=uuid 순): {other}"
                ))
            }
        };
        let normalize = opt_bool(a, "normalize")?.unwrap_or(false);
        let skip_ref_like = opt_bool(a, "skip_ref_like")?.unwrap_or(false);
        let compact = compact_of(a)?;
        let any = non_empty_list(opt_str_list(a, "any")?).unwrap_or_default();
        if relevance && opt_str(a, "cursor")?.is_some() {
            return Err(FindOptionsError::RelevanceWithCursor.to_string());
        }
        let (v, after) = self.view_pinned(&sess, s(a, "graph")?, opt_str(a, "cursor")?)?;
        let labels = opt_str_list(a, "labels")?.unwrap_or_default();
        let props_eq = match a.get("props") {
            None | Some(J::Null) => Props::new(),
            Some(p) => props_from_json_obj(p)?,
        };
        let text_keys = non_empty_list(opt_str_list(a, "text_keys")?);
        let inc = include_of(a)?;
        let prop_keys = non_empty_list(opt_str_list(a, "prop_keys")?);
        let limit = opt_count(a, "limit")?
            .unwrap_or(FIND_LIMIT_DEFAULT)
            .min(FIND_LIMIT_MAX);
        let query = FindQuery {
            labels: &labels,
            props_eq: &props_eq,
            text: opt_str(a, "text")?,
            text_keys: text_keys.as_deref(),
            limit,
            after,
        };
        let opts = FindOptions {
            relevance,
            normalize,
            any: &any,
        };
        let filter = TextFilter { skip_ref_like };
        let (hits, next_cursor, done, total) =
            if relevance || normalize || skip_ref_like || !any.is_empty() {
                let out = v
                    .find_filtered(&query, &opts, &filter)
                    .map_err(|e| e.to_string())?;
                (out.items, out.next_cursor, out.done, out.total)
            } else {
                let page = v.find(&query);
                let hits = page
                    .items
                    .into_iter()
                    .map(|found| FindHit {
                        found,
                        matched_expr: None,
                        score: None,
                    })
                    .collect();
                (hits, page.next_cursor, page.done, None)
            };
        let items: Vec<J> = hits
            .iter()
            .map(|h| {
                let m = &h.found;
                let mut o = node_stub(&v, m.node, compact);
                if let Some(k) = &m.matched_key {
                    o["matched_key"] = json!(k.as_str());
                }
                if let Some(e) = &m.excerpt {
                    o["excerpt"] = json!(e);
                }
                if !any.is_empty() {
                    if let Some(e) = &h.matched_expr {
                        o["matched_expr"] = json!(e);
                    }
                }
                if let Some(score) = h.score {
                    o["score"] = json!(score);
                }
                put_content(&mut o, &v, m.node, inc, prop_keys.as_deref());
                o
            })
            .collect();
        let mut out = json!({
            "items": items,
            "next_cursor": next_cursor.map(|u| cursor_encode(v.epoch(), u)),
            "done": done,
        });
        put_epoch(&mut out, &v, compact);
        if relevance {
            out["order"] = json!("relevance");
            out["total_matches"] = json!(total);
        }
        Ok(out)
    }

    // ---------- JSON 내보내기·가져오기 (R5) ----------

    fn t_export(&self, id: &Identity, a: &J) -> Result<J, String> {
        let max = export_part_size(a)?;
        let cursor = opt_str(a, "cursor")?;
        if max.is_some() || cursor.is_some() {
            return self.t_export_part(id, a, max, cursor);
        }
        // 쓸 곳을 먼저 검사한다 — 거부될 요청이 그래프를 읽느라 시간을 쓰지 않게.
        let target = match opt_str(a, "path")? {
            Some(path) => Some((path, self.path_policy.resolve(path)?)),
            None => None,
        };
        let sess = self.sess(id);
        let ex = interchange::export(&sess, s(a, "graph")?).map_err(interchange_err)?;
        let mut out = json!({
            "epoch": ex.epoch.to_string(), "nodes": ex.nodes, "edges": ex.edges,
        });
        match target {
            Some((path, real)) => {
                ex.write_to(&real).map_err(interchange_err)?;
                out["path"] = json!(path);
            }
            None => {
                out["document"] = serde_json::from_str(&ex.text)
                    .map_err(|e| format!("내보낸 문서를 JSON으로 읽지 못했습니다: {e}"))?;
            }
        }
        Ok(out)
    }

    /// 나눠 받기: 첫 호출(커서 없음)은 지금 head 커밋에 고정하고, 이어 받기는 커서의 커밋에서
    /// 같은 문서를 다시 만들어 그 위치부터 자른다 — 서버 메모리 상태 없음.
    fn t_export_part(
        &self,
        id: &Identity,
        a: &J,
        max: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<J, String> {
        if opt_str(a, "path")?.is_some() {
            return Err(
                "path는 max_bytes·cursor와 함께 쓸 수 없습니다 — 나눠 받기는 응답의 chunk로 받고, 파일로 쓰려면 path만 주세요"
                    .into(),
            );
        }
        let max = max.ok_or(
            "cursor를 쓸 때는 max_bytes도 함께 주세요 (첫 호출과 같은 값이 아니어도 됩니다)",
        )?;
        let graph = s(a, "graph")?;
        let sess = self.sess(id);
        let ex = match cursor {
            None => interchange::export(&sess, graph).map_err(interchange_err)?,
            Some(c) => {
                let (epoch, offset, total) = export_cursor_decode(c)?;
                let pinned = pinned_addr(graph, epoch)?;
                let ex = interchange::export(&sess, &pinned).map_err(|e| {
                    format!(
                        "커서가 가리키는 판을 열지 못했습니다: {}",
                        interchange_err(e)
                    )
                })?;
                if ex.text.len() != total {
                    return Err(format!(
                        "커서가 이 문서와 맞지 않습니다 (커서 {total}바이트 ≠ 문서 {}바이트) — 첫 호출부터 다시 시작하세요",
                        ex.text.len()
                    ));
                }
                return self
                    .export_part_response(&ex, offset, max)
                    .map_err(|e| format!("커서가 가리키는 위치가 올바르지 않습니다: {e}"));
            }
        };
        self.export_part_response(&ex, 0, max)
    }

    fn export_part_response(
        &self,
        ex: &interchange::Exported,
        offset: usize,
        max: usize,
    ) -> Result<J, String> {
        let part = ex.part(offset, max).map_err(interchange_err)?;
        Ok(json!({
            "epoch": ex.epoch.to_string(), "nodes": ex.nodes, "edges": ex.edges,
            "chunk": part.text,
            "offset": part.offset,
            "total_bytes": ex.text.len(),
            "done": part.next_offset.is_none(),
            "next_cursor": part
                .next_offset
                .map(|n| export_cursor_encode(ex.epoch, n, ex.text.len())),
        }))
    }

    fn t_import(&self, id: &Identity, a: &J) -> Result<J, String> {
        if let Some(action) = opt_str(a, "upload")? {
            return self.t_import_upload(id, a, action);
        }
        if let Some(k) = present_key(a, &["upload_id", "chunk", "seq"]) {
            return Err(format!(
                "{k}는 upload(begin|append|commit|abort)와 함께만 쓸 수 있습니다"
            ));
        }
        let plan = ImportPlan::from_args(a)?;
        let doc = match (
            a.get("document").filter(|d| !d.is_null()),
            opt_str(a, "path")?,
        ) {
            (Some(J::String(text)), None) => {
                interchange::parse_document(text).map_err(interchange_err)?
            }
            (Some(obj @ J::Object(_)), None) => {
                interchange::parse_document(&obj.to_string()).map_err(interchange_err)?
            }
            (Some(_), None) => return Err("document는 객체 또는 JSON 문자열이어야 합니다".into()),
            (None, Some(path)) => {
                let real = self.path_policy.resolve(path)?;
                interchange::read_document(&real).map_err(interchange_err)?
            }
            _ => return Err("document와 path 중 정확히 하나를 주세요".into()),
        };
        self.apply_import(id, &plan, doc)
    }

    /// 검증을 마친 문서를 넣는다 — 한 번에 가져오기와 나눠 올리기 확정이 같은 경로를 쓴다.
    fn apply_import(
        &self,
        id: &Identity,
        plan: &ImportPlan,
        mut doc: interchange::Document,
    ) -> Result<J, String> {
        let sess = self.sess(id);
        let graph = plan.graph.as_str();
        // edit 와 같은 Ref 보정·조기 검증 (UX-1·UX-2) — 대상 미등록이면 저장소를 건드리기 전에 거부
        for n in &mut doc.nodes {
            self.fix_refs(id, &mut n.props)?;
        }
        for e in &mut doc.edges {
            self.fix_refs(id, &mut e.props)?;
        }
        let message = plan.message.as_str();
        if let Some(via) = plan.via.as_deref() {
            return self.import_merge_response(id, &sess, graph, via, doc, message);
        }
        let report = interchange::import_replace_in(&sess, graph, id.workspace(), doc, message)
            .map_err(interchange_err)?;
        Ok(json!({
            "outcome": if report.commit.is_some() { "committed" } else { "empty" },
            "commit": report.commit.map(|c| c.to_string()),
            "nodes": report.nodes, "edges": report.edges,
            "warnings": report.warnings.iter().map(issue_str).collect::<Vec<_>>(),
        }))
    }

    /// 나눠 올리기. begin → append(seq 0,1,2…) → commit | abort.
    fn t_import_upload(&self, id: &Identity, a: &J, action: &str) -> Result<J, String> {
        if let Some(k) = present_key(a, &["document", "path"]) {
            return Err(format!(
                "upload와 {k}는 함께 줄 수 없습니다 — 문서는 append의 chunk로 올립니다"
            ));
        }
        let graph = s(a, "graph")?;
        let now = Instant::now();
        match action {
            "begin" => {
                if let Some(k) = present_key(a, &["upload_id", "chunk", "seq"]) {
                    return Err(format!("upload=begin에는 {k}를 줄 수 없습니다"));
                }
                let plan = ImportPlan::from_args(a)?;
                let mut up = self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
                let upload_id = up.begin(now, id, plan)?;
                Ok(json!({
                    "upload_id": upload_id, "next_seq": 0,
                    "max_bytes": up.limits.max_bytes,
                    "idle_timeout_secs": up.limits.idle.as_secs(),
                }))
            }
            "append" | "commit" | "abort" => {
                if let Some(k) = present_key(a, &["mode", "via_branch", "message"]) {
                    return Err(format!(
                        "{k}는 upload=begin에서 정한 값입니다 — 여기서는 빼세요"
                    ));
                }
                let upload_id = s(a, "upload_id")?;
                match action {
                    "append" => {
                        let seq = a
                            .get("seq")
                            .and_then(J::as_u64)
                            .ok_or("seq(0부터 시작하는 조각 번호, 정수)가 필요합니다")?;
                        let chunk = s(a, "chunk")?;
                        let mut up = self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
                        let (received, next_seq) =
                            up.append(now, upload_id, id, graph, seq, chunk)?;
                        Ok(json!({
                            "upload_id": upload_id, "received_bytes": received,
                            "next_seq": next_seq,
                        }))
                    }
                    "commit" => {
                        // 꺼내면서 지운다 — 같은 업로드의 동시 append·commit은 "없음"을 만난다.
                        // 확정이 실패해도 업로드는 돌려놓지 않는다(처음부터 다시).
                        let taken = {
                            let mut up =
                                self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
                            up.take(now, upload_id, id, graph)?
                        };
                        let doc =
                            interchange::parse_document(&taken.text).map_err(interchange_err)?;
                        self.apply_import(id, &taken.plan, doc)
                    }
                    _ => {
                        let mut up = self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
                        up.take(now, upload_id, id, graph)?;
                        Ok(json!({ "upload_id": upload_id, "aborted": true }))
                    }
                }
            }
            other => Err(format!(
                "upload는 begin|append|commit|abort 중 하나여야 합니다 (받은 값: {other})"
            )),
        }
    }

    /// mode=merge: 가져오기 전용 브랜치를 문서에 맞추고 기존 머지로 대상에 합친다.
    fn import_merge_response(
        &self,
        id: &Identity,
        sess: &Session,
        graph: &str,
        via: &str,
        doc: interchange::Document,
        message: &str,
    ) -> Result<J, String> {
        let r = interchange::import_merge_in(sess, graph, via, id.workspace(), doc, message)
            .map_err(interchange_err)?;
        let a: GraphAddr = full_addr(graph)
            .parse()
            .map_err(|e| format!("주소 형식 오류: {e}"))?;
        let dst = a.branch.as_deref().unwrap_or("main").to_string();
        let mut out = match r.outcome {
            interchange::SyncOutcome::FirstImport { commit } => {
                json!({ "outcome": "first_import", "commit": commit.to_string() })
            }
            interchange::SyncOutcome::Merge(m) => {
                let conflicted = matches!(m, MergeOutcome::Conflicts(_));
                let mut j = merge_outcome_j(m);
                if conflicted {
                    j["resolve"] = json!(conflict_resolve_hint(&a.ns, &a.slug, via, &dst));
                }
                j
            }
        };
        out["via_branch"] = json!(via);
        out["via_commit"] = json!(r.via_commit.map(|c| c.to_string()));
        out["nodes"] = json!(r.nodes);
        out["edges"] = json!(r.edges);
        out["warnings"] = json!(r.warnings.iter().map(issue_str).collect::<Vec<_>>());
        Ok(out)
    }

    fn t_scan(&self, id: &Identity, a: &J) -> Result<J, String> {
        let sess = self.sess(id);
        let compact = compact_of(a)?;
        let (v, after) = self.view_pinned(&sess, s(a, "graph")?, opt_str(a, "cursor")?)?;
        let kind = s(a, "kind")?;
        let limit = opt_count(a, "limit")?
            .unwrap_or(SCAN_LIMIT_DEFAULT)
            .min(SCAN_LIMIT_MAX);
        let include_props = match a.get("include_props") {
            None | Some(J::Null) => false,
            Some(b) => b.as_bool().ok_or("include_props는 true|false여야 합니다")?,
        };
        let labels = non_empty_list(opt_str_list(a, "labels")?);
        let (items, next, done): (Vec<J>, Option<Uuid>, bool) = match kind {
            "nodes" => {
                let page = v.scan_nodes(labels.as_deref().unwrap_or_default(), after, limit);
                let inc = Include {
                    labels: true,
                    props: include_props,
                };
                let items = page
                    .items
                    .iter()
                    .map(|u| {
                        let mut o = node_stub(&v, *u, compact);
                        put_content(&mut o, &v, *u, inc, None);
                        o
                    })
                    .collect();
                (items, page.next_cursor, page.done)
            }
            "edges" => {
                if labels.is_some() {
                    return Err("labels는 kind=nodes에서만 쓸 수 있습니다".into());
                }
                let page = v.scan_edges(after, limit);
                let items = page
                    .items
                    .iter()
                    .filter_map(|u| {
                        let info = v.edge_info(*u)?;
                        let from_slug = v.node_slug(info.from).flatten();
                        let to_slug = v.node_slug(info.to).flatten();
                        let mut o = if compact {
                            // 엣지 uuid 는 뺀다. 양끝은 slug 가 있으면 *_slug 만, 없으면 uuid(from/to).
                            let mut o = json!({ "type": info.etype.as_str() });
                            if from_slug.is_none() {
                                o["from"] = json!(info.from.to_string());
                            }
                            if to_slug.is_none() {
                                o["to"] = json!(info.to.to_string());
                            }
                            o
                        } else {
                            json!({
                                "uuid": u.to_string(), "type": info.etype.as_str(),
                                "from": info.from.to_string(), "to": info.to.to_string(),
                            })
                        };
                        if include_props {
                            o["props"] = J::Object(
                                v.edge_props_of(*u)
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|(k, val)| (k.to_string(), v2j(val)))
                                    .collect(),
                            );
                        }
                        if let Some(sl) = &from_slug {
                            o["from_slug"] = json!(sl.as_str());
                        }
                        if let Some(sl) = &to_slug {
                            o["to_slug"] = json!(sl.as_str());
                        }
                        Some(o)
                    })
                    .collect();
                (items, page.next_cursor, page.done)
            }
            other => return Err(format!("kind는 nodes|edges: {other}")),
        };
        let mut out = json!({
            "kind": kind,
            "items": items,
            "next_cursor": next.map(|u| cursor_encode(v.epoch(), u)),
            "done": done,
        });
        put_epoch(&mut out, &v, compact);
        Ok(out)
    }
}

// ---------- 도구 정의 (tools/list) ----------

fn tool(name: &str, desc: &str, props: J, required: &[&str]) -> J {
    json!({
        "name": name, "description": desc,
        "inputSchema": { "type": "object", "properties": props, "required": required },
    })
}

/// 툴 20종(기존 16 + vgraph_find·vgraph_scan + vgraph_export·vgraph_import)의 MCP 정의. pub: 합성 마운트 호스트가 tools/list에 병합할 수 있도록.
pub fn tool_defs() -> Vec<J> {
    let addr = |d: &str| json!({ "type": "string", "description": d });
    vec![
        tool("vgraph_list", "내 그래프 목록과 head", json!({}), &[]),
        tool("vgraph_create", "그래프 생성", json!({ "name": addr("ns/slug") }), &["name"]),
        tool(
            "vgraph_show",
            "그래프 요약 또는 노드 상세. addr에 #slug를 붙이면 노드 조회. 응답에 epoch(commit) 포함",
            json!({ "addr": addr("ns/slug[@branch][.commit][#node]") }),
            &["addr"],
        ),
        tool(
            "vgraph_search",
            concat!(
                "검색 — mode=prop: 속성 등가(key+value), mode=similar: 유사도(query, 임베더 필요). ",
                "선택 인자(주지 않으면 위 동작·응답 그대로): compact=true — epoch를 빼고 hit은 slug가 있으면 ",
                "slug만, 없으면 uuid만 싣는다(mode별 score·part·model·pending_embeddings는 유지). ",
                "예: {\"graph\":\"acme/catalog\",\"mode\":\"prop\",\"key\":\"status\",\"value\":\"active\",\"compact\":true}"
            ),
            json!({
                "graph": addr("ns/slug[@branch][.commit]"),
                "mode": { "type": "string", "enum": ["prop", "similar"] },
                "key": { "type": "string" }, "value": {},
                "query": { "type": "string" }, "k": { "type": "integer" },
                "compact": { "type": "boolean",
                    "description": "true면 epoch를 빼고 hit을 slug(없으면 uuid)만으로. 예: true" },
            }),
            &["graph"],
        ),
        tool(
            "vgraph_neighbors",
            concat!(
                "순회 — addr#node에서 dir/etype/depth로. ",
                "[선택 인자 — 하나도 안 주면 위 기본 동작 그대로] ",
                "etypes[]: 엣지 타입 여러 개(있으면 etype보다 우선) · ",
                "emit_labels[]: 결과에 낼 노드의 라벨(하나라도 가진 노드만) · ",
                "expand_labels[]: 더 뻗을 노드의 라벨(시작 노드는 항상 뻗음, 1단계 이후 노드는 ",
                "이 라벨 중 하나를 가질 때만 다음 단계로 뻗음) · ",
                "include[]: \"labels\"·\"props\" 중 골라 각 hit에 싣기 · ",
                "prop_keys[]: include에 props가 있을 때 실을 속성 키(생략=전부) · ",
                "limit: 결과 개수 상한(1 이상) · ",
                "max_bytes: hits 배열을 JSON으로 직렬화한 바이트 수 상한(1 이상). ",
                "새 인자를 하나라도 주면 hit에 via_type(거쳐 온 엣지 타입)이 더해지고 응답에 ",
                "truncated(limit·max_bytes에 걸려 잘렸는지)가 붙는다. ",
                "예(1단계는 모든 이웃, 2단계는 Category·Brand 노드에서만): ",
                "{\"addr\":\"acme/catalog#item-001\",\"depth\":2,",
                "\"expand_labels\":[\"Category\",\"Brand\"],",
                "\"include\":[\"labels\",\"props\"],\"limit\":100,\"max_bytes\":20000}. ",
                "예(엣지 타입 둘만): {\"addr\":\"acme/catalog#item-001\",\"etypes\":[\"참조\",\"관련\"],\"depth\":2}. ",
                "compact=true(위 새 인자와 별개로 단독 사용 가능 — via_type·truncated를 더하지 않는다): ",
                "epoch와 via(엣지 uuid)를 빼고 hit은 slug가 있으면 slug만, 없으면 uuid만 싣는다",
                "(depth·via_type·truncated·labels·props는 유지). ",
                "예: {\"addr\":\"acme/catalog#item-001\",\"depth\":2,\"compact\":true}"
            ),
            json!({
                "addr": addr("…#node 필수"),
                "dir": { "type": "string", "enum": ["out", "in", "both"] },
                "etype": { "type": "string" }, "depth": { "type": "integer" },
                "etypes": { "type": "array", "items": { "type": "string" },
                    "description": "엣지 타입 여러 개. 있으면 etype보다 우선. 예: [\"참조\",\"관련\"]" },
                "emit_labels": { "type": "array", "items": { "type": "string" },
                    "description": "결과에 낼 노드 라벨(하나라도 가진 노드). 예: [\"Category\",\"Brand\"]" },
                "expand_labels": { "type": "array", "items": { "type": "string" },
                    "description": "1단계 이후 노드가 더 뻗는 조건 라벨. 예: [\"Category\",\"Brand\"]" },
                "include": { "type": "array", "items": { "type": "string", "enum": ["labels", "props"] },
                    "description": "각 hit에 실을 내용. 예: [\"labels\",\"props\"]" },
                "prop_keys": { "type": "array", "items": { "type": "string" },
                    "description": "include에 props가 있을 때 실을 속성 키. 예: [\"text\",\"title\"]" },
                "limit": { "type": "integer", "description": "결과 개수 상한(1 이상). 걸리면 truncated:true" },
                "max_bytes": { "type": "integer",
                    "description": "hits 배열 JSON 직렬화 바이트 수 상한(1 이상). 걸리면 truncated:true" },
                "compact": { "type": "boolean",
                    "description": "true면 epoch·via를 빼고 hit을 slug(없으면 uuid)만으로. 예: true" },
            }),
            &["addr"],
        ),
        tool(
            "vgraph_edit",
            "working copy에 op 배치 적용 (호출 간 유지 — commit 전까지 pending). \
             op: add_node{labels,slug,props} | add_edge{etype,from,to} | set_prop{node,key,value} | \
             unset_prop{node,key} | remove_node{node,cascade} | remove_edge{edge} | \
             set_slug{node,slug} | add_label/remove_label{node,label}. \
             props에 embed_target(문자열 또는 문자열 배열)을 지정하면 유사도 검색용 임베딩이 \
             그 텍스트들만으로 만들어진다 (미지정 시 전체 텍스트 폴백). \
             external link는 prop 값에 {\"$ref\":\"graph://ns/slug#node\"} — 축약 ns/slug#node도 \
             허용(자동 보정)되며 commit 시 lockfile에 대상 버전이 핀 고정된다",
            json!({ "graph": addr("ns/slug[@branch]"), "ops": { "type": "array" } }),
            &["graph", "ops"],
        ),
        tool("vgraph_pending", "미커밋 변경 요약", json!({ "graph": addr("ns/slug[@branch]") }), &["graph"]),
        tool(
            "vgraph_commit",
            "working copy를 commit으로 확정. 결과: committed|rebased|empty|conflicts|schema_violation",
            json!({ "graph": addr("ns/slug[@branch]"), "message": { "type": "string" } }),
            &["graph", "message"],
        ),
        tool("vgraph_discard", "working copy 폐기", json!({ "graph": addr("ns/slug[@branch]") }), &["graph"]),
        tool(
            "vgraph_branch",
            "브랜치 생성 (graph 주소가 가리키는 head에서)",
            json!({ "graph": addr("ns/slug[@branch]"), "name": { "type": "string" } }),
            &["graph", "name"],
        ),
        tool(
            "vgraph_merge",
            "src를 dst로 3-way 머지. 충돌 시 items[index] 반환 — theirs:[index]로 해소 재시도 \
             (선택: ours:[index]는 dst 쪽 값을 채택). 번호로 풀 수 있는 것은 prop_both_set(같은 속성을 \
             양쪽이 다르게 바꿈)뿐이다",
            json!({
                "graph": addr("ns/slug"), "src": { "type": "string" }, "dst": { "type": "string" },
                "theirs": { "type": "array", "items": { "type": "integer" } },
                "ours": { "type": "array", "items": { "type": "integer" },
                    "description": "dst(ours) 쪽 값을 채택할 충돌 번호" },
            }),
            &["graph", "src", "dst"],
        ),
        tool("vgraph_log", "커밋 이력 (머지 계보 포함, 최신순)", json!({ "graph": addr("ns/slug[@branch]") }), &["graph"]),
        tool(
            "vgraph_diff",
            "두 버전 비교 (같은 그래프)",
            json!({ "from": addr("주소(브랜치/커밋)"), "to": addr("주소") }),
            &["from", "to"],
        ),
        tool(
            "vgraph_links",
            "external link — action: lockfile(핀 목록) | drifts(핀과 대상 실체의 어긋남 감지) | \
             upgrade(핀 갱신: target=\"ns/slug\", to=대상 그래프의 commit hash 전체 — \
             drifts 응답의 target_head 값을 그대로 사용)",
            json!({
                "graph": addr("ns/slug[@branch]"),
                "action": { "type": "string", "enum": ["lockfile", "drifts", "upgrade"] },
                "target": { "type": "string" }, "to": { "type": "string" },
            }),
            &["graph", "action"],
        ),
        tool(
            "vgraph_admin",
            "관리 — action: gc_plan|gc_run|validate|set_schema(schema: {mode,node_classes,edge_classes}). \
             set_schema는 working copy와 무관하게 **즉시 커밋**된다 (별도 vgraph_commit 불요)",
            json!({
                "graph": addr("ns/slug[@branch]"),
                "action": { "type": "string", "enum": ["gc_plan", "gc_run", "validate", "set_schema"] },
                "schema": { "type": "object" },
            }),
            &["graph", "action"],
        ),
        tool(
            "vgraph_embed_pending",
            "임베딩 staleness 큐 소화 (누락/낡은 노드 재임베딩) — similar 검색 전 실행 권장",
            json!({ "graph": addr("ns/slug[@branch]") }),
            &["graph"],
        ),
        tool(
            "vgraph_find",
            concat!(
                "노드 찾기 — 라벨·속성 완전 일치·부분 문자열(text)로. 모든 조건을 AND로 만족하는 노드를 ",
                "uuid 오름차순으로 준다. 조건을 하나도 안 주면 전체 노드를 나열한다. ",
                "text: 대소문자 무시 부분 문자열(한글 가능). text_keys를 주면 그 속성들만, 안 주면 ",
                "모든 문자열 속성(문자열 리스트 안의 문자열 포함)을 본다. 노드마다 첫 번째로 맞은 키 하나만 보고 ",
                "(matched_key)하고 발췌(excerpt: 맞은 부분 앞뒤 20글자, 잘린 쪽 끝에 …)도 하나만 준다. ",
                "키 순서는 text_keys를 준 순서, 안 주면 속성 키 오름차순. ",
                "대소문자 접기는 문자 1:1 — 소문자가 여러 글자로 바뀌는 드문 문자(예: İ)는 정확히 접히지 않음. ",
                "labels: 나열한 라벨을 모두 가진 노드. props: 속성 완전 일치 객체(모두 일치). ",
                "include: [\"labels\",\"props\"] 중 골라 각 항목에 싣기, prop_keys: include에 props가 있을 때 ",
                "실을 속성 키(생략=전부). limit: 기본 50, 최대 500(넘으면 500). ",
                "응답: {epoch, items:[{uuid, slug, matched_key?, excerpt?, labels?, props?}], next_cursor, done}. ",
                "done=false면 next_cursor를 cursor에 그대로 넣어 이어 받는다. 커서에는 epoch(commit)가 들어 있어 ",
                "그 버전에서 이어 읽고, 주소가 다른 .commit을 가리키면 오류. ",
                "예(시작 노드 찾기): {\"graph\":\"acme/catalog\",\"labels\":[\"item\"],\"text\":\"배송\",",
                "\"text_keys\":[\"name\",\"desc\"],\"include\":[\"labels\",\"props\"],\"limit\":5}. ",
                "예(이어 받기): {\"graph\":\"acme/catalog\",\"text\":\"배송\",\"cursor\":\"<이전 응답의 next_cursor>\"}. ",
                "예(속성 완전 일치): {\"graph\":\"acme/catalog\",\"props\":{\"status\":\"active\"}}. ",
                "선택 인자(주지 않으면 위 동작·응답 그대로): ",
                "order=\"relevance\" — uuid 순 대신 일치 강도 순으로 정렬한다. 점수 = 값 일치 강도×10 + 출처 가산. ",
                "강도: 값 전체 일치 3 > 앞부분 일치 2 > 부분 일치 1, 출처 가산: slug 일치 +2 > 라벨 일치 +1 > 속성 일치 +0 ",
                "(속성에서 text가 맞은 노드만 후보이고 slug·라벨은 점수만 올린다). 동점은 uuid 순. ",
                "커서 없이 상위 limit개만 주며 cursor와 함께 주면 오류. 응답에 order, total_matches(후보 전체 수), ",
                "각 항목 score가 붙고 next_cursor는 null, done=true는 후보가 모두 담겼다는 뜻. ",
                "예: {\"graph\":\"acme/catalog\",\"text\":\"로그인\",\"order\":\"relevance\",\"limit\":5}. ",
                "normalize=true — text·any와 값 양쪽에서 띄어쓰기·밑줄·붙임표(- ‐ ‑ ‒ – — ― − ﹣ －)를 무시하고 비교한다(대소문자 무시는 그대로, ",
                "excerpt는 원문). 예: {\"graph\":\"acme/catalog\",\"text\":\"로그인\",\"normalize\":true}는 \"로그 인\"·\"로그-인\"에도 맞는다. ",
                "any=[\"표현\",…] — 여러 표현 중 하나라도 맞으면 나온다(OR). text도 후보 표현에 포함되고(text가 먼저), ",
                "빈 문자열은 무시한다. 응답 항목에 맞은 표현 matched_expr가 붙는다. ",
                "예: {\"graph\":\"acme/catalog\",\"any\":[\"결제\",\"배송\"],\"labels\":[\"item\"]}. ",
                "skip_ref_like=true — 값 전체가 그래프 주소(graph://ns/slug[@branch][.commit][#node], 앞뒤 공백 허용·",
                "안에 공백 없음)뿐인 문자열 속성은 text·any 검색 대상에서 뺀다(문자열 리스트는 원소마다 판정). ",
                "주소가 문장 일부로 섞인 값과 graph:// 없는 축약형(ns/slug#node)은 일반 텍스트라 그대로 검색한다. ",
                "text·any가 없으면 효과 없음. order·normalize·any와 함께 쓸 수 있다. ",
                "예: {\"graph\":\"acme/catalog\",\"text\":\"acme/rules\",\"skip_ref_like\":true}. ",
                "compact=true — 간결 응답: epoch를 빼고(커서가 epoch를 담고 있어 이어 받기는 그대로) 항목은 slug가 ",
                "있으면 slug만, 없으면 uuid만 싣는다(system·matched_key·excerpt·matched_expr·score·labels·props는 유지). ",
                "예: {\"graph\":\"acme/catalog\",\"text\":\"배송\",\"compact\":true}"
            ),
            json!({
                "graph": addr("ns/slug[@branch][.commit]"),
                "labels": { "type": "array", "items": { "type": "string" },
                    "description": "모두 가진 노드. 예: [\"item\"]" },
                "props": { "type": "object",
                    "description": "속성 완전 일치(모두). 예: {\"status\":\"active\"}" },
                "text": { "type": "string",
                    "description": "대소문자 무시 부분 문자열. 예: \"배송\"" },
                "text_keys": { "type": "array", "items": { "type": "string" },
                    "description": "text를 볼 속성 키(생략=모든 문자열 속성). 예: [\"name\",\"desc\"]" },
                "include": { "type": "array", "items": { "type": "string", "enum": ["labels", "props"] },
                    "description": "각 항목에 실을 내용. 예: [\"labels\",\"props\"]" },
                "prop_keys": { "type": "array", "items": { "type": "string" },
                    "description": "include에 props가 있을 때 실을 속성 키. 예: [\"text\"]" },
                "limit": { "type": "integer", "description": "기본 50, 최대 500" },
                "cursor": { "type": "string", "description": "이전 응답의 next_cursor 그대로" },
                "order": { "type": "string", "enum": ["relevance"],
                    "description": "생략=uuid 순. \"relevance\"=일치 강도 순(커서 없이 상위 limit개, cursor와 함께 주면 오류). 예: \"relevance\"" },
                "normalize": { "type": "boolean",
                    "description": "true면 띄어쓰기·밑줄·붙임표를 무시하고 비교. 예: true" },
                "any": { "type": "array", "items": { "type": "string" },
                    "description": "여러 표현 중 하나라도 맞으면(OR). text도 후보에 포함. 예: [\"결제\",\"배송\"]" },
                "skip_ref_like": { "type": "boolean",
                    "description": "true면 값 전체가 graph:// 주소뿐인 문자열 속성은 text·any 검색에서 제외(문장 속 주소는 그대로). 예: true" },
                "compact": { "type": "boolean",
                    "description": "true면 epoch를 빼고 항목을 slug(없으면 uuid)만으로. cursor는 그대로. 예: true" },
            }),
            &["graph"],
        ),
        tool(
            "vgraph_scan",
            concat!(
                "커서로 노드·엣지를 끝까지 나열 (vgraph_show의 200개 자르기를 넘어 전체를 받는 용도). ",
                "kind=nodes|edges. uuid 오름차순. limit: 기본 200, 최대 1000(넘으면 1000). ",
                "labels[](kind=nodes만): 나열한 라벨을 모두 가진 노드. include_props: true면 항목에 props 포함 ",
                "(노드·엣지 모두, 속성 없는 엣지는 {}). ",
                "응답: {epoch, kind, items, next_cursor, done}. 노드 항목 {uuid, slug, labels, props?, system?}, ",
                "엣지 항목 {uuid, type, from, to, from_slug?, to_slug?, props?}. 시스템 노드(slug가 __로 시작)는 ",
                "system:true로 표시되어 함께 나온다 — vgraph_show의 node_count는 시스템 노드를 뺀 수다. ",
                "done=false면 next_cursor를 cursor에 그대로 넣어 이어 받는다. 커서에는 epoch(commit)가 들어 있어 ",
                "첫 호출과 같은 버전에서 이어 읽고, 주소가 다른 .commit을 가리키면 오류. ",
                "예: {\"graph\":\"acme/catalog\",\"kind\":\"nodes\",\"include_props\":true,\"limit\":200}. ",
                "예(이어 받기): {\"graph\":\"acme/catalog\",\"kind\":\"nodes\",\"cursor\":\"<이전 응답의 next_cursor>\"}. ",
                "예(엣지, 속성 포함): {\"graph\":\"acme/catalog\",\"kind\":\"edges\",\"include_props\":true}. ",
                "compact=true — 간결 응답: epoch를 빼고(next_cursor·done은 그대로), 노드 항목은 slug가 있으면 slug만 ",
                "없으면 uuid만, 엣지 항목은 엣지 uuid를 빼고 양끝을 slug가 있으면 from_slug/to_slug만 없으면 from/to(uuid)로 싣는다. ",
                "엣지 uuid가 필요하면(편집 등) compact를 끄고 부른다. ",
                "예: {\"graph\":\"acme/catalog\",\"kind\":\"edges\",\"compact\":true}"
            ),
            json!({
                "graph": addr("ns/slug[@branch][.commit]"),
                "kind": { "type": "string", "enum": ["nodes", "edges"] },
                "labels": { "type": "array", "items": { "type": "string" },
                    "description": "kind=nodes만. 모두 가진 노드. 예: [\"Category\"]" },
                "include_props": { "type": "boolean", "description": "항목에 props 포함(nodes·edges 모두)" },
                "limit": { "type": "integer", "description": "기본 200, 최대 1000" },
                "cursor": { "type": "string", "description": "이전 응답의 next_cursor 그대로" },
                "compact": { "type": "boolean",
                    "description": "true면 epoch·엣지 uuid를 빼고 slug(없으면 uuid)만으로. cursor는 그대로. 예: true" },
            }),
            &["graph", "kind"],
        ),
        tool(
            "vgraph_export",
            concat!(
                "그래프를 JSON 문서(형식 vgraph-json/1)로 내보낸다 — 백업·다른 그래프로 옮기기·외부 변환기 입력용. ",
                "형식: {\"format\":\"vgraph-json/1\",\"nodes\":[{\"slug\":\"item-001\",\"labels\":[\"Item\"],",
                "\"props\":{\"name\":\"예시\"}}],\"edges\":[{\"etype\":\"참조\",\"from\":\"item-001\",",
                "\"to\":\"rule-001\",\"props\":{}}]}. 엣지의 from·to는 노드 slug다(uuid는 내보내지 않음). ",
                "노드는 slug 순, 엣지는 (etype, from, to, props) 순으로 정렬되어 같은 그래프는 늘 같은 결과가 나온다. ",
                "graph로 브랜치·커밋을 고를 수 있다(기본 main의 head). ",
                "path를 주면 그 파일에 쓰고 응답에는 본문을 싣지 않는다(큰 그래프는 path 권장), 안 주면 응답의 document에 담는다. ",
                "응답: {epoch, nodes, edges, path | document}. ",
                "slug 없는 노드가 하나라도 있으면 내보내기를 거부하고 그 노드 목록(uuid·라벨)을 준다 — ",
                "vgraph_edit의 set_slug로 slug를 붙인 뒤 다시 시도한다. ",
                "__schema(스키마)는 일반 노드로 포함되고, __lockfile(링크 핀 기록)은 내보내지 않는다 ",
                "(__lockfile에 닿는 엣지가 있으면 거부). ",
                "예: {\"graph\":\"acme/catalog\",\"path\":\"C:/work/catalog.json\"}. ",
                "예(특정 브랜치): {\"graph\":\"acme/catalog@draft\"}. ",
                "나눠 받기(원격·큰 그래프): max_bytes(조각 하나의 최대 바이트, 1024~4194304)를 주면 응답에 문서 대신 ",
                "{epoch, nodes, edges, chunk, offset, total_bytes, done, next_cursor} 한 조각이 온다. ",
                "첫 호출은 그 순간의 커밋에 고정한다. done=false이면 next_cursor를 cursor로, 같은 graph·max_bytes와 함께 다시 부른다 — ",
                "그 사이 브랜치에 새 커밋이 생겨도 같은 판의 다음 조각이 온다. done=true가 올 때까지 chunk를 순서대로 이어 붙이면 ",
                "한 번에 내보낸 문서와 바이트 단위로 같다(조각은 문자 중간에서 자르지 않는다). path와는 함께 쓸 수 없다. ",
                "cursor는 불투명 문자열이니 그대로 돌려준다. ",
                "권장 max_bytes: 64KB~1MB(조각이 응답 안에서 JSON 이스케이프로 커질 수 있다). ",
                "예(나눠 받기): {\"graph\":\"acme/catalog\",\"max_bytes\":65536} → {\"graph\":\"acme/catalog\",\"max_bytes\":65536,\"cursor\":\"<next_cursor>\"}"
            ),
            json!({
                "graph": addr("ns/slug[@branch][.commit]"),
                "path": { "type": "string", "description": "쓸 파일 경로(없으면 응답에 document로). 예: \"C:/work/catalog.json\"" },
                "max_bytes": { "type": "integer", "description": "나눠 받기: 조각 하나의 최대 바이트(1024~4194304). 주면 chunk·next_cursor·done 응답. path와 함께 쓸 수 없음. 권장 64KB~1MB. 예: 65536" },
                "cursor": { "type": "string", "description": "나눠 받기 이어 받기: 이전 응답의 next_cursor 그대로(max_bytes 필수)" },
            }),
            &["graph"],
        ),
        tool(
            "vgraph_import",
            concat!(
                "JSON 문서(형식 vgraph-json/1)를 그래프에 넣는다. mode=replace(기본): ",
                "빈 그래프에만 허용되고 비어 있지 않으면 거부한다. 문서 전체를 커밋 한 번으로 넣는다(가져오기 1회 = 커밋 1개). ",
                "mode=merge(문서가 바뀔 때마다 다시 가져와 동기화): via_branch(가져오기 전용 브랜치 이름, 필수, 기본값 없음)를 ",
                "문서와 똑같이 맞춘 뒤 기존 3-way 머지로 대상 브랜치에 합친다. 첫 가져오기(via_branch가 아직 없을 때)는 ",
                "대상이 빈 그래프일 때만 되고, 대상에 넣은 커밋에서 via_branch를 만든다(비어 있지 않으면 기준점이 없어 거부). ",
                "이후 가져오기는 노드를 slug로, 엣지를 (etype, from, to)로 맞춰 속성·라벨을 문서대로 바꾸고(문서에 없는 키·라벨·노드·엣지는 지움) ",
                "문서와 이미 같으면 커밋을 만들지 않는다. mode=merge에서는 문서 안에 같은 (etype, from, to) 엣지가 둘 이상이면 거부한다. ",
                "머지 결과 outcome: first_import | fast_forward | merged | up_to_date | conflicts. ",
                "충돌이면 대상 브랜치는 바뀌지 않고, report.items와 resolve 안내가 온다 — 기존 vgraph_merge를 ",
                "{\"graph\":\"acme/catalog\",\"src\":\"<via_branch>\",\"dst\":\"main\",\"theirs\":[0]}처럼 불러 해소한다 ",
                "(theirs=문서 값, ours=대상 브랜치 값). 번호로 풀 수 있는 것은 prop_both_set(같은 속성을 양쪽이 다르게 바꿈)뿐이고, ",
                "removed_vs_edit(삭제 대 수정)·dangling_edge·slug_collision·schema_clash는 번호로 풀 수 없다 — 대상이나 문서를 고쳐 다시 가져온다. ",
                "충돌이 난 채로 다시 가져오면 via_branch가 새 문서에 맞춰지고 충돌을 새로 계산한다. ",
                "문서는 document(객체 또는 JSON 문자열) 또는 path(파일 경로) 중 정확히 하나로 준다 — 큰 문서는 path. ",
                "형식: {\"format\":\"vgraph-json/1\",\"nodes\":[{\"slug\":\"item-001\",\"labels\":[\"Item\"],",
                "\"props\":{\"name\":\"예시\"}}],\"edges\":[{\"etype\":\"참조\",\"from\":\"item-001\",",
                "\"to\":\"rule-001\",\"props\":{}}]}. 엣지의 from·to는 노드 slug. 새 uuid가 발급된다. ",
                "저장소에 쓰기 전에 검증해 문제가 하나라도 있으면 아무것도 쓰지 않고 전체를 거부한다: ",
                "형식 오류, slug 중복, 엣지가 문서에 없는 slug를 가리킴(빠진 slug 목록을 준다), ",
                "__lockfile 노드 포함, 모르는 칸(문서 최상위·nodes[i]·edges[i]에 형식이 정의하지 않은 칸 — 예: labels를 label로 씀)이 있으면 ",
                "위치 목록을 돌려주며 거부(replace·merge 모두). 예시 오류: \"모르는 칸 1 건 … nodes[3].label\". 미커밋 편집이 남아 있으면 거부(먼저 vgraph_commit·vgraph_discard). ",
                "커밋 단계에서 스키마 위반 등으로 실패하면 작업분을 버리고 오류를 돌려준다(head 불변). ",
                "링크: 다른 그래프를 가리키는 {\"$ref\":\"ns/slug#node\"} 값은 그대로 가져오되(vgraph_edit와 같은 보정·검증), ",
                "고정 버전(핀)은 가져오는 시점의 대상 최신으로 다시 잡힌다. ",
                "응답: {outcome: committed|empty, commit, nodes, edges, warnings}. message로 커밋 메시지를 줄 수 있다. ",
                "예: {\"graph\":\"acme/catalog\",\"path\":\"C:/work/catalog.json\"}. ",
                "예(merge, 반복 동기화): {\"graph\":\"acme/catalog\",\"mode\":\"merge\",\"via_branch\":\"import\",\"path\":\"C:/work/catalog.json\"}. ",
                "예(인라인): {\"graph\":\"acme/catalog\",\"document\":{\"format\":\"vgraph-json/1\",",
                "\"nodes\":[{\"slug\":\"item-001\",\"labels\":[\"Item\"],\"props\":{}}],\"edges\":[]}}. ",
                "나눠 올리기(원격·큰 문서): upload로 문서를 조각으로 올린 뒤 확정한다. ",
                "① {\"graph\":\"acme/catalog\",\"upload\":\"begin\"}(mode·via_branch·message는 여기서만 주고 검증됨) → upload_id. ",
                "② {\"graph\":\"acme/catalog\",\"upload\":\"append\",\"upload_id\":\"…\",\"seq\":0,\"chunk\":\"문서의 앞부분\"} — ",
                "seq는 0부터 하나씩 늘리고 틀리면 기대 값을 알려 주며 거부, 응답은 {received_bytes, next_seq}. ",
                "③ {\"graph\":\"acme/catalog\",\"upload\":\"commit\",\"upload_id\":\"…\"} — 모은 문서를 위 규칙 그대로(검증·모르는 칸 거부·커밋 한 번·replace/merge) 넣고 ",
                "한 번에 가져오기와 같은 응답을 준다. 확정이 거부돼도 업로드는 지워지니 처음부터 다시 올린다. ",
                "권장 chunk 크기: 64KB~1MB, 8MiB 이하(상주 서버(HTTP)는 요청 본문 16MiB까지만 받아 넘으면 413 — 이스케이프 포함). ",
                "그만두려면 {\"graph\":\"acme/catalog\",\"upload\":\"abort\",\"upload_id\":\"…\"}. graph는 매 호출에 주고 begin과 같아야 한다. ",
                "document·path와는 함께 쓸 수 없다. 업로드는 서버 프로세스 메모리에만 있다(서버를 재시작하면 사라짐). ",
                "시작한 사용자·작업 공간만 이어 쓸 수 있고 다른 쪽에는 없는 업로드처럼 보인다. ",
                "상한(기본): 문서 64MiB, 동시 8개, 마지막 사용 후 10분 지나면 만료 — 넘으면 명확한 오류."
            ),
            json!({
                "graph": addr("ns/slug[@branch] — 비어 있어야 함"),
                "mode": { "type": "string", "enum": ["replace", "merge"], "description": "기본 replace(빈 그래프에만). merge는 via_branch 필요" },
                "via_branch": { "type": "string", "description": "mode=merge 전용: 가져오기 전용 브랜치 이름(영문·숫자·-·_·/ — 빈 이름·앞뒤 /·// 불가), 기본값 없음. 예: \"import\"" },
                "document": { "description": "가져올 문서(객체 또는 JSON 문자열). path와 둘 중 하나" },
                "path": { "type": "string", "description": "가져올 문서 파일 경로. document와 둘 중 하나" },
                "message": { "type": "string", "description": "커밋 메시지(생략 시 기본 문구)" },
                "upload": { "type": "string", "enum": ["begin", "append", "commit", "abort"],
                    "description": "나눠 올리기 단계. document·path와 함께 쓸 수 없음. 예: \"begin\"" },
                "upload_id": { "type": "string", "description": "upload=append|commit|abort: begin 응답의 upload_id" },
                "seq": { "type": "integer", "description": "upload=append: 조각 번호(0부터 하나씩)" },
                "chunk": { "type": "string", "description": "upload=append: 문서의 다음 부분(문자열 그대로 이어 붙임). 권장 64KB~1MB, 8MiB 이하(HTTP 요청 본문 16MiB 초과 시 413)" },
            }),
            &["graph"],
        ),
    ]
}

// ---------- 명령줄 진입점 ----------
//
// - 하위 명령 없이 실행하면 stdio MCP 서버다. 등록 예: claude mcp add vgraph -- vgraph-mcp --root ./vgraph-data --user local
// - `serve`는 여러 클라이언트가 함께 쓰는 상주 HTTP 서버, `stop`·`status`는 그 서버를 끄고 살핀다
//   (기능 `server`, 기본 켬 — `src/serve.rs`).
// - `service`는 그 서버를 사용자 로그인 때 자동으로 띄우도록 등록·해제·확인한다 (`src/service.rs`).

/// 데이터 폴더 기본값. stdio 모드와 서버 모드가 같은 값을 쓴다.
const DEFAULT_ROOT: &str = "./vgraph-data";

/// stdio 모드와 `serve`가 함께 쓰는 인자. 해석(`build_server`)도 하나를 같이 거친다.
#[derive(Args)]
struct CommonArgs {
    /// 저장소 루트 디렉터리
    #[arg(long, default_value = DEFAULT_ROOT)]
    root: PathBuf,
    /// 세션 사용자 (ACL 소유자)
    #[arg(long, default_value = "local")]
    user: String,
    /// 임베더 spec: none | toy | toy:<dim> | openai:<model> | ollama:<model>[@url] | http:<url>
    #[arg(long, default_value = "none")]
    embedder: String,
    /// 임베딩 대상 필드 (쉼표 구분, 미지정 = 모든 문자열 prop)
    #[arg(long)]
    embed_fields: Option<String>,
    /// 청킹: whole[:max_chars] | props | window:<size>:<overlap>
    #[arg(long, default_value = "whole")]
    chunker: String,
    /// embed_target 없는 노드의 폴백(전체 텍스트) 임베딩 비활성
    #[arg(long)]
    no_embed_fallback: bool,
}

#[derive(Parser)]
#[command(
    name = "vgraph-mcp",
    version,
    about = "revive-vgraph MCP 서버 (stdio)",
    after_help = AFTER_HELP,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,
    /// 이름 있는 작업 공간 (생략하면 기본 작업 공간).
    /// 여러 프로세스·에이전트가 같은 사용자로 같은 그래프·같은 브랜치를 동시에 편집할 때는 각자 다른 --workspace 이름으로 서버를 띄워야 합니다.
    /// 이름 없이(기본 작업 공간) 동시에 편집하면 편집 내용이 사라질 수 있습니다.
    /// 작업 공간 이름은 동시에 쓰는 프로세스마다 고유해야 하며, 같은 이름을 동시에 쓰면 이름 없이 쓸 때와 같이 편집이 사라질 수 있습니다.
    /// 이름이 다르면 커밋할 때 자동으로 합쳐지고, 같은 값을 다르게 바꿨으면 충돌로 보고됩니다.
    /// 이름은 영문·숫자·-·_ 만 쓸 수 있습니다.
    #[arg(long)]
    workspace: Option<String>,
    #[cfg(feature = "server")]
    #[command(subcommand)]
    command: Option<serve::Command>,
}

#[cfg(feature = "server")]
const AFTER_HELP: &str =
    "서버 모드: 여러 클라이언트가 한 데이터 폴더를 함께 쓰려면 하위 명령으로 상주 서버를 띄웁니다 \
(vgraph-mcp serve --help 참고), 로그인 때 자동으로 뜨게 하려면 vgraph-mcp service --help 를 보세요. 하위 명령 없이 실행하면 위의 stdio 서버입니다.";
#[cfg(not(feature = "server"))]
const AFTER_HELP: &str =
    "이 빌드에는 상주 서버 기능(serve·stop·status·service)이 들어 있지 않습니다.";

fn chunker_of(s: &str) -> anyhow::Result<Chunker> {
    if s == "props" {
        return Ok(Chunker::Props);
    }
    if let Some(rest) = s.strip_prefix("window:") {
        let (size, overlap) = rest
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("window:<size>:<overlap>"))?;
        return Ok(Chunker::Window {
            size: size.parse()?,
            overlap: overlap.parse()?,
        });
    }
    if s == "whole" {
        return Ok(Chunker::Whole { max_chars: 8000 });
    }
    if let Some(max) = s.strip_prefix("whole:") {
        return Ok(Chunker::Whole {
            max_chars: max.parse()?,
        });
    }
    anyhow::bail!("chunker는 whole[:max] | props | window:<size>:<overlap>: {s}")
}

/// 공통 인자로 서버를 만든다. stdio 모드와 `serve`가 같은 해석을 거친다.
fn build_server(a: CommonArgs) -> anyhow::Result<McpServer> {
    McpServer::new(McpConfig {
        root: a.root,
        user: a.user,
        embedder: if a.embedder == "none" {
            None
        } else {
            Some(a.embedder.clone())
        },
        embed: EmbedSpec {
            fields: a
                .embed_fields
                .map(|f| f.split(',').map(|x| x.trim().to_string()).collect()),
            chunker: chunker_of(&a.chunker)?,
            fallback: !a.no_embed_fallback,
        },
    })
}

/// `vgraph-mcp` 바이너리의 본체(하위 명령 해석 → stdio 서버 또는 상주 서버 명령).
fn run_mcp_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    #[cfg(feature = "server")]
    if let Some(command) = cli.command {
        return serve::run(command);
    }

    let server = build_server(cli.common)?.with_workspace(cli.workspace)?;
    mcp_kit::serve_stdio(|line| server.handle_line(line))?;
    Ok(())
}

/// `vgraph-mcp` 바이너리의 본체. 프로세스 인자를 읽어 실행하고 종료 코드를 돌려준다(성공 0, 오류 1).
///
/// 반환값을 `main` 에서 그대로 돌려주면 된다: `fn main() -> ExitCode { vgraph_mcp::run_mcp() }`.
pub fn run_mcp() -> ExitCode {
    run_mcp_with(&|_| {})
}

/// [`run_mcp`] 와 같되, 명령이 실패해 `Error: …` 를 표준 오류에 쓴 **직후** `on_fatal_error` 를 부른다.
/// 출력과 종료 코드는 처리기를 주지 않았을 때와 같다 — 처리기는 구경만 한다(예: 오류를 별도 수집기에 알리기).
/// 표준 출력은 JSON-RPC 전용이므로 처리기는 표준 출력에 아무것도 쓰면 안 된다.
pub fn run_mcp_with(on_fatal_error: &dyn Fn(&anyhow::Error)) -> ExitCode {
    match run_mcp_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `fn main() -> anyhow::Result<()>` 가 오류를 돌려줄 때 표준 라이브러리가 하던 출력과 같다(표준 오류가 닫혀 있어도
            // 패닉하지 않고 넘어간다).
            {
                use std::io::Write as _;
                let _ = writeln!(std::io::stderr(), "Error: {e:?}");
            }
            on_fatal_error(&e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    fn plan() -> ImportPlan {
        ImportPlan {
            graph: "acme/catalog".into(),
            via: None,
            message: "m".into(),
        }
    }

    fn limits(idle_secs: u64) -> UploadLimits {
        UploadLimits {
            max_bytes: 10,
            max_open: 2,
            idle: Duration::from_secs(idle_secs),
        }
    }

    fn who(user: &str) -> Identity {
        Identity::new(user, None).unwrap()
    }

    #[test]
    fn idle_expiry_boundary_and_touch_on_append() {
        let t0 = Instant::now();
        let owner = who("a");
        let mut u = Uploads::new(limits(60));
        let id = u.begin(t0, &owner, plan()).unwrap();
        // 만료 직전까지는 살아 있고, append가 유휴 시계를 다시 시작한다
        let t59 = t0 + Duration::from_secs(59);
        assert_eq!(
            u.append(t59, &id, &owner, "acme/catalog", 0, "ab").unwrap(),
            (2, 1)
        );
        let t118 = t59 + Duration::from_secs(59);
        assert_eq!(
            u.append(t118, &id, &owner, "acme/catalog", 1, "cd")
                .unwrap(),
            (4, 2)
        );
        // 정확히 유휴 시간이 지나면 만료되어 폐기된다
        let t178 = t118 + Duration::from_secs(60);
        assert_eq!(
            u.append(t178, &id, &owner, "acme/catalog", 2, "x")
                .unwrap_err(),
            UPLOAD_GONE
        );
        assert!(u.open.is_empty(), "접근 때 폐기");
    }

    #[test]
    fn sweep_on_begin_frees_slots_and_foreign_identity_sees_gone() {
        let t0 = Instant::now();
        let (a, b) = (who("a"), who("b"));
        let mut u = Uploads::new(limits(60));
        let id1 = u.begin(t0, &a, plan()).unwrap();
        u.begin(t0, &a, plan()).unwrap();
        assert!(u.begin(t0, &a, plan()).is_err(), "동시 상한");
        // 남의 접근은 없는 업로드와 같은 오류이며 업로드를 지우지 않는다
        assert_eq!(
            u.take(t0, &id1, &b, "acme/catalog").err().unwrap(),
            UPLOAD_GONE
        );
        assert_eq!(
            u.take(t0, "up-none", &b, "acme/catalog").err().unwrap(),
            UPLOAD_GONE
        );
        assert_eq!(u.open.len(), 2);
        // 시간이 지나면 begin이 만료분을 치워 슬롯이 돌아온다
        u.begin(t0 + Duration::from_secs(61), &a, plan()).unwrap();
        assert_eq!(u.open.len(), 1);
    }
}
