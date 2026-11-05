//! `vgraph-mcp service` — 서버(`serve`)를 사용자 로그인 때 자동으로 띄우도록 등록·해제·확인한다. 기능 `server` 뒤에 있다.
//!
//! - 등록은 **사용자당 하나**다. 관리자 권한이 필요한 시스템 서비스 등록은 만들지 않는다.
//! - Windows: 현재 사용자 시작 항목 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. 로그인 때 콘솔 창이
//!   뜨지 않게 등록 명령은 `serve --detach`(창 없는 자식 프로세스로 다시 띄우고 끝남)이고, 로그는 파일에 남는다.
//! - macOS: `~/Library/LaunchAgents/<이름>.plist` + `launchctl`. Linux: `~/.config/systemd/user/<이름>.service` +
//!   `systemctl --user`. 이 두 곳은 관리자(launchd·systemd)가 프로세스를 직접 지키므로 `--detach` 없이 `serve`를 등록한다.
//! - 토큰은 등록 항목에 값으로 넣지 않는다 — `--token-file` 경로만 들어간다.
//! - 등록 내용을 만드는 함수(`windows_command`·`launch_agent`·`systemd_unit`)는 입출력이 없는 순수 함수다.
//!   `install --print <운영체제>`는 그 결과만 출력한다.

use crate::serve::{self, absolute, path_text, ServeSettings, LOG_FILE};
use anyhow::{ensure, Context};
use clap::{Args, Subcommand, ValueEnum};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

/// 등록 이름의 기본값 — 제품 이름이다 (레지스트리 값 이름 · launchd 라벨 · systemd 단위 이름).
pub const DEFAULT_ENTRY_NAME: &str = "vgraph-mcp";
/// 등록 이름 최대 길이.
const ENTRY_NAME_MAX: usize = 64;
/// systemd 가 오류 종료 뒤 다시 띄우기 전에 기다리는 시간(초).
const RESTART_SECONDS: u32 = 5;
/// 알림: macOS·Linux 등록은 이 개발 환경에서 실기로 확인하지 못했다.
const UNVERIFIED_NOTE: &str =
    "macOS·Linux 등록은 실기 미검증입니다 (Windows만 실기에서 확인했습니다).";

pub const SERVICE_ABOUT: &str = "서버를 로그인할 때 자동으로 띄우도록 등록·해제·확인합니다";
pub const SERVICE_LONG_ABOUT: &str = "\
서버(vgraph-mcp serve)를 현재 사용자가 로그인할 때 자동으로 띄우도록 등록합니다. 등록은 사용자당 하나이고
관리자 권한이 필요 없습니다.

  Windows  현재 사용자 시작 항목(HKCU\\...\\Run). 로그인 때 창이 뜨지 않습니다(serve --detach).
  macOS    ~/Library/LaunchAgents 의 LaunchAgent (launchctl)
  Linux    ~/.config/systemd/user 의 사용자 단위 (systemctl --user)

참고: macOS·Linux 등록은 실기 미검증입니다 (Windows만 실기에서 확인했습니다).

등록할 서버의 설정은 serve 와 같은 인자로 줍니다: vgraph-mcp service install --root <폴더> ...
토큰은 등록 항목에 평문으로 넣지 않습니다 — --token-file 로 파일 경로만 넣습니다.";

const INSTALL_LONG_ABOUT: &str = "\
서버를 등록하고 지금 바로 띄웁니다 (--no-start 이면 등록만). serve 와 같은 인자를 받습니다.
이미 등록돼 있으면 덮어쓰고, 그 서버가 떠 있으면 먼저 끕니다.

경로(--root --token-file --tokens --tls-cert --tls-key --log-file)는 절대 경로로 바뀌어 등록되고, 실행 파일은 지금 실행 중인 vgraph-mcp 입니다
(파일을 옮기면 다시 install 하세요). 비밀 값은 등록하지 않고 파일 경로만 등록합니다: 토큰은 --token-file·--tokens, 인증서·키는
--tls-cert·--tls-key 입니다 (--token 은 거부합니다). --behind-proxy·--allow-origin 도 serve 와 같게 옮겨집니다.
밖으로 여는 주소(--listen)는 serve 와 같은 규칙으로 등록 단계에서 검사합니다(TLS와 --tokens 필수).
--print <windows|macos|linux> 는 등록하지 않고 그 운영체제용 등록 내용만 출력합니다.

