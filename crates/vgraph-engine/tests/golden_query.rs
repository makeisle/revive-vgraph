//! 검색 기능(R1·R2·R4)의 엔진 계층 골든 — View::find / walk / scan_*.
//!
//! 픽스처 모양은 `crates/vgraph-mcp/tests/mcp_query.rs` 상단 주석의 겹층 그래프와 같다.
//! 외부 경계(소스 계층)는 `NoSupport`로 대체한다 — View::from_state 는 props 를 상주시키므로
//! 폴트가 일어나지 않는다. 순회·찾기·나열 로직은 실제 View 구현을 그대로 탄다.

use std::collections::BTreeSet;
use std::sync::Arc;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::view::{FindQuery, ScanPage, View, ViewSupport, WalkQuery};

struct NoSupport;
impl ViewSupport for NoSupport {
    fn fetch_props(&self, _h: &Hash256) -> Props {
        Props::default()
    }
    fn scan_index(&self, _key: &str, _value_json: &str) -> Vec<Hash256> {
        Vec::new()
    }
}

struct Ids(TestIdGen);
impl Ids {
    fn new() -> Self {
        Ids(TestIdGen::new())
    }
    fn next(&self) -> Uuid {
        self.0.next_uuid()
    }
}

fn node(ids: &Ids, labels: &[&str], slug: &str, p: Props) -> Node {
    Node {
        uuid: ids.next(),
        labels: labels.iter().map(|l| (*l).into()).collect(),
        slug: Some(slug.into()),
        props: p,
    }
}

fn edge(ids: &Ids, etype: &str, from: Uuid, to: Uuid) -> Edge {
    Edge {
        uuid: ids.next(),
        etype: etype.into(),
        from,
        to,
        props: Props::default(),
    }
}

fn view_of(state: &GraphState) -> View {
    View::from_state(
        TestIdGen::new().next_graph(),
        Hash256([0; 32]),
        state,
        Arc::new(NoSupport),
    )
}

struct Layered {
    view: View,
    state: GraphState,
    start: Uuid,
    by_slug: std::collections::BTreeMap<&'static str, Uuid>,
}

fn layered() -> Layered {
    let ids = Ids::new();
    let mut st = GraphState::new();
    let mut by_slug = std::collections::BTreeMap::new();
    let mut add = |st: &mut GraphState, labels: &[&str], slug: &'static str, p: Props| {
        let n = node(&ids, labels, slug, p);
        by_slug.insert(slug, n.uuid);
        st.insert_node(n).unwrap();
    };
    add(
        &mut st,
        &["page"],
        "PAGE",
        props([
            ("name", "주문 조회".into()),
            ("method", "GET".into()),
            ("tags", Value::List(vec!["주문".into(), "조회".into()])),
            ("desc", "Order 를 ID 로 조회한다".into()),
        ]),
    );
    add(
        &mut st,
        &["category"],
        "CAT-1",
        props([
            ("title", "주문 관리".into()),
            ("keywords", Value::List(vec!["주문".into(), "order".into()])),
        ]),
    );
    add(
        &mut st,
        &["entity"],
        "Order",
        props([("name", "Order".into())]),
    );
    add(
        &mut st,
        &["check"],
        "CHK-1",
        props([("text", "주문 ID 는 양수여야 한다".into())]),
    );
    add(
        &mut st,
        &["privacy"],
        "PRV-1",
        props([("text", "본인 것만 허용".into())]),
    );
    add(
        &mut st,
        &["log"],
        "LOG-9",
        props([("text", "CHK-1 에서 뻗는 2단계".into())]),
    );
    add(
        &mut st,
        &["doc"],
        "NOTE-P",
        props([("text", "PRV-1 에서 뻗는 2단계".into())]),
    );
    add(
        &mut st,
        &["category"],
        "CAT-2",
        props([("title", "CAT-1 에서만 닿음".into())]),
    );
    add(
        &mut st,
        &["entity"],
        "Customer",
        props([("name", "Customer".into())]),
    );
    let u = |s: &str| by_slug[s];
    let es = [
        ("분류", "PAGE", "CAT-1"),
        ("엔티티", "PAGE", "Order"),
        ("점검", "PAGE", "CHK-1"),
        ("점검", "PAGE", "PRV-1"),
        ("기록", "CHK-1", "LOG-9"),
        ("기록", "PRV-1", "NOTE-P"),
        ("관련", "CAT-1", "CAT-2"),
        ("관련", "Order", "Customer"),
    ];
    for (t, a, b) in es {
        st.insert_edge(edge(&ids, t, u(a), u(b))).unwrap();
    }
    let start = u("PAGE");
    Layered {
        view: view_of(&st),
        state: st,
        start,
        by_slug,
    }
}

