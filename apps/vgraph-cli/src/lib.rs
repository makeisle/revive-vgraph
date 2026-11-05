//! vgraph — 로컬 조작 CLI (v0).
//!
//! 데이터: `--root` 디렉터리 (기본 ./vgraph-data, 그래프당 SQLite 파일 + registry).
//! 편집은 CLI 호출 사이에도 이어진다 — working WAL이 저장소에 남고,
//! 다음 호출이 resume 한다 (엔진 C5 경로의 실사용 시연).
//!
//! 이 크레이트의 `vgraph` 바이너리는 [`run_cli`] 를 부를 뿐이다. 같은 동작(인자 해석·출력·종료 코드)을 다른
//! 프로그램이 라이브러리 함수로 불러 쓰고 싶으면 [`run_cli`] 나, 치명적 오류 때 호출자의 처리기를 덧붙이는
//! [`run_cli_with`] 를 쓴다.

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use vgraph_core::{
    Dir, GraphAddr, GraphId, Hash256, NodeSel, Props, SystemClock, SystemIdGen, Uuid, Value,
};
use vgraph_engine::engine::EditSession;
use vgraph_engine::*;

/// `--workspace` 도움말 (안내문 — 옵션을 쓰지 않을 때의 손실 위험 포함).
const WORKSPACE_HELP: &str = "이름 있는 작업 공간 (생략하면 기본 작업 공간). 여러 프로세스·에이전트가 같은 사용자로 같은 그래프·같은 브랜치를 동시에 편집할 때는 각자 다른 --workspace 이름을 써야 합니다. 이름 없이(기본 작업 공간) 동시에 편집하면 편집 내용이 사라질 수 있습니다. 작업 공간 이름은 동시에 쓰는 프로세스마다 고유해야 하며, 같은 이름을 동시에 쓰면 이름 없이 쓸 때와 같이 편집이 사라질 수 있습니다. 이름이 다르면 커밋할 때 자동으로 합쳐지고, 같은 값을 다르게 바꿨으면 충돌로 보고됩니다. 이름은 영문·숫자·-·_ 만 쓸 수 있습니다";

