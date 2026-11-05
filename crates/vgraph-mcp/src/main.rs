//! vgraph-mcp 바이너리. 본체는 라이브러리 `vgraph_mcp`(`run_mcp`)에 있고, 이 바이너리는 그것을 부를 뿐이다.
//!
//! - 하위 명령 없이 실행하면 stdio MCP 서버다. 등록 예: claude mcp add vgraph -- vgraph-mcp --root ./vgraph-data --user local
//! - `serve`는 여러 클라이언트가 함께 쓰는 상주 HTTP 서버, `stop`·`status`는 그 서버를 끄고 살핀다
//!   (기능 `server`, 기본 켬 — `src/serve.rs`).
//! - `service`는 그 서버를 사용자 로그인 때 자동으로 띄우도록 등록·해제·확인한다 (`src/service.rs`).

use std::process::ExitCode;

fn main() -> ExitCode {
    vgraph_mcp::run_mcp()
}
