//! JSON 가져오기·내보내기 (R5) — 형식 `vgraph-json/1`.
//!
//! ```json
//! {"format":"vgraph-json/1",
//!  "nodes":[{"slug":"rule-1","labels":["Rule"],"props":{"name":"규칙"}}],
//!  "edges":[{"etype":"참조","from":"rule-1","to":"item-1","props":{}}]}
//! ```
//!
//! - 엣지의 `from`·`to`는 노드 slug다. uuid는 내보내지 않고, 가져오면 새 uuid가 발급된다.
//! - 내보내기는 노드를 slug 순, 엣지를 (etype, from, to, props canonical 표기) 순으로 정렬한다.
//!   같은 그래프는 늘 같은 바이트가 된다. 값 표기는 `vgraph_core::canon`을 그대로 쓴다
//!   (Int/Float/Ref/Decimal/Ts/Bytes 구분 보존).
//! - slug 없는 노드가 있으면 내보내기를 거부한다. `__schema`는 일반 노드로 포함하고
//!   `__lockfile`은 뺀다(`__lockfile`에 닿는 엣지가 있으면 거부) — 같은 root의 그래프 ID·커밋
//!   핀을 담고 있어 옮겨도 뜻이 없고, 가져오기 commit이 다시 만든다.
//! - 가져오기는 두 가지다. `replace`: 빈 그래프에만([`import_replace`]). `merge`: 가져오기 전용 브랜치를
//!   문서와 똑같이 맞춘 뒤 기존 `Session::merge`로 대상 브랜치에 합친다([`import_merge`]) —
//!   충돌 판정·해소는 엔진의 기존 머지를 그대로 쓴다(이 모듈은 머지 코드를 건드리지 않는다).
//!   replace 의 단계는 다음과 같다. 단계: 문서 검증([`parse_document`], 저장소
//!   접근 없음) → 대상 확인 → 세션 열기 → 적용 → commit/discard. 커밋은 기존
//!   `EditSession → Session::commit` 경로만 쓴다(저장소 직접 쓰기 없음).
//! - 문서·`nodes[i]`·`edges[i]`에 위 형식이 정의하지 않은 칸이 하나라도 있으면(예: `labels`를 `label`로 씀)
//!   아무것도 넣지 않고 전부 거부한다([`InterchangeError::UnknownFields`] — 위치 목록). `props` 안의 키는 사용자 데이터라 검사하지 않는다.
//! - 이 모듈이 JSON 파싱·직렬화를 전담한다. 호출자는 문자열이나 파일 경로만 넘긴다.

use crate::error::{
    CommitOutcome, ConflictReport, EditStartError, EngineError, MergeOpts, MergeOutcome,
    ResolveError,
};
use crate::schema::SchemaIssue;
use crate::{EditSession, Session};
use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use vgraph_core::canon::{esc, props_from_json, props_json};
use vgraph_core::{Edge, GraphAddr, GraphId, GraphState, Hash256, Node, Props, Uuid};

/// 문서 형식 식별자.
pub const FORMAT: &str = "vgraph-json/1";
/// 내보내지 않는 시스템 노드 slug. 엔진 내부 상수(비공개)와 같은 값이다.
const LOCKFILE_SLUG: &str = "__lockfile";
/// 거부 사유를 오류 문장에 풀어 쓸 때 보여 줄 최대 개수.
const PROBLEMS_SHOWN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProblemCode {
    /// 내보내기: slug 없는 노드 (subject = "uuid [라벨,…]").
    SluglessNode,
    /// 내보내기: `__lockfile`에 닿는 엣지 (subject = "etype from→to").
    LockfileEdge,
    /// 가져오기: 문서 안 slug 중복 (subject = slug).
    DuplicateSlug,
    /// 가져오기: 엣지가 가리키는데 문서에 없는 slug (subject = slug).
    MissingSlug,
    /// 가져오기: 문서에 `__lockfile` 노드가 있음 (subject = slug).
    LockfileNode,
    /// merge 가져오기: 문서 안에 같은 (etype, from, to)가 둘 이상 (subject = "etype from→to").
    DuplicateEdge,
}