fn slugs(g: &Layered, uuids: impl IntoIterator<Item = Uuid>) -> BTreeSet<String> {
    uuids
        .into_iter()
        .map(|u| g.view.node_slug(u).unwrap().unwrap().to_string())
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

// ---------- R2③ 확장 라벨 노드 확장 재현 ----------

#[test]
fn walk_rule_expand_depth2() {
    let g = layered();
    let hits = g.view.walk(&WalkQuery {
        start: g.start,
        dir: Dir::Out,
        etypes: None,
        max_depth: 2,
        emit_labels: None,
        expand_labels: Some(&["check", "privacy", "log", "audit"]),
        limit: None,
    });
    let d1 = slugs(&g, hits.iter().filter(|h| h.depth == 1).map(|h| h.node));
    let d2 = slugs(&g, hits.iter().filter(|h| h.depth == 2).map(|h| h.node));
    assert_eq!(d1, set(&["CAT-1", "Order", "CHK-1", "PRV-1"]), "{hits:?}");
    assert_eq!(d2, set(&["LOG-9", "NOTE-P"]), "{hits:?}");
    let all = slugs(&g, hits.iter().map(|h| h.node));
    assert!(
        !all.contains("CAT-2") && !all.contains("Customer"),
        "{all:?}"
    );
}

#[test]
fn walk_reports_edge_type_and_via_uuid() {
    let g = layered();
    let hits = g.view.walk(&WalkQuery {
        start: g.start,
        dir: Dir::Out,
        etypes: None,
        max_depth: 1,
        emit_labels: None,
        expand_labels: None,
        limit: None,
    });
    let order = hits.iter().find(|h| h.node == g.by_slug["Order"]).unwrap();
    assert_eq!(order.via_type.as_str(), "엔티티");
    assert!(g.view.has_edge(order.via));
}

#[test]
fn walk_emit_filter_limits_output_not_expansion() {
    let g = layered();
    // emit = log 만: emit 에 안 든 CHK-1 도 계속 뻗으므로 LOG-9 에 닿는다.
    let hits = g.view.walk(&WalkQuery {
        start: g.start,
        dir: Dir::Out,
        etypes: None,
        max_depth: 2,
        emit_labels: Some(&["log"]),
        expand_labels: None,
        limit: None,
    });
    assert_eq!(slugs(&g, hits.iter().map(|h| h.node)), set(&["LOG-9"]));
}

#[test]
fn walk_multiple_etypes_and_limit() {
    let g = layered();
    let q = |limit| WalkQuery {
        start: g.start,
        dir: Dir::Out,
        etypes: Some(&["분류", "엔티티"]),
        max_depth: 3,
        emit_labels: None,
        expand_labels: None,
        limit,
    };
    let hits = g.view.walk(&q(None));
    assert_eq!(
        slugs(&g, hits.iter().map(|h| h.node)),
        set(&["CAT-1", "Order"])
    );
    assert_eq!(g.view.walk(&q(Some(1))).len(), 1);
}

#[test]
fn walk_unknown_start_is_empty() {
    let g = layered();
    // 픽스처가 쓰지 않는 번호대의 uuid — 뷰에 없는 시작 노드
    let absent = TestIdGen::starting_at(10_000).next_uuid();
    let hits = g.view.walk(&WalkQuery {
        start: absent,
        dir: Dir::Both,
        etypes: None,
        max_depth: 3,
        emit_labels: None,
        expand_labels: None,
        limit: None,
    });
    assert!(hits.is_empty());
}

// ---------- differential: 필터 없는 walk == neighbors ----------

#[test]
fn walk_without_filters_equals_neighbors_all_dirs_and_depths() {
    let g = layered();
    let starts: Vec<Uuid> = g.by_slug.values().copied().collect();
    for start in starts {
        for dir in [Dir::Out, Dir::In, Dir::Both] {
            for depth in 1..=3u32 {
                let old = g.view.neighbors(start, dir, None, None, 1, depth, None);
                let new = g.view.walk(&WalkQuery {
                    start,
                    dir,
                    etypes: None,
                    max_depth: depth,
                    emit_labels: None,
                    expand_labels: None,
                    limit: None,
                });
                let new_h: Vec<Hit> = new
                    .iter()
                    .map(|h| Hit {
                        node: h.node,
                        via: h.via,
                        depth: h.depth,
                    })
                    .collect();
                assert_eq!(old, new_h, "start={start} dir={dir:?} depth={depth}");
                // 오라클(GraphState)과도 같다
                let oracle = g.state.neighbors(start, dir, None, None, 1, depth, None);
                assert_eq!(
                    oracle, new_h,
                    "oracle start={start} dir={dir:?} depth={depth}"
                );
            }
        }
    }
}

// ---------- R1·R2① 찾기 ----------

fn find<'a>(g: &Layered, q: FindQuery<'a>) -> Vec<(String, Option<String>, Option<String>)> {
    g.view
        .find(&q)
        .items
        .iter()
        .map(|m| {
            (
                g.view.node_slug(m.node).unwrap().unwrap().to_string(),
                m.matched_key.as_ref().map(|k| k.to_string()),
                m.excerpt.clone(),
            )
        })
        .collect()
}