참고: macOS·Linux 등록은 실기 미검증입니다 (Windows만 실기에서 확인했습니다).";

#[derive(Subcommand)]
pub enum ServiceCommand {
    #[command(about = "서버를 로그인 때 자동으로 뜨도록 등록한다", long_about = INSTALL_LONG_ABOUT)]
    Install(Box<InstallArgs>),
    /// 서버를 끄고 등록을 지운다 (등록돼 있지 않으면 아무것도 하지 않는다)
    Uninstall(EntryArgs),
    /// 등록 여부와 서버 상태를 보인다. 등록돼 있고 서버가 떠 있을 때만 0, 아니면 1로 끝난다
    Status(EntryArgs),
}

#[derive(Args)]
pub struct InstallArgs {
    #[command(flatten)]
    settings: ServeSettings,
    /// 서버 출력을 남기는 로그 파일 (기본: <데이터 폴더>/server.log)
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// 등록만 하고 지금 서버는 띄우지 않는다
    #[arg(long)]
    no_start: bool,
    /// 등록하지 않고, 해당 운영체제용 등록 내용만 출력한다
    #[arg(long, value_enum, value_name = "OS")]
    print: Option<Target>,
    #[command(flatten)]
    entry: EntryArgs,
}

#[derive(Args)]
pub struct EntryArgs {
    /// 시험용으로 등록 이름을 바꾼다 (제품 기본 이름과 달라야 사용자의 실제 등록을 건드리지 않는다)
    #[arg(long, hide = true, default_value = DEFAULT_ENTRY_NAME)]
    entry_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Windows,
    Macos,
    Linux,
}

impl Target {
    fn host() -> Target {
        if cfg!(windows) {
            Target::Windows
        } else if cfg!(target_os = "macos") {
            Target::Macos
        } else {
            Target::Linux
        }
    }
}

pub fn run(command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install(a) => install(*a),
        ServiceCommand::Uninstall(a) => uninstall(&a.entry_name),
        ServiceCommand::Status(a) => status(&a.entry_name),
    }
}

// ---------------------------------------------------------------- 등록 내용 (순수 함수)

/// 등록에 들어가는 값 전부. 경로는 이미 절대 경로다.
struct Registration<'a> {
    name: &'a str,
    exe: &'a Path,
    /// `serve` 뒤에 붙는 인자 (`--detach`·`--log-file` 제외).
    serve_args: &'a [String],
    log: &'a Path,
}

/// 등록 이름: 파일 이름·라벨·레지스트리 값 이름으로 안전한 문자만.
fn validate_entry_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= ENTRY_NAME_MAX
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ensure!(
        ok,
        "등록 이름은 영문·숫자·-·_·. 만 쓸 수 있고 {ENTRY_NAME_MAX}자 이하여야 합니다 (점으로 시작할 수 없음): {name:?}"
    );
    Ok(())
}

/// Windows 명령줄 인자 하나를 인용한다 (`CommandLineToArgvW` 규칙).
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // 닫는 따옴표 바로 앞의 역슬래시는 따옴표를 이스케이프하지 않도록 두 배로 한다.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// `quote_windows_arg`로 이어 붙인 명령줄을 다시 인자로 나눈다.
fn split_windows_command(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut in_quotes = false;
    let mut backslashes = 0usize;
    for c in line.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            cur.extend(std::iter::repeat_n('\\', backslashes / 2));
            if backslashes % 2 == 1 {
                cur.push('"');
            } else {
                in_quotes = !in_quotes;
            }
            backslashes = 0;
            started = true;
            continue;
        }
        cur.extend(std::iter::repeat_n('\\', backslashes));
        if backslashes > 0 {
            started = true;
        }
        backslashes = 0;
        if c.is_whitespace() && !in_quotes {
            if started {
                args.push(std::mem::take(&mut cur));
                started = false;
            }
        } else {
            cur.push(c);
            started = true;
        }
    }
    cur.extend(std::iter::repeat_n('\\', backslashes));
    if started || backslashes > 0 {
        args.push(cur);
    }
    args
}