impl ProblemCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemCode::SluglessNode => "slugless_node",
            ProblemCode::LockfileEdge => "lockfile_edge",
            ProblemCode::DuplicateSlug => "duplicate_slug",
            ProblemCode::MissingSlug => "missing_slug",
            ProblemCode::LockfileNode => "lockfile_node",
            ProblemCode::DuplicateEdge => "duplicate_edge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: ProblemCode,
    pub subject: String,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InterchangeError {
    /// 문서가 JSON·형식 규칙을 어김.
    #[error("가져오기 문서 형식 오류: {0}")]
    Format(String),
    /// 내용 검증 실패 — 저장소에는 아무것도 쓰지 않았다.
    #[error("거부됨 {} 건: {}", .0.len(), problems_text(.0))]
    Rejected(Vec<Problem>),
    /// 문서에 정의되지 않은 칸이 있음 — 저장소에는 아무것도 쓰지 않았다. 위치는 전부 담는다(자르지 않음).
    #[error("가져오기 문서에 모르는 칸 {} 건이 있어 거부됨 (아무것도 넣지 않음): {}", .0.len(), .0.join(", "))]
    UnknownFields(Vec<String>),
    #[error("replace 가져오기는 빈 그래프에만 가능합니다 (현재 노드 {nodes}개, 엣지 {edges}개)")]
    NotEmpty { nodes: usize, edges: usize },
    #[error("미커밋 편집이 남아 있습니다 — 먼저 vgraph_commit 하거나 vgraph_discard 하세요")]
    PendingEdits,
    /// merge 가져오기에서 기준점(공통 조상)을 만들 수 없음.
    #[error("merge 가져오기를 시작할 수 없습니다: {0}")]
    NoBaseline(String),
    #[error("대상 주소 오류: {0}")]
    BadAddr(String),
    #[error("파일 입출력 오류: {0}")]
    Io(String),
    /// 나눠 받기 요청이 범위를 벗어남(조각 크기·위치).
    #[error("나눠 받기 오류: {0}")]
    Part(String),
    #[error("스키마 위반으로 거부됨 ({} 건) — head 불변", .0.len())]
    SchemaViolation(Vec<SchemaIssue>),
    #[error("커밋 충돌로 거부됨 ({} 건) — head 불변", .0.items.len())]
    Conflicts(Box<ConflictReport>),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

fn problems_text(ps: &[Problem]) -> String {
    let mut out = String::new();
    for (i, p) in ps.iter().take(PROBLEMS_SHOWN).enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let _ = write!(out, "{}: {}", p.code.as_str(), p.subject);
    }
    if ps.len() > PROBLEMS_SHOWN {
        let _ = write!(out, " … 외 {}건", ps.len() - PROBLEMS_SHOWN);
    }
    out
}

// ====================== 내보내기 ======================

/// 조각 하나의 최소 바이트. UTF-8 문자 하나(최대 4바이트)보다 충분히 커서 조각이 늘 앞으로 나아간다.
pub const PART_MIN_BYTES: usize = 1024;
/// 조각 하나의 최대 바이트 — 응답 하나가 메모리·전송에서 감당할 상한.
pub const PART_MAX_BYTES: usize = 4 * 1024 * 1024;

/// 내보내기 결과 — `text`는 문서 전문(끝에 개행 한 줄).
#[derive(Debug, Clone)]
pub struct Exported {
    /// 내보낸 버전의 commit hash.
    pub epoch: Hash256,
    pub nodes: usize,
    pub edges: usize,
    pub text: String,
}

/// 문서 전문에서 잘라 낸 조각 하나.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportPart<'a> {
    pub text: &'a str,
    /// 이 조각이 시작하는 바이트 위치.
    pub offset: usize,
    /// 다음 조각의 시작 위치. 마지막 조각이면 `None`.
    pub next_offset: Option<usize>,
}

impl Exported {
    /// 문서 전문의 `offset` 바이트부터 최대 `max_bytes` 바이트를 UTF-8 문자 경계에서 잘라 돌려준다.
    /// 모든 조각을 순서대로 이으면 `text`와 바이트 단위로 같다.
    /// `max_bytes`는 [`PART_MIN_BYTES`]..=[`PART_MAX_BYTES`], `offset`은 문자 경계이면서 끝 앞이어야 한다.
    pub fn part(
        &self,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ExportPart<'_>, InterchangeError> {
        if !(PART_MIN_BYTES..=PART_MAX_BYTES).contains(&max_bytes) {
            return Err(InterchangeError::Part(format!(
                "max_bytes는 {PART_MIN_BYTES} 이상 {PART_MAX_BYTES} 이하여야 합니다 (받은 값: {max_bytes})"
            )));
        }
        let len = self.text.len();
        if offset >= len || !self.text.is_char_boundary(offset) {
            return Err(InterchangeError::Part(format!(
                "조각 시작 위치가 문서 범위·문자 경계를 벗어났습니다 (위치 {offset}, 문서 {len}바이트)"
            )));
        }
        let mut end = offset.saturating_add(max_bytes).min(len);
        while !self.text.is_char_boundary(end) {
            end -= 1; // end > offset: max_bytes >= 4 이고 offset 은 문자 경계
        }
        let text = self
            .text
            .get(offset..end)
            .ok_or_else(|| InterchangeError::Part("조각 범위 계산 오류".into()))?;
        Ok(ExportPart {
            text,
            offset,
            next_offset: (end < len).then_some(end),
        })
    }

    pub fn write_to(&self, path: &Path) -> Result<(), InterchangeError> {
        std::fs::write(path, self.text.as_bytes())
            .map_err(|e| InterchangeError::Io(format!("{}: {e}", path.display())))
    }
}

fn full_addr(s: &str) -> String {
    if s.starts_with("graph://") {
        s.to_string()
    } else {
        format!("graph://{s}")
    }
}

/// `addr` = "ns/slug[@branch][.commit]" (기본 main의 head).
pub fn export(sess: &Session, addr: &str) -> Result<Exported, InterchangeError> {
    let (g, commit) = sess
        .resolve(&full_addr(addr))
        .map_err(|e| InterchangeError::Engine(EngineError::from(e)))?;
    let view = sess.checkout(g, commit)?;
    export_state(&view.to_state(), commit)
}