fn text_query<'a>(text: &'a str, keys: Option<&'a [&'a str]>, empty: &'a Props) -> FindQuery<'a> {
    FindQuery {
        labels: &[],
        props_eq: empty,
        text: Some(text),
        text_keys: keys,
        limit: 100,
        after: None,
    }
}

#[test]
fn find_text_case_insensitive() {
    let g = layered();
    let none = Props::default();
    let r = find(&g, text_query("ORDER", Some(&["name", "desc"]), &none));
    // Order(name="Order") 와 PAGE(desc="Order 를 ID 로 조회한다")
    let slugs_hit: BTreeSet<_> = r.iter().map(|x| x.0.clone()).collect();
    assert_eq!(slugs_hit, set(&["PAGE", "Order"]), "{r:?}");
    let page = r.iter().find(|x| x.0 == "PAGE").unwrap();
    assert_eq!(page.1.as_deref(), Some("desc"));
    assert_eq!(page.2.as_deref(), Some("Order 를 ID 로 조회한다"));
}

#[test]
fn find_text_korean() {
    let g = layered();
    let none = Props::default();
    let r = find(&g, text_query("양수", None, &none));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, "CHK-1");
    assert_eq!(r[0].1.as_deref(), Some("text"));
    assert_eq!(r[0].2.as_deref(), Some("주문 ID 는 양수여야 한다"));
}

#[test]
fn find_text_inside_list_strings() {
    let g = layered();
    let none = Props::default();
    // "조회" 는 PAGE.tags 리스트 안(그리고 PAGE.name 에도 있음) — 키 지정 없이 모든 문자열 속성
    let r = find(&g, text_query("keywords-없음", None, &none));
    assert!(r.is_empty());
    let r = find(&g, text_query("order", Some(&["keywords"]), &none));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, "CAT-1");
    assert_eq!(r[0].1.as_deref(), Some("keywords"));
    assert_eq!(r[0].2.as_deref(), Some("order"));
}

#[test]
fn find_text_key_restriction_excludes_other_keys() {
    let g = layered();
    let none = Props::default();
    // "조회" 는 PAGE 의 name·tags·desc 에 있다. keys=[title] 이면 아무것도 안 나온다.
    assert!(find(&g, text_query("조회", Some(&["title"]), &none)).is_empty());
    // keys=[tags] 면 tags 만 맞은 키로 보고된다.
    let r = find(&g, text_query("조회", Some(&["tags"]), &none));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, "PAGE");
    assert_eq!(r[0].1.as_deref(), Some("tags"));
}

