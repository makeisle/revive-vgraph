//! K6 — 시드 고정 무작위 편집 시퀀스: fold ≡ 증분, 가역 왕복, canonical 안정성.
//! (골든 TC K6: seed=42, 20 커밋. proptest 대신 자체 xorshift — 의존성 0, 완전 결정적)

use vgraph_core::fixtures::build_f0;
use vgraph_core::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn rand_value(rng: &mut Rng) -> Value {
    match rng.below(5) {
        0 => Value::Int(rng.below(100) as i64),
        1 => Value::Str(format!("s{}", rng.below(10))),
        2 => Value::Float(rng.below(100) as f64 / 4.0),
        3 => Value::Bool(rng.below(2) == 0),
        _ => Value::List(vec![Value::Int(1), Value::Str("x".into())]),
    }
}

#[test]
fn k6_fold_equals_incremental_seed42() {
    let mut rng = Rng(42);
    let idg = TestIdGen::starting_at(100);
    let keys = ["a", "b", "c"];
    let labels = ["X", "Y"];

    let mut cur = build_f0().state;

    for round in 0..20 {
        let mut b = DeltaBuilder::new(&cur);
        let ops = 1 + rng.below(8);
        for _ in 0..ops {
            let nodes: Vec<Uuid> = b.work.nodes().map(|n| n.uuid).collect();
            let edges: Vec<Uuid> = b.work.edges().map(|e| e.uuid).collect();
            match rng.below(9) {
                0 => {
                    // add node (slug = uuid 문자열 → 전역 유일)
                    let u = idg.next_uuid();
                    let lab = labels[rng.below(2)];
                    let slug = format!("s-{u}");
                    let mut p = Props::new();
                    for _ in 0..rng.below(3) {
                        p.insert(keys[rng.below(3)].into(), rand_value(&mut rng));
                    }
                    b.work
                        .insert_node(Node::new(u, &[lab], Some(&slug), p))
                        .unwrap();
                }
                1 => {
                    if !nodes.is_empty() {
                        let u = nodes[rng.below(nodes.len())];
                        let _ = b.work.remove_node(u, true);
                    }
                }
                2 => {
                    if !nodes.is_empty() {
                        let u = idg.next_uuid();
                        let from = nodes[rng.below(nodes.len())];
                        let to = nodes[rng.below(nodes.len())];
                        b.work
                            .insert_edge(Edge::new(u, "rel", from, to, Props::new()))
                            .unwrap();
                    }
                }
                3 => {
                    if !edges.is_empty() {
                        let _ = b.work.remove_edge(edges[rng.below(edges.len())]);
                    }
                }
                4 => {
                    let all: Vec<Uuid> = nodes.iter().chain(edges.iter()).copied().collect();
                    if !all.is_empty() {
                        let elem = all[rng.below(all.len())];
                        let v = rand_value(&mut rng);
                        let _ = b.work.set_prop(elem, keys[rng.below(3)], v);
                    }
                }
                5 => {
                    let all: Vec<Uuid> = nodes.iter().chain(edges.iter()).copied().collect();
                    if !all.is_empty() {
                        let _ = b
                            .work
                            .unset_prop(all[rng.below(all.len())], keys[rng.below(3)]);
                    }
                }
                6 => {
                    if !nodes.is_empty() {
                        let _ = b
                            .work
                            .add_label(nodes[rng.below(nodes.len())], labels[rng.below(2)]);
                    }
                }
                7 => {
                    if !nodes.is_empty() {
                        let _ = b
                            .work
                            .remove_label(nodes[rng.below(nodes.len())], labels[rng.below(2)]);
                    }
                }
                _ => {
                    if !nodes.is_empty() {
                        let u = nodes[rng.below(nodes.len())];
                        let slug = format!("r{}-{u}", round);
                        let _ = b.work.set_slug(u, Some(&slug));
                    }
                }
            }
        }

        let d = b.build();

        // ① 증분 적용 ≡ work (fold 항등 — §설계 6-5)
        let mut applied = cur.clone();
        apply(&mut applied, &d).expect("apply must succeed");
        assert_eq!(applied, b.work, "round {round}: apply(base, diff) != work");

        // ② 가역 왕복 (D4 일반화)
        let mut back = applied.clone();
        apply(&mut back, &d.invert()).expect("inverse apply must succeed");
        assert_eq!(back, cur, "round {round}: invert roundtrip failed");

        // ③ canonical 안정성: 재-diff 동일, json 왕복 동일
        assert_eq!(
            d.canonical_json(),
            b.build().canonical_json(),
            "round {round}: diff nondeterministic"
        );
        let d2 = Delta::from_json(&d.canonical_json()).expect("parse");
        assert_eq!(
            d2.canonical_json(),
            d.canonical_json(),
            "round {round}: json roundtrip"
        );

        cur = b.work;
    }
}