fn export_state(
    state: &vgraph_core::GraphState,
    epoch: Hash256,
) -> Result<Exported, InterchangeError> {
    let mut problems: Vec<Problem> = Vec::new();
    let mut slug_of: HashMap<Uuid, &str> = HashMap::new();
    let mut lockfile: Option<Uuid> = None;
    let mut nodes: Vec<(&str, &vgraph_core::Node)> = Vec::new();
    for n in state.nodes() {
        match n.slug.as_deref() {
            None => {
                let labels: Vec<&str> = n.labels.iter().map(SmolStr::as_str).collect();
                problems.push(Problem {
                    code: ProblemCode::SluglessNode,
                    subject: format!("{} [{}]", n.uuid, labels.join(",")),
                });
            }
            Some(LOCKFILE_SLUG) => lockfile = Some(n.uuid),
            Some(s) => {
                slug_of.insert(n.uuid, s);
                nodes.push((s, n));
            }
        }
    }

    // (etype, from, to, props canonical 표기)
    let mut edges: Vec<(&str, &str, &str, String)> = Vec::new();
    for e in state.edges() {
        if Some(e.from) == lockfile || Some(e.to) == lockfile {
            problems.push(Problem {
                code: ProblemCode::LockfileEdge,
                subject: format!("{} {}→{}", e.etype, e.from, e.to),
            });
            continue;
        }
        // 끝점이 slug 없는 노드면 그 노드가 이미 문제로 기록됐다.
        if let (Some(&f), Some(&t)) = (slug_of.get(&e.from), slug_of.get(&e.to)) {
            edges.push((e.etype.as_str(), f, t, props_json(&e.props)));
        }
    }
    if !problems.is_empty() {
        return Err(InterchangeError::Rejected(problems));
    }

    nodes.sort_by(|a, b| a.0.cmp(b.0));
    edges.sort();

    let mut out = String::new();
    let _ = write!(out, "{{\"format\":{},\n\"nodes\":[", esc(FORMAT));
    for (i, (slug, n)) in nodes.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        let labels: Vec<String> = n.labels.iter().map(|l| esc(l)).collect();
        let _ = write!(
            out,
            "{{\"slug\":{},\"labels\":[{}],\"props\":{}}}",
            esc(slug),
            labels.join(","),
            props_json(&n.props)
        );
    }
    out.push_str("\n],\n\"edges\":[");
    for (i, (etype, from, to, props)) in edges.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        let _ = write!(
            out,
            "{{\"etype\":{},\"from\":{},\"to\":{},\"props\":{}}}",
            esc(etype),
            esc(from),
            esc(to),
            props
        );
    }
    out.push_str("\n]}\n");
    Ok(Exported {
        epoch,
        nodes: nodes.len(),
        edges: edges.len(),
        text: out,
    })
}

// ====================== 문서 파싱·검증 ======================

#[derive(Debug, Clone, PartialEq)]
pub struct DocNode {
    pub slug: String,
    pub labels: Vec<String>,
    pub props: Props,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocEdge {
    pub etype: String,
    pub from: String,
    pub to: String,
    pub props: Props,
}

/// 검증을 통과한 가져오기 문서. 필드는 공개라 호출자가 값(예: Ref 주소)을 보정할 수 있다.
/// 보정은 slug·엣지 끝을 바꾸지 않는 범위에서만 한다(검증은 이미 끝난 상태).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document {
    pub nodes: Vec<DocNode>,
    pub edges: Vec<DocEdge>,
}

pub fn read_document(path: &Path) -> Result<Document, InterchangeError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| InterchangeError::Io(format!("{}: {e}", path.display())))?;
    parse_document(&text)
}

/// 문서 전체를 검증한다 — 저장소에 접근하지 않는다.
/// 형식 오류는 첫 오류로, 내용 문제(slug 중복·빠진 slug·`__lockfile`)는 모아서 돌려준다.
pub fn parse_document(text: &str) -> Result<Document, InterchangeError> {
    let doc = parse_shape(text)?;
    let mut problems: Vec<Problem> = Vec::new();

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut dup: BTreeSet<&str> = BTreeSet::new();
    for n in &doc.nodes {
        if !seen.insert(n.slug.as_str()) {
            dup.insert(n.slug.as_str());
        }
    }
    problems.extend(dup.into_iter().map(|s| Problem {
        code: ProblemCode::DuplicateSlug,
        subject: s.into(),
    }));
    if seen.contains(LOCKFILE_SLUG) {
        problems.push(Problem {
            code: ProblemCode::LockfileNode,
            subject: LOCKFILE_SLUG.into(),
        });
    }
    let mut missing: BTreeSet<&str> = BTreeSet::new();
    for e in &doc.edges {
        for end in [e.from.as_str(), e.to.as_str()] {
            if !seen.contains(end) {
                missing.insert(end);
            }
        }
    }
    problems.extend(missing.into_iter().map(|s| Problem {
        code: ProblemCode::MissingSlug,
        subject: s.into(),
    }));
    if problems.is_empty() {
        Ok(doc)
    } else {
        Err(InterchangeError::Rejected(problems))
    }
}