#[derive(Parser)]
#[command(
    name = "vgraph",
    version,
    about = "revive-vgraph 버저닝·브랜칭 그래프 CLI"
)]
struct Cli {
    /// 저장소 루트 디렉터리
    #[arg(long, global = true, default_value = "./vgraph-data")]
    root: PathBuf,
    /// 세션 사용자 (ACL 소유자)
    #[arg(long, global = true, default_value = "local")]
    user: String,
    #[arg(long, global = true, help = WORKSPACE_HELP)]
    workspace: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 그래프 생성: vgraph init acme/bookclub
    Init { name: String },
    /// 내 그래프 목록
    Graphs,
    /// 노드 추가 (working copy에): vgraph add-node acme/bookclub --label Capability --slug cap-auth --prop name=인증
    AddNode {
        graph: String,
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long = "prop")]
        props: Vec<String>,
    },
    /// 엣지 추가: vgraph add-edge acme/bookclub --etype parent --from req-001 --to cap-auth
    AddEdge {
        graph: String,
        #[arg(long)]
        etype: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// 속성 설정: vgraph set acme/bookclub#cap-auth priority=9 status=beta
    Set { target: String, kv: Vec<String> },
    /// 속성 제거
    Unset { target: String, keys: Vec<String> },
    /// working copy의 미커밋 변경 요약
    Pending { graph: String },
    /// working copy 폐기
    Discard { graph: String },
    /// seal → commit: vgraph commit acme/bookclub -m "F0"
    Commit {
        graph: String,
        #[arg(short, long)]
        message: String,
    },
    /// 커밋 이력 (머지 계보 포함, 최신순)
    Log { graph: String },
    /// 그래프/노드 조회: vgraph show acme/bookclub[@branch][.hash][#node]
    Show { addr: String },
    /// 속성 등가 검색: vgraph find acme/bookclub status=active
    Find { graph: String, kv: String },
    /// 순회: vgraph neighbors acme/bookclub#req-002 --dir out --depth 2
    Neighbors {
        addr: String,
        #[arg(long, default_value = "out")]
        dir: String,
        #[arg(long)]
        etype: Option<String>,
        #[arg(long, default_value_t = 1)]
        depth: u32,
    },
    /// 브랜치 생성 (main head에서): vgraph branch acme/bookclub dev-b
    Branch { graph: String, name: String },
    /// 머지: vgraph merge acme/bookclub dev-b main [--theirs 0]
    Merge {
        graph: String,
        src: String,
        dst: String,
        /// 충돌 항목 해소: 해당 인덱스를 theirs로 채택
        #[arg(long = "theirs")]
        theirs: Vec<usize>,
        /// 충돌 항목 해소: 해당 인덱스를 ours(dst 쪽 값)로 채택
        #[arg(long = "ours")]
        ours: Vec<usize>,
    },
    /// external link 드리프트 목록
    Drifts { graph: String },
    /// lockfile (external link 핀)
    Lockfile { graph: String },
    /// GC 실행
    Gc { graph: String },
    /// JSON 내보내기(vgraph-json/1): vgraph export acme/catalog[@branch][.commit] [--out catalog.json]
    /// (--out 없으면 표준 출력). slug 없는 노드가 있으면 거부한다.
    Export {
        graph: String,
        /// 쓸 파일 경로
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// JSON 가져오기(vgraph-json/1): vgraph import acme/catalog --file catalog.json
    /// 또는 --json '{"format":"vgraph-json/1","nodes":[…],"edges":[…]}' (둘 중 하나).
    /// 기본 replace는 빈 그래프에만. 반복 동기화는 --mode merge --via-branch <이름>:
    /// 가져오기 전용 브랜치를 문서와 똑같이 맞춘 뒤 기존 3-way 머지로 대상 브랜치에 합친다
    /// (첫 가져오기는 빈 그래프에서만). 충돌이면 대상은 그대로이고 해소 안내가 나온다 —
    /// vgraph merge <그래프> <via-branch> <대상> --theirs|--ours <번호> 로 해소.
    /// 예: vgraph import acme/catalog --mode merge --via-branch import --file catalog.json
    Import {
        graph: String,
        /// 가져올 문서 파일 경로
        #[arg(long)]
        file: Option<PathBuf>,
        /// 가져올 문서(인라인 JSON 문자열)
        #[arg(long, conflicts_with = "file")]
        json: Option<String>,
        /// replace(빈 그래프에만, 기본) | merge(--via-branch 필요)
        #[arg(long, default_value = "replace")]
        mode: String,
        /// mode=merge 전용: 가져오기 전용 브랜치 이름(영문·숫자·-·_·/ — 빈 이름·앞뒤 /·// 불가, 기본값 없음)
        #[arg(long)]
        via_branch: Option<String>,
        /// 커밋 메시지
        #[arg(short, long, default_value = "import (vgraph-json/1)")]
        message: String,
    },
}

/// `vgraph` 바이너리의 본체. 프로세스 인자를 읽어 실행하고 종료 코드를 돌려준다(성공 0, 오류 1;
/// 인자 오류·`--help`·`--version` 은 이전처럼 해석 단계에서 바로 종료한다).
///
/// 반환값을 `main` 에서 그대로 돌려주면 된다: `fn main() -> ExitCode { vgraph_cli::run_cli() }`.
pub fn run_cli() -> ExitCode {
    run_cli_with(&|_| {})
}

/// [`run_cli`] 와 같되, 명령이 실패해 `오류: …` 를 표준 오류에 쓴 **직후** `on_fatal_error` 를 부른다.
/// 출력과 종료 코드는 처리기를 주지 않았을 때와 같다 — 처리기는 구경만 한다(예: 오류를 별도 수집기에 알리기).
pub fn run_cli_with(on_fatal_error: &dyn Fn(&anyhow::Error)) -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("오류: {e:#}");
        on_fatal_error(&e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run(cli: Cli) -> Result<()> {
    let engine = Engine::open(
        Box::new(FsProvider::new(&cli.root).map_err(|e| anyhow!("{e}"))?),
        Arc::new(SystemIdGen),
        Arc::new(SystemClock),
        EngineOptions::default(),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let sess = engine.session(&cli.user);
    let ws = cli.workspace.as_deref();
    if let Some(w) = ws {
        check_workspace_name(w)?;
    }

    match cli.cmd {
        Cmd::Init { name } => {
            let (ns, slug) = split_name(&name)?;
            let id = sess.create_graph(ns, slug).map_err(err)?;
            println!("생성됨: {name}  (id {id})");
        }
        Cmd::Graphs => {
            for (ns, slug, id) in sess.graphs() {
                let head = sess
                    .resolve(&format!("graph://{ns}/{slug}"))
                    .map(|(_, h)| short(&h))
                    .unwrap_or_else(|_| "(커밋 없음)".into());
                println!("{ns}/{slug}  head={head}  id={id}");
            }
        }
        Cmd::AddNode {
            graph,
            labels,
            slug,
            props,
        } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let es = open_work(&sess, g, &branch, ws)?;
            let labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
            let p = parse_props(&props)?;
            let uuid = es.add_node(&labels, slug.as_deref(), p).map_err(err)?;
            println!("+node {uuid}  (pending — commit으로 확정)");
        }
        Cmd::AddEdge {
            graph,
            etype,
            from,
            to,
        } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let es = open_work(&sess, g, &branch, ws)?;
            let f = node_in_work(&es, &from)?;
            let t = node_in_work(&es, &to)?;
            let uuid = es.add_edge(&etype, f, t, Props::new()).map_err(err)?;
            println!("+edge {etype} {from}→{to}  {uuid}  (pending)");
        }
        Cmd::Set { target, kv } => {
            let (g, branch, node) = target_of(&sess, &target)?;
            let node = node.ok_or_else(|| anyhow!("노드를 지정하세요: …#slug"))?;
            let es = open_work(&sess, g, &branch, ws)?;
            let u = node_sel_in_work(&es, &node)?;
            for pair in kv {
                let (k, v) = parse_kv(&pair)?;
                es.set_prop(u, &k, v).map_err(err)?;
                println!("set {target} {k}  (pending)");
            }
        }
        Cmd::Unset { target, keys } => {
            let (g, branch, node) = target_of(&sess, &target)?;
            let node = node.ok_or_else(|| anyhow!("노드를 지정하세요: …#slug"))?;
            let es = open_work(&sess, g, &branch, ws)?;
            let u = node_sel_in_work(&es, &node)?;
            for k in keys {
                es.unset_prop(u, &k).map_err(err)?;
                println!("unset {k}  (pending)");
            }
        }
        Cmd::Pending { graph } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            match find_work(&sess, g, &branch, ws).map_err(err)? {
                None => println!("working copy 없음"),
                Some(es) => {
                    let d = es.pending();
                    println!(
                        "working {} (base {})",
                        es.wid(),
                        es.base().map(|h| short(&h)).unwrap_or_else(|| "-".into())
                    );
                    println!(
                        "  +node {}  -node {}  +edge {}  -edge {}  prop {}  slug {}",
                        d.nodes_added.len(),
                        d.nodes_removed.len(),
                        d.edges_added.len(),
                        d.edges_removed.len(),
                        d.props_set.len(),
                        d.slugs_set.len()
                    );
                }
            }
        }
        Cmd::Discard { graph } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            match find_work(&sess, g, &branch, ws).map_err(err)? {
                None => println!("working copy 없음"),
                Some(es) => {
                    sess.discard(es).map_err(err)?;
                    println!("폐기됨");
                }
            }
        }
        Cmd::Commit { graph, message } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let es = find_work(&sess, g, &branch, ws)
                .map_err(err)?
                .ok_or_else(|| anyhow!("working copy 없음 — 먼저 add-node/set 등을 실행"))?;
            print_commit_outcome(sess.commit(&es, &message).map_err(err)?);
        }
        Cmd::Log { graph } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let head = sess.resolve(&full_addr(&graph)).map_err(err2)?.1;
            let _ = head;
            for h in sess.log(g, &branch).map_err(err)? {
                let c = sess.commit_meta(g, h).map_err(err)?;
                let merge = if c.parents.len() > 1 { " (merge)" } else { "" };
                println!("{}  {}  {}  {}{}", short(&h), c.ts, c.author, c.msg, merge);
            }
        }
        Cmd::Show { addr } => {
            let a: GraphAddr = full_addr(&addr).parse().map_err(|e| anyhow!("{e:?}"))?;
            let (g, commit) = sess.resolve(&full_addr(&addr)).map_err(err2)?;
            let v = sess.checkout(g, commit).map_err(err)?;
            match &a.node {
                Some(sel) => {
                    let u = match sel {
                        NodeSel::Slug(s) => {
                            v.by_slug(s).ok_or_else(|| anyhow!("미존재 노드: {s}"))?
                        }
                        NodeSel::Uuid(u) => *u,
                    };
                    let labels = v.node_labels(u).ok_or_else(|| anyhow!("미존재 노드"))?;
                    println!("{u}  [{}]", labels.join(","));
                    for key in ["name", "text", "status", "priority", "rev", "extern"] {
                        if let Some(val) = v.node_prop(u, key) {
                            let mut s = String::new();
                            vgraph_core::canon::write_value(&val, &mut s);
                            println!("  {key} = {s}");
                        }
                    }
                }
                None => {
                    println!("{}  epoch={}", addr, short(&v.epoch()));
                    println!("nodes {}  edges {}", v.node_count(), v.edge_count());
                }
            }
        }
        Cmd::Find { graph, kv } => {
            let (_, _, _) = target_of(&sess, &graph)?;
            let (g, commit) = sess.resolve(&full_addr(&graph)).map_err(err2)?;
            let v = sess.checkout(g, commit).map_err(err)?;
            let (k, val) = parse_kv(&kv)?;
            for u in v.find_by_prop(&k, &val) {
                let slug = v.node_slug(u).flatten().unwrap_or_default();
                println!("{slug}  {u}");
            }
        }
        Cmd::Neighbors {
            addr,
            dir,
            etype,
            depth,
        } => {
            let a: GraphAddr = full_addr(&addr).parse().map_err(|e| anyhow!("{e:?}"))?;
            let (g, commit) = sess.resolve(&full_addr(&addr)).map_err(err2)?;
            let v = sess.checkout(g, commit).map_err(err)?;
            let start = match a.node.ok_or_else(|| anyhow!("시작 노드 필요: …#slug"))? {
                NodeSel::Slug(s) => v.by_slug(&s).ok_or_else(|| anyhow!("미존재 노드: {s}"))?,
                NodeSel::Uuid(u) => u,
            };
            let d = match dir.as_str() {
                "in" => Dir::In,
                "both" => Dir::Both,
                _ => Dir::Out,
            };
            let types_vec;
            let types = match &etype {
                Some(t) => {
                    types_vec = vec![t.as_str()];
                    Some(types_vec.as_slice())
                }
                None => None,
            };
            for hit in v.neighbors(start, d, types, None, 1, depth, None) {
                let slug = v.node_slug(hit.node).flatten().unwrap_or_default();
                println!("depth {}  {}  {}", hit.depth, slug, hit.node);
            }
        }
        Cmd::Branch { graph, name } => {
            let (g, _, _) = target_of(&sess, &graph)?;
            let head = sess.resolve(&full_addr(&graph)).map_err(err2)?.1;
            sess.branch(g, head, &name).map_err(err)?;
            println!("브랜치 {name}  @ {}", short(&head));
        }
        Cmd::Merge {
            graph,
            src,
            dst,
            theirs,
            ours,
        } => {
            let (g, _, _) = target_of(&sess, &graph)?;
            let opts = MergeOpts {
                resolutions: theirs
                    .into_iter()
                    .map(|item| Resolution {
                        item,
                        choose: Choice::Theirs,
                    })
                    .chain(ours.into_iter().map(|item| Resolution {
                        item,
                        choose: Choice::Ours,
                    }))
                    .collect(),
            };
            match sess.merge(g, &src, &dst, &opts).map_err(err)? {
                MergeOutcome::FastForward(h) => println!("fast-forward → {}", short(&h)),
                MergeOutcome::Merged(h) => println!("merged → {}", short(&h)),
                MergeOutcome::UpToDate(h) => println!("이미 최신 ({})", short(&h)),
                MergeOutcome::Conflicts(r) => print_conflicts(&r),
            }
        }
        Cmd::Drifts { graph } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let items = sess.drifts(g, &branch).map_err(err)?;
            if items.is_empty() {
                println!("드리프트 없음");
            }
            for it in items {
                println!(
                    "drift: {} 노드 {}  핀 {} → 현재 내용 변경됨",
                    it.target,
                    it.node,
                    short(&it.pin)
                );
            }
        }
        Cmd::Lockfile { graph } => {
            let (g, branch, _) = target_of(&sess, &graph)?;
            let lock = sess.lockfile(g, &branch).map_err(err)?;
            if lock.is_empty() {
                println!("external link 없음");
            }
            for (target, e) in lock {
                println!("{target}  @{}  pin {}", e.branch, short(&e.pin));
            }
        }
        Cmd::Export { graph, out } => {
            let ex = interchange::export(&sess, &graph).map_err(interchange_err)?;
            match out {
                Some(path) => {
                    ex.write_to(&path).map_err(interchange_err)?;
                    println!(
                        "내보냄: {graph} → {}  (노드 {}, 엣지 {}, epoch {})",
                        path.display(),
                        ex.nodes,
                        ex.edges,
                        short(&ex.epoch)
                    );
                }
                None => print!("{}", ex.text),
            }
        }
        Cmd::Import {
            graph,
            file,
            json,
            mode,
            via_branch,
            message,
        } => {
            match (mode.as_str(), via_branch.as_deref()) {
                ("replace", None) | ("merge", Some(_)) => {}
                ("replace", Some(_)) => bail!("--via-branch는 --mode merge에서만 씁니다"),
                ("merge", None) => bail!(
                    "--mode merge에는 --via-branch <가져오기 전용 브랜치 이름>이 필요합니다 (기본값 없음)"
                ),
                (other, _) => bail!("--mode는 replace 또는 merge여야 합니다 (받은 값: {other})"),
            }
            let doc = match (file, json) {
                (Some(path), None) => interchange::read_document(&path),
                (None, Some(text)) => interchange::parse_document(&text),
                _ => bail!("--file 또는 --json 중 정확히 하나를 주세요"),
            }
            .map_err(interchange_err)?;
            if let Some(via) = via_branch {
                let r = interchange::import_merge_in(&sess, &graph, &via, ws, doc, &message)
                    .map_err(interchange_err)?;
                print_import_merge(&graph, &via, r);
                return Ok(());
            }
            let r = interchange::import_replace_in(&sess, &graph, ws, doc, &message)
                .map_err(interchange_err)?;
            match r.commit {
                Some(c) => println!(
                    "가져옴: {graph}  (노드 {}, 엣지 {}) → committed {}",
                    r.nodes,
                    r.edges,
                    short(&c)
                ),
                None => println!("문서가 비어 있어 커밋을 만들지 않았습니다"),
            }
            for w in r.warnings {
                println!("  경고(스키마): {w:?}");
            }
        }
        Cmd::Gc { graph } => {
            let (g, _, _) = target_of(&sess, &graph)?;
            let r = sess.gc_run(g).map_err(err)?;
            println!(
                "GC: blob {}건, payload {}건 회수",
                r.removed_blobs, r.removed_payloads
            );
        }
    }
    Ok(())
}

