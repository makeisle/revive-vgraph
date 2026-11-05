//! P0 골든 게이트: id 리터럴, F0_DUMP lock(C0 리터럴 고정), D2·D3·D4, G 오라클.
//! (골든 TC v0.2 — docs/vgraph_golden_tc_v0.md)

use std::fs;
use std::path::PathBuf;
use vgraph_core::fixtures::build_f0;
use vgraph_core::value::props;
use vgraph_core::*;

#[test]
fn f0_id_literals() {
    let f = build_f0();
    assert_eq!(f.n[0].to_string(), "00000000000000000000000003"); // n1
    assert_eq!(f.n[4].to_string(), "00000000000000000000000007"); // n5
    assert_eq!(f.e[0].to_string(), "00000000000000000000000008"); // e1
    assert_eq!(f.e[3].to_string(), "00000000000000000000000011"); // e4
    assert_eq!(f.idg.peek(), 12); // 픽스처 후 카운터=12 (골든 §1.1)
    assert_eq!(f.commit.ts, "2026-08-09T00:00:01Z");
}

/// F0_DUMP + C0 골든 lock — 최초 실행 시 생성, 이후 회귀 비교 (골든 §5-1).
/// canonical 스펙 변경 시 lock 파일 삭제 후 재생성 = 의도적 커밋으로만.
#[test]
fn f0_golden_lock() {
    let f = build_f0();
    let dump = f.state.dump(Some(&f.c0));

    // 스냅샷 내용 자체 검증 (문서 F0_DUMP와 구조 일치)
    assert!(dump.contains("\"cap-auth\""));
    assert!(dump.contains("\"회원 인증\""));
    assert!(dump.contains("\"weight\":0.7"));

    let lock_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/f0.lock");
    let content = format!("C0={}\nDELTA={}\nDUMP={}\n", f.c0, f.delta.hash(), dump);
    if lock_path.exists() {
        let existing = fs::read_to_string(&lock_path)
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(
            existing, content,
            "golden lock 불일치 — canonical 스펙이 변했다"
        );
    } else {
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::write(&lock_path, &content).unwrap();
        eprintln!("golden lock 생성: {}", lock_path.display());
    }
}

#[test]
fn golden_d2_normalize_dup_set() {
    let f = build_f0();
    let [n1, ..] = f.n;
    let mut b = DeltaBuilder::new(&f.state);
    b.work.set_prop(n1, "priority", 5.into()).unwrap();
    b.work.set_prop(n1, "priority", 9.into()).unwrap();
    let d = b.build();
    assert_eq!(
        d.props_set,
        vec![PropSet {
            elem: n1,
            key: "priority".into(),
            old: Some(Value::Int(1)),
            new: Some(Value::Int(9)),
        }]
    );
    assert!(d.nodes_added.is_empty() && d.nodes_removed.is_empty());
    assert!(d.edges_added.is_empty() && d.edges_removed.is_empty());
}

#[test]
fn golden_d3_normalize_cancel() {
    let f = build_f0();
    let [n1, ..] = f.n;
    let idg = TestIdGen::starting_at(12);
    let mut b = DeltaBuilder::new(&f.state);
    let tmp = idg.next_uuid();
    b.work
        .insert_node(Node::new(tmp, &["X"], Some("tmp"), Props::new()))
        .unwrap();
    b.work.remove_node(tmp, true).unwrap();
    b.work.set_prop(n1, "priority", 9.into()).unwrap();
    let d = b.build();
    assert_eq!(d.props_set.len(), 1); // tmp 흔적 없음
    assert!(d.nodes_added.is_empty() && d.nodes_removed.is_empty());
}

#[test]
fn golden_d4_invertibility() {
    let f = build_f0();
    let [n1, _, n3, _, n5] = f.n;
    let mut b = DeltaBuilder::new(&f.state);
    b.work.set_prop(n1, "priority", 9.into()).unwrap();
    b.work.set_prop(n3, "status", "active".into()).unwrap();
    b.work.remove_node(n5, true).unwrap(); // e4 cascade
    let d = b.build();

    let mut s2 = f.state.clone();
    apply(&mut s2, &d).unwrap();
    assert_eq!(s2, b.work);

    let mut back = s2.clone();
    apply(&mut back, &d.invert()).unwrap();
    assert_eq!(back, f.state); // 완전 역행
}

#[test]
fn golden_d5_empty_delta() {
    let f = build_f0();
    let b = DeltaBuilder::new(&f.state);
    assert!(b.build().is_empty()); // 엔진(P3)이 Empty로 매핑할 근거
}

#[test]
fn delta_json_roundtrip() {
    let f = build_f0();
    let json = f.delta.canonical_json();
    let d2 = Delta::from_json(&json).unwrap();
    assert_eq!(f.delta, d2);
    assert_eq!(d2.canonical_json(), json);
    assert_eq!(d2.hash(), f.delta.hash());
}

#[test]
fn commit_json_roundtrip() {
    let f = build_f0();
    let c2 = Commit::from_json(&f.commit.canonical_json()).unwrap();
    assert_eq!(f.commit, c2);
    assert_eq!(c2.hash(), f.c0);
}

// ---------- G 오라클 (골든 G1·G2·G3·G4·G5·G9·G12 core 수준) ----------

#[test]
fn golden_g1_one_hop_typed() {
    let f = build_f0();
    let [n1, _, n3, ..] = f.n;
    let hits = f
        .state
        .neighbors(n1, Dir::In, Some(&["parent"]), None, 1, 1, None);
    assert_eq!(
        hits,
        vec![Hit {
            node: n3,
            via: f.e[0],
            depth: 1
        }]
    );
}