/// 등록된 명령의 `--root` 값.
fn root_in_args(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--root")?;
    args.get(at + 1).map(PathBuf::from)
}

/// Windows 시작 항목의 값: 실행 파일은 항상 인용하고, `serve --detach`로 창 없이 띄운다.
fn windows_command(r: &Registration<'_>) -> anyhow::Result<String> {
    let mut parts = vec![format!("\"{}\"", path_text(r.exe)?)];
    let mut args = vec!["serve".to_string(), "--detach".to_string()];
    args.extend(r.serve_args.iter().cloned());
    args.extend(["--log-file".to_string(), path_text(r.log)?.to_string()]);
    parts.extend(args.iter().map(|a| quote_windows_arg(a)));
    Ok(parts.join(" "))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// macOS LaunchAgent plist: 로그인 때 시작하고, 오류로 끝나면 다시 띄운다 (정상 종료 = `stop`은 다시 띄우지 않는다).
fn launch_agent(r: &Registration<'_>) -> anyhow::Result<String> {
    let mut arguments = String::new();
    let all = std::iter::once(path_text(r.exe)?)
        .chain(std::iter::once("serve"))
        .chain(r.serve_args.iter().map(String::as_str));
    for a in all {
        arguments.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let log = xml_escape(path_text(r.log)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
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
"#,
        label = xml_escape(r.name),
    ))
}

/// systemd 명령줄 인자 하나. 공백·따옴표·역슬래시가 있으면 큰따옴표로 감싸고, `%`·`$`는 두 번 쓴다.
fn systemd_quote(arg: &str) -> String {
    let escaped = arg.replace('%', "%%").replace('$', "$$");
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\'', '\\']) {
        return escaped;
    }
    format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
}

/// systemd 사용자 단위: 오류로 끝나면 다시 띄운다 (정상 종료 = `stop`은 다시 띄우지 않는다).
fn systemd_unit(r: &Registration<'_>) -> anyhow::Result<String> {
    let mut exec = vec![systemd_quote(path_text(r.exe)?), "serve".to_string()];
    exec.extend(r.serve_args.iter().map(|a| systemd_quote(a)));
    let log = path_text(r.log)?.replace('%', "%%");
    Ok(format!(
        "[Unit]
Description=vgraph-mcp resident server

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec={RESTART_SECONDS}
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
",
        exec = exec.join(" "),
    ))
}

fn launch_agent_path(home: &Path, name: &str) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{name}.plist"))
}

fn systemd_unit_path(home: &Path, name: &str) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(format!("{name}.service"))
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME 환경 변수가 없어 사용자 폴더를 알 수 없습니다")
}

// ---------------------------------------------------------------- install · uninstall · status

fn install(a: InstallArgs) -> anyhow::Result<()> {
    let name = a.entry.entry_name.as_str();
    validate_entry_name(name)?;
    ensure!(
        !a.settings.has_inline_token(),
        "--token 은 등록할 수 없습니다 (등록 항목에 토큰이 평문으로 남습니다). 토큰을 파일에 적고 --token-file <경로> 를 쓰세요"
    );
    // 바인드 정책(밖으로 열려면 TLS·사용자별 토큰) · 토큰·인증서·키 파일을 읽을 수 있는지.
    // 검사 결과(토큰 값·키)는 쓰지 않고 버린다 — 등록 항목에는 경로만 들어간다.
    a.settings.validate()?;
    let root = a.settings.root_abs()?;
    let serve_args = a.settings.to_args()?;
    let log = match &a.log_file {
        Some(p) => absolute(p)?,
        None => root.join(LOG_FILE),
    };
    let exe = std::env::current_exe().context("실행 파일 경로를 알 수 없습니다")?;
    let reg = Registration {
        name,
        exe: &exe,
        serve_args: &serve_args,
        log: &log,
    };

    if let Some(target) = a.print {
        print!("{}", render(target, &reg)?);
        if target != Target::Windows {
            eprintln!("{UNVERIFIED_NOTE}");
        }
        return Ok(());
    }

    let backend = backend();
    let existing = backend.find(name)?;
    if existing.is_some() {
        println!("'{name}' 은(는) 이미 등록돼 있어 덮어씁니다.");
    }
    backend.write(&reg, existing.as_ref(), !a.no_start)?;
    println!("등록했습니다: {} ('{name}')", backend.place(name)?);
    println!("실행 파일: {}", exe.display());
    if a.no_start {
        println!("--no-start 라서 서버는 띄우지 않았습니다. 다음 로그인 때 시작됩니다.");
    }
    if Target::host() != Target::Windows {
        eprintln!("{UNVERIFIED_NOTE}");
    }
    Ok(())
}