#[test]
fn find_excerpt_is_char_bounded_and_marks_truncation() {
    let ids = Ids::new();
    let mut st = GraphState::new();
    let long = format!("{}한글NEEDLE끝{}", "가".repeat(50), "나".repeat(50));
    st.insert_node(node(&ids, &["doc"], "long", props([("body", long.into())])))
        .unwrap();
    let v = view_of(&st);
    let none = Props::default();
    let page = v.find(&text_query("needle", None, &none));
    assert_eq!(page.items.len(), 1);
    let ex = page.items[0].excerpt.as_deref().unwrap();
    // 앞뒤 잘림 표시 + 맞은 부분 포함 + 유효한 UTF-8(String 이므로 패닉 없이 도달)
    assert!(ex.starts_with('…') && ex.ends_with('…'), "{ex}");
    assert!(ex.contains("한글NEEDLE끝"), "{ex}");
    assert!(ex.chars().count() < 100, "{ex}");
}

#[test]
fn find_labels_and_props_conditions_without_text() {
    let g = layered();
    let none = Props::default();
    let by_label = FindQuery {
        labels: &["category"],
        props_eq: &none,
        text: None,
        text_keys: None,
        limit: 100,
        after: None,
    };
    let r = find(&g, by_label);
    assert_eq!(
        r.iter().map(|x| x.0.clone()).collect::<BTreeSet<_>>(),
        set(&["CAT-1", "CAT-2"])
    );
    assert!(r.iter().all(|x| x.1.is_none() && x.2.is_none()));

    let eq = props([("method", "GET".into())]);
    let r = find(
        &g,
        FindQuery {
            labels: &["page"],
            props_eq: &eq,
            text: None,
            text_keys: None,
            limit: 100,
            after: None,
        },
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "PAGE");

    let miss = props([("method", "POST".into())]);
    assert!(find(
        &g,
        FindQuery {
            labels: &["page"],
            props_eq: &miss,
            text: None,
            text_keys: None,
            limit: 100,
            after: None,
        }
    )
    .is_empty());
}

#[test]
fn find_pages_by_uuid_cursor_without_duplicates() {
    let g = layered();
    let none = Props::default();
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = g.view.find(&FindQuery {
            labels: &[],
            props_eq: &none,
            text: None,
            text_keys: None,
            limit: 2,
            after,
        });
        seen.extend(page.items.iter().map(|m| m.node));
        if page.done {
            assert!(page.next_cursor.is_none());
            break;
        }
        after = page.next_cursor;
        assert!(after.is_some());
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen, sorted, "uuid 오름차순·중복 없음");
    assert_eq!(seen.len(), g.view.node_count());
}

// ---------- R4 커서 나열 ----------

fn big_graph(n: usize, m: usize) -> View {
    let ids = Ids::new();
    let mut st = GraphState::new();
    let mut us = Vec::new();
    for i in 0..n {
        let label = if i % 2 == 0 { "even" } else { "odd" };
        let nd = Node {
            uuid: ids.next(),
            labels: vec![label.into()],
            slug: None,
            props: Props::default(),
        };
        us.push(nd.uuid);
        st.insert_node(nd).unwrap();
    }
    for i in 0..m {
        st.insert_edge(edge(&ids, "t", us[i % n], us[(i * 7 + 1) % n]))
            .unwrap();
    }
    view_of(&st)
}

