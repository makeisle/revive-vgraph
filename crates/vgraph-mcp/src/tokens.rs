//! 사용자별 접속 토큰 — 발급·목록·폐기(`vgraph-mcp token ...`)와 서버의 검증. 기능 `server` 뒤에 있다.
//!
//! 저장: JSON Lines 파일(기본 `<데이터 폴더>/tokens.jsonl`), 한 줄이 토큰 하나다.
//! `{"id","user","hash","created","revoked"?}` — 원문은 어디에도 저장하지 않고 SHA-256 해시만 둔다.
//!
//! 원문은 OS 난수 32바이트(256비트)를 16진수로 적은 것(접두어 `vgt_`)이다. 추측으로 맞힐 수 있는
//! 공간이 아니므로 느린 키 도출 함수(argon2 등) 없이 단순 해시로 충분하다 — 느린 해시는 사람이 고른
//! 약한 비밀번호를 막으려는 장치다. 해시를 비교할 때는 `mcp_kit::http::constant_time_eq`를 쓴다.
//!
//! 폐기 반영: 서버는 요청마다 파일의 (수정 시각, 길이)를 살펴 달라졌을 때만 다시 읽는다.
//! 그래서 재시작 없이 폐기·새 발급이 바로 적용된다. 파일이 사라지면 모든 토큰이 거부되고,
//! 내용이 깨지면(해석 실패) 마찬가지로 모두 거부한다(닫힌 쪽으로 실패). 읽는 도중 입출력 오류는
//! 일시적일 수 있어 직전에 읽은 내용을 유지하고 다음 요청에서 다시 시도한다.
//!
//! 관리 명령(`add`·`revoke`)은 한 번에 하나씩 실행한다고 가정한다(동시 실행 중 한 줄이 유실될 수 있다).

use clap::{Args, Subcommand};
use mcp_kit::http::constant_time_eq;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::DEFAULT_ROOT;

/// 토큰 파일의 기본 이름. 데이터 폴더 아래에 둔다.
pub const DEFAULT_FILE: &str = "tokens.jsonl";
/// 토큰 원문 앞에 붙는 식별 접두어 (비밀 스캐너·사람이 알아보기 위한 것이고 보안 강도와는 무관).
const TOKEN_PREFIX: &str = "vgt_";
/// 토큰 난수 바이트 수(256비트).
const SECRET_BYTES: usize = 32;
/// 사용자 이름 최대 글자 수.
const MAX_USER_CHARS: usize = 128;
/// 폐기 때 파일 바꿔치기가 읽는 쪽과 겹쳐 실패하면(Windows 공유 위반) 다시 시도하는 횟수·간격.
const REPLACE_TRIES: u32 = 10;
const REPLACE_WAIT: Duration = Duration::from_millis(50);
const HASH_HEX_LEN: usize = 64;

const ABOUT: &str = "사용자별 접속 토큰을 발급·조회·폐기합니다";
const LONG_ABOUT: &str = "\
사용자별 접속 토큰을 관리합니다. 서버를 `serve --tokens <파일>` 로 띄우면 이 토큰으로만 접속할 수 있고,
토큰이 사용자를 정합니다(X-Vgraph-User 헤더는 무시됩니다).

  token add <사용자>   새 토큰을 발급해 표준 출력에 한 줄로 냅니다. 원문은 이때 한 번만 보입니다.
  token list           id·사용자·생성 시각·상태를 탭으로 나눠 보입니다 (원문·해시는 없습니다).
  token revoke <id>    토큰을 폐기합니다. 실행 중인 서버에 곧바로 반영됩니다(재시작 불필요).

토큰 파일은 기본 <--root>/tokens.jsonl 입니다 (--tokens 로 다른 파일을 줄 수 있습니다).";

#[derive(Subcommand)]
pub enum TokenCommand {
    /// 사용자 토큰을 새로 발급한다 (원문은 표준 출력에 한 번만 나온다)
    Add(AddArgs),
    /// 발급한 토큰을 보인다 (원문·해시는 보이지 않는다)
    List(FileArgs),
    /// 토큰을 폐기한다
    Revoke(RevokeArgs),
}