fn render(target: Target, reg: &Registration<'_>) -> anyhow::Result<String> {
    Ok(match target {
        Target::Windows => format!("{}\n", windows_command(reg)?),
        Target::Macos => launch_agent(reg)?,
        Target::Linux => systemd_unit(reg)?,
    })
}

fn uninstall(name: &str) -> anyhow::Result<()> {
    validate_entry_name(name)?;
    let backend = backend();
    let Some(existing) = backend.find(name)? else {
        println!("등록되어 있지 않습니다 ('{name}'). 할 일이 없습니다.");
        return Ok(());
    };
    backend.remove(name, &existing)?;
    println!("서버를 끄고 등록을 지웠습니다 ('{name}').");
    Ok(())
}

fn status(name: &str) -> anyhow::Result<()> {
    validate_entry_name(name)?;
    let backend = backend();
    let Some(existing) = backend.find(name)? else {
        println!("등록되어 있지 않습니다 ('{name}').");
        std::process::exit(1);
    };
    println!("등록: {} ('{name}')", backend.place(name)?);
    let running = backend.report(name, &existing)?;
    if running {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// 서버 상태를 데이터 폴더의 상태 파일로 보고 출력한다. 떠 있으면 `true`.
fn report_server(root: &Path) -> anyhow::Result<bool> {
    match serve::running_server(root)? {
        None => {
            println!("서버: 떠 있지 않습니다 (데이터 폴더 {})", root.display());
            Ok(false)
        }
        Some(s) => {
            match s.addr {
                Some(addr) => println!("서버: 실행 중 (주소 {})", serve::mcp_url(addr, s.tls)),
                None => println!("서버: 실행 중 (주소는 아직 알 수 없음, 기동 중)"),
            }
            if let Some(pid) = s.pid {
                println!("PID: {pid}");
            }
            println!("데이터 폴더: {}", root.display());
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------- 운영체제별 등록 저장소

/// 이미 있는 등록. 데이터 폴더를 알아낼 수 있으면 `root`에 담는다.
struct Existing {
    root: Option<PathBuf>,
}

/// 등록 저장소 하나(레지스트리 · LaunchAgent · systemd). 세 구현이 모두 모든 운영체제에서 컴파일되고,
/// 실행 때 `Target::host()`로 하나를 고른다 (레지스트리 접근만 Windows에서만 켜진다).
trait Backend {
    /// 사용자에게 보이는 등록 위치.
    fn place(&self, name: &str) -> anyhow::Result<String>;
    fn find(&self, name: &str) -> anyhow::Result<Option<Existing>>;
    /// 등록하고 `start`이면 서버를 지금 띄운다. `replacing`이 있으면 그 서버를 먼저 끈다.
    fn write(
        &self,
        reg: &Registration<'_>,
        replacing: Option<&Existing>,
        start: bool,
    ) -> anyhow::Result<()>;
    /// 서버를 끄고 등록을 지운다.
    fn remove(&self, name: &str, existing: &Existing) -> anyhow::Result<()>;
    /// 등록·서버 상태를 출력한다. 서버가 떠 있으면 `true`.
    fn report(&self, name: &str, existing: &Existing) -> anyhow::Result<bool>;
}

fn backend() -> &'static dyn Backend {
    match Target::host() {
        Target::Windows => &RunKey,
        Target::Macos => &LaunchAgent,
        Target::Linux => &SystemdUser,
    }
}

/// 외부 명령을 실행해 성공 여부를 본다. 출력은 가린다.
fn command_ok(program: &str, args: &[&str]) -> bool {
    Process::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 외부 명령을 실행해 표준 출력을 받는다. 실행하지 못하면 `None`.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Process::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_checked(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Process::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("{program} 를 실행할 수 없습니다"))?;
    ensure!(
        status.success(),
        "{program} {} 실패: {status}",
        args.join(" ")
    );
    Ok(())
}

fn write_file(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("폴더를 만들 수 없습니다: {}", dir.display()))?;
    }
    fs::write(path, text).with_context(|| format!("파일을 쓸 수 없습니다: {}", path.display()))
}

fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("파일을 지울 수 없습니다: {}", path.display()))
        }
        _ => Ok(()),
    }
}