#[test]
fn scan_nodes_and_edges_2500_by_100_totals_match_no_duplicates() {
    let v = big_graph(2500, 2500);
    let mut nodes = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let p: ScanPage<Uuid> = v.scan_nodes(&[], after, 100);
        assert!(p.items.len() <= 100);
        nodes.extend(p.items.iter().copied());
        pages += 1;
        if p.done {
            assert!(p.next_cursor.is_none());
            break;
        }
        after = p.next_cursor;
    }
    assert_eq!(pages, 25);
    assert_eq!(nodes.len(), v.node_count());
    assert_eq!(nodes.iter().collect::<BTreeSet<_>>().len(), nodes.len());

    let mut edges = Vec::new();
    let mut after = None;
    loop {
        let p = v.scan_edges(after, 100);
        edges.extend(p.items.iter().copied());
        if p.done {
            break;
        }
        after = p.next_cursor;
    }
    assert_eq!(edges.len(), v.edge_count());
    assert_eq!(edges.iter().collect::<BTreeSet<_>>().len(), edges.len());
    assert!(edges.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn scan_nodes_label_filter_and_edge_info() {
    let v = big_graph(10, 4);
    let evens = v.scan_nodes(&["even"], None, 100);
    assert_eq!(evens.items.len(), 5);
    assert!(evens.done);
    assert!(v.scan_nodes(&["nope"], None, 100).items.is_empty());

    let e = v.scan_edges(None, 1);
    assert_eq!(e.items.len(), 1);
    assert!(!e.done);
    let info = v.edge_info(e.items[0]).unwrap();
    assert_eq!(info.etype.as_str(), "t");
    assert!(v.has_node(info.from) && v.has_node(info.to));
    assert!(v.edge_info(e.next_cursor.unwrap_or(e.items[0])).is_some());
}

#[test]
fn scan_empty_graph_is_done_immediately_and_zero_limit_returns_nothing() {
    let v = view_of(&GraphState::new());
    let p = v.scan_nodes(&[], None, 10);
    assert!(p.items.is_empty() && p.done && p.next_cursor.is_none());

    let v = big_graph(3, 0);
    let p = v.scan_nodes(&[], None, 0);
    assert!(p.items.is_empty() && !p.done);
}

// ---------- 찾기 선택 옵션: 관련도 정렬 · 표기 정규화 · 여러 표현 ----------

use vgraph_engine::view::{FindMatch, FindOptions, FindOptionsError, FindOutcome};

/// 노드 목록으로 그래프를 만든다. 입력 순서가 uuid 순서다.
fn graph_of(specs: &[(&[&str], &str, Props)]) -> View {
    let ids = Ids::new();
    let mut st = GraphState::new();
    for (labels, slug, p) in specs {
        st.insert_node(node(&ids, labels, slug, p.clone())).unwrap();
    }
    view_of(&st)
}

fn query<'a>(text: Option<&'a str>, limit: usize, empty: &'a Props) -> FindQuery<'a> {
    FindQuery {
        labels: &[],
        props_eq: empty,
        text,
        text_keys: None,
        limit,
        after: None,
    }
}

fn slugs_of(v: &View, out: &FindOutcome) -> Vec<String> {
    out.items
        .iter()
        .map(|h| v.node_slug(h.found.node).unwrap().unwrap().to_string())
        .collect()
}

fn name(s: &str) -> Props {
    props([("name", s.into())])
}

fn relevance() -> FindOptions<'static> {
    FindOptions {
        relevance: true,
        ..FindOptions::default()
    }
}

fn normalized() -> FindOptions<'static> {
    FindOptions {
        normalize: true,
        ..FindOptions::default()
    }
}

/// [핵심] uuid 순이면 `limit:1`에서 빠지는 완전 일치 노드가 relevance 에서는 첫째.
#[test]
fn relevance_puts_exact_match_first_even_with_limit_one() {
    let v = graph_of(&[
        (&["item"], "a", name("소셜 로그인 안내")),
        (&["item"], "b", name("로그인")),
    ]);
    let none = Props::default();
    let plain = v.find(&query(Some("로그인"), 1, &none));
    assert_eq!(
        v.node_slug(plain.items[0].node).unwrap().unwrap().as_str(),
        "a"
    );
    let out = v
        .find_with(&query(Some("로그인"), 1, &none), &relevance())
        .unwrap();
    assert_eq!(slugs_of(&v, &out), ["b"]);
    assert_eq!(out.total, Some(2));
    assert_eq!(out.next_cursor, None);
    assert!(!out.done, "limit 에 잘렸으면 done=false");
}

/// 정렬 규칙 표: 점수 = 값 일치 강도(완전 30·앞부분 20·부분 10) + 출처 가산(slug 2·라벨 1·속성 0).
#[test]
fn relevance_score_rule_table() {
    let v = graph_of(&[
        (&["item"], "s1", name("소셜 로그인 안내")),
        (&["item"], "s2", name("로그인 화면")),
        (&["item"], "s3", name("로그인")),
        (&["item"], "로그인", name("가 로그인 나")),
        (&["로그인"], "s5", name("가 로그인 나")),
        (&["item"], "로그인-흐름", name("가 로그인 나")),
    ]);
    let none = Props::default();
    let out = v
        .find_with(&query(Some("로그인"), 10, &none), &relevance())
        .unwrap();
    let got: Vec<(String, u32)> = slugs_of(&v, &out)
        .into_iter()
        .zip(out.items.iter().map(|h| h.score.unwrap()))
        .collect();
    let want: Vec<(String, u32)> = [
        ("로그인", 32),
        ("s5", 31),
        ("s3", 30),
        ("로그인-흐름", 22),
        ("s2", 20),
        ("s1", 10),
    ]
    .iter()
    .map(|(s, n)| ((*s).to_string(), *n))
    .collect();
    assert_eq!(got, want);
    assert!(out.done);
    assert_eq!(out.total, Some(6));
}

