//! 사람(CLI)과 에이전트(MCP) 사이의 JSON 왕복 — export·import.
//!
//! CLI 는 실제 바이너리(`CARGO_BIN_EXE_vgraph`)를 프로세스로 실행하고, MCP 는 같은 프로세스 안에서
//! `McpServer::handle_message`(라이브러리 직접 호출)로 부른다. 모킹 없음 — 저장소는 임시 디렉터리.

use serde_json::{json, Value as J};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;
use vgraph_mcp::{McpConfig, McpServer};

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vgraph"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("vgraph 실행")
}

fn cli_ok(root: &Path, args: &[&str]) -> String {
    let o = cli(root, args);
    assert!(
        o.status.success(),
        "vgraph {args:?} 실패: stdout={} stderr={}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn cli_err(root: &Path, args: &[&str]) -> String {
    let o = cli(root, args);
    assert!(
        !o.status.success(),
        "실패를 기대했는데 성공: vgraph {args:?}"
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn mcp(root: &Path) -> McpServer {
    McpServer::new(McpConfig {
        root: root.to_path_buf(),
        user: "local".into(),
        embedder: None,
        embed: Default::default(),
    })
    .expect("mcp 서버")
}

fn call(srv: &McpServer, name: &str, args: J) -> J {
    let resp = srv
        .handle_message(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args } }))
        .expect("response");
    let r = &resp["result"];
    assert!(
        !r["isError"].as_bool().unwrap_or(false),
        "tool error: {}",
        r["content"][0]["text"]
    );
    serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// CLI 로 그래프를 만들어 커밋한다 (노드 3, 엣지 2).
fn build_with_cli(root: &Path, name: &str) {
    cli_ok(root, &["init", name]);
    cli_ok(
        root,
        &[
            "add-node",
            name,
            "--label",
            "Item",
            "--slug",
            "item-a",
            "--prop",
            "name=항목 A",
            "--prop",
            "n=3",
        ],
    );
    cli_ok(
        root,
        &[
            "add-node",
            name,
            "--label",
            "Item",
            "--label",
            "Rule",
            "--slug",
            "item-b",
            "--prop",
            "ratio=1.5",
        ],
    );
    cli_ok(
        root,
        &["add-node", name, "--label", "Doc", "--slug", "doc-1"],
    );
    cli_ok(
        root,
        &[
            "add-edge", name, "--etype", "참조", "--from", "item-a", "--to", "item-b",
        ],
    );
    cli_ok(
        root,
        &[
            "add-edge", name, "--etype", "참조", "--from", "item-b", "--to", "doc-1",
        ],
    );
    cli_ok(root, &["commit", name, "-m", "seed"]);
}

/// [핵심] CLI export 로 쓴 파일을 MCP vgraph_import 가 읽고, 다시 내보내면 바이트가 같다.
#[test]
fn cli_export_file_is_importable_by_mcp_and_roundtrips() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    build_with_cli(src.path(), "acme/catalog");
    let file = src.path().join("catalog.json");
    let out = cli_ok(
        src.path(),
        &["export", "acme/catalog", "--out", file.to_str().unwrap()],
    );
    assert!(
        out.contains("노드 3") && out.contains("엣지 2"),
        "요약 출력: {out}"
    );

    let srv = mcp(dst.path());
    call(&srv, "vgraph_create", json!({ "name": "acme/copy" }));
    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/copy", "path": file.to_str().unwrap() }),
    );
    assert_eq!(r["outcome"], "committed");
    assert_eq!(
        (r["nodes"].clone(), r["edges"].clone()),
        (json!(3), json!(2))
    );

    let back = dst.path().join("back.json");
    call(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/copy", "path": back.to_str().unwrap() }),
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(&back).unwrap(),
        "왕복 바이트 동일"
    );
}