// ---------- helpers ----------

/// 작업 공간 이름 규칙(엔진과 같음: 영문·숫자·-·_, 비어 있으면 안 됨)을 먼저 검사해 자연스러운 문구로 거절한다.
fn check_workspace_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        bail!("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다: \"{name}\"")
    }
}

fn err(e: EngineError) -> anyhow::Error {
    anyhow!("{e}")
}
fn err2(e: ResolveError) -> anyhow::Error {
    anyhow!("{e}")
}

/// 가져오기·내보내기 오류 → 오류 문장 (스키마 위반은 문제 목록까지 보여 준다).
fn interchange_err(e: interchange::InterchangeError) -> anyhow::Error {
    match e {
        interchange::InterchangeError::SchemaViolation(issues) => {
            let list: Vec<String> = issues.iter().map(|i| format!("{i:?}")).collect();
            anyhow!("스키마 위반으로 거부됨 — head 불변: {}", list.join("; "))
        }
        other => anyhow!("{other}"),
    }
}

fn short(h: &Hash256) -> String {
    h.to_string()[7..19].to_string()
}

fn full_addr(s: &str) -> String {
    if s.starts_with("graph://") {
        s.to_string()
    } else {
        format!("graph://{s}")
    }
}

fn split_name(s: &str) -> Result<(&str, &str)> {
    s.split_once('/')
        .ok_or_else(|| anyhow!("형식: <ns>/<slug>"))
}