/// 동점은 uuid 순 — 몇 번을 불러도 같다.
#[test]
fn relevance_ties_break_by_uuid_deterministically() {
    let v = graph_of(&[
        (&["item"], "t1", name("로그인 가")),
        (&["item"], "t2", name("로그인 나")),
        (&["item"], "t3", name("로그인 다")),
    ]);
    let none = Props::default();
    for _ in 0..3 {
        let out = v
            .find_with(&query(Some("로그인"), 2, &none), &relevance())
            .unwrap();
        assert_eq!(slugs_of(&v, &out), ["t1", "t2"]);
    }
}

/// relevance 는 커서와 함께 쓸 수 없다.
#[test]
fn relevance_with_cursor_is_an_error() {
    let v = graph_of(&[(&["item"], "a", name("로그인"))]);
    let none = Props::default();
    let mut q = query(Some("로그인"), 5, &none);
    q.after = Some(TestIdGen::new().next_uuid());
    assert_eq!(
        v.find_with(&q, &relevance()).unwrap_err(),
        FindOptionsError::RelevanceWithCursor
    );
}

/// 텍스트 조건이 없으면 점수가 모두 같아 uuid 순, 라벨 필터는 그대로 적용.
#[test]
fn relevance_without_text_is_uuid_order_and_filters_apply() {
    let v = graph_of(&[
        (&["item"], "a", name("x")),
        (&["doc"], "b", name("y")),
        (&["item"], "c", name("z")),
    ]);
    let none = Props::default();
    let mut q = query(None, 10, &none);
    q.labels = &["item"];
    let out = v.find_with(&q, &relevance()).unwrap();
    assert_eq!(slugs_of(&v, &out), ["a", "c"]);
    assert!(out.items.iter().all(|h| h.found.matched_key.is_none()));
}

/// [핵심] normalize: "로그 인"·"로그-인"·"로그_인"이 "로그인"과 맞고 발췌는 원문 기준.
#[test]
fn normalize_ignores_space_hyphen_underscore() {
    let v = graph_of(&[
        (&["item"], "sp", name("로그 인")),
        (&["item"], "hy", name("로그-인")),
        (&["item"], "un", name("로그_인")),
        (&["item"], "uh", name("로그\u{2010}인")),
        (&["item"], "ok", name("로그인")),
        (&["item"], "no", name("로그아웃")),
    ]);
    let none = Props::default();
    let plain = v.find(&query(Some("로그인"), 10, &none));
    assert_eq!(plain.items.len(), 1, "옵션 없이는 정확히 붙은 것만");
    let out = v
        .find_with(&query(Some("로그인"), 10, &none), &normalized())
        .unwrap();
    assert_eq!(slugs_of(&v, &out), ["sp", "hy", "un", "uh", "ok"]);
    assert_eq!(out.items[0].found.excerpt.as_deref(), Some("로그 인"));
    assert_eq!(out.items[1].found.excerpt.as_deref(), Some("로그-인"));
    // 바늘 쪽의 구분 문자도 무시, 대소문자 무시는 기존대로
    let v2 = graph_of(&[(&["item"], "e", name("Sign-In Page"))]);
    let out = v2
        .find_with(&query(Some("sign in"), 10, &none), &normalized())
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].found.excerpt.as_deref(), Some("Sign-In Page"));
}

/// normalize 발췌: 앞뒤 20자 문맥이 원문 위치 기준으로 잡힌다.
#[test]
fn normalize_excerpt_context_uses_original_positions() {
    let long = format!("{} 로그 - 인 {}", "가".repeat(30), "나".repeat(30));
    let v = graph_of(&[(&["item"], "a", name(&long))]);
    let none = Props::default();
    let out = v
        .find_with(&query(Some("로그인"), 10, &none), &normalized())
        .unwrap();
    let ex = out.items[0].found.excerpt.clone().unwrap();
    // 맞은 구간은 원문 "로그 - 인"(5자). 앞 20자 = "가"×19 + 공백, 뒤 20자 = 공백 + "나"×19
    assert_eq!(
        ex,
        format!("…{} 로그 - 인 {}…", "가".repeat(19), "나".repeat(19))
    );
}

