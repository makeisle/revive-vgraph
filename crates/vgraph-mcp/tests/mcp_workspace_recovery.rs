//! `find_work`가 되살릴 수 없는 작업본을 삼키지 않고 격리·경고하는지 (인시던트 2026-11-07 결함 E).
//!
//! 모킹 없음: 실제 엔진·SQLite 파일. 되살릴 수 없는 작업본은 저장소 파일에 직접 넣는다 —
//! `FsProvider::open_graph`가 돌려주는 백엔드로 커밋 블롭·ref·작업본 줄을 쓴다
//! (이 크레이트는 vgraph-store를 직접 의존하지 않으므로 trait object의 메서드만 쓴다).
//!
//! 오염 체인(C-2) 재현 방법: 같은 delta를 두 번 쌓은 커밋을 별도 ref(`old`)에 박는다.
//! main 체인은 건강하게 두고, 작업본의 base만 그 오염 커밋을 가리키게 한다. 처음부터 접기가
//! 항상 실패하므로(같은 uuid 추가 2회) 되살리기가 실패한다. 엔진 API만으로는 STEP-03 이후
//! 오염 체인을 만들 수 없다.

use serde_json::{json, Value as J};
use std::path::Path;
use tempfile::TempDir;
use vgraph_core::{Commit, GraphId, Hash256, IdGen, TestIdGen, WorkingId};
use vgraph_engine::ops::WalHeader;
use vgraph_engine::{FsProvider, GraphStoreProvider};
use vgraph_mcp::{McpConfig, McpServer};

const USER: &str = "local";
const GRAPH: &str = "acme/catalog";

fn config(root: &Path) -> McpConfig {
    McpConfig {
        root: root.to_path_buf(),
        user: USER.into(),
        embedder: None,
        embed: Default::default(),
    }
}

fn server(root: &Path, workspace: Option<&str>) -> McpServer {
    McpServer::new(config(root))
        .expect("mcp 서버")
        .with_workspace(workspace.map(String::from))
        .expect("작업 공간 이름")
}

fn raw(srv: &McpServer, name: &str, args: J) -> (bool, J) {
    let resp = srv
        .handle_message(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args } }))
        .expect("response");
    let r = &resp["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    let is_err = r["isError"].as_bool().unwrap_or(false);
    let body = serde_json::from_str(&text).unwrap_or(J::String(text));
    (is_err, body)
}

fn call(srv: &McpServer, name: &str, args: J) -> J {
    let (is_err, body) = raw(srv, name, args);
    assert!(!is_err, "tool error ({name}): {body}");
    body
}

fn add_node(srv: &McpServer, slug: &str) -> J {
    call(
        srv,
        "vgraph_edit",
        json!({ "graph": GRAPH, "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": slug, "props": { "v": 1 } } ] }),
    )
}

fn commit(srv: &McpServer, msg: &str) -> J {
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": GRAPH, "message": msg }),
    )
}

/// n1 하나를 커밋해 둔 저장소.
fn seeded() -> TempDir {
    let dir = TempDir::new().unwrap();
    let s = server(dir.path(), None);
    call(&s, "vgraph_create", json!({ "name": GRAPH }));
    add_node(&s, "n1");
    commit(&s, "seed");
    dir
}

fn graph_id(root: &Path) -> GraphId {
    let reg = FsProvider::new(root).unwrap().registry().unwrap();
    assert_eq!(reg.len(), 1, "그래프는 하나");
    reg[0].id
}

/// 엔진 밖에서 저장소 파일을 여는 손잡이 (trait object — 타입 이름을 쓸 수 없어 매크로로 편다).
macro_rules! backend {
    ($root:expr) => {
        FsProvider::new($root)
            .unwrap()
            .open_graph(graph_id($root))
            .unwrap()
    };
}

/// 작업본 id: 시험용 낮은 값(ULID 시간 부분이 0이라 실제 id보다 먼저 정렬됨).
fn low_id(n: u64) -> WorkingId {
    TestIdGen::starting_at(n).next_working()
}

/// 실제 id보다 항상 뒤에 정렬되는 id.
fn high_id() -> WorkingId {
    "7ZZZZZZZZZZZZZZZZZZZZZZZZZ".parse().unwrap()
}

