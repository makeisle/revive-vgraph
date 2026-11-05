[English](README.md) | **한국어**

# revive-vgraph

SQLite 기반의 버저닝·브랜칭 속성 그래프(LPG) 저장소입니다. 터미널용 CLI(`vgraph`)와 AI 에이전트용 MCP 서버(`vgraph-mcp`)를 함께 제공합니다.

## 주요 기능

- **그래프의 버전 관리** — 커밋, 브랜치, 머지(3-way, 충돌 해소 포함), 작업 중 변경 폐기. 모든 변경은 되돌릴 수 있는 delta이고, 커밋은 내용 기반 주소를 가집니다.
- **속성 그래프** — 라벨과 속성을 가진 노드, 타입이 있는 엣지, 속성 일치 검색, 방향·깊이를 지정하는 이웃 순회.
- **벡터 사이드카 검색** — 그래프 옆에 임베딩을 저장하고 브루트포스 유사도로 검색합니다(선택 기능). `--embedder`로 임베더를 고릅니다(`none`, `toy`, `openai:<model>`, `ollama:<model>`, `http:<url>`).
- **JSON 내보내기·가져오기** — `vgraph-json/1` 형식. 가져오기 전용 브랜치를 만들어 기존 3-way 머지로 합치는 반복 동기화도 지원합니다.
- **사용자와 작업 공간** — `--user`는 세션 사용자(ACL 소유자)입니다. 이름 있는 작업 공간(`--workspace`)을 쓰면 여러 프로세스·에이전트가 같은 그래프를 동시에 편집할 수 있습니다.
- **MCP 서버** — 클라이언트 하나를 위한 stdio 서버, 그리고 여러 클라이언트가 함께 쓰는 상주 HTTP 서버(`serve`). 사용자별 토큰과 TLS로 다른 PC에서도 접속할 수 있습니다.

## 설치

### GitHub Releases에서 받기

릴리스마다 6개 대상의 빌드된 압축본이 올라갑니다.

| 대상 |
|---|
| `x86_64-unknown-linux-gnu` |
| `x86_64-unknown-linux-musl` (정적 링크) |
| `aarch64-unknown-linux-gnu` |
| `aarch64-apple-darwin` |
| `x86_64-apple-darwin` |
| `x86_64-pc-windows-msvc` (`.zip`, 나머지는 `.tar.gz`) |

압축본에는 `vgraph`, `vgraph-mcp`, `README.md`, `LICENSE`가 들어 있습니다. 바이너리를 `PATH`에 있는 폴더에 두십시오.

바이너리는 **코드 서명이 없습니다.** macOS에서는 처음 실행할 때 수동으로 허용해야 합니다(시스템 설정 > 개인정보 보호 및 보안).

### 소스에서 빌드

Rust 1.85 이상(`Cargo.toml`의 `rust-version`)과 C 컴파일러가 필요합니다(SQLite를 소스로 함께 빌드합니다).

```text
cargo build --release -p vgraph-cli -p vgraph-mcp
```

결과물은 `target/release/vgraph`, `target/release/vgraph-mcp`입니다.

## 빠른 시작

그래프 이름은 `소유자/이름` 형태입니다. 데이터는 기본적으로 `./vgraph-data`에 저장되고, `--root <폴더>`로 바꿀 수 있습니다.

```text
vgraph init acme/bookclub
vgraph add-node acme/bookclub --label Capability --slug cap-auth --prop name=auth
vgraph pending acme/bookclub        # 아직 커밋하지 않은 변경 요약
vgraph commit acme/bookclub -m "first"
vgraph log acme/bookclub
vgraph show acme/bookclub#cap-auth
vgraph find acme/bookclub name=auth
vgraph branch acme/bookclub dev-b
vgraph merge acme/bookclub dev-b main
vgraph export acme/bookclub --out bookclub.json
```

그 밖의 하위 명령: `graphs`, `add-edge`, `set`, `unset`, `discard`, `neighbors`, `drifts`, `lockfile`, `gc`, `import`. 자세한 내용은 `vgraph --help`, `vgraph <명령> --help`를 보십시오.

## MCP 클라이언트 연결

하위 명령 없이 실행한 `vgraph-mcp`는 stdio MCP 서버입니다. 사용하는 MCP 클라이언트의 설정에 등록합니다(키 이름은 클라이언트마다 다를 수 있으니 해당 클라이언트 문서에 맞추십시오).

```json
{
  "mcpServers": {
    "vgraph": {
      "command": "vgraph-mcp",
      "args": ["--root", "/path/to/vgraph-data"]
    }
  }
}
```

쓸 만한 옵션: `--user`, `--workspace`, `--embedder`, `--embed-fields`, `--chunker`, `--no-embed-fallback`. 전체 목록은 `vgraph-mcp --help`입니다.