/// 바늘이 구분 문자뿐이면 normalize 에서는 그 표현을 버린다(텍스트 조건 없음과 같다).
#[test]
fn normalize_blank_needle_means_no_text_condition() {
    let v = graph_of(&[(&["item"], "a", name("x")), (&["item"], "b", name("y"))]);
    let none = Props::default();
    let out = v
        .find_with(&query(Some(" - _ "), 10, &none), &normalized())
        .unwrap();
    assert_eq!(slugs_of(&v, &out), ["a", "b"]);
    assert!(out.items[0].found.matched_key.is_none());
}

/// [핵심] any: 여러 표현 중 하나라도 맞으면 나온다. text 도 후보 표현.
#[test]
fn any_is_or_and_text_joins_the_expressions() {
    let v = graph_of(&[
        (&["item"], "p", name("결제 수단")),
        (&["item"], "q", name("배송 조회")),
        (&["item"], "r", name("로그인")),
        (&["item"], "s", name("기타")),
    ]);
    let none = Props::default();
    let opts = FindOptions {
        any: &["결제", "배송"],
        ..FindOptions::default()
    };
    let out = v.find_with(&query(None, 10, &none), &opts).unwrap();
    assert_eq!(slugs_of(&v, &out), ["p", "q"]);
    assert_eq!(out.items[0].matched_expr.as_deref(), Some("결제"));
    assert_eq!(out.items[1].matched_expr.as_deref(), Some("배송"));
    let out = v
        .find_with(&query(Some("로그인"), 10, &none), &opts)
        .unwrap();
    assert_eq!(slugs_of(&v, &out), ["p", "q", "r"]);
    assert_eq!(out.items[2].matched_expr.as_deref(), Some("로그인"));
    // 한 노드가 두 표현에 모두 맞으면 먼저 준 표현(text → any 순)으로 보고
    let both = graph_of(&[(&["item"], "z", name("결제 배송"))]);
    let out = both.find_with(&query(None, 10, &none), &opts).unwrap();
    assert_eq!(out.items[0].matched_expr.as_deref(), Some("결제"));
}

/// any 의 빈 문자열은 무시하고, 모두 비면 텍스트 조건 없음.
#[test]
fn any_ignores_empty_expressions() {
    let v = graph_of(&[(&["item"], "a", name("x")), (&["item"], "b", name("y"))]);
    let none = Props::default();
    let opts = FindOptions {
        any: &["", ""],
        ..FindOptions::default()
    };
    let out = v.find_with(&query(None, 10, &none), &opts).unwrap();
    assert_eq!(out.items.len(), 2);
    let opts = FindOptions {
        any: &["", "y"],
        ..FindOptions::default()
    };
    let out = v.find_with(&query(None, 10, &none), &opts).unwrap();
    assert_eq!(slugs_of(&v, &out), ["b"]);
}

/// 옵션 조합(any + normalize + relevance) 과 노드가 가장 잘 맞은 표현 보고.
#[test]
fn options_combine_and_relevance_reports_best_expression() {
    let v = graph_of(&[
        (&["item"], "a", name("소셜 로그인 안내")),
        (&["item"], "b", name("배송 조회")),
        (&["item"], "c", name("로그인 배송")),
    ]);
    let none = Props::default();
    let opts = FindOptions {
        relevance: true,
        normalize: true,
        any: &["로그 인", "배송"],
    };
    let out = v.find_with(&query(None, 10, &none), &opts).unwrap();
    // b: "배송" 앞부분 일치(20) / c: "로그 인" 앞부분 일치(20) 동점 → uuid 순(b 먼저), a: 부분(10)
    assert_eq!(slugs_of(&v, &out), ["b", "c", "a"]);
    assert_eq!(out.items[1].matched_expr.as_deref(), Some("로그 인"));
}

