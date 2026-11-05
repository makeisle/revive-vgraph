//! 레지스트리 다시 읽기와 중복 이름 감지 — 같은 폴더를 여러 엔진(= 여러 프로세스)이 함께 쓸 때.
//! 모킹 없음: 실제 `FsProvider`와 임시 폴더. 엔진 둘을 한 폴더에 여는 것이 "두 프로세스"의 모형이다
//! (프로세스 간 관통은 vgraph-mcp 의 serve_http 시험이 맡는다).

mod util;

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use util::*;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;

const USER: &str = "ceo";
const NS: &str = "acme";
const SLUG: &str = "shared";
/// 엔진마다 다른 ID 시작값 — 같은 값이면 서로 다른 프로세스가 같은 GraphId 를 낼 수 없는 현실과 어긋난다.
const START_A: u64 = 1;
const START_B: u64 = 1_000;
const RACERS: u64 = 6;
const STALE_AGE: Duration = Duration::from_secs(3_600);
const LOCK_FILE: &str = "registry.lock";

fn registry_lines(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("registry.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

fn is_head_committed(out: &CommitOutcome) -> bool {
    matches!(
        out,
        CommitOutcome::Committed { .. } | CommitOutcome::Rebased { .. }
    )
}

/// 다른 엔진이 만든 그래프를, 처음 엔진이 다시 열지 않고 이름으로 찾아 조회·편집·커밋한다.
#[test]
fn engine_sees_graph_created_by_another_engine_without_restart() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let sa = a.session(USER);
    assert_eq!(sa.graph_id(NS, SLUG), None, "만들기 전에는 없다");

    let b = engine_at(dir.path(), true, START_B);
    let sb = b.session(USER);
    let gb = sb.create_graph(NS, SLUG).unwrap();
    let es = sb.edit(gb, "main").unwrap();
    es.add_node(&["Item"], Some("x"), props([("v", 1.into())]))
        .unwrap();
    assert!(is_head_committed(&sb.commit(&es, "from b").unwrap()));

    // A: 이름 찾기 · 주소 해석 · 조회 · 편집 · 커밋 — 재시작 없음
    assert_eq!(sa.graph_id(NS, SLUG), Some(gb));
    let (g, head) = sa.resolve(&format!("graph://{NS}/{SLUG}@main")).unwrap();
    assert_eq!(g, gb);
    let view = sa.checkout(g, head).unwrap();
    assert!(
        view.by_slug("x").is_some(),
        "B 가 커밋한 노드가 A 에서 보인다"
    );
    let ea = sa.edit(g, "main").unwrap();
    ea.add_node(&["Item"], Some("y"), props([("v", 2.into())]))
        .unwrap();
    assert!(is_head_committed(&sa.commit(&ea, "from a").unwrap()));
    assert!(
        sa.graphs().iter().any(|(n, s, _)| n == NS && s == SLUG),
        "다시 읽은 뒤 목록에도 나온다"
    );
}

/// 다른 엔진이 먼저 만든 이름을 만들려 하면 `graph exists`.
#[test]
fn create_is_refused_when_another_engine_made_the_name_first() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let sa = a.session(USER);
    let b = engine_at(dir.path(), true, START_B);
    let first = b.session(USER).create_graph(NS, SLUG).unwrap();

    let err = sa.create_graph(NS, SLUG).unwrap_err();
    assert!(err.to_string().contains("graph exists"), "got: {err}");
    assert_eq!(registry_lines(dir.path()).len(), 1, "레지스트리에 한 줄만");
    // 거부 뒤 A 는 그 이름을 알고 있고, 먼저 만든 쪽의 그래프를 가리킨다.
    assert_eq!(sa.graph_id(NS, SLUG), Some(first));
}

/// 같은 엔진 안의 중복 만들기는 기존과 같은 오류(회귀).
#[test]
fn create_twice_in_one_engine_still_refused() {
    let env = new_env(true);
    let s = env.engine.session(USER);
    s.create_graph(NS, SLUG).unwrap();
    let err = s.create_graph(NS, SLUG).unwrap_err();
    assert!(err.to_string().contains("graph exists"), "got: {err}");
    s.create_graph(NS, "other").unwrap();
}

