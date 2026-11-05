**English** | [한국어](README.ko.md)

# revive-vgraph

A versioned, branching property-graph (LPG) store built on SQLite, with a command-line tool (`vgraph`) and an MCP server for AI agents (`vgraph-mcp`).

## Features

- **Version control for graphs** — commit, branch, merge (3-way, with conflict resolution), and discard working changes. Every change is a reversible delta, and commits are content-addressed.
- **Labeled property graph** — nodes with labels and properties, typed edges, lookups by property equality, and neighbor traversal with direction and depth.
- **Vector sidecar search** — optional embeddings stored alongside the graph, with brute-force similarity search. Embedders are selectable with `--embedder` (`none`, `toy`, `openai:<model>`, `ollama:<model>`, `http:<url>`).
- **JSON export / import** — the `vgraph-json/1` format, including repeatable synchronization through a dedicated import branch merged with the normal 3-way merge.
- **Users and workspaces** — `--user` sets the session user (ACL owner). Named workspaces (`--workspace`) let several processes or agents edit the same graph concurrently.
- **MCP server** — stdio server for a single client, plus a resident HTTP server (`serve`) that several clients share, with per-user tokens and TLS for access from other machines.

## Installation

### From GitHub Releases

Pre-built archives for six targets are attached to each release:

| Target |
|---|
| `x86_64-unknown-linux-gnu` |
| `x86_64-unknown-linux-musl` (statically linked) |
| `aarch64-unknown-linux-gnu` |
| `aarch64-apple-darwin` |
| `x86_64-apple-darwin` |
| `x86_64-pc-windows-msvc` (`.zip`; the others are `.tar.gz`) |

Each archive contains `vgraph`, `vgraph-mcp`, `README.md` and `LICENSE`. Put the binaries somewhere on your `PATH`.

The binaries are **not code-signed**. On macOS you have to allow them manually the first time you run them (System Settings > Privacy & Security).

### From source

Requires Rust 1.85 or newer (`rust-version` in `Cargo.toml`) and a C compiler (SQLite is bundled and compiled from source).

```text
cargo build --release -p vgraph-cli -p vgraph-mcp
```

The binaries are written to `target/release/vgraph` and `target/release/vgraph-mcp`.

## Quick start

Graph names look like `owner/name`. By default data is stored in `./vgraph-data`; change it with `--root <DIR>`.

```text
vgraph init acme/bookclub
vgraph add-node acme/bookclub --label Capability --slug cap-auth --prop name=auth
vgraph pending acme/bookclub        # summary of uncommitted changes
vgraph commit acme/bookclub -m "first"
vgraph log acme/bookclub
vgraph show acme/bookclub#cap-auth
vgraph find acme/bookclub name=auth
vgraph branch acme/bookclub dev-b
vgraph merge acme/bookclub dev-b main
vgraph export acme/bookclub --out bookclub.json
```

Other subcommands: `graphs`, `add-edge`, `set`, `unset`, `discard`, `neighbors`, `drifts`, `lockfile`, `gc`, `import`. Run `vgraph --help` or `vgraph <command> --help` for the details. Note that the built-in help text is currently in Korean.

## Connecting an MCP client

`vgraph-mcp` without a subcommand is an MCP server over stdio. Register it in your MCP client's configuration (key names vary between clients, so follow your client's documentation):

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

Useful options: `--user`, `--workspace`, `--embedder`, `--embed-fields`, `--chunker`, `--no-embed-fallback`. Run `vgraph-mcp --help` for the full list.

The server exposes task-oriented tools such as `vgraph_create`, `vgraph_list`, `vgraph_show`, `vgraph_find`, `vgraph_search`, `vgraph_neighbors`, `vgraph_edit`, `vgraph_pending`, `vgraph_commit`, `vgraph_log`, `vgraph_diff`, `vgraph_branch`, `vgraph_merge`, `vgraph_export` and `vgraph_import`.

To share one data directory between several clients, run the resident server instead and connect over HTTP; see [docs/server-mode.md](docs/server-mode.md) section 3 for client configuration examples.

## Resident server and remote access

```text
vgraph-mcp serve --root ./vgraph-data     # listens on 127.0.0.1:7437
vgraph-mcp status --root ./vgraph-data
vgraph-mcp stop --root ./vgraph-data
```

- By default the server only accepts connections from the same machine. Listening on a non-loopback address requires both TLS (`--tls-cert`, `--tls-key`) and per-user tokens (`--tokens`); otherwise it refuses to start.
- Tokens are managed with `vgraph-mcp token add <user>`, `token list` and `token revoke <id>`.
- `vgraph-mcp service install|uninstall|status` registers the server to start at login. Only Windows has been verified on real hardware; macOS and Linux registration is unverified.

Details are in [docs/server-mode.md](docs/server-mode.md) and [docs/remote-access.md](docs/remote-access.md). **These documents are currently written in Korean only.**

## Using it as a library

The two binaries are thin entry points. Their bodies are public functions, so you can build your own executable around them:

| Function | Crate | Purpose |
|---|---|---|
| `run_cli()` | `vgraph-cli` | Body of the `vgraph` binary; reads process arguments and returns an `ExitCode` |
| `run_cli_with(on_fatal_error)` | `vgraph-cli` | Same, and calls your handler right after a fatal error message is written to stderr |
| `run_mcp()` | `vgraph-mcp` | Body of the `vgraph-mcp` binary |
| `run_mcp_with(on_fatal_error)` | `vgraph-mcp` | Same, with a fatal-error handler (it must not write to stdout, which is reserved for JSON-RPC) |

```rust
fn main() -> std::process::ExitCode {
    vgraph_cli::run_cli()
}
```

`vgraph-mcp` has a default feature `server` (the resident HTTP server). Disable it with `default-features = false` to leave only the stdio server and drop the server-related dependencies. See [docs/server-mode.md](docs/server-mode.md) section 7 (Korean).

## Crates

| Crate | Role |
|---|---|
| `vgraph-core` | Core model: LPG, reversible deltas, canonical serialization and hashing, addresses. No I/O. |
| `vgraph-store` | Storage: the `StorageBackend` traits, a SQLite backend, and a conformance suite. |
| `vgraph-engine` | Engine: view cache for reads and a synced / on_update state machine for writes. |
| `vgraph-vec` | Vector sidecar: embedding tables, brute-force similarity, epoch join, and a computed staleness queue. |
| `mcp-kit` | Minimal MCP stdio server skeleton (newline-delimited JSON-RPC 2.0), shared protocol layer. |
| `vgraph-mcp` | MCP server for agents (stdio, plus the resident server). Builds the `vgraph-mcp` binary. |
| `vgraph-cli` | Local command-line tool. Builds the `vgraph` binary. |

## Development

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --locked --workspace
```

## Releases

Pushing a tag of the form `vX.Y.Z` runs the `release.yml` workflow. It builds the six targets above, packages them, and publishes a GitHub Release.

- The tag must equal `[workspace.package] version` in `Cargo.toml`; otherwise the build fails before compiling.
- Native targets are smoke-tested, and the binaries' `--version` output must match the tag.
- Archives are not code-signed.

## License

[MIT](LICENSE) — Copyright (c) 2026 Gongdo Co., Ltd.
