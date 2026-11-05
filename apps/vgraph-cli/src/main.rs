//! vgraph — 로컬 조작 CLI (v0). 본체는 라이브러리 `vgraph_cli` 에 있고, 이 바이너리는 그것을 부를 뿐이다.

use std::process::ExitCode;

fn main() -> ExitCode {
    vgraph_cli::run_cli()
}