fn write_duplicate_registry(root: &Path) -> (GraphId, GraphId) {
    // 실제 엔진 둘로 서로 다른 두 그래프를 만든 뒤, 같은 이름의 줄을 손으로 하나 더 붙인다
    // (잠금이 없던 옛 실행 파일이 만들었을 상태).
    let a = engine_at(root, true, START_A);
    let first = a.session(USER).create_graph(NS, "first").unwrap();
    let b = engine_at(root, true, START_B);
    let second = b.session(USER).create_graph(NS, "second").unwrap();
    let text = fs::read_to_string(root.join("registry.jsonl")).unwrap();
    let rewritten = text.replace("\"slug\":\"second\"", "\"slug\":\"first\"");
    fs::write(root.join("registry.jsonl"), rewritten).unwrap();
    (first, second)
}

/// 레지스트리에 같은 (ns, slug)가 둘이면 어느 한쪽을 조용히 고르지 않고 알린다.
#[test]
fn duplicate_registry_names_are_reported_not_hidden() {
    let dir = TempDir::new().unwrap();
    let (first, second) = write_duplicate_registry(dir.path());
    let fresh = engine_at(dir.path(), true, 5_000);
    let s = fresh.session(USER);

    let dups = fresh.duplicate_names();
    assert_eq!(dups.len(), 1, "{dups:?}");
    assert_eq!((dups[0].0.as_str(), dups[0].1.as_str()), (NS, "first"));
    assert!(dups[0].2.contains(&first) && dups[0].2.contains(&second));

    assert_eq!(
        s.graph_id(NS, "first"),
        None,
        "모호한 이름은 한쪽으로 확정하지 않는다"
    );
    let err = s.resolve(&format!("graph://{NS}/first@main")).unwrap_err();
    assert!(err.to_string().contains("ambiguous"), "got: {err}");
    assert!(!err.to_string().starts_with("bad address"), "got: {err}");
    assert!(
        matches!(err, ResolveError::AmbiguousGraph(_)),
        "got: {err:?}"
    );
    let err = s.create_graph(NS, "first").unwrap_err();
    assert!(err.to_string().contains("graph exists"), "got: {err}");
    // 두 그래프의 존재는 목록에서 그대로 보인다(데이터를 가리지 않음).
    let listed = s.graphs();
    assert!(listed.iter().any(|(_, _, id)| *id == first));
    assert!(listed.iter().any(|(_, _, id)| *id == second));
    // 다른 이름은 영향 없음.
    s.create_graph(NS, "unrelated").unwrap();
    assert!(s.graph_id(NS, "unrelated").is_some());
}

/// 열려 있는 동안 다시 읽기에서 중복이 새로 생겨도 감지한다.
#[test]
fn duplicate_appearing_after_open_is_detected_on_refresh() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let sa = a.session(USER);
    sa.create_graph(NS, "first").unwrap();
    // 잠금 없는 쓰기: 같은 이름의 줄을 직접 덧붙임
    let rogue = format!(
        "{{\"id\":\"{:026}\",\"ns\":\"{NS}\",\"slug\":\"first\",\"owner\":\"{USER}\"}}\n",
        9_999u64
    );
    let mut text = fs::read_to_string(dir.path().join("registry.jsonl")).unwrap();
    text.push_str(&rogue);
    text.push_str(&format!(
        "{{\"id\":\"{:026}\",\"ns\":\"{NS}\",\"slug\":\"late\",\"owner\":\"{USER}\"}}\n",
        9_998u64
    ));
    fs::write(dir.path().join("registry.jsonl"), text).unwrap();

    // 모르는 이름 조회가 다시 읽기를 일으킨다
    assert!(sa.graph_id(NS, "late").is_some());
    let dups = a.duplicate_names();
    assert_eq!(dups.len(), 1, "{dups:?}");
}

/// 중복이 없는 저장소는 `duplicate_names`가 비어 있다(기존 정상 저장소 불변).
#[test]
fn healthy_registry_has_no_duplicates() {
    let env = new_env(true);
    let s = env.engine.session(USER);
    s.create_graph(NS, "a").unwrap();
    s.create_graph(NS, "b").unwrap();
    assert!(env.engine.duplicate_names().is_empty());
}