/// 처음부터 접기가 실패하는 커밋(같은 delta를 두 번 쌓음)을 `old` ref에 박고 그 해시를 돌려준다.
fn plant_contaminated_commit(root: &Path) -> Hash256 {
    let b = backend!(root);
    let c1 = b.get_ref("main").unwrap().expect("main head");
    let c1_commit =
        Commit::from_json(std::str::from_utf8(&b.get_blob(&c1).unwrap().unwrap()).unwrap())
            .unwrap();
    let dup = Commit {
        parents: vec![c1],
        delta: c1_commit.delta, // 이미 쌓인 delta를 한 번 더 -> 같은 uuid 추가 2회
        schema_rev: None,
        author: USER.into(),
        ts: "2026-11-08T00:00:00Z".into(),
        msg: "contaminated".into(),
    };
    let h = dup.hash();
    b.put_blob(&h, dup.canonical_json().as_bytes()).unwrap();
    b.cas_ref("old", None, &h).unwrap();
    h
}

fn header(base: Hash256, branch: &str, ws: Option<&str>) -> String {
    WalHeader {
        base: Some(base),
        branch: Some(branch.into()),
        owner: USER.into(),
        workspace: ws.map(String::from),
    }
    .to_json()
}

/// 되살릴 수 없는 작업본 셋을 심는다. 반환 = 심은 id들.
/// 1) base = 오염 커밋 (처음부터 접기 실패)  2) 머리말이 깨진 WAL  3) base 블롭이 없는 WAL
fn plant_poison(root: &Path) -> Vec<WorkingId> {
    let bad_base = plant_contaminated_commit(root);
    let b = backend!(root);
    let (p1, p2, p3) = (low_id(11), low_id(12), low_id(13));
    b.append_wal(p1, USER, &header(bad_base, "main", None))
        .unwrap();
    b.append_wal(p2, USER, "this is not a wal header").unwrap();
    let missing = vgraph_core::canon::sha256(b"no-such-commit");
    b.append_wal(p3, USER, &header(missing, "main", None))
        .unwrap();
    vec![p1, p2, p3]
}

fn live(root: &Path) -> Vec<WorkingId> {
    backend!(root).list_wals(USER).unwrap()
}

