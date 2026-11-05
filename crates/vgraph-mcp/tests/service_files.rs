//! `vgraph-mcp service install --print <운영체제>` — 등록 내용을 만들어 **출력만** 한다(파일·레지스트리를
//! 건드리지 않는다). 그 출력을 골든으로 고정한다: 실행 경로·인자·로그 경로·자동 재시작 설정,
//! 토큰이 등록 내용에 평문으로 들어가지 않는 것, `--token` 거부, 실기 미검증 표시.
//!
//! 바이너리 크레이트라 시험이 내부 함수를 직접 부를 수 없어, 실제 실행 파일을 임시 폴더 안에서만 돌린다.
//! 이 파일의 어떤 시험도 사용자 홈·레지스트리를 쓰지 않는다.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_vgraph-mcp");
const SECRET: &str = "TOP-SECRET-TOKEN-9f31";
const DIR: &str = "<DIR>";
const EXE: &str = "<EXE>";

struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = TempDir::new().unwrap();
        // 공백이 든 경로에서도 인용이 맞는지 보려고 하위 폴더 이름에 공백을 둔다.
        let base = tmp.path().join("my data");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("token.txt"), format!("{SECRET}\n")).unwrap();
        Fixture { tmp }
    }

    fn base(&self) -> std::path::PathBuf {
        self.tmp.path().join("my data")
    }

    fn root(&self) -> std::path::PathBuf {
        self.base().join("root")
    }

    fn token_file(&self) -> std::path::PathBuf {
        self.base().join("token.txt")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.tmp.path())
            .stdin(Stdio::null())
            .output()
            .expect("vgraph-mcp 실행")
    }

    /// `service install --print <target>` 을 기본 인자와 함께 실행한다.
    fn print(&self, target: &str, extra: &[&str]) -> Output {
        let root = self.root();
        let tf = self.token_file();
        let mut args = vec![
            "service",
            "install",
            "--print",
            target,
            "--root",
            root.to_str().unwrap(),
            "--user",
            "alice",
            "--token-file",
            tf.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// 임시 폴더와 실행 파일 경로를 자리표시자로 바꾸고 `\`를 `/`로 맞춘다(운영체제와 무관한 골든).
    /// Windows 에서 `--print linux`를 돌리면 Windows 경로의 역슬래시 때문에 systemd 인용이 실행 파일에도 붙고
    /// 역슬래시가 두 배가 된다. 그 호스트 차이만 여기서 지운다(Linux 호스트에서는 아무 일도 하지 않는다).
    fn normalize(&self, s: &str) -> String {
        let slash = |t: &str| t.replace("\\\\", "/").replace('\\', "/");
        s.replace("\\\\", "/")
            .replace('\\', "/")
            .replace(&slash(self.tmp.path().to_str().unwrap()), DIR)
            .replace(&slash(BIN), EXE)
            .replace(
                &format!("\"{EXE}\" serve --root"),
                &format!("{EXE} serve --root"),
            )
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_ok(o: &Output) {
    assert!(
        o.status.success(),
        "실패: {:?}\nstdout={}\nstderr={}",
        o.status,
        stdout(o),
        stderr(o)
    );
}

// ---------------------------------------------------------------- 골든

#[test]
fn windows_run_value_golden() {
    let f = Fixture::new();
    let o = f.print("windows", &[]);
    assert_ok(&o);
    let expected = format!(
        "\"{EXE}\" serve --detach --root \"{DIR}/my data/root\" --user alice --embedder none \
--chunker whole --listen 127.0.0.1:7437 --token-file \"{DIR}/my data/token.txt\" \
--log-file \"{DIR}/my data/root/server.log\"\n"
    );
    assert_eq!(f.normalize(&stdout(&o)), expected);
}

#[test]
fn windows_run_value_carries_every_serve_option() {
    let f = Fixture::new();
    let o = f.print(
        "windows",
        &[
            "--embedder",
            "toy:8",
            "--embed-fields",
            "title,body",
            "--chunker",
            "props",
            "--no-embed-fallback",
            "--listen",
            "127.0.0.1:9000",
        ],
    );
    assert_ok(&o);
    let out = f.normalize(&stdout(&o));
    for needle in [
        "--embedder toy:8",
        "--embed-fields title,body",
        "--chunker props",
        "--no-embed-fallback",
        "--listen 127.0.0.1:9000",
        "--detach",
    ] {
        assert!(out.contains(needle), "{needle:?} 없음: {out}");
    }
}

#[test]
fn macos_launch_agent_golden() {
    let f = Fixture::new();
    let o = f.print("macos", &[]);
    assert_ok(&o);
    let log = format!("{DIR}/my data/root/server.log");
    let expected = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>vgraph-mcp</string>
  <key>ProgramArguments</key>
  <array>
    <string>{EXE}</string>
    <string>serve</string>
    <string>--root</string>
    <string>{DIR}/my data/root</string>
    <string>--user</string>
    <string>alice</string>
    <string>--embedder</string>
    <string>none</string>
    <string>--chunker</string>
    <string>whole</string>
    <string>--listen</string>
    <string>127.0.0.1:7437</string>
    <string>--token-file</string>
    <string>{DIR}/my data/token.txt</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    );
    assert_eq!(f.normalize(&stdout(&o)), expected);
}

#[test]
fn linux_systemd_unit_golden() {
    let f = Fixture::new();
    let o = f.print("linux", &[]);
    assert_ok(&o);
    let log = format!("{DIR}/my data/root/server.log");
    let expected = format!(
        "[Unit]
Description=vgraph-mcp resident server

[Service]
Type=simple
ExecStart={EXE} serve --root \"{DIR}/my data/root\" --user alice --embedder none --chunker whole \
--listen 127.0.0.1:7437 --token-file \"{DIR}/my data/token.txt\"
Restart=on-failure
RestartSec=5
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
"
    );
    assert_eq!(f.normalize(&stdout(&o)), expected);
}

#[test]
fn explicit_log_file_is_used_by_every_target() {
    let f = Fixture::new();
    let log = f.base().join("custom.log");
    let log_s = log.to_str().unwrap().to_string();
    for target in ["windows", "macos", "linux"] {
        let o = f.print(target, &["--log-file", &log_s]);
        assert_ok(&o);
        let out = f.normalize(&stdout(&o));
        assert!(
            out.contains(&format!("{DIR}/my data/custom.log")),
            "{target}: {out}"
        );
        assert!(
            !out.contains("root/server.log"),
            "{target}: 기본 로그 경로가 남음: {out}"
        );
    }
}

#[test]
fn relative_paths_become_absolute() {
    let f = Fixture::new();
    // 시험 프로세스의 현재 폴더는 임시 폴더다. 상대 경로 --root / --token-file 이 절대 경로로 바뀌어야 한다.
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "linux",
        "--root",
        "rel-root",
        "--token-file",
        "my data/token.txt",
    ]);
    assert_ok(&o);
    let out = f.normalize(&stdout(&o));
    assert!(out.contains(&format!("{DIR}/rel-root")), "{out}");
    assert!(
        out.contains(&format!("--token-file \"{DIR}/my data/token.txt\"")),
        "{out}"
    );
}