fn fmt_err(msg: impl Into<String>) -> InterchangeError {
    InterchangeError::Format(msg.into())
}

fn str_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    at: &str,
) -> Result<&'a str, InterchangeError> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| fmt_err(format!("{at}: {key} 필요 (문자열)")))
}

fn props_field(
    obj: &serde_json::Map<String, serde_json::Value>,
    at: &str,
) -> Result<Props, InterchangeError> {
    match obj.get("props") {
        None => Ok(Props::new()),
        Some(p) => props_from_json(p).map_err(|e| fmt_err(format!("{at}: props — {e}"))),
    }
}

/// 문서 최상위가 정의하는 칸 (내보내기 형식과 같다).
const DOC_FIELDS: &[&str] = &["format", "nodes", "edges"];
/// `nodes[i]`가 정의하는 칸.
const NODE_FIELDS: &[&str] = &["slug", "labels", "props"];
/// `edges[i]`가 정의하는 칸.
const EDGE_FIELDS: &[&str] = &["etype", "from", "to", "props"];

/// `obj`의 칸 중 `known`에 없는 것의 위치(`{at}.{키}`, 최상위는 키만)를 `out`에 모은다.
fn collect_unknown(
    obj: &serde_json::Map<String, serde_json::Value>,
    known: &[&str],
    at: &str,
    out: &mut Vec<String>,
) {
    for key in obj.keys().filter(|k| !known.contains(&k.as_str())) {
        out.push(if at.is_empty() {
            key.clone()
        } else {
            format!("{at}.{key}")
        });
    }
}

fn parse_shape(text: &str) -> Result<Document, InterchangeError> {
    let j: serde_json::Value =
        serde_json::from_str(text).map_err(|e| fmt_err(format!("JSON 파싱 실패: {e}")))?;
    let obj = j
        .as_object()
        .ok_or_else(|| fmt_err("최상위는 객체여야 합니다"))?;
    match obj.get("format").and_then(serde_json::Value::as_str) {
        Some(FORMAT) => {}
        Some(other) => {
            return Err(fmt_err(format!(
                "지원하지 않는 format: {other} (필요: {FORMAT})"
            )))
        }
        None => return Err(fmt_err(format!("format 필드 필요 (값: \"{FORMAT}\")"))),
    }
    let nodes_j = obj
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| fmt_err("nodes 배열 필요"))?;
    let edges_j = obj
        .get("edges")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| fmt_err("edges 배열 필요"))?;

    let mut unknown: Vec<String> = Vec::new();
    collect_unknown(obj, DOC_FIELDS, "", &mut unknown);
    let mut doc = Document::default();
    for (i, n) in nodes_j.iter().enumerate() {
        let at = format!("nodes[{i}]");
        let o = n
            .as_object()
            .ok_or_else(|| fmt_err(format!("{at}: 객체 필요")))?;
        collect_unknown(o, NODE_FIELDS, &at, &mut unknown);
        let slug = str_field(o, "slug", &at)?;
        if slug.is_empty() {
            return Err(fmt_err(format!("{at}: slug는 빈 문자열일 수 없습니다")));
        }
        let labels = match o.get("labels") {
            None => Vec::new(),
            Some(serde_json::Value::Array(xs)) => xs
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| fmt_err(format!("{at}: labels는 문자열 배열이어야 합니다")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(fmt_err(format!("{at}: labels는 문자열 배열이어야 합니다"))),
        };
        doc.nodes.push(DocNode {
            slug: slug.to_string(),
            labels,
            props: props_field(o, &at)?,
        });
    }
    for (i, e) in edges_j.iter().enumerate() {
        let at = format!("edges[{i}]");
        let o = e
            .as_object()
            .ok_or_else(|| fmt_err(format!("{at}: 객체 필요")))?;
        collect_unknown(o, EDGE_FIELDS, &at, &mut unknown);
        doc.edges.push(DocEdge {
            etype: str_field(o, "etype", &at)?.to_string(),
            from: str_field(o, "from", &at)?.to_string(),
            to: str_field(o, "to", &at)?.to_string(),
            props: props_field(o, &at)?,
        });
    }
    if unknown.is_empty() {
        Ok(doc)
    } else {
        Err(InterchangeError::UnknownFields(unknown))
    }
}

// ====================== 가져오기 (replace) ======================

#[derive(Debug)]
pub struct ImportReport {
    /// 새로 만든 커밋. 문서가 비어 아무것도 쓰지 않았으면 None.
    pub commit: Option<Hash256>,
    pub nodes: usize,
    pub edges: usize,
    /// warn 모드 스키마 경고.
    pub warnings: Vec<SchemaIssue>,
}

struct Target {
    g: GraphId,
    ns_slug: String,
    branch: String,
    /// 이름 있는 작업 공간 (`None` = 기본 작업 공간).
    workspace: Option<String>,
}