/// 기본 옵션이면 find 와 같은 항목·커서·done (옵션 없는 경로는 바뀌지 않는다).
#[test]
fn default_options_equal_plain_find_including_cursor() {
    let g = layered();
    let none = Props::default();
    let mut after = None;
    loop {
        let q = FindQuery {
            labels: &[],
            props_eq: &none,
            text: Some("주문"),
            text_keys: None,
            limit: 1,
            after,
        };
        let plain = g.view.find(&q);
        let with = g.view.find_with(&q, &FindOptions::default()).unwrap();
        let w: Vec<FindMatch> = with.items.iter().map(|h| h.found.clone()).collect();
        assert_eq!(plain.items, w);
        assert_eq!(plain.next_cursor, with.next_cursor);
        assert_eq!(plain.done, with.done);
        assert_eq!(with.total, None);
        match plain.next_cursor {
            Some(c) => after = Some(c),
            None => break,
        }
    }
}

// ---------- 찾기 텍스트 필터: 주소 문자열 제외(skip_ref_like) ----------

use vgraph_engine::view::TextFilter;

fn skip_refs() -> TextFilter {
    TextFilter {
        skip_ref_like: true,
    }
}

fn text_props(key: &str, v: Value) -> Props {
    props([(key, v)])
}

fn s_val(s: &str) -> Value {
    Value::Str(s.to_string())
}

/// [핵심] 값이 주소 형식뿐인 문자열(리스트 원소 포함)은 빠지고 문장 속 주소·축약형은 남는다.
#[test]
fn skip_ref_like_drops_address_only_strings() {
    let v = graph_of(&[
        (&["x"], "a", text_props("link", s_val("graph://ns/doc#n1"))),
        (
            &["x"],
            "b",
            text_props("link", s_val("see graph://ns/doc#n1 now")),
        ),
        (
            &["x"],
            "c",
            text_props(
                "links",
                Value::List(vec![
                    s_val("graph://ns/doc"),
                    s_val("graph://ns/doc@dev#n2"),
                ]),
            ),
        ),
        (
            &["x"],
            "d",
            text_props(
                "links",
                Value::List(vec![s_val("graph://ns/doc#n1"), s_val("ns/doc 문서")]),
            ),
        ),
        (
            &["x"],
            "e",
            text_props("link", s_val(" graph://ns/doc#n1 ")),
        ),
        (&["x"], "f", text_props("link", s_val("ns/doc#n1"))),
        (
            &["x"],
            "g",
            text_props("link", s_val("graph://ns/doc#n1 graph://ns/doc#n2")),
        ),
        (&["x"], "h", text_props("link", s_val("graph://x/ns/doc"))),
    ]);
    let none = Props::default();
    let q = query(Some("ns/doc"), 10, &none);
    let off = v.find_filtered(&q, &FindOptions::default(), &TextFilter::default());
    assert_eq!(
        slugs_of(&v, &off.unwrap()),
        ["a", "b", "c", "d", "e", "f", "g", "h"]
    );
    let on = v
        .find_filtered(&q, &FindOptions::default(), &skip_refs())
        .unwrap();
    // a·c·e 는 주소뿐, d 는 일반 원소로 맞고, b·f·g(공백이 낀 값)·h(주소 문법 위반)는 일반 텍스트
    assert_eq!(slugs_of(&v, &on), ["b", "d", "f", "g", "h"]);
    let d = &on.items[1].found;
    assert_eq!(d.excerpt.as_deref(), Some("ns/doc 문서"));
}

/// 필터를 끄면 `find_with`와 같고, 옵션(relevance·normalize·any)과 함께 쓸 수 있다.
#[test]
fn text_filter_default_equals_find_with_and_combines_with_options() {
    let v = graph_of(&[
        (&["x"], "a", text_props("link", s_val("graph://ns/doc#n1"))),
        (&["x"], "b", text_props("note", s_val("Ns Doc 안내"))),
    ]);
    let none = Props::default();
    let q = query(Some("ns/doc"), 10, &none);
    let plain = v.find_with(&q, &FindOptions::default()).unwrap();
    let filtered = v
        .find_filtered(&q, &FindOptions::default(), &TextFilter::default())
        .unwrap();
    assert_eq!(plain, filtered);

    let opts = FindOptions {
        relevance: true,
        normalize: true,
        any: &["ns doc"],
    };
    let out = v.find_filtered(&q, &opts, &skip_refs()).unwrap();
    assert_eq!(slugs_of(&v, &out), ["b"]);
    assert_eq!(out.total, Some(1));
}