#[test]
fn xml_special_characters_are_escaped_in_plist() {
    let f = Fixture::new();
    let root = f.root();
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "macos",
        "--root",
        root.to_str().unwrap(),
        "--user",
        "a&b<c>",
    ]);
    assert_ok(&o);
    let out = stdout(&o);
    assert!(out.contains("<string>a&amp;b&lt;c&gt;</string>"), "{out}");
    assert!(!out.contains("a&b<c>"), "{out}");
}

#[test]
fn systemd_percent_and_dollar_are_doubled() {
    let f = Fixture::new();
    let root = f.base().join("100%$x");
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "linux",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_ok(&o);
    let out = stdout(&o);
    assert!(out.contains("100%%$$x"), "{out}");
}

// ---------------------------------------------------------------- 토큰

#[test]
fn token_value_never_appears_in_any_target() {
    let f = Fixture::new();
    for target in ["windows", "macos", "linux"] {
        let o = f.print(target, &[]);
        assert_ok(&o);
        let all = format!("{}{}", stdout(&o), stderr(&o));
        assert!(!all.contains(SECRET), "{target}: 토큰 값이 출력에 있음");
        assert!(
            all.contains("token.txt"),
            "{target}: 토큰 파일 경로는 있어야 함: {all}"
        );
    }
}

#[test]
fn install_with_token_is_refused_and_points_to_token_file() {
    let f = Fixture::new();
    let root = f.root();
    let value = "s3cr3t-VALUE-77";
    for target in ["windows", "macos", "linux"] {
        let o = f.run(&[
            "service",
            "install",
            "--print",
            target,
            "--root",
            root.to_str().unwrap(),
            "--token",
            value,
        ]);
        assert!(!o.status.success(), "{target}: --token 이 받아들여짐");
        let err = stderr(&o);
        assert!(err.contains("--token-file"), "{target}: 안내 없음: {err}");
        assert!(stdout(&o).is_empty(), "{target}: 거부했는데 출력이 있음");
        assert!(
            !format!("{}{}", stdout(&o), err).contains(value),
            "{target}: 오류에 토큰 값이 있음"
        );
    }
}