/// `addr` = "ns/slug[@branch]" (기본 main). 빈 그래프에만 문서 전체를 커밋 한 번으로 넣는다.
pub fn import_replace(
    sess: &Session,
    addr: &str,
    doc: Document,
    message: &str,
) -> Result<ImportReport, InterchangeError> {
    import_replace_in(sess, addr, None, doc, message)
}

/// `import_replace`를 이름 있는 작업 공간에서 실행한다. 미커밋 편집 검사와 편집 세션이 모두
/// 그 작업 공간을 따르고, 다른 작업 공간의 편집은 검사 대상이 아니다. `None`이면 `import_replace`와 같다.
pub fn import_replace_in(
    sess: &Session,
    addr: &str,
    workspace: Option<&str>,
    doc: Document,
    message: &str,
) -> Result<ImportReport, InterchangeError> {
    let target = resolve_target(sess, addr, workspace)?;
    ensure_empty(sess, &target)?;
    ensure_no_pending_edits(sess, &target)?;
    let (nodes, edges) = (doc.nodes.len(), doc.edges.len());
    if nodes == 0 && edges == 0 {
        return Ok(ImportReport {
            commit: None,
            nodes,
            edges,
            warnings: Vec::new(),
        });
    }
    let es = open_session(sess, &target)?;
    if let Err(e) = apply_replace(&es, doc) {
        discard_after_failure(sess, es);
        return Err(e);
    }
    finish(sess, es, message, nodes, edges)
}

fn resolve_target(
    sess: &Session,
    addr: &str,
    workspace: Option<&str>,
) -> Result<Target, InterchangeError> {
    let a: GraphAddr = full_addr(addr)
        .parse()
        .map_err(|e| InterchangeError::BadAddr(format!("{addr}: {e}")))?;
    if a.commit.is_some() {
        return Err(InterchangeError::BadAddr(format!(
            "{addr}: 커밋 주소로는 가져올 수 없습니다 (ns/slug[@branch] 형식)"
        )));
    }
    let g = sess.graph_id(&a.ns, &a.slug).ok_or_else(|| {
        InterchangeError::BadAddr(format!(
            "미등록 그래프: {}/{} (vgraph_create 먼저)",
            a.ns, a.slug
        ))
    })?;
    Ok(Target {
        g,
        ns_slug: format!("{}/{}", a.ns, a.slug),
        branch: a.branch.as_deref().unwrap_or("main").to_string(),
        workspace: workspace.map(String::from),
    })
}

fn ensure_empty(sess: &Session, t: &Target) -> Result<(), InterchangeError> {
    match sess.resolve(&format!("graph://{}@{}", t.ns_slug, t.branch)) {
        Ok((g, c)) => {
            let view = sess.checkout(g, c)?;
            if view.node_count() == 0 && view.edge_count() == 0 {
                Ok(())
            } else {
                Err(InterchangeError::NotEmpty {
                    nodes: view.node_count(),
                    edges: view.edge_count(),
                })
            }
        }
        // 커밋이 아직 없는 브랜치 = 빈 그래프
        Err(ResolveError::UnknownBranch) => Ok(()),
        Err(e) => Err(InterchangeError::Engine(EngineError::from(e))),
    }
}

/// 같은 브랜치의 working copy 중 이 사용자의 것을 찾는다 (resume 실패는 건너뜀 — MCP/CLI와 같은 규칙).
fn find_work(sess: &Session, t: &Target) -> Result<Option<EditSession>, InterchangeError> {
    for wid in sess.list_working(t.g)? {
        if let Ok(es) = sess.resume(t.g, wid) {
            if es.branch().as_deref() == Some(t.branch.as_str())
                && es.workspace().as_deref() == t.workspace.as_deref()
            {
                return Ok(Some(es));
            }
        }
    }
    Ok(None)
}

fn ensure_no_pending_edits(sess: &Session, t: &Target) -> Result<(), InterchangeError> {
    match find_work(sess, t)? {
        Some(es) if es.dirty() => Err(InterchangeError::PendingEdits),
        _ => Ok(()),
    }
}

fn open_session(sess: &Session, t: &Target) -> Result<EditSession, InterchangeError> {
    if let Some(es) = find_work(sess, t)? {
        return Ok(es); // 변경 없는 기존 working copy (dirty 는 앞에서 거부됨)
    }
    match sess.edit_in(t.g, &t.branch, t.workspace.as_deref()) {
        Ok(es) => Ok(es),
        Err(EditStartError::Engine(e)) => Err(InterchangeError::Engine(e)),
        Err(EditStartError::NotHead { .. }) => Err(InterchangeError::Engine(EngineError::Corrupt(
            "unexpected NotHead".into(),
        ))),
    }
}

fn apply_replace(es: &EditSession, doc: Document) -> Result<(), InterchangeError> {
    let mut ids: HashMap<String, Uuid> = HashMap::with_capacity(doc.nodes.len());
    for n in doc.nodes {
        let labels: Vec<&str> = n.labels.iter().map(String::as_str).collect();
        let u = es.add_node(&labels, Some(&n.slug), n.props)?;
        ids.insert(n.slug, u);
    }
    for e in doc.edges {
        // 검증(parse_document)이 끝점 존재를 보장하지만, Document 필드는 공개라 방어한다.
        let (Some(&from), Some(&to)) = (ids.get(&e.from), ids.get(&e.to)) else {
            return Err(InterchangeError::Rejected(vec![Problem {
                code: ProblemCode::MissingSlug,
                subject: if ids.contains_key(&e.from) {
                    e.to
                } else {
                    e.from
                },
            }]));
        };
        es.add_edge(&e.etype, from, to, e.props)?;
    }
    Ok(())
}