// ---- Windows: HKCU\...\Run

struct RunKey;

#[cfg(windows)]
mod registry {
    use anyhow::Context;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    /// 현재 사용자 시작 항목 키. HKCU 쓰기는 관리자 권한이 필요 없다.
    pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn get(name: &str) -> anyhow::Result<Option<String>> {
        let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)
        {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("시작 항목 키를 열 수 없습니다"),
        };
        match key.get_value::<String, _>(name) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("시작 항목 값을 읽을 수 없습니다: {name}")),
        }
    }

    pub fn set(name: &str, value: &str) -> anyhow::Result<()> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_KEY)
            .context("시작 항목 키를 열 수 없습니다")?;
        key.set_value(name, &value)
            .with_context(|| format!("시작 항목 값을 쓸 수 없습니다: {name}"))
    }

    pub fn delete(name: &str) -> anyhow::Result<()> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .context("시작 항목 키를 열 수 없습니다")?;
        match key.delete_value(name) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("시작 항목 값을 지울 수 없습니다: {name}"))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(windows))]
mod registry {
    fn unsupported<T>() -> anyhow::Result<T> {
        anyhow::bail!("Windows 시작 항목은 Windows에서만 쓸 수 있습니다")
    }
    pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub fn get(_: &str) -> anyhow::Result<Option<String>> {
        unsupported()
    }
    pub fn set(_: &str, _: &str) -> anyhow::Result<()> {
        unsupported()
    }
    pub fn delete(_: &str) -> anyhow::Result<()> {
        unsupported()
    }
}

impl Backend for RunKey {
    fn place(&self, name: &str) -> anyhow::Result<String> {
        Ok(format!(
            "Windows 시작 항목 HKCU\\{}\\{name}",
            registry::RUN_KEY
        ))
    }

    fn find(&self, name: &str) -> anyhow::Result<Option<Existing>> {
        Ok(registry::get(name)?.map(|value| Existing {
            root: root_in_args(&split_windows_command(&value)),
        }))
    }

    fn write(
        &self,
        reg: &Registration<'_>,
        replacing: Option<&Existing>,
        start: bool,
    ) -> anyhow::Result<()> {
        if let Some(old_root) = replacing.and_then(|e| e.root.as_deref()) {
            serve::stop(old_root).context("이전 등록의 서버를 끄지 못했습니다")?;
        }
        registry::set(reg.name, &windows_command(reg)?)?;
        if start {
            // 로그인 때 시작 항목이 하는 것과 똑같이 `serve --detach`를 실행한다.
            let mut argv = vec!["serve".to_string(), "--detach".to_string()];
            argv.extend(reg.serve_args.iter().cloned());
            argv.extend(["--log-file".to_string(), path_text(reg.log)?.to_string()]);
            let status = Process::new(reg.exe)
                .args(&argv)
                .stdin(Stdio::null())
                .status()
                .context("서버를 띄울 수 없습니다")?;
            ensure!(
                status.success(),
                "등록은 했지만 서버를 지금 띄우지 못했습니다 ({status}). 로그: {}",
                reg.log.display()
            );
        }
        Ok(())
    }

    fn remove(&self, name: &str, existing: &Existing) -> anyhow::Result<()> {
        // 서버를 먼저 끈다. 끄지 못하면 등록을 남겨 두어 다시 시도할 수 있게 한다.
        match existing.root.as_deref() {
            Some(root) => serve::stop(root)?,
            None => println!("등록된 명령에서 데이터 폴더를 읽지 못해 서버는 끄지 않았습니다."),
        }
        registry::delete(name)
    }

    fn report(&self, name: &str, existing: &Existing) -> anyhow::Result<bool> {
        if let Some(value) = registry::get(name)? {
            println!("명령: {value}");
        }
        match existing.root.as_deref() {
            Some(root) => report_server(root),
            None => {
                println!("서버: 등록된 명령에서 데이터 폴더를 읽지 못해 상태를 알 수 없습니다");
                Ok(false)
            }
        }
    }
}