/// 대상 문자열 → (GraphId, branch, node). 커밋이 없는 신규 그래프도 동작해야 하므로
/// resolve가 아니라 registry 조회.
fn target_of(sess: &Session, s: &str) -> Result<(GraphId, String, Option<NodeSel>)> {
    let a: GraphAddr = full_addr(s)
        .parse()
        .map_err(|e| anyhow!("주소 파싱: {e:?}"))?;
    let g = sess
        .graph_id(&a.ns, &a.slug)
        .ok_or_else(|| anyhow!("미등록 그래프: {}/{} (init 먼저)", a.ns, a.slug))?;
    let branch = a.branch.as_deref().unwrap_or("main").to_string();
    Ok((g, branch, a.node))
}

/// 해당 브랜치의 기존 working copy 찾기 (WAL resume — CLI 호출 간 지속).
fn find_work(
    sess: &Session,
    g: GraphId,
    branch: &str,
    ws: Option<&str>,
) -> EngineResult<Option<EditSession>> {
    for wid in sess.list_working(g)? {
        if let Ok(es) = sess.resume(g, wid) {
            // 브랜치와 작업 공간 이름이 모두 같은 것만 (미지정 = 이름 없는 것만)
            if es.branch().as_deref() == Some(branch) && es.workspace().as_deref() == ws {
                return Ok(Some(es));
            }
        }
    }
    Ok(None)
}