fn finish(
    sess: &Session,
    es: EditSession,
    message: &str,
    nodes: usize,
    edges: usize,
) -> Result<ImportReport, InterchangeError> {
    match sess.commit(&es, message) {
        Ok(CommitOutcome::Committed { commit, warnings }) => Ok(ImportReport {
            commit: Some(commit),
            nodes,
            edges,
            warnings,
        }),
        Ok(CommitOutcome::Rebased { committed, .. }) => Ok(ImportReport {
            commit: Some(committed),
            nodes,
            edges,
            warnings: Vec::new(),
        }),
        Ok(CommitOutcome::Empty) => {
            discard_after_failure(sess, es);
            Ok(ImportReport {
                commit: None,
                nodes,
                edges,
                warnings: Vec::new(),
            })
        }
        Ok(CommitOutcome::SchemaViolation(issues)) => {
            discard_after_failure(sess, es);
            Err(InterchangeError::SchemaViolation(issues))
        }
        Ok(CommitOutcome::Conflicts(report)) => {
            discard_after_failure(sess, es);
            Err(InterchangeError::Conflicts(Box::new(report)))
        }
        Err(e) => {
            discard_after_failure(sess, es);
            Err(InterchangeError::Engine(e))
        }
    }
}

/// 실패 경로의 작업분 정리. 정리 자체가 실패해도 호출자에게는 원래 오류가 더 쓸모 있으므로
/// 정리 오류는 원래 오류를 가리지 않게 버린다.
fn discard_after_failure(sess: &Session, es: EditSession) {
    let _ = sess.discard(es);
}

// ====================== 가져오기 (merge: 가져오기 전용 브랜치 + 기존 3-way 머지) ======================

/// merge 가져오기의 결과 종류.
#[derive(Debug)]
pub enum SyncOutcome {
    /// 첫 가져오기: 빈 대상에 replace 로 넣고 같은 커밋에서 가져오기 전용 브랜치를 만들었다.
    FirstImport { commit: Hash256 },
    /// 이후 가져오기: 기존 `Session::merge` 의 결과 그대로 (충돌이면 대상 브랜치는 불변).
    Merge(MergeOutcome),
}

#[derive(Debug)]
pub struct MergeImportReport {
    /// 가져오기 전용 브랜치에 새로 만든 커밋. 문서가 이미 같으면 None (첫 가져오기에서는 대상과 같은 커밋).
    pub via_commit: Option<Hash256>,
    pub outcome: SyncOutcome,
    pub nodes: usize,
    pub edges: usize,
    /// warn 모드 스키마 경고.
    pub warnings: Vec<SchemaIssue>,
}

/// merge 가져오기. `addr` = "ns/slug[@branch]"(대상), `via_branch` = 가져오기 전용 브랜치 이름([A-Za-z0-9_-]와 구분용 `/`).
///
/// - 첫 가져오기(`via_branch` 없음): 대상이 빈 그래프일 때만. replace 로 넣고 그 커밋에서 `via_branch`를 만든다.
/// - 이후: `via_branch`를 문서와 똑같이 맞추고(삭제 포함, 변경이 없으면 커밋하지 않음) 기존 머지로 대상에 합친다.
///   - 노드는 slug 로, 엣지는 (etype, from, to)로 맞춘다. `__lockfile`과 slug 없는 노드는 건드리지 않는다.
///   - `via_branch` 쪽에 같은 (etype, from, to)가 여럿이면 uuid 가 가장 작은 하나만 남기고 나머지는 지운다.
///   - 실패(검증·스키마 위반·링크 대상 미등록 등)하면 두 브랜치 head 와 작업분 모두 그대로다.
///     머지 충돌은 실패가 아니다 — `via_branch`는 문서를 반영한 채 남고 대상은 불변이다.
///     다음 가져오기는 `via_branch`를 새 문서에 다시 맞추고 충돌을 새로 계산한다.
pub fn import_merge(
    sess: &Session,
    addr: &str,
    via_branch: &str,
    doc: Document,
    message: &str,
) -> Result<MergeImportReport, InterchangeError> {
    import_merge_in(sess, addr, via_branch, None, doc, message)
}