/// 반대 방향: MCP 가 쓴 파일을 CLI import 가 읽는다. 표준 출력 내보내기도 같은 바이트.
#[test]
fn mcp_export_file_is_importable_by_cli() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let srv = mcp(a.path());
    call(&srv, "vgraph_create", json!({ "name": "acme/catalog" }));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/catalog", "ops": [
        { "op": "add_node", "labels": ["Item"], "slug": "x", "props": { "name": "엑스" } },
        { "op": "add_node", "labels": ["Rule"], "slug": "y", "props": {} },
        { "op": "add_edge", "etype": "관련", "from": "x", "to": "y" } ] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/catalog", "message": "m" }),
    );
    let file = a.path().join("m.json");
    call(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/catalog", "path": file.to_str().unwrap() }),
    );

    cli_ok(b.path(), &["init", "acme/copy"]);
    let out = cli_ok(
        b.path(),
        &["import", "acme/copy", "--file", file.to_str().unwrap()],
    );
    assert!(out.contains("노드 2") && out.contains("엣지 1"), "{out}");
    let log = cli_ok(b.path(), &["log", "acme/copy"]);
    assert_eq!(log.lines().count(), 1, "가져오기 1회 = 커밋 1개: {log}");

    let stdout_doc = cli_ok(b.path(), &["export", "acme/copy"]);
    assert_eq!(
        stdout_doc.as_bytes(),
        std::fs::read(&file).unwrap().as_slice(),
        "표준 출력 내보내기"
    );
}

/// 인라인 JSON(--json) 가져오기와 거부 경로: 끝 없는 엣지, 비어 있지 않은 그래프, 인자 오류.
#[test]
fn cli_import_inline_and_rejections() {
    let root = TempDir::new().unwrap();
    cli_ok(root.path(), &["init", "acme/a"]);
    let bad = r#"{"format":"vgraph-json/1","nodes":[{"slug":"x","labels":[],"props":{}}],"edges":[{"etype":"t","from":"x","to":"없는-slug","props":{}}]}"#;
    let e = cli_err(root.path(), &["import", "acme/a", "--json", bad]);
    assert!(e.contains("없는-slug"), "{e}");
    // 거부된 가져오기는 커밋을 남기지 않는다 — 아직 커밋이 없어 log 가 실패한다
    assert!(!cli(root.path(), &["log", "acme/a"]).status.success());

    let good = r#"{"format":"vgraph-json/1","nodes":[{"slug":"x","labels":["Item"],"props":{}}],"edges":[]}"#;
    let out = cli_ok(
        root.path(),
        &["import", "acme/a", "--json", good, "-m", "인라인"],
    );
    assert!(out.contains("노드 1"), "{out}");
    let log = cli_ok(root.path(), &["log", "acme/a"]);
    assert!(log.contains("인라인"), "{log}");

    let e = cli_err(root.path(), &["import", "acme/a", "--json", good]);
    assert!(e.contains("빈 그래프"), "{e}");
    let e = cli_err(root.path(), &["import", "acme/a"]);
    assert!(e.contains("--file") && e.contains("--json"), "{e}");
    let e = cli_err(
        root.path(),
        &["import", "acme/a", "--json", good, "--mode", "bogus"],
    );
    assert!(e.contains("replace"), "{e}");
    let e = cli_err(root.path(), &["export", "acme/none"]);
    assert!(!e.is_empty());
}

/// slug 없는 노드는 CLI 에서도 내보내기가 거부된다.
#[test]
fn cli_export_rejects_slugless_nodes() {
    let root = TempDir::new().unwrap();
    cli_ok(root.path(), &["init", "acme/s"]);
    cli_ok(root.path(), &["add-node", "acme/s", "--label", "Item"]);
    cli_ok(root.path(), &["commit", "acme/s", "-m", "m"]);
    let e = cli_err(root.path(), &["export", "acme/s"]);
    assert!(e.contains("slugless_node"), "{e}");
}

fn doc_json(a_name: &str, with_c: bool) -> String {
    let c = if with_c {
        r#",{"slug":"c","labels":["Rule"],"props":{}}"#
    } else {
        ""
    };
    format!(
        r#"{{"format":"vgraph-json/1","nodes":[{{"slug":"a","labels":["Item"],"props":{{"name":"{a_name}"}}}},{{"slug":"b","labels":["Item"],"props":{{}}}}{c}],"edges":[{{"etype":"t","from":"a","to":"b","props":{{}}}}]}}"#
    )
}