fn open_work(sess: &Session, g: GraphId, branch: &str, ws: Option<&str>) -> Result<EditSession> {
    if let Some(es) = find_work(sess, g, branch, ws).map_err(err)? {
        return Ok(es);
    }
    match sess.edit_in(g, branch, ws) {
        Ok(es) => Ok(es),
        Err(EditStartError::Engine(e)) => Err(err(e)),
        Err(EditStartError::NotHead { .. }) => bail!("과거 commit 편집은 CLI v0 미지원"),
    }
}

fn node_in_work(es: &EditSession, slug_or_uuid: &str) -> Result<Uuid> {
    if let Ok(u) = slug_or_uuid.parse::<Uuid>() {
        return Ok(u);
    }
    es.by_slug(slug_or_uuid)
        .ok_or_else(|| anyhow!("미존재 노드: {slug_or_uuid}"))
}

fn node_sel_in_work(es: &EditSession, sel: &NodeSel) -> Result<Uuid> {
    match sel {
        NodeSel::Uuid(u) => Ok(*u),
        NodeSel::Slug(s) => es.by_slug(s).ok_or_else(|| anyhow!("미존재 노드: {s}")),
    }
}

fn parse_kv(s: &str) -> Result<(String, Value)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("형식: key=value ({s})"))?;
    Ok((k.to_string(), parse_value(v)))
}