fn warned_ids(resp: &J) -> Vec<String> {
    resp["recovery_warnings"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|w| w["working"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------- T6: 되살릴 수 없는 작업본이 있어도 쓰기가 계속되고 쌓이지 않는다 ----------------

#[test]
fn t6_unresumable_workings_are_quarantined_warned_and_writes_continue() {
    let dir = seeded();
    let root = dir.path();
    let poison = plant_poison(root);
    assert_eq!(live(root).len(), 3, "심은 작업본 3개");
    let srv = server(root, None);

    // 첫 호출: 격리 + 응답 경고
    let first = add_node(&srv, "k0");
    let mut warned = warned_ids(&first);
    warned.sort();
    let mut want: Vec<String> = poison.iter().map(ToString::to_string).collect();
    want.sort();
    assert_eq!(
        warned, want,
        "격리된 작업본 id가 응답 경고에 실려야 함: {first}"
    );
    assert!(first["recovery_warnings"][0]["message"].as_str().is_some());
    assert_eq!(
        live(root).len(),
        1,
        "심은 작업본은 목록에서 빠지고 새 작업본 하나만 남음"
    );
    // 기존 응답 필드는 그대로
    assert_eq!(first["applied"], json!(1), "{first}");
    assert!(first["pending"].is_object() && first["results"].is_array());
    commit(&srv, "k0");

    // 이후 20회: 작업본 수가 늘지 않고 쓰기가 계속 완료되며, 같은 경고가 반복되지 않는다
    for i in 1..=20 {
        let slug = format!("k{i}");
        let r = add_node(&srv, &slug);
        assert!(r.get("recovery_warnings").is_none(), "{i}회차 재경고: {r}");
        assert!(
            live(root).len() <= 1,
            "{i}회차 작업본 누적: {:?}",
            live(root)
        );
        let c = commit(&srv, &slug);
        assert!(
            c.get("recovery_warnings").is_none(),
            "{i}회차 커밋 응답 재경고: {c}"
        );
        assert!(live(root).is_empty(), "{i}회차 커밋 뒤 작업본 잔존");
    }
    let shown = call(
        &srv,
        "vgraph_show",
        json!({ "addr": format!("{GRAPH}#k20") }),
    );
    assert!(
        shown.get("props").is_some(),
        "마지막 쓰기가 보여야 함: {shown}"
    );

    // 격리는 삭제가 아니다: 내용 보존
    let b = backend!(root);
    assert_eq!(b.read_wal(poison[0]).unwrap().len(), 1);
    assert_eq!(
        b.read_wal(poison[1]).unwrap(),
        vec!["this is not a wal header".to_string()]
    );
    assert_eq!(b.read_wal(poison[2]).unwrap().len(), 1);
}

#[test]
fn poison_met_during_commit_is_warned_in_commit_response() {
    let dir = seeded();
    let root = dir.path();
    let srv = server(root, None);
    add_node(&srv, "pre");
    let poison = plant_poison(root);
    let c = commit(&srv, "pre");
    assert_eq!(warned_ids(&c).len(), poison.len(), "{c}");
    assert!(c["commit"].is_string() || c.get("outcome").is_some() || c.is_object());
    assert_eq!(
        live(root).len(),
        0,
        "커밋한 작업본은 정리되고 심은 것은 격리"
    );
}

// ---------------- 반영된 남은 작업본(UnknownWorking): 정상 정리, 격리·경고 없음 ----------------

#[test]
fn reflected_leftover_is_cleaned_without_quarantine_or_warning() {
    let dir = seeded();
    let root = dir.path();
    let srv = server(root, Some("w"));
    add_node(&srv, "victim");
    let pending = call(&srv, "vgraph_pending", json!({ "graph": GRAPH }));
    let wid: WorkingId = pending["working"].as_str().unwrap().parse().unwrap();

    // 같은 내용의 복사본 W2 (커밋이 ref까지 반영된 뒤 정리가 안 된 작업본과 같은 상태)
    let b = backend!(root);
    let w2 = high_id(); // 원래 작업본(W)이 먼저 선택되도록 더 큰 id
    for line in b.read_wal(wid).unwrap() {
        b.append_wal(w2, USER, &line).unwrap();
    }
    commit(&srv, "victim"); // W 반영 + 정리. W2는 이름만 남음
    assert_eq!(b.list_wals(USER).unwrap(), vec![w2]);

    let r = add_node(&srv, "after");
    assert!(
        r.get("recovery_warnings").is_none(),
        "정상 정리는 경고하지 않음: {r}"
    );
    assert!(
        b.read_wal(w2).unwrap().is_empty(),
        "격리가 아니라 삭제여야 함(내용 없음)"
    );
    assert_eq!(b.list_wals(USER).unwrap().len(), 1, "새 작업본 하나만");
    commit(&srv, "after");
    assert!(live(root).is_empty());
}

// ---------------- 선택 규칙 불변 ----------------

#[test]
fn resumable_workings_keep_selection_rule() {
    let dir = seeded();
    let root = dir.path();
    let (a, bsrv, d) = (
        server(root, Some("a")),
        server(root, Some("b")),
        server(root, None),
    );
    add_node(&a, "na");
    add_node(&bsrv, "nb");
    add_node(&d, "nd");

    let pend = |s: &McpServer| call(s, "vgraph_pending", json!({ "graph": GRAPH }));
    let (pa, pb, pd) = (pend(&a), pend(&bsrv), pend(&d));
    assert_eq!(live(root).len(), 3);
    for p in [&pa, &pb, &pd] {
        assert!(p.get("recovery_warnings").is_none(), "{p}");
    }
    assert_ne!(pa["working"], pb["working"]);
    assert_ne!(pa["working"], pd["working"]);

    // 같은 (브랜치, 작업 공간 a)에 ID가 더 큰 사본이 있어도 ID 순 첫 일치(원래 것)가 선택됨
    let wa: WorkingId = pa["working"].as_str().unwrap().parse().unwrap();
    let b = backend!(root);
    let dup = high_id();
    for line in b.read_wal(wa).unwrap() {
        b.append_wal(dup, USER, &line).unwrap();
    }
    // 새 서버(세션 캐시 없음)로 확인
    let a2 = server(root, Some("a"));
    let pa2 = pend(&a2);
    assert_eq!(pa2["working"], pa["working"], "ID 순 첫 일치: {pa2}");
    assert!(pa2.get("recovery_warnings").is_none());
    // 다른 이름 작업 공간의 것은 선택되지 않음
    let c = server(root, Some("c"));
    assert_eq!(pend(&c)["working"], J::Null);
    // 아무 것도 격리되지 않았다
    assert_eq!(live(root).len(), 4);
}