/// `import_merge`를 이름 있는 작업 공간에서 실행한다. 가져오기 전용 브랜치(via)의 편집 세션도
/// 같은 작업 공간을 따르고, 다른 작업 공간의 편집은 검사·합치기 대상이 아니다.
/// `None`이면 `import_merge`와 같다.
pub fn import_merge_in(
    sess: &Session,
    addr: &str,
    via_branch: &str,
    workspace: Option<&str>,
    doc: Document,
    message: &str,
) -> Result<MergeImportReport, InterchangeError> {
    validate_branch_name(via_branch)?;
    reject_duplicate_edges(&doc)?;
    let target = resolve_target(sess, addr, workspace)?;
    if via_branch == target.branch {
        return Err(InterchangeError::BadAddr(format!(
            "via_branch({via_branch})는 대상 브랜치와 달라야 합니다"
        )));
    }
    let (nodes, edges) = (doc.nodes.len(), doc.edges.len());
    let via = Target {
        g: target.g,
        ns_slug: target.ns_slug.clone(),
        branch: via_branch.to_string(),
        workspace: target.workspace.clone(),
    };
    match branch_head(sess, &via)? {
        None => first_import(sess, &target, via_branch, doc, message, (nodes, edges)),
        Some(via_head) => {
            sync_existing(sess, &target, &via, via_head, doc, message, (nodes, edges))
        }
    }
}

fn validate_branch_name(name: &str) -> Result<(), InterchangeError> {
    vgraph_core::addr::valid_branch_name(name).map_err(|_| {
        InterchangeError::BadAddr(format!(
            "via_branch 이름은 영문·숫자·-·_ 와 구분용 / 만 쓸 수 있고, 비어 있거나 / 로 시작·끝나거나 // 가 들어갈 수 없습니다 (받은 값: \"{name}\")"
        ))
    })
}

fn reject_duplicate_edges(doc: &Document) -> Result<(), InterchangeError> {
    let mut count: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for e in &doc.edges {
        *count
            .entry((e.etype.as_str(), e.from.as_str(), e.to.as_str()))
            .or_default() += 1;
    }
    let problems: Vec<Problem> = count
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|((etype, from, to), _)| Problem {
            code: ProblemCode::DuplicateEdge,
            subject: format!("{etype} {from}→{to}"),
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(InterchangeError::Rejected(problems))
    }
}

/// 브랜치 head. 커밋이 아직 없는 브랜치는 None.
fn branch_head(sess: &Session, t: &Target) -> Result<Option<Hash256>, InterchangeError> {
    match sess.resolve(&format!("graph://{}@{}", t.ns_slug, t.branch)) {
        Ok((_, c)) => Ok(Some(c)),
        Err(ResolveError::UnknownBranch) => Ok(None),
        Err(e) => Err(InterchangeError::Engine(EngineError::from(e))),
    }
}

fn first_import(
    sess: &Session,
    target: &Target,
    via_branch: &str,
    doc: Document,
    message: &str,
    (nodes, edges): (usize, usize),
) -> Result<MergeImportReport, InterchangeError> {
    let addr = format!("{}@{}", target.ns_slug, target.branch);
    let report = match import_replace_in(sess, &addr, target.workspace.as_deref(), doc, message) {
        Err(InterchangeError::NotEmpty { .. }) => {
            return Err(InterchangeError::NoBaseline(format!(
                "가져오기 전용 브랜치 {via_branch}가 아직 없는데 대상 브랜치 {}가 비어 있지 않습니다 — \
                 기준점(공통 조상)이 없어 충돌을 가릴 수 없으므로 첫 가져오기는 빈 그래프에서만 할 수 있습니다",
                target.branch
            )))
        }
        other => other?,
    };
    let Some(commit) = report.commit else {
        return Err(InterchangeError::NoBaseline(
            "문서가 비어 있어 기준점이 될 커밋을 만들 수 없습니다".into(),
        ));
    };
    sess.branch(target.g, commit, via_branch)?;
    Ok(MergeImportReport {
        via_commit: Some(commit),
        outcome: SyncOutcome::FirstImport { commit },
        nodes,
        edges,
        warnings: report.warnings,
    })
}

fn sync_existing(
    sess: &Session,
    target: &Target,
    via: &Target,
    via_head: Hash256,
    doc: Document,
    message: &str,
    (nodes, edges): (usize, usize),
) -> Result<MergeImportReport, InterchangeError> {
    if branch_head(sess, target)?.is_none() {
        return Err(InterchangeError::NoBaseline(format!(
            "가져오기 전용 브랜치 {}는 있는데 대상 브랜치 {}에 커밋이 없습니다",
            via.branch, target.branch
        )));
    }
    // 변경 없는 옛 working copy 는 base 가 낡았을 수 있어 버리고 새로 연다 (미커밋 편집은 거부)
    if let Some(old) = find_work(sess, via)? {
        if old.dirty() {
            return Err(InterchangeError::PendingEdits);
        }
        sess.discard(old)?;
    }
    let state = sess.checkout(via.g, via_head)?.to_state();
    let es = match sess.edit_in(via.g, &via.branch, via.workspace.as_deref()) {
        Ok(es) => es,
        Err(EditStartError::Engine(e)) => return Err(InterchangeError::Engine(e)),
        Err(EditStartError::NotHead { .. }) => {
            return Err(InterchangeError::Engine(EngineError::Corrupt(
                "unexpected NotHead".into(),
            )))
        }
    };
    if let Err(e) = apply_sync(&es, &state, doc) {
        discard_after_failure(sess, es);
        return Err(e);
    }
    let via_report = finish(sess, es, message, nodes, edges)?;
    let outcome = sess.merge(via.g, &via.branch, &target.branch, &MergeOpts::default())?;
    Ok(MergeImportReport {
        via_commit: via_report.commit,
        outcome: SyncOutcome::Merge(outcome),
        nodes,
        edges,
        warnings: via_report.warnings,
    })
}