서버는 `vgraph_create`, `vgraph_list`, `vgraph_show`, `vgraph_find`, `vgraph_search`, `vgraph_neighbors`, `vgraph_edit`, `vgraph_pending`, `vgraph_commit`, `vgraph_log`, `vgraph_diff`, `vgraph_branch`, `vgraph_merge`, `vgraph_export`, `vgraph_import` 같은 과업 지향 도구를 제공합니다.

여러 클라이언트가 데이터 폴더 하나를 함께 쓰려면 상주 서버를 띄워 HTTP로 접속하십시오. 클라이언트 설정 예는 [docs/server-mode.md](docs/server-mode.md) 3절에 있습니다.

## 상주 서버와 원격 접속

```text
vgraph-mcp serve --root ./vgraph-data     # 127.0.0.1:7437 에서 대기
vgraph-mcp status --root ./vgraph-data
vgraph-mcp stop --root ./vgraph-data
```

- 기본값은 같은 PC에서만 접속할 수 있습니다. 루프백이 아닌 주소로 열려면 TLS(`--tls-cert`, `--tls-key`)와 사용자별 토큰(`--tokens`)이 모두 있어야 하며, 하나라도 없으면 시작하지 않습니다.
- 토큰은 `vgraph-mcp token add <사용자>`, `token list`, `token revoke <id>`로 관리합니다.
- `vgraph-mcp service install|uninstall|status`는 서버를 로그인할 때 자동으로 띄우도록 등록합니다. 실기 검증은 Windows만 되어 있고, macOS·Linux 등록은 실기 미검증입니다.

자세한 내용은 [docs/server-mode.md](docs/server-mode.md)와 [docs/remote-access.md](docs/remote-access.md)를 보십시오.

## 라이브러리로 쓰기

두 바이너리는 얇은 진입점입니다. 본체가 공개 함수라서 이를 감싸 나만의 실행 파일을 만들 수 있습니다.

| 함수 | 크레이트 | 용도 |
|---|---|---|
| `run_cli()` | `vgraph-cli` | `vgraph` 바이너리의 본체. 프로세스 인자를 읽어 실행하고 `ExitCode`를 돌려줍니다 |
| `run_cli_with(on_fatal_error)` | `vgraph-cli` | 같되, 치명적 오류 메시지를 표준 오류에 쓴 직후 주어진 처리기를 부릅니다 |
| `run_mcp()` | `vgraph-mcp` | `vgraph-mcp` 바이너리의 본체 |
| `run_mcp_with(on_fatal_error)` | `vgraph-mcp` | 같되 치명적 오류 처리기를 받습니다(표준 출력은 JSON-RPC 전용이므로 처리기가 표준 출력에 쓰면 안 됩니다) |

```rust
fn main() -> std::process::ExitCode {
    vgraph_cli::run_cli()
}
```

`vgraph-mcp`에는 기본으로 켜지는 기능 `server`(상주 HTTP 서버)가 있습니다. `default-features = false`로 끄면 stdio 서버만 남고 서버 계열 의존성이 들어오지 않습니다. [docs/server-mode.md](docs/server-mode.md) 7절을 보십시오.

## 크레이트 구성

| 크레이트 | 역할 |
|---|---|
| `vgraph-core` | 코어 모델: LPG, 가역 delta, canonical 직렬화·해시, 주소. IO 없음. |
| `vgraph-store` | 스토리지: `StorageBackend` 트레이트, SQLite 백엔드, conformance 스위트. |
| `vgraph-engine` | 엔진: 읽기용 View 캐시와 쓰기용 synced / on_update 상태머신. |
| `vgraph-vec` | 벡터 사이드카: 임베딩 테이블, 브루트포스 유사도, epoch 조인, 계산형 staleness 큐. |
| `mcp-kit` | 미니멀 MCP stdio 서버 골격(newline JSON-RPC 2.0), 공용 프로토콜 계층. |
| `vgraph-mcp` | 에이전트용 MCP 서버(stdio와 상주 서버). `vgraph-mcp` 바이너리를 만듭니다. |
| `vgraph-cli` | 로컬 조작 CLI. `vgraph` 바이너리를 만듭니다. |

## 개발

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --locked --workspace
```

## 릴리스

`vX.Y.Z` 형태의 태그를 push 하면 `release.yml` 워크플로가 위 6개 대상을 빌드·압축해 GitHub Release를 만듭니다.

- 태그가 `Cargo.toml`의 `[workspace.package] version`과 같아야 합니다. 다르면 빌드 전에 실패합니다.
- 네이티브 대상은 연막 시험을 거치고, 바이너리의 `--version` 출력이 태그와 같아야 합니다.
- 압축본에는 코드 서명이 없습니다.

## 라이선스

[MIT](LICENSE) — Copyright (c) 2026 Gongdo Co., Ltd.