/// `token` 하위 명령 설명 (`serve::Command`가 붙인다).
pub const TOKEN_ABOUT: &str = ABOUT;
pub const TOKEN_LONG_ABOUT: &str = LONG_ABOUT;

#[derive(Args)]
pub struct FileArgs {
    /// 서버의 데이터 폴더 (토큰 파일 기본 위치는 <폴더>/tokens.jsonl)
    #[arg(long, default_value = DEFAULT_ROOT)]
    root: PathBuf,
    /// 토큰 파일 (생략하면 <--root>/tokens.jsonl)
    #[arg(long)]
    tokens: Option<PathBuf>,
}

impl FileArgs {
    fn path(&self) -> PathBuf {
        self.tokens
            .clone()
            .unwrap_or_else(|| self.root.join(DEFAULT_FILE))
    }
}

#[derive(Args)]
pub struct AddArgs {
    /// 토큰이 대표할 사용자 이름 (공백·제어 문자 없이)
    user: String,
    #[command(flatten)]
    file: FileArgs,
}

#[derive(Args)]
pub struct RevokeArgs {
    /// `token list` 에 나온 토큰 id
    id: String,
    #[command(flatten)]
    file: FileArgs,
}

pub fn run(command: TokenCommand) -> anyhow::Result<()> {
    match command {
        TokenCommand::Add(a) => {
            let store = TokenFile::new(a.file.path());
            let (id, secret) = store.add(&a.user)?;
            // 원문은 표준 출력 한 줄만(스크립트가 그대로 받을 수 있게). 안내는 표준 오류로 낸다.
            println!("{secret}");
            eprintln!("토큰을 발급했습니다 (id {id}, 사용자 {}).", a.user);
            eprintln!("이 토큰은 지금만 볼 수 있습니다. 잃어버리면 폐기하고 새로 발급하세요.");
            Ok(())
        }
        TokenCommand::List(f) => {
            for e in TokenFile::new(f.path()).read()? {
                let state = if e.revoked { "폐기됨" } else { "사용 중" };
                println!("{}\t{}\t{}\t{state}", e.id, e.user, e.created);
            }
            Ok(())
        }
        TokenCommand::Revoke(a) => {
            let store = TokenFile::new(a.file.path());
            store.revoke(&a.id)?;
            println!("토큰을 폐기했습니다 (id {}).", a.id);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- 항목·직렬화

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: String,
    user: String,
    /// SHA-256 해시의 소문자 16진수 64자.
    hash: String,
    /// UTC RFC 3339 (`2026-10-06T12:00:00Z`).
    created: String,
    revoked: bool,
}

impl Entry {
    fn to_line(&self) -> String {
        let mut v = json!({
            "id": self.id,
            "user": self.user,
            "hash": self.hash,
            "created": self.created,
        });
        if self.revoked {
            v["revoked"] = json!(true);
        }
        v.to_string()
    }

    fn from_value(v: &Value) -> Result<Entry, String> {
        let field = |name: &str| {
            v.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("\"{name}\" 필드가 없습니다"))
        };
        let hash = field("hash")?;
        if hash.len() != HASH_HEX_LEN
            || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err("\"hash\" 는 소문자 16진수 64자여야 합니다".to_string());
        }
        Ok(Entry {
            id: field("id")?,
            user: field("user")?,
            hash,
            created: field("created")?,
            revoked: v.get("revoked").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// 파일 전체를 해석한다. 오류에는 줄 번호만 담고 줄 내용은 담지 않는다.
fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let n = i + 1;
        let v: Value =
            serde_json::from_str(line).map_err(|_| format!("{n}번째 줄이 JSON이 아닙니다"))?;
        out.push(Entry::from_value(&v).map_err(|e| format!("{n}번째 줄: {e}"))?);
    }
    Ok(out)
}

fn hash_hex(token: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(token.as_bytes());
    let mut s = String::with_capacity(HASH_HEX_LEN);
    for b in digest {
        // String에 쓰는 write!는 실패하지 않는다.
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn new_secret() -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("OS 난수를 얻지 못했습니다: {e}"))?;
    let mut s = String::with_capacity(TOKEN_PREFIX.len() + SECRET_BYTES * 2);
    s.push_str(TOKEN_PREFIX);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// 사용자 이름 규칙: 비어 있지 않고, 공백·제어 문자가 없고, 128자 이하.
/// (사용자 이름에 대한 기존 규칙이 서버에 없어, 파일 형식·목록 출력이 깨지지 않는 선까지만 막는다.)
pub fn check_user(user: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!user.is_empty(), "사용자 이름이 비어 있습니다");
    anyhow::ensure!(
        user.chars().count() <= MAX_USER_CHARS,
        "사용자 이름은 {MAX_USER_CHARS}자 이하여야 합니다"
    );
    anyhow::ensure!(
        !user.chars().any(|c| c.is_whitespace() || c.is_control()),
        "사용자 이름에는 공백·제어 문자를 쓸 수 없습니다"
    );
    Ok(())
}

/// UTC 시각을 RFC 3339 초 단위 문자열로 (시민력 변환: Howard Hinnant `civil_from_days`).
fn rfc3339(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(0);
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

// ---------------------------------------------------------------- 파일 관리(발급·폐기)

/// 토큰 파일 하나에 대한 관리 작업.
struct TokenFile {
    path: PathBuf,
}

/// 소유자만 읽고 쓰게 연다(유닉스). 해시만 들어 있어 원문 유출은 아니지만 목록은 가린다.
fn open_options() -> OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut o = OpenOptions::new();
        o.mode(0o600);
        o
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
    }
}

impl TokenFile {
    fn new(path: PathBuf) -> Self {
        TokenFile { path }
    }

    /// 파일이 없으면 빈 목록.
    fn read(&self) -> anyhow::Result<Vec<Entry>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => parse(&text).map_err(|e| {
                anyhow::anyhow!(
                    "토큰 파일을 해석할 수 없습니다: {}: {e}",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(anyhow::anyhow!(
                "토큰 파일을 읽을 수 없습니다: {}: {e}",
                self.path.display()
            )),
        }
    }

    /// 새 토큰을 만들어 (id, 원문)을 돌려준다. 파일에는 해시만 덧붙인다.
    fn add(&self, user: &str) -> anyhow::Result<(String, String)> {
        check_user(user)?;
        // 깨진 파일 뒤에 덧붙이지 않는다.
        self.read()?;
        let secret = new_secret()?;
        let entry = Entry {
            id: ulid::Ulid::new().to_string(),
            user: user.to_string(),
            hash: hash_hex(&secret),
            created: now_rfc3339(),
            revoked: false,
        };
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| {
                anyhow::anyhow!("토큰 폴더를 만들 수 없습니다: {}: {e}", dir.display())
            })?;
        }
        let mut file = open_options()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                anyhow::anyhow!("토큰 파일을 열 수 없습니다: {}: {e}", self.path.display())
            })?;
        // 줄 하나를 한 번에 써서 읽는 쪽이 쓰다 만 줄을 볼 가능성을 줄인다.
        file.write_all(format!("{}\n", entry.to_line()).as_bytes())
            .map_err(|e| anyhow::anyhow!("토큰 파일에 쓸 수 없습니다: {e}"))?;
        Ok((entry.id, secret))
    }

    /// 사용 중인 토큰 하나를 폐기한다. 오류 문구에는 입력값을 되풀이하지 않는다
    /// (원문을 id 자리에 잘못 붙여 넣었을 때 화면·로그에 남지 않게).
    fn revoke(&self, id: &str) -> anyhow::Result<()> {
        let mut entries = self.read()?;
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            anyhow::bail!("그 id의 토큰이 없습니다 (vgraph-mcp token list 로 id를 확인하세요)");
        };
        anyhow::ensure!(!entry.revoked, "이미 폐기된 토큰입니다 (id {id})");
        entry.revoked = true;

        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut body = String::new();
        for e in &entries {
            body.push_str(&e.to_line());
            body.push('\n');
        }
        open_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .and_then(|mut f| f.write_all(body.as_bytes()))
            .map_err(|e| anyhow::anyhow!("임시 파일을 쓸 수 없습니다: {}: {e}", tmp.display()))?;
        replace_file(&tmp, &self.path)
    }
}

