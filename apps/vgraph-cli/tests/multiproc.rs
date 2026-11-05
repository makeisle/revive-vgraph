//! 여러 CLI 프로세스가 같은 root 에 동시에 쓸 때의 관측 시험 (선택-3: 결함은 고치지 않고 기록만).
//!
//! 단언은 "끝난 뒤 저장소를 다시 열 수 있다(log·export 성공)"까지만이다. 커밋 유실·오류 종류는
//! `--nocapture` 출력으로 남긴다. 실행:
//!   cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture   (기본 시험에서는 제외 — #[ignore])
//!
//! 동시성: 워커마다 스레드 하나가 CLI 프로세스를 차례로 띄운다(워커 사이는 동시, 워커 안은 순차).
//! 같은 사용자(`local`)의 같은 브랜치 working copy 는 WAL 로 공유되므로 ① 같은 브랜치에서는
//! 한 워커의 `set`이 다른 워커의 `commit`에 함께 실려 갈 수 있다 — 이것도 관측 대상이다.

use serde_json::Value as J;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

const WORKERS: usize = 6;
const ROUNDS: usize = 8;
const GRAPH: &str = "acme/c";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(root: &Path, args: &[&str]) -> Run {
    let o = Command::new(env!("CARGO_BIN_EXE_vgraph"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("vgraph 실행");
    Run {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn ok(root: &Path, args: &[&str]) -> String {
    let r = run(root, args);
    assert_eq!(r.code, Some(0), "vgraph {args:?} 실패: {}", r.stderr);
    r.stdout
}

/// 오류 문구를 종류로 분류한다 (원문 발췌는 따로 출력).
fn classify(stderr: &str) -> &'static str {
    let l = stderr.to_lowercase();
    if l.contains("locked") || l.contains("busy") {
        "잠금(locked/busy)"
    } else if l.contains("충돌") || l.contains("conflict") {
        "충돌"
    } else if l.contains("rebase") || l.contains("retry") {
        "rebase/재시도"
    } else if l.contains("working copy") || l.contains("미존재") || l.contains("unknown") {
        "working copy/요소 없음"
    } else if l.contains("corrupt") {
        "손상(corrupt)"
    } else if l.trim().is_empty() {
        "(없음)"
    } else {
        "기타"
    }
}

#[derive(Default)]
struct WorkerStats {
    set_ok: usize,
    set_err: usize,
    commit_ok: usize,
    commit_rebased: usize,
    commit_conflict: usize,
    commit_empty: usize,
    commit_err: usize,
    last_set_ok: Option<usize>,
    errors: Vec<(String, String)>, // (종류, stderr 첫 줄)
}

fn setup(root: &Path, branches: bool) {
    ok(root, &["init", GRAPH]);
    for k in 0..WORKERS {
        ok(
            root,
            &[
                "add-node",
                GRAPH,
                "--label",
                "Item",
                "--slug",
                &format!("w{k}"),
                "--prop",
                "v=-1",
            ],
        );
    }
    ok(root, &["commit", GRAPH, "-m", "seed"]);
    if branches {
        for k in 0..WORKERS {
            ok(root, &["branch", GRAPH, &format!("b{k}")]);
        }
    }
}

fn worker(root: Arc<TempDir>, k: usize, branch: Option<String>) -> WorkerStats {
    let graph = match &branch {
        Some(b) => format!("{GRAPH}@{b}"),
        None => GRAPH.to_string(),
    };
    let mut st = WorkerStats::default();
    for j in 0..ROUNDS {
        let set = run(
            root.path(),
            &["set", &format!("{graph}#w{k}"), &format!("v={j}")],
        );
        if set.code == Some(0) {
            st.set_ok += 1;
            st.last_set_ok = Some(j);
        } else {
            st.set_err += 1;
            st.errors
                .push((classify(&set.stderr).into(), first_line(&set.stderr)));
        }
        let c = run(root.path(), &["commit", &graph, "-m", &format!("w{k}-{j}")]);
        if c.code == Some(0) && c.stdout.contains("committed") {
            st.commit_ok += 1;
        } else if c.code == Some(0) && c.stdout.contains("rebased") {
            st.commit_rebased += 1; // 다른 워커가 먼저 head 를 옮겨 자동 rebase 로 성공
        } else if c.code == Some(0) && c.stdout.contains("충돌") {
            st.commit_conflict += 1;
            st.errors
                .push(("충돌(stdout, 종료코드 0)".into(), first_line(&c.stdout)));
        } else if c.code == Some(0) {
            st.commit_empty += 1; // 변경 없음 — 다른 워커의 commit 이 내 변경을 이미 실어 갔을 수 있음
        } else {
            st.commit_err += 1;
            st.errors
                .push((classify(&c.stderr).into(), first_line(&c.stderr)));
        }
    }
    st
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(160).collect()
}

fn log_len(root: &Path, graph: &str) -> usize {
    ok(root, &["log", graph])
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// export 로 노드별 v 값을 읽는다.
fn values(root: &Path, graph: &str) -> BTreeMap<String, i64> {
    let doc: J = serde_json::from_str(&ok(root, &["export", graph])).expect("export JSON");
    doc["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|n| {
            (
                n["slug"].as_str().unwrap().to_string(),
                n["props"]["v"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn scenario(name: &str, branches: bool) {
    let root = Arc::new(TempDir::new().unwrap());
    setup(root.path(), branches);
    let started = std::time::Instant::now();
    let handles: Vec<_> = (0..WORKERS)
        .map(|k| {
            let root = Arc::clone(&root);
            let branch = branches.then(|| format!("b{k}"));
            thread::spawn(move || worker(root, k, branch))
        })
        .collect();
    let stats: Vec<WorkerStats> = handles
        .into_iter()
        .map(|h| h.join().expect("워커 스레드"))
        .collect();
    let elapsed = started.elapsed();

    eprintln!(
        "\n===== {name}: 워커 {WORKERS}개 × {ROUNDS}회 (set+commit), 소요 {elapsed:.2?} ====="
    );
    eprintln!(
        "| 워커 | set 성공/실패 | commit 성공/변경없음/실패 | 마지막 성공 set |\n|---|---|---|---|"
    );
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples: BTreeMap<String, String> = BTreeMap::new();
    for (k, s) in stats.iter().enumerate() {
        eprintln!(
            "| w{k} | {}/{} | {}/{}/{}/{}/{} | {:?} |",
            s.set_ok,
            s.set_err,
            s.commit_ok,
            s.commit_rebased,
            s.commit_conflict,
            s.commit_empty,
            s.commit_err,
            s.last_set_ok
        );
        for (kind, line) in &s.errors {
            *kinds.entry(kind.clone()).or_default() += 1;
            samples.entry(kind.clone()).or_insert_with(|| line.clone());
        }
    }
    eprintln!("오류 종류: {kinds:?}");
    for (kind, line) in &samples {
        eprintln!("  오류 원문 발췌 [{kind}]: {line}");
    }

    // ---- 다시 열기: 저장소가 읽히는지 (이것만 단언) + 유실 대조 ----
    let commit_ok_total: usize = stats.iter().map(|s| s.commit_ok + s.commit_rebased).sum();
    if branches {
        for (k, s) in stats.iter().enumerate() {
            let g = format!("{GRAPH}@b{k}");
            let n = log_len(root.path(), &g); // seed 1개 + 성공 커밋
            let v = values(root.path(), &g);
            eprintln!(
                "  b{k}: log {n}개 (seed 포함, 성공 commit {} → 기대 {}), w{k}.v={} (마지막 성공 set {:?})",
                s.commit_ok + s.commit_rebased,
                s.commit_ok + s.commit_rebased + 1,
                v[&format!("w{k}")],
                s.last_set_ok
            );
        }
        ok(root.path(), &["log", GRAPH]);
        ok(root.path(), &["show", GRAPH]);
    } else {
        let n = log_len(root.path(), GRAPH);
        eprintln!(
            "  main: log {n}개 (seed 포함), 성공 commit 합 {commit_ok_total} → 기대 {}",
            commit_ok_total + 1
        );
        let v = values(root.path(), GRAPH);
        for (k, s) in stats.iter().enumerate() {
            eprintln!(
                "  w{k}.v={} (마지막 성공 set {:?})",
                v[&format!("w{k}")],
                s.last_set_ok
            );
        }
        ok(root.path(), &["show", GRAPH]);
        let pend = run(root.path(), &["pending", GRAPH]);
        eprintln!("  남은 working copy: {}", first_line(&pend.stdout));
    }
}

/// ① 모든 워커가 같은 브랜치(main)에 동시에 편집·커밋.
#[test]
#[ignore = "관찰용 — 프로세스 동시 실행, 결과가 매번 다름. 실행: cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture"]
fn concurrent_writes_same_branch_are_recorded_and_store_reopens() {
    scenario("① 같은 브랜치", false);
}

/// ② 워커마다 다른 브랜치(b0..b5)에 동시에 편집·커밋.
#[test]
#[ignore = "관찰용 — 프로세스 동시 실행, 결과가 매번 다름. 실행: cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture"]
fn concurrent_writes_different_branches_are_recorded_and_store_reopens() {
    scenario("② 서로 다른 브랜치", true);
}

// ====================================================================================
// ③ 같은 브랜치 + 워커마다 다른 `--workspace` (이름 있는 작업 공간으로 공유를 피하는 사용법)
//
// 실행: cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture distinct_workspaces
// 환경 변수 VGRAPH_MULTIPROC_WS 로 조건을 바꾼다 (대조군 확인용):
//   distinct(기본) = 워커마다 다른 이름 · none = 이름 없이 · same = 모두 같은 이름
// 단언은 "분리 조건의 무손실"이다 — none·same 으로 돌리면 (공유 때문에) 이 단언이 깨질 수 있고,
// 그것이 옵션 없이 동시에 쓸 때 손실 위험이 있다는 안내의 근거다.
// ====================================================================================

#[derive(Clone, Copy, PartialEq, Debug)]
enum WsMode {
    None,
    Distinct,
    Same,
}

fn ws_mode() -> WsMode {
    match std::env::var("VGRAPH_MULTIPROC_WS").as_deref() {
        Ok("none") => WsMode::None,
        Ok("same") => WsMode::Same,
        _ => WsMode::Distinct,
    }
}

fn ws_name(mode: WsMode, k: usize) -> Option<String> {
    match mode {
        WsMode::None => None,
        WsMode::Distinct => Some(format!("ws{k}")),
        WsMode::Same => Some("shared".to_string()),
    }
}

fn run_ws(root: &Path, ws: Option<&str>, args: &[&str]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vgraph"));
    cmd.arg("--root").arg(root);
    if let Some(w) = ws {
        cmd.arg("--workspace").arg(w);
    }
    let o = cmd.args(args).output().expect("vgraph 실행");
    Run {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[derive(Default)]
struct WsStats {
    commit_ok: usize,
    conflicts: usize,
    errors: Vec<String>,
    no_working_copy: usize,
}

fn worker_ws(root: Arc<TempDir>, k: usize, ws: Option<String>) -> WsStats {
    let mut st = WsStats::default();
    for j in 0..ROUNDS {
        let set = run_ws(
            root.path(),
            ws.as_deref(),
            &["set", &format!("{GRAPH}#w{k}"), &format!("v={j}")],
        );
        if set.code != Some(0) {
            st.errors.push(first_line(&set.stderr));
        }
        let c = run_ws(
            root.path(),
            ws.as_deref(),
            &["commit", GRAPH, "-m", &format!("w{k}-{j}")],
        );
        if c.code == Some(0) && (c.stdout.contains("committed") || c.stdout.contains("rebased")) {
            st.commit_ok += 1;
        } else if c.code == Some(0) && c.stdout.contains("충돌") {
            st.conflicts += 1;
        } else if c.code != Some(0) {
            if c.stderr.contains("working copy 없음") {
                st.no_working_copy += 1;
            }
            st.errors.push(first_line(&c.stderr));
        }
    }
    st
}

/// [핵심] 같은 브랜치에서 워커마다 다른 `--workspace` 로 동시에 편집·커밋해도 유실이 없다.
#[test]
#[ignore = "관찰용 — 프로세스 동시 실행. 실행: cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture distinct_workspaces"]
fn concurrent_writes_same_branch_distinct_workspaces_lose_nothing() {
    let mode = ws_mode();
    let root = Arc::new(TempDir::new().unwrap());
    setup(root.path(), false);
    let handles: Vec<_> = (0..WORKERS)
        .map(|k| {
            let root = Arc::clone(&root);
            let ws = ws_name(mode, k);
            thread::spawn(move || worker_ws(root, k, ws))
        })
        .collect();
    let stats: Vec<WsStats> = handles
        .into_iter()
        .map(|h| h.join().expect("워커 스레드"))
        .collect();

    let finals = values(root.path(), GRAPH);
    let lost: Vec<String> = (0..WORKERS)
        .filter(|k| finals[&format!("w{k}")] != (ROUNDS as i64 - 1))
        .map(|k| format!("w{k}={}", finals[&format!("w{k}")]))
        .collect();
    let commit_ok: usize = stats.iter().map(|s| s.commit_ok).sum();
    let conflicts: usize = stats.iter().map(|s| s.conflicts).sum();
    let no_wc: usize = stats.iter().map(|s| s.no_working_copy).sum();
    let log = log_len(root.path(), GRAPH);
    let leftover = run_ws(
        root.path(),
        ws_name(mode, 0).as_deref(),
        &["pending", GRAPH],
    );
    eprintln!(
        "\n[③ {mode:?}] 최종 유실 {lost:?} · 성공 commit {commit_ok} → log {log} (기대 {}) · 충돌 {conflicts} · working copy 없음 {no_wc} · 남은: {}",
        commit_ok + 1,
        first_line(&leftover.stdout)
    );
    for s in &stats {
        for e in &s.errors {
            eprintln!("  오류: {e}");
        }
    }
    assert!(lost.is_empty(), "최종 값 유실: {lost:?}");
    assert_eq!(log, commit_ok + 1, "성공 commit 수와 log 수가 다르다");
    assert_eq!(conflicts, 0, "서로 다른 노드를 쓰는데 충돌이 보고됐다");
    assert_eq!(no_wc, 0, "working copy 없음 오류");
    assert!(stats.iter().all(|s| s.errors.is_empty()), "오류가 있었다");
}

/// [핵심] 한 프로세스 안에서 스레드마다 서로 다른 작업 공간(`Session::edit_in`)으로 같은 브랜치를 편집·커밋한다.
/// 10회 반복, 각 회는 새 저장소.
#[test]
#[ignore = "관찰용 — 스레드 동시 실행. 실행: cargo test -p vgraph-cli --test multiproc -- --ignored --nocapture threads_distinct"]
fn threads_distinct_workspaces_same_branch_lose_nothing() {
    use vgraph_core::{SystemClock, SystemIdGen, Value};
    use vgraph_engine::{CommitOutcome, Engine, EngineOptions, FsProvider};
    const REPEATS: usize = 10;
    for rep in 0..REPEATS {
        let dir = TempDir::new().unwrap();
        let engine = Arc::new(
            Engine::open(
                Box::new(FsProvider::new(dir.path()).unwrap()),
                Arc::new(SystemIdGen),
                Arc::new(SystemClock),
                EngineOptions::default(),
            )
            .unwrap(),
        );
        let g = {
            let sess = engine.session("local");
            let g = sess.create_graph("acme", "c").unwrap();
            let es = sess.edit(g, "main").unwrap();
            for k in 0..WORKERS {
                es.add_node(
                    &["Item"],
                    Some(&format!("w{k}")),
                    vgraph_core::value::props([("v", Value::Int(-1))]),
                )
                .unwrap();
            }
            sess.commit(&es, "seed").unwrap();
            g
        };
        let handles: Vec<_> = (0..WORKERS)
            .map(|k| {
                let engine = Arc::clone(&engine);
                thread::spawn(move || {
                    // 스레드마다 자기 Session(같은 사용자)·자기 작업 공간
                    let sess = engine.session("local");
                    let ws = format!("ws{k}");
                    let mut ok = 0usize;
                    for j in 0..ROUNDS {
                        let es = sess.edit_in(g, "main", Some(&ws)).expect("edit_in");
                        let node = es.by_slug(&format!("w{k}")).expect("노드");
                        es.set_prop(node, "v", Value::Int(j as i64)).expect("set");
                        match sess.commit(&es, &format!("w{k}-{j}")).expect("commit") {
                            CommitOutcome::Committed { .. } | CommitOutcome::Rebased { .. } => {
                                ok += 1
                            }
                            other => panic!("w{k} r{j}: 예상 밖 결과 {other:?}"),
                        }
                    }
                    ok
                })
            })
            .collect();
        let ok_total: usize = handles.into_iter().map(|h| h.join().expect("스레드")).sum();
        let sess = engine.session("local");
        let (_, head) = sess.resolve("graph://acme/c@main").unwrap();
        let view = sess.checkout(g, head).unwrap();
        for k in 0..WORKERS {
            let node = view.by_slug(&format!("w{k}")).expect("노드");
            assert_eq!(
                view.node_prop(node, "v"),
                Some(Value::Int(ROUNDS as i64 - 1)),
                "회 {rep}: w{k} 최종 값 유실"
            );
        }
        let log = sess.log(g, "main").unwrap().len();
        assert_eq!(
            log,
            ok_total + 1,
            "회 {rep}: 성공 commit 수와 log 수 불일치"
        );
        assert!(
            sess.list_working(g).unwrap().is_empty(),
            "회 {rep}: 종료 후 남은 작업 공간"
        );
        eprintln!("[스레드 회 {rep}] 성공 commit {ok_total}, log {log}, 유실 0, 남은 작업 공간 0");
    }
}