/// via 브랜치의 현재 상태(`state`)를 문서와 똑같이 맞추는 편집을 쌓는다 (커밋은 호출자).
fn apply_sync(es: &EditSession, state: &GraphState, doc: Document) -> Result<(), InterchangeError> {
    // slug 있는 노드만 문서와 맞춘다. __lockfile 과 slug 없는 노드는 그대로 둔다.
    let mut nodes: HashMap<&str, &Node> = HashMap::new();
    let mut slug_of: HashMap<Uuid, &str> = HashMap::new();
    for n in state.nodes() {
        if let Some(slug) = n.slug.as_deref().filter(|s| *s != LOCKFILE_SLUG) {
            nodes.insert(slug, n);
            slug_of.insert(n.uuid, slug);
        }
    }
    // 같은 (etype, from, to)끼리 uuid 순으로 모은다 (state.edges()는 uuid 순)
    let mut via_edges: BTreeMap<(&str, &str, &str), Vec<&Edge>> = BTreeMap::new();
    for e in state.edges() {
        if let (Some(&f), Some(&t)) = (slug_of.get(&e.from), slug_of.get(&e.to)) {
            via_edges
                .entry((e.etype.as_str(), f, t))
                .or_default()
                .push(e);
        }
    }

    // 1) 문서에 없는 엣지·노드 제거, 중복 엣지는 하나만 남김. 엣지를 먼저 지워 노드 제거가 깔끔하다.
    {
        let doc_edges: HashSet<(&str, &str, &str)> = doc
            .edges
            .iter()
            .map(|e| (e.etype.as_str(), e.from.as_str(), e.to.as_str()))
            .collect();
        let doc_slugs: HashSet<&str> = doc.nodes.iter().map(|n| n.slug.as_str()).collect();
        for (key, list) in &via_edges {
            let keep = usize::from(doc_edges.contains(key));
            for e in list.iter().skip(keep) {
                es.remove_edge(e.uuid)?;
            }
        }
        for (slug, n) in &nodes {
            if !doc_slugs.contains(slug) {
                es.remove_node(n.uuid, true)?;
            }
        }
    }

    // 2) 노드 추가·갱신
    let mut ids: HashMap<String, Uuid> = HashMap::with_capacity(doc.nodes.len());
    for n in doc.nodes {
        match nodes.get(n.slug.as_str()) {
            Some(old) => {
                sync_labels(es, old.uuid, &old.labels, &n.labels)?;
                sync_props(es, old.uuid, &old.props, n.props)?;
                ids.insert(n.slug, old.uuid);
            }
            None => {
                let labels: Vec<&str> = n.labels.iter().map(String::as_str).collect();
                let u = es.add_node(&labels, Some(&n.slug), n.props)?;
                ids.insert(n.slug, u);
            }
        }
    }

    // 3) 엣지 추가·속성 갱신
    for e in doc.edges {
        let key = (e.etype.as_str(), e.from.as_str(), e.to.as_str());
        if let Some(old) = via_edges.get(&key).and_then(|list| list.first()) {
            sync_props(es, old.uuid, &old.props, e.props)?;
            continue;
        }
        // 검증(parse_document)이 끝점 존재를 보장하지만 Document 필드는 공개라 방어한다.
        let (Some(&from), Some(&to)) = (ids.get(&e.from), ids.get(&e.to)) else {
            return Err(InterchangeError::Rejected(vec![Problem {
                code: ProblemCode::MissingSlug,
                subject: if ids.contains_key(&e.from) {
                    e.to
                } else {
                    e.from
                },
            }]));
        };
        es.add_edge(&e.etype, from, to, e.props)?;
    }
    Ok(())
}

/// 속성을 문서대로: 다르거나 새 키는 설정하고, 문서에 없는 키는 지운다.
fn sync_props(
    es: &EditSession,
    elem: Uuid,
    old: &Props,
    new: Props,
) -> Result<(), InterchangeError> {
    for key in old.keys().filter(|k| !new.contains_key(*k)) {
        es.unset_prop(elem, key)?;
    }
    for (key, value) in new {
        if old.get(&key) != Some(&value) {
            es.set_prop(elem, &key, value)?;
        }
    }
    Ok(())
}

/// 라벨을 문서대로: 없는 것은 더하고, 문서에 없는 것은 뺀다.
fn sync_labels(
    es: &EditSession,
    elem: Uuid,
    old: &[SmolStr],
    new: &[String],
) -> Result<(), InterchangeError> {
    for l in old.iter().filter(|l| !new.iter().any(|n| n == l.as_str())) {
        es.remove_label(elem, l)?;
    }
    for l in new
        .iter()
        .filter(|l| !old.iter().any(|o| o.as_str() == l.as_str()))
    {
        es.add_label(elem, l)?;
    }
    Ok(())
}
