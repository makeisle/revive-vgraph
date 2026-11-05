//! 이름 있는 작업 공간(`--workspace`) — 실제 바이너리를 프로세스로 실행한다. 저장소는 임시 디렉터리.
//! 같은 사용자·같은 브랜치라도 이름이 다르면 편집이 섞이지 않고, 커밋 때 합쳐지거나 충돌로 보고된다.

use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vgraph"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("vgraph 실행")
}

fn ok(root: &Path, args: &[&str]) -> String {
    let o = cli(root, args);
    assert!(
        o.status.success(),
        "vgraph {args:?} 실패: stdout={} stderr={}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn fail(root: &Path, args: &[&str]) -> String {
    let o = cli(root, args);
    assert!(
        !o.status.success(),
        "실패를 기대했는데 성공: vgraph {args:?}"
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const G: &str = "acme/catalog";

/// 노드 두 개(n1·n2)를 만들어 커밋한 저장소.
fn seeded() -> TempDir {
    let dir = TempDir::new().unwrap();
    let r = dir.path();
    ok(r, &["init", G]);
    for slug in ["n1", "n2"] {
        ok(
            r,
            &[
                "add-node", G, "--label", "Item", "--slug", slug, "--prop", "v=0",
            ],
        );
    }
    ok(r, &["commit", G, "-m", "seed"]);
    dir
}

#[test]
fn workspaces_are_isolated_and_both_land() {
    let dir = seeded();
    let r = dir.path();
    ok(r, &["--workspace", "a", "set", &format!("{G}#n1"), "v=1"]);
    ok(r, &["--workspace", "b", "set", &format!("{G}#n2"), "v=2"]);

    // 서로의 pending 에 상대 편집이 보이지 않는다 (각자 prop 1건)
    let pa = ok(r, &["--workspace", "a", "pending", G]);
    let pb = ok(r, &["--workspace", "b", "pending", G]);
    assert!(pa.contains("prop 1"), "a pending: {pa}");
    assert!(pb.contains("prop 1"), "b pending: {pb}");
    // 기본 작업 공간에는 편집이 없다
    assert!(ok(r, &["pending", G]).contains("working copy 없음"));

    ok(r, &["--workspace", "a", "commit", G, "-m", "a"]);
    ok(r, &["--workspace", "b", "commit", G, "-m", "b"]);

    let file = r.join("out.json");
    ok(r, &["export", G, "--out", file.to_str().unwrap()]);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let v_of = |slug: &str| {
        doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["slug"] == slug)
            .map(|n| n["props"]["v"].clone())
    };
    assert_eq!(v_of("n1"), Some(serde_json::json!(1)), "doc: {doc}");
    assert_eq!(v_of("n2"), Some(serde_json::json!(2)), "doc: {doc}");
}

#[test]
fn same_prop_in_two_workspaces_reports_conflict() {
    let dir = seeded();
    let r = dir.path();
    ok(r, &["--workspace", "a", "set", &format!("{G}#n1"), "v=1"]);
    ok(r, &["--workspace", "b", "set", &format!("{G}#n1"), "v=2"]);
    ok(r, &["--workspace", "a", "commit", G, "-m", "a"]);
    let out = ok(r, &["--workspace", "b", "commit", G, "-m", "b"]);
    assert!(
        out.contains("충돌") || out.to_lowercase().contains("conflict"),
        "got: {out}"
    );
}

#[test]
fn invalid_workspace_name_is_rejected_with_natural_message() {
    let dir = seeded();
    let e = fail(dir.path(), &["--workspace", "a b", "pending", G]);
    assert!(
        e.contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
        "got: {e}"
    );
    let e = fail(dir.path(), &["--workspace", "", "pending", G]);
    assert!(
        e.contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
        "got: {e}"
    );
}

#[test]
fn help_explains_workspace_usage_and_risk() {
    let out = ok(Path::new("."), &["--help"]);
    for needle in [
        "--workspace",
        "각자 다른 --workspace 이름",
        "이름 없이(기본 작업 공간) 동시에 편집하면 편집 내용이 사라질 수 있습니다",
        "동시에 쓰는 프로세스마다 고유",
        "같은 이름을 동시에 쓰면",
        "자동으로 합쳐지고",
        "충돌로 보고",
    ] {
        assert!(out.contains(needle), "--help 에 없음: {needle}\n{out}");
    }
}

/// [핵심] "/"가 든 브랜치: 만들기 → 주소로 조회·편집·커밋 → 병합. 규칙 밖 이름은 만들 때 거부.
#[test]
fn slash_branch_flow_and_invalid_names_rejected() {
    let dir = seeded();
    let r = dir.path();
    ok(r, &["branch", G, "feature/x"]);
    let at = format!("{G}@feature/x");
    let out = ok(r, &["show", &format!("{at}#n1")]);
    assert!(out.contains("[Item]"), "got: {out}");
    ok(r, &["set", &format!("{at}#n1"), "v=7"]);
    ok(r, &["commit", &at, "-m", "x"]);
    let lines = |addr: &str| ok(r, &["log", addr]).lines().count();
    assert_eq!(lines(&at), 2, "feature/x 에 커밋이 쌓임");
    assert_eq!(lines(G), 1, "main 은 그대로");
    let m = ok(r, &["merge", G, "feature/x", "main"]);
    assert!(m.contains("fast-forward"), "got: {m}");
    assert_eq!(lines(G), 2, "병합 뒤 main 이 따라옴");

    for bad in ["a//b", "/a", "a/", "a b", "a.b"] {
        let e = fail(r, &["branch", G, bad]);
        assert!(e.contains("branch") && e.contains('/'), "{bad:?}: {e}");
    }
}