/// 다시 읽기는 이미 연 그래프의 캐시·편집 세션을 건드리지 않는다.
#[test]
fn refresh_keeps_open_graph_edits() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let sa = a.session(USER);
    let g1 = sa.create_graph(NS, "one").unwrap();
    let es = sa.edit(g1, "main").unwrap();
    let n = es
        .add_node(&["Item"], Some("pending"), props([("v", 1.into())]))
        .unwrap();
    let wid = es.wid();

    // 다른 엔진이 새 그래프를 만들고, A 가 그 이름을 찾는다 → 다시 읽기 발생
    let b = engine_at(dir.path(), true, START_B);
    let g2 = b.session(USER).create_graph(NS, "two").unwrap();
    assert_eq!(sa.graph_id(NS, "two"), Some(g2));
    assert_eq!(sa.graph_id(NS, "nothing-here"), None);

    // 편집 세션이 그대로(재사용·미커밋 편집 유지) → 커밋도 된다
    let again = sa.edit(g1, "main").unwrap();
    assert_eq!(again.wid(), wid, "세션 재사용이 유지된다");
    assert!(again.dirty());
    assert_eq!(again.by_slug("pending"), Some(n));
    assert!(is_head_committed(
        &sa.commit(&again, "after refresh").unwrap()
    ));
}

/// 모르는 이름이 계속 모르는 이름이면 오류는 기존과 같다(`UnknownGraph`).
#[test]
fn unknown_name_stays_unknown_graph() {
    let env = new_env(true);
    let s = env.engine.session(USER);
    assert_eq!(
        s.resolve("graph://nope/none@main").unwrap_err(),
        ResolveError::UnknownGraph
    );
}

/// 여러 스레드가 동시에 모르는 이름을 찾아 다시 읽기가 겹쳐도 모두 같은 답을 얻는다.
#[test]
fn concurrent_refresh_is_safe() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let b = engine_at(dir.path(), true, START_B);
    let g = b.session(USER).create_graph(NS, SLUG).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let a = a.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let s = a.session(USER);
                barrier.wait();
                s.graph_id(NS, SLUG)
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), Some(g));
    }
    assert!(a.duplicate_names().is_empty());
}

/// 서로 다른 엔진이 같은 순간 같은 이름을 만들면 정확히 하나만 성공한다.
#[test]
fn simultaneous_creates_of_same_name_yield_exactly_one() {
    let dir = TempDir::new().unwrap();
    let engines: Vec<Engine> = (0..RACERS)
        .map(|i| engine_at(dir.path(), true, START_B + i * 100))
        .collect();
    let barrier = Arc::new(Barrier::new(engines.len()));
    let handles: Vec<_> = engines
        .into_iter()
        .map(|e| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let s = e.session(USER);
                barrier.wait();
                s.create_graph(NS, SLUG).map_err(|e| e.to_string())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(r.contains("graph exists"), "got: {r}");
    }
    assert_eq!(registry_lines(dir.path()).len(), 1);
    assert!(
        !dir.path().join(LOCK_FILE).exists(),
        "잠금 파일은 끝난 뒤 남지 않는다"
    );
    let fresh = engine_at(dir.path(), true, 9_000);
    assert!(fresh.duplicate_names().is_empty());
}

/// 비정상 종료로 남은 오래된 잠금 파일은 무시하고 만들기가 진행된다.
#[test]
fn stale_lock_file_does_not_block_create() {
    let dir = TempDir::new().unwrap();
    let lock = dir.path().join(LOCK_FILE);
    let f = fs::File::create(&lock).unwrap();
    f.set_modified(SystemTime::now() - STALE_AGE).unwrap();
    drop(f);

    let a = engine_at(dir.path(), true, START_A);
    a.session(USER).create_graph(NS, SLUG).unwrap();
    assert_eq!(registry_lines(dir.path()).len(), 1);
    assert!(!lock.exists(), "오래된 잠금은 치워지고 끝난 뒤 남지 않는다");
}