// ---- macOS: LaunchAgent

struct LaunchAgent;

impl LaunchAgent {
    fn domain() -> anyhow::Result<String> {
        let uid = command_stdout("id", &["-u"]).context("사용자 번호를 알 수 없습니다 (id -u)")?;
        ensure!(
            !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()),
            "사용자 번호를 읽지 못했습니다: {uid:?}"
        );
        Ok(format!("gui/{uid}"))
    }

    fn path(name: &str) -> anyhow::Result<PathBuf> {
        Ok(launch_agent_path(&home_dir()?, name))
    }

    /// 올라가 있는 에이전트를 내린다. 올라가 있지 않으면 launchctl 이 실패하므로 결과는 무시한다.
    fn unload(name: &str) -> anyhow::Result<()> {
        let target = format!("{}/{name}", Self::domain()?);
        let _ = command_ok("launchctl", &["bootout", &target]);
        Ok(())
    }
}

impl Backend for LaunchAgent {
    fn place(&self, name: &str) -> anyhow::Result<String> {
        Ok(format!("macOS LaunchAgent {}", Self::path(name)?.display()))
    }

    fn find(&self, name: &str) -> anyhow::Result<Option<Existing>> {
        Ok(Self::path(name)?
            .is_file()
            .then_some(Existing { root: None }))
    }

    fn write(
        &self,
        reg: &Registration<'_>,
        replacing: Option<&Existing>,
        start: bool,
    ) -> anyhow::Result<()> {
        let path = Self::path(reg.name)?;
        if replacing.is_some() {
            Self::unload(reg.name)?;
        }
        write_file(&path, &launch_agent(reg)?)?;
        if start {
            run_checked(
                "launchctl",
                &["bootstrap", &Self::domain()?, path_text(&path)?],
            )?;
        }
        Ok(())
    }

    fn remove(&self, name: &str, _: &Existing) -> anyhow::Result<()> {
        Self::unload(name)?; // 내리면 launchd 가 SIGTERM 으로 서버를 정상 종료시킨다
        remove_file_if_present(&Self::path(name)?)
    }

    fn report(&self, name: &str, _: &Existing) -> anyhow::Result<bool> {
        let target = format!("{}/{name}", Self::domain()?);
        let loaded = command_ok("launchctl", &["print", &target]);
        println!(
            "서버: {}",
            if loaded {
                "launchd 에 올라가 있습니다"
            } else {
                "launchd 에 올라가 있지 않습니다"
            }
        );
        Ok(loaded)
    }
}

// ---- Linux: systemd --user

struct SystemdUser;

impl SystemdUser {
    fn path(name: &str) -> anyhow::Result<PathBuf> {
        Ok(systemd_unit_path(&home_dir()?, name))
    }
}

impl Backend for SystemdUser {
    fn place(&self, name: &str) -> anyhow::Result<String> {
        Ok(format!(
            "Linux systemd 사용자 단위 {}",
            Self::path(name)?.display()
        ))
    }

    fn find(&self, name: &str) -> anyhow::Result<Option<Existing>> {
        Ok(Self::path(name)?
            .is_file()
            .then_some(Existing { root: None }))
    }

    fn write(
        &self,
        reg: &Registration<'_>,
        replacing: Option<&Existing>,
        start: bool,
    ) -> anyhow::Result<()> {
        let unit = format!("{}.service", reg.name);
        if replacing.is_some() {
            let _ = command_ok("systemctl", &["--user", "stop", &unit]);
        }
        write_file(&Self::path(reg.name)?, &systemd_unit(reg)?)?;
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        if start {
            run_checked("systemctl", &["--user", "enable", "--now", &unit])
        } else {
            run_checked("systemctl", &["--user", "enable", &unit])
        }
    }

    fn remove(&self, name: &str, _: &Existing) -> anyhow::Result<()> {
        let unit = format!("{name}.service");
        // 끄기(SIGTERM → 서버가 정상 종료)와 자동 시작 해제. 이미 꺼져 있어도 실패할 수 있어 결과는 무시한다.
        let _ = command_ok("systemctl", &["--user", "disable", "--now", &unit]);
        remove_file_if_present(&Self::path(name)?)?;
        run_checked("systemctl", &["--user", "daemon-reload"])
    }