#[test]
fn missing_token_file_is_an_error() {
    let f = Fixture::new();
    let root = f.root();
    let missing = f.base().join("nope.txt");
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "linux",
        "--root",
        root.to_str().unwrap(),
        "--token-file",
        missing.to_str().unwrap(),
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("토큰 파일"), "{}", stderr(&o));
}

// ---------------------------------------------------------------- 표시 · 부작용 없음

#[test]
fn help_and_output_say_macos_linux_are_unverified() {
    let f = Fixture::new();
    for args in [
        &["service", "--help"][..],
        &["service", "install", "--help"][..],
    ] {
        let o = f.run(args);
        assert_ok(&o);
        assert!(
            stdout(&o).contains("실기 미검증"),
            "{args:?}: {}",
            stdout(&o)
        );
    }
    for target in ["macos", "linux"] {
        let o = f.print(target, &[]);
        assert_ok(&o);
        assert!(
            stderr(&o).contains("실기 미검증"),
            "{target}: {}",
            stderr(&o)
        );
    }
}

#[test]
fn print_writes_nothing() {
    let f = Fixture::new();
    for target in ["windows", "macos", "linux"] {
        assert_ok(&f.print(target, &[]));
    }
    assert!(!f.root().exists(), "--print 가 데이터 폴더를 만들었다");
    assert_dir_has_only(&f.base(), &["token.txt"]);
}

fn assert_dir_has_only(dir: &Path, names: &[&str]) {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, names, "{} 안에 예상 밖의 파일", dir.display());
}

#[test]
fn default_registration_name_is_the_product_name() {
    // 출력에 등록 이름이 드러나는 것은 macOS 의 launchd 라벨뿐이다(Windows 값 이름·systemd 단위 이름은
    // 출력이 아니라 등록 위치에 쓰인다). 기본 라벨이 제품 이름과 정확히 같아야 한다.
    let f = Fixture::new();
    for target in ["windows", "linux"] {
        let o = f.print(target, &[]);
        assert_ok(&o);
        assert!(!stdout(&o).trim().is_empty(), "{target}: 출력이 비었다");
    }
    let o = f.print("macos", &[]);
    assert_ok(&o);
    assert_eq!(launchd_label(&stdout(&o)), "vgraph-mcp");
}

#[test]
fn explicit_entry_name_is_used_exactly_as_the_label() {
    let f = Fixture::new();
    let o = f.print("macos", &["--entry-name", "custom-entry_1.x"]);
    assert_ok(&o);
    assert_eq!(launchd_label(&stdout(&o)), "custom-entry_1.x");
}

/// plist 의 `<key>Label</key>` 바로 다음 `<string>` 값.
fn launchd_label(plist: &str) -> String {
    let after = plist
        .split("<key>Label</key>")
        .nth(1)
        .unwrap_or_else(|| panic!("Label 키가 없다: {plist}"));
    let value = after
        .split("<string>")
        .nth(1)
        .and_then(|s| s.split("</string>").next())
        .unwrap_or_else(|| panic!("Label 값이 없다: {plist}"));
    value.to_string()
}

#[test]
fn bad_entry_name_is_refused() {
    let f = Fixture::new();
    let root = f.root();
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "linux",
        "--root",
        root.to_str().unwrap(),
        "--entry-name",
        "bad name/..",
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("등록 이름"), "{}", stderr(&o));
}

#[test]
fn entry_name_changes_label_and_unit_name_in_output() {
    let f = Fixture::new();
    let root = f.root();
    let o = f.run(&[
        "service",
        "install",
        "--print",
        "macos",
        "--root",
        root.to_str().unwrap(),
        "--entry-name",
        "vgraph-mcp-test",
    ]);
    assert_ok(&o);
    assert!(
        stdout(&o).contains("<string>vgraph-mcp-test</string>"),
        "{}",
        stdout(&o)
    );
}

// ---------------------------------------------------------------- serve 옵션(더하기만)

#[test]
fn serve_log_file_without_detach_is_refused() {
    let f = Fixture::new();
    let root = f.root();
    let log = f.base().join("x.log");
    let o = f.run(&[
        "serve",
        "--root",
        root.to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
        "--log-file",
        log.to_str().unwrap(),
    ]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("--detach"), "{}", stderr(&o));
    assert!(!root.exists(), "거절했는데 데이터 폴더가 만들어짐");
}