/// [핵심] merge 모드: 첫 가져오기 → 문서 변경 반영(fast-forward) → 같은 문서는 이미 최신.
#[test]
fn cli_import_merge_first_then_sync_then_up_to_date() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    cli_ok(r, &["init", "acme/m"]);
    let out = cli_ok(
        r,
        &[
            "import",
            "acme/m",
            "--mode",
            "merge",
            "--via-branch",
            "import",
            "--json",
            &doc_json("A", false),
        ],
    );
    assert!(
        out.contains("첫 가져오기") && out.contains("import"),
        "{out}"
    );
    let out = cli_ok(
        r,
        &[
            "import",
            "acme/m",
            "--mode",
            "merge",
            "--via-branch",
            "import",
            "--json",
            &doc_json("A2", true),
        ],
    );
    assert!(out.contains("fast-forward"), "{out}");
    assert!(cli_ok(r, &["export", "acme/m"]).contains("\"name\":\"A2\""));
    let out = cli_ok(
        r,
        &[
            "import",
            "acme/m",
            "--mode",
            "merge",
            "--via-branch",
            "import",
            "--json",
            &doc_json("A2", true),
        ],
    );
    assert!(out.contains("이미 최신"), "{out}");
}

/// [핵심] 충돌: 대상은 그대로, 해소 방법 안내가 나오고, 기존 merge 명령(--theirs/--ours)으로 해소된다.
#[test]
fn cli_import_merge_conflict_is_reported_and_resolved_with_merge_command() {
    for pick in ["--theirs", "--ours"] {
        let root = TempDir::new().unwrap();
        let r = root.path();
        cli_ok(r, &["init", "acme/m"]);
        let merge_import = |doc: &str| {
            cli_ok(
                r,
                &[
                    "import",
                    "acme/m",
                    "--mode",
                    "merge",
                    "--via-branch",
                    "import",
                    "--json",
                    doc,
                ],
            )
        };
        merge_import(&doc_json("A", false));
        cli_ok(r, &["set", "acme/m#a", "name=T"]);
        cli_ok(r, &["commit", "acme/m", "-m", "direct"]);
        let head = cli_ok(r, &["log", "acme/m"]);

        let out = merge_import(&doc_json("D", false));
        assert!(
            out.contains("충돌 1건") && out.contains("vgraph merge") && out.contains("--theirs"),
            "{out}"
        );
        assert!(out.contains("번호로 풀 수 없"), "{out}");
        assert_eq!(cli_ok(r, &["log", "acme/m"]), head, "대상 브랜치 불변");

        let out = cli_ok(r, &["merge", "acme/m", "import", "main", pick, "0"]);
        assert!(out.contains("merged"), "{out}");
        let want = if pick == "--theirs" {
            "\"name\":\"D\""
        } else {
            "\"name\":\"T\""
        };
        assert!(cli_ok(r, &["export", "acme/m"]).contains(want));
    }
}

/// 인자 오류: merge 에는 via-branch 필요, replace 에서는 쓰지 않음, 첫 가져오기인데 대상이 비어 있지 않음.
#[test]
fn cli_import_merge_argument_errors() {
    let root = TempDir::new().unwrap();
    let r = root.path();
    cli_ok(r, &["init", "acme/m"]);
    let doc = doc_json("A", false);
    let e = cli_err(r, &["import", "acme/m", "--mode", "merge", "--json", &doc]);
    assert!(e.contains("--via-branch"), "{e}");
    let e = cli_err(
        r,
        &["import", "acme/m", "--via-branch", "import", "--json", &doc],
    );
    assert!(e.contains("--via-branch") && e.contains("merge"), "{e}");
    cli_ok(r, &["add-node", "acme/m", "--label", "Item", "--slug", "x"]);
    cli_ok(r, &["commit", "acme/m", "-m", "x"]);
    let e = cli_err(
        r,
        &[
            "import",
            "acme/m",
            "--mode",
            "merge",
            "--via-branch",
            "import",
            "--json",
            &doc,
        ],
    );
    assert!(e.contains("기준점"), "{e}");
}