    fn report(&self, name: &str, _: &Existing) -> anyhow::Result<bool> {
        let unit = format!("{name}.service");
        let state = command_stdout("systemctl", &["--user", "is-active", &unit])
            .unwrap_or_else(|| "알 수 없음".to_string());
        println!("서버: systemd 상태 {state}");
        Ok(state == "active")
    }
}

// ---------------------------------------------------------------- 단위 시험

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(args: &[&str]) {
        let line = args
            .iter()
            .map(|a| quote_windows_arg(a))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(split_windows_command(&line), args, "명령줄: {line}");
    }

    #[test]
    fn windows_quote_leaves_plain_words_alone() {
        assert_eq!(quote_windows_arg("--root"), "--root");
        assert_eq!(quote_windows_arg("127.0.0.1:7437"), "127.0.0.1:7437");
    }

    #[test]
    fn windows_quote_wraps_spaces_empty_and_quotes() {
        assert_eq!(quote_windows_arg("a b"), "\"a b\"");
        assert_eq!(quote_windows_arg(""), "\"\"");
        assert_eq!(quote_windows_arg("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn windows_quote_doubles_backslashes_before_closing_quote() {
        assert_eq!(quote_windows_arg(r"C:\my dir\"), r#""C:\my dir\\""#);
    }

    #[test]
    fn windows_quote_split_roundtrip() {
        roundtrip(&["serve", "--detach"]);
        roundtrip(&["--root", r"C:\Users\me\my data\root"]);
        roundtrip(&[r"C:\my dir\", "x"]);
        roundtrip(&["a\"b", "", "c d", r"back\\slash", r"trail\\"]);
        roundtrip(&["한글 경로", "\t탭"]);
    }

    #[test]
    fn windows_split_handles_quoted_program() {
        assert_eq!(
            split_windows_command(r#""C:\Program Files\v.exe" serve --root "D:\a b""#),
            [r"C:\Program Files\v.exe", "serve", "--root", r"D:\a b"]
        );
    }

    #[test]
    fn root_is_read_back_from_registered_command() {
        let args = split_windows_command(r#""x.exe" serve --detach --root "D:\a b\r" --user u"#);
        assert_eq!(root_in_args(&args), Some(PathBuf::from(r"D:\a b\r")));
        assert_eq!(root_in_args(&["serve".to_string()]), None);
        assert_eq!(root_in_args(&["--root".to_string()]), None);
    }

    #[test]
    fn systemd_quote_rules() {
        assert_eq!(systemd_quote("plain"), "plain");
        assert_eq!(systemd_quote("a b"), "\"a b\"");
        assert_eq!(systemd_quote("100%$x"), "100%%$$x");
        assert_eq!(systemd_quote("q\"q"), "\"q\\\"q\"");
        assert_eq!(systemd_quote(r"b\s"), r#""b\\s""#);
        assert_eq!(systemd_quote(""), "\"\"");
    }

    #[test]
    fn xml_escape_covers_markup_characters() {
        assert_eq!(xml_escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }

    #[test]
    fn entry_names() {
        for ok in ["vgraph-mcp", "vgraph-mcp-test-1_2.x", "a"] {
            assert!(validate_entry_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "a b", "a/b", "..", "한글", &"x".repeat(65)] {
            assert!(validate_entry_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn registration_paths_follow_each_os_convention() {
        let home = Path::new("/home/u");
        assert_eq!(
            launch_agent_path(home, "vgraph-mcp"),
            Path::new("/home/u/Library/LaunchAgents/vgraph-mcp.plist")
        );
        assert_eq!(
            systemd_unit_path(home, "vgraph-mcp"),
            Path::new("/home/u/.config/systemd/user/vgraph-mcp.service")
        );
    }

    #[test]
    fn non_utf8_paths_are_refused_not_mangled() {
        // UTF-8로 표현할 수 없는 경로를 만들 수 있는 운영체제(unix)에서만 확인한다.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
            assert!(path_text(bad).is_err());
        }
    }
}