// Windows 한정: 디렉터리를 쓰기 모드로 열 수 있는 것은 Windows(백업 시맨틱 플래그)뿐이라, 다른 OS 에서는 이 상황을 만들 수 없다.
#[cfg(windows)]
/// 오래된 mtime 을 가진 **디렉터리** 잠금 — `remove_file` 로 치울 수 없는 항목.
fn stale_undeletable_lock(root: &Path) {
    let lock = root.join(LOCK_FILE);
    fs::create_dir(&lock).unwrap();
    let mut opts = fs::OpenOptions::new();
    opts.write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // 디렉터리 핸들 열기
        opts.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    let f = opts.open(&lock).unwrap();
    f.set_modified(SystemTime::now() - STALE_AGE).unwrap();
}

/// 오래됐지만 지울 수 없는 잠금이 있으면, 무한 반복하지 않고 대기 상한 안에 오류로 끝난다.
#[cfg(windows)]
#[test]
fn undeletable_stale_lock_ends_in_busy_error_not_a_hang() {
    const DEADLINE: Duration = Duration::from_secs(10);
    let dir = TempDir::new().unwrap();
    stale_undeletable_lock(dir.path());
    let root = dir.path().to_path_buf();

    let (tx, rx) = std::sync::mpsc::channel();
    // 수정 전 코드에서는 이 스레드가 끝나지 않는다 — 판정은 시간 제한으로 한다(스레드는 시험 프로세스와 함께 끝남).
    std::thread::spawn(move || {
        let a = engine_at(&root, true, START_A);
        let r = a.session(USER).create_graph(NS, SLUG);
        let _ = tx.send(r.map(|_| ()).map_err(|e| e.to_string()));
    });
    let r = rx.recv_timeout(DEADLINE).unwrap_or_else(|e| {
        panic!("지울 수 없는 오래된 잠금에서 {DEADLINE:?} 안에 끝나지 않음: {e}")
    });
    let msg = r.expect_err("잠금을 못 얻었는데 만들기가 성공함");
    assert!(msg.contains("registry lock busy"), "{msg}");
    assert!(msg.contains(LOCK_FILE), "경로가 빠짐: {msg}");
    assert!(registry_lines(dir.path()).is_empty(), "등록이 새어 나감");
}

/// 단일 엔진의 기존 사용: 만들기 뒤 폴더에 잠금 파일이 남지 않고 레지스트리 형식은 그대로.
#[test]
fn single_engine_leaves_no_lock_and_keeps_jsonl_format() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let g = a.session(USER).create_graph(NS, SLUG).unwrap();
    assert!(!dir.path().join(LOCK_FILE).exists());
    assert_eq!(
        registry_lines(dir.path()),
        vec![format!(
            "{{\"id\":\"{g}\",\"ns\":\"{NS}\",\"slug\":\"{SLUG}\",\"owner\":\"{USER}\"}}"
        )]
    );
}

/// 목록 조회도 다른 엔진이 만든 그래프를 보여 준다(이름을 먼저 찾지 않아도).
#[test]
fn graphs_listing_includes_graphs_made_by_another_engine() {
    let dir = TempDir::new().unwrap();
    let a = engine_at(dir.path(), true, START_A);
    let sa = a.session(USER);
    assert!(sa.graphs().is_empty());
    let b = engine_at(dir.path(), true, START_B);
    let g = b.session(USER).create_graph(NS, SLUG).unwrap();
    let listed = sa.graphs();
    assert_eq!(listed, vec![(NS.to_string(), SLUG.to_string(), g)]);
}

/// 표준 픽스처(F0)를 다른 엔진이 만들어 커밋하면, 먼저 열려 있던 엔진이 같은 내용을 읽는다.
#[test]
fn fixture_built_by_another_engine_reads_back_identically() {
    let dir = TempDir::new().unwrap();
    let reader = engine_at(dir.path(), true, START_B); // F0 의 ID 순서가 1부터여야 해서 쓰는 쪽이 1
    let writer = engine_at(dir.path(), true, START_A);
    let f = build_f0_engine(&writer.session(USER));

    let sr = reader.session(USER);
    let (g, head) = sr.resolve("graph://acme/bookclub@main").unwrap();
    assert_eq!(g, f.g);
    assert_eq!(head, f.c0);
    let view = sr.checkout(g, head).unwrap();
    assert_eq!(view.node_count(), f.n.len());
    assert!(view.by_slug("cap-auth").is_some());
}