fn parse_value(v: &str) -> Value {
    if let Some(r) = v.strip_prefix("ref:") {
        return Value::Ref(r.into());
    }
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if let Ok(i) = v.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = v.parse::<f64>() {
        return Value::Float(f);
    }
    Value::Str(v.to_string())
}

fn parse_props(pairs: &[String]) -> Result<Props> {
    let mut p = Props::new();
    for pair in pairs {
        let (k, v) = parse_kv(pair)?;
        p.insert(smol_str::SmolStr::new(&k), v);
    }
    Ok(p)
}

fn print_commit_outcome(o: CommitOutcome) {
    match o {
        CommitOutcome::Committed { commit, warnings } => {
            println!("committed → {}", short(&commit));
            for w in warnings {
                println!("  경고(스키마): {w:?}");
            }
        }
        CommitOutcome::Rebased { onto, committed } => {
            println!("rebased onto {} → {}", short(&onto), short(&committed));
        }
        CommitOutcome::Empty => println!("변경 없음 — commit 생략"),
        CommitOutcome::SchemaViolation(issues) => {
            println!("거부(스키마 위반):");
            for i in issues {
                println!("  {i:?}");
            }
        }
        CommitOutcome::Conflicts(r) => print_conflicts(&r),
    }
}

/// merge 가져오기 결과 출력. 충돌은 실패가 아니라 보고다(대상 브랜치 불변).
fn print_import_merge(graph: &str, via: &str, r: interchange::MergeImportReport) {
    let head = format!("{graph}  (노드 {}, 엣지 {})", r.nodes, r.edges);
    for w in &r.warnings {
        println!("  경고(스키마): {w:?}");
    }
    match r.outcome {
        interchange::SyncOutcome::FirstImport { commit } => println!(
            "첫 가져오기: {head} → committed {}, 가져오기 전용 브랜치 {via} 생성",
            short(&commit)
        ),
        interchange::SyncOutcome::Merge(MergeOutcome::FastForward(h)) => {
            println!("가져옴: {head} → fast-forward {}", short(&h))
        }
        interchange::SyncOutcome::Merge(MergeOutcome::Merged(h)) => {
            println!("가져옴: {head} → merged {}", short(&h))
        }
        interchange::SyncOutcome::Merge(MergeOutcome::UpToDate(h)) => {
            println!("이미 최신 ({}) — 새 커밋 없음", short(&h))
        }
        interchange::SyncOutcome::Merge(MergeOutcome::Conflicts(c)) => {
            print_conflicts(&c);
            let (name, dst) = graph_and_branch(graph);
            println!("대상 브랜치({dst})는 바뀌지 않았고 가져오기 전용 브랜치({via})에 문서가 반영돼 있습니다.");
            println!(
                "해소: vgraph merge {name} {via} {dst} --theirs <번호> (문서 값 채택) 또는 --ours <번호> (대상 값 유지)"
            );
            println!("번호로 풀 수 있는 것은 속성 충돌(같은 속성을 양쪽이 다르게 바꿈)뿐입니다.");
            println!(
                "삭제 대 수정·끝이 사라진 엣지·slug 겹침·스키마 충돌은 번호로 풀 수 없습니다 — 대상이나 문서를 고쳐 다시 가져오세요"
            );
        }
    }
}

/// "ns/slug[@branch]" → ("ns/slug", 브랜치(기본 main)).
fn graph_and_branch(graph: &str) -> (String, String) {
    let g = graph.strip_prefix("graph://").unwrap_or(graph);
    match g.split_once('@') {
        Some((name, branch)) => (name.to_string(), branch.to_string()),
        None => (g.to_string(), "main".to_string()),
    }
}

fn print_conflicts(r: &ConflictReport) {
    println!("충돌 {}건:", r.items.len());
    for (i, item) in r.items.iter().enumerate() {
        match item {
            ConflictItem::PropBothSet {
                elem,
                key,
                ours,
                theirs,
                ..
            } => {
                println!("  [{i}] 속성 충돌 {elem}.{key}: ours={ours:?} vs theirs={theirs:?}");
            }
            other => println!("  [{i}] {other:?}"),
        }
    }
    println!("해소: merge --theirs <idx> 재실행 (theirs 채택) 또는 review_queue로");
}