/// 임시 파일로 원본을 바꿔치기한다. 서버가 막 읽는 중이면(Windows) 잠시 뒤 다시 시도한다.
fn replace_file(tmp: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut last = None;
    for _ in 0..REPLACE_TRIES {
        match fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(REPLACE_WAIT);
    }
    let _ = fs::remove_file(tmp);
    Err(anyhow::anyhow!(
        "토큰 파일을 갱신할 수 없습니다: {}: {}",
        dest.display(),
        last.map_or_else(String::new, |e| e.to_string())
    ))
}

// ---------------------------------------------------------------- 서버의 검증

/// 파일 상태 표지: (수정 시각, 길이). 달라졌을 때만 다시 읽는다.
type Stamp = Option<(SystemTime, u64)>;

struct Cache {
    stamp: Stamp,
    entries: Vec<Entry>,
}

/// 요청마다 불리는 토큰 검증기. 파일이 바뀌면 다시 읽는다.
pub struct Verifier {
    path: PathBuf,
    cache: Mutex<Cache>,
}

fn stamp_of(path: &Path) -> std::io::Result<Stamp> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some((m.modified()?, m.len()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl Verifier {
    /// 서버 기동 때 한 번 연다. 파일이 없거나 해석할 수 없으면 오류 — 오타로 아무도 못 들어오는
    /// 서버가 조용히 뜨는 일을 막는다.
    pub fn open(path: &Path) -> anyhow::Result<Verifier> {
        let stamp = stamp_of(path).map_err(|e| {
            anyhow::anyhow!("토큰 파일을 읽을 수 없습니다: {}: {e}", path.display())
        })?;
        anyhow::ensure!(
            stamp.is_some(),
            "토큰 파일이 없습니다: {} (먼저 vgraph-mcp token add <사용자> 로 토큰을 발급하세요)",
            path.display()
        );
        let entries = TokenFile::new(path.to_path_buf()).read()?;
        Ok(Verifier {
            path: path.to_path_buf(),
            cache: Mutex::new(Cache { stamp, entries }),
        })
    }

    // 잠금은 이 파일의 동기 함수 안에서만 잡는다. 캐시는 패닉으로 반쯤 바뀌는 일이 없으므로
    // 중독된 잠금도 그대로 쓴다.
    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 제시된 토큰 원문이 사용 중인 토큰이면 그 사용자 이름. 폐기된 토큰·없는 토큰은 `None`.
    pub fn authenticate(&self, presented: &str) -> Option<String> {
        let mut cache = self.cache();
        self.refresh(&mut cache);
        let presented_hash = hash_hex(presented);
        // 어느 항목에서 맞았는지가 시간으로 새지 않게 전부 훑는다.
        let mut found: Option<&Entry> = None;
        for e in cache.entries.iter().filter(|e| !e.revoked) {
            if constant_time_eq(presented_hash.as_bytes(), e.hash.as_bytes()) {
                found = Some(e);
            }
        }
        found.map(|e| e.user.clone())
    }

    fn refresh(&self, cache: &mut Cache) {
        let Ok(now) = stamp_of(&self.path) else {
            return; // 일시적 오류일 수 있다: 직전 내용을 유지하고 다음 요청에서 다시 본다
        };
        if now == cache.stamp {
            return;
        }
        match now {
            None => cache.entries.clear(), // 파일이 사라졌다: 모두 거부
            Some(_) => match fs::read_to_string(&self.path) {
                Ok(text) => match parse(&text) {
                    Ok(entries) => cache.entries = entries,
                    Err(e) => {
                        // 닫힌 쪽으로 실패: 깨진 파일에서는 아무도 통과시키지 않는다.
                        eprintln!(
                            "토큰 파일을 해석할 수 없어 모든 접속을 거부합니다: {}: {e}",
                            self.path.display()
                        );
                        cache.entries.clear();
                    }
                },
                Err(_) => return, // 읽는 중 오류(바꿔치기와 겹침 등): 표지를 갱신하지 않아 다시 시도한다
            },
        }
        cache.stamp = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> TokenFile {
        TokenFile::new(dir.path().join(DEFAULT_FILE))
    }

    #[test]
    fn secret_has_prefix_and_256_bits_and_differs_each_time() {
        let a = new_secret().unwrap();
        let b = new_secret().unwrap();
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + SECRET_BYTES * 2);
        assert!(a[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_sha256_hex() {
        // SHA-256("abc")
        assert_eq!(
            hash_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // 윤일
    }

    #[test]
    fn user_name_rules() {
        for ok in ["alice", "a.b-c_d@x", "김철수", &"a".repeat(MAX_USER_CHARS)] {
            assert!(check_user(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            " ",
            "a b",
            "a\tb",
            "a\nb",
            "a\u{0}b",
            &"a".repeat(MAX_USER_CHARS + 1),
        ] {
            assert!(check_user(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn entry_line_roundtrip_and_parse_errors_do_not_echo_content() {
        let e = Entry {
            id: "I".into(),
            user: "u".into(),
            hash: hash_hex("x"),
            created: "2026-01-01T00:00:00Z".into(),
            revoked: true,
        };
        assert_eq!(parse(&e.to_line()).unwrap(), vec![e.clone()]);
        let plain = Entry {
            revoked: false,
            ..e
        };
        assert!(!plain.to_line().contains("revoked"));
        assert_eq!(parse(&plain.to_line()).unwrap(), vec![plain]);

        let err = parse("SECRET-LOOKING-VALUE").unwrap_err();
        assert!(err.contains('1') && !err.contains("SECRET"), "{err}");
        assert!(parse(r#"{"id":"i","user":"u","hash":"zz","created":"c"}"#).is_err());
        assert!(parse("\n  \n").unwrap().is_empty());
    }

    #[test]
    fn add_stores_only_the_hash_and_verifier_accepts_the_secret() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let (id, secret) = s.add("alice").unwrap();
        let file = fs::read_to_string(&s.path).unwrap();
        assert!(!file.contains(&secret));
        assert!(file.contains(&hash_hex(&secret)));
        assert!(file.contains(&id));

        let v = Verifier::open(&s.path).unwrap();
        assert_eq!(v.authenticate(&secret).as_deref(), Some("alice"));
        assert_eq!(v.authenticate(&format!("{secret}x")), None);
        assert_eq!(v.authenticate(""), None);
        assert_eq!(
            v.authenticate(&file),
            None,
            "파일 내용(해시)은 토큰이 아니다"
        );
    }

    #[test]
    fn verifier_follows_revoke_and_new_tokens_without_reopen() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let (id_a, a) = s.add("alice").unwrap();
        let v = Verifier::open(&s.path).unwrap();
        assert!(v.authenticate(&a).is_some());

        s.revoke(&id_a).unwrap();
        assert_eq!(v.authenticate(&a), None, "폐기 뒤에도 통과");
        let (_, b) = s.add("bob").unwrap();
        assert_eq!(v.authenticate(&b).as_deref(), Some("bob"));
    }

    #[test]
    fn revoke_errors_do_not_echo_input_and_double_revoke_fails() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let (id, _) = s.add("alice").unwrap();
        let err = s.revoke("vgt_SECRETVALUE").unwrap_err().to_string();
        assert!(!err.contains("SECRETVALUE"), "{err}");
        s.revoke(&id).unwrap();
        assert!(s.revoke(&id).is_err());
    }

    #[test]
    fn deleted_or_corrupt_file_denies_everyone_and_recovers() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let (_, a) = s.add("alice").unwrap();
        let v = Verifier::open(&s.path).unwrap();
        let good = fs::read_to_string(&s.path).unwrap();

        fs::remove_file(&s.path).unwrap();
        assert_eq!(v.authenticate(&a), None, "파일이 없어졌는데 통과");

        fs::write(&s.path, "깨진 내용\n").unwrap();
        assert_eq!(v.authenticate(&a), None, "깨진 파일인데 통과");

        fs::write(&s.path, good).unwrap();
        assert!(v.authenticate(&a).is_some(), "복구되면 다시 통한다");
    }

    #[test]
    fn open_requires_existing_parsable_file_and_add_refuses_corrupt_file() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        assert!(Verifier::open(&s.path).is_err());
        fs::write(&s.path, "garbage\n").unwrap();
        assert!(Verifier::open(&s.path).is_err());
        assert!(s.add("alice").is_err());
        assert_eq!(fs::read_to_string(&s.path).unwrap(), "garbage\n");
        assert!(s.add("bad name").is_err());
    }
}