#[test]
fn golden_g2_depth_range() {
    let f = build_f0();
    let [n1, n2, _, n4, n5] = f.n;
    let hits = f.state.neighbors(n4, Dir::Out, None, None, 1, 2, None);
    assert_eq!(
        hits,
        vec![
            Hit {
                node: n2,
                via: f.e[1],
                depth: 1
            },
            Hit {
                node: n5,
                via: f.e[3],
                depth: 1
            },
            Hit {
                node: n1,
                via: f.e[2],
                depth: 2
            },
        ]
    );
}

#[test]
fn golden_g3_limit() {
    let f = build_f0();
    let [_, n2, _, n4, n5] = f.n;
    let hits = f.state.neighbors(n4, Dir::Out, None, None, 1, 2, Some(2));
    assert_eq!(
        hits.iter().map(|h| h.node).collect::<Vec<_>>(),
        vec![n2, n5]
    );
}

#[test]
fn golden_g5_matches() {
    let f = build_f0();
    let [n1, n2, ..] = f.n;
    let m = f
        .state
        .matches(&["Capability"], &props([("status", "active".into())]));
    assert_eq!(m, vec![n1, n2]);
}

#[test]
fn golden_find_by_prop() {
    let f = build_f0();
    let [n1, n2, _, n4, _] = f.n;
    let r = f.state.find_by_prop("status", &Value::Str("active".into()));
    assert_eq!(r, vec![n1, n2, n4]);
}

#[test]
fn golden_g9_by_slug() {
    let f = build_f0();
    assert_eq!(f.state.by_slug("cap-auth"), Some(f.n[0]));
    assert_eq!(f.state.by_slug("nope"), None);
}

#[test]
fn golden_g12_cycle_terminates() {
    let f = build_f0();
    let [n1, n2, ..] = f.n;
    let idg = TestIdGen::starting_at(12);
    let mut b = DeltaBuilder::new(&f.state);
    let e5 = idg.next_uuid();
    b.work
        .insert_edge(Edge::new(e5, "related", n1, n2, Props::new()))
        .unwrap(); // e3(n2→n1)과 사이클
    let hits = b.work.neighbors(n1, Dir::Out, None, None, 1, 3, None);
    assert_eq!(
        hits,
        vec![Hit {
            node: n2,
            via: e5,
            depth: 1
        }]
    ); // 종료·시작노드 제외
}

// ---------- 브랜치 이름 규칙 ----------

/// [핵심] 규칙 표: 영문·숫자·-·_·/ 허용, 빈 문자열·앞뒤 /·// 와 주소 문법 기호는 거부.
#[test]
fn branch_name_rule_table() {
    use vgraph_core::addr::valid_branch_name;
    for ok in [
        "main",
        "dev-b",
        "a_b",
        "feature/x",
        "a/b/c",
        "v1/Feat-2_x",
        "0",
    ] {
        assert!(valid_branch_name(ok).is_ok(), "허용해야 함: {ok:?}");
    }
    for bad in [
        "", "/", "/a", "a/", "a//b", "a b", "a.b", "a@b", "a#b", "한글", "a\x08b", "a\tb", "a\nb",
    ] {
        assert!(valid_branch_name(bad).is_err(), "거부해야 함: {bad:?}");
    }
}

/// [핵심] 같은 위반은 같은 문구로 거부된다(주소 해석 경로와 직접 검사 경로).
#[test]
fn branch_name_violation_message_is_shared() {
    use vgraph_core::addr::valid_branch_name;
    let direct = valid_branch_name("a//b").unwrap_err().to_string();
    let via_addr = "graph://acme/book@a//b"
        .parse::<GraphAddr>()
        .unwrap_err()
        .to_string();
    assert_eq!(direct, via_addr);
    assert!(direct.contains("branch"), "{direct}");
}

/// [핵심] "/"가 든 브랜치 주소: 해석·Display 왕복, 커밋 핀·노드 선택과의 조합.
#[test]
fn branch_with_slash_addr_roundtrip() {
    let h = vgraph_core::canon::sha256(b"c");
    for s in [
        "graph://acme/book@feature/x".to_string(),
        format!("graph://acme/book@feature/x.{h}"),
        "graph://acme/book@a/b#n1".to_string(),
        format!("graph://acme/book@a/b/c.{h}#cap-auth"),
    ] {
        let a: GraphAddr = s.parse().unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_eq!(a.to_string(), s, "왕복");
        assert_eq!(a.ns, "acme");
        assert_eq!(a.slug, "book");
    }
    let a: GraphAddr = format!("graph://acme/book@feature/x.{h}").parse().unwrap();
    assert_eq!(a.branch.as_deref(), Some("feature/x"));
    assert_eq!(a.commit, Some(h));
    let b: GraphAddr = "graph://acme/book@a/b#n1".parse().unwrap();
    assert_eq!(b.branch.as_deref(), Some("a/b"));
    assert!(b.node.is_some());
}

/// [핵심] 주소로 들어온 규칙 밖 브랜치와 ns·slug 규칙(변경 없음).
#[test]
fn branch_with_bad_shape_in_addr_is_rejected_and_ns_slug_rules_unchanged() {
    for s in [
        "graph://acme/book@a//b",
        "graph://acme/book@/a",
        "graph://acme/book@a/",
        "graph://acme/book@",
        "graph://acme/book@a b",
    ] {
        assert!(s.parse::<GraphAddr>().is_err(), "거부해야 함: {s}");
    }
    // ns·slug 에는 "/" 를 허용하지 않는다
    assert!("graph://ac/me/book".parse::<GraphAddr>().is_err());
    assert!("graph://acme/bo/ok@main".parse::<GraphAddr>().is_err());
}
