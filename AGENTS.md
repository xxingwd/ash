# Repository Guidelines

## Project Structure & Module Organization

Ash is a Rust 2021 workspace under `crates/`. `ash-core` defines shared types; `ash-protocol` implements streaming adapters; `ash-tools` contains file and shell tools; `ash-agent` owns sessions, prompts, skills, and MCP; `ash-orchestrator` provides experimental coordination; `ash-tui` implements the inline UI; and `ash-cli` builds the binary. Unit tests generally sit beside modules. Integration and Insta snapshot tests are in `crates/ash-core/tests/`. Architecture constraints are documented in `DESIGN.md`; keep `target/` untracked.

## Build, Test, and Development Commands

- `cargo build --workspace` builds every crate in debug mode.
- `cargo run -p ash-cli -- --print "inspect this project"` runs a single non-interactive request.
- `./start.sh` launches the CLI and forwards any arguments to `ash-cli`.
- `cargo test --workspace` runs all unit, integration, and snapshot tests.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings` treats every lint warning as an error.

Run formatting, Clippy, and the full test suite before submitting changes.

## Coding Style & Naming Conventions

Use default `rustfmt` output (four-space indentation). Follow Rust conventions: `snake_case` for modules, functions, and tests; `PascalCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants. Keep protocol-specific translation inside `ash-protocol`, shared abstractions in `ash-core`, and terminal state out of the agent layer. Prefer typed errors and cancellation-aware async code over panics or blocking operations.

## Testing Guidelines

Add focused `#[test]` or `#[tokio::test]` cases near changed code. Name tests after observable behavior, such as `rejects_path_outside_workdir`. Use `insta::assert_json_snapshot!` for stable serialization contracts; review changed `.snap` files rather than accepting them mechanically. New default-path behavior should have a regression test. No numeric coverage target is defined, but affected branches and error cases should be exercised.

## Commit & Pull Request Guidelines

This checkout has no Git history, so use concise, imperative subjects with an optional crate scope, for example `ash-tools: reject symlink escapes`. Keep commits focused. Pull requests should explain the behavior change, identify affected crates, link relevant issues, and list verification commands. Include terminal captures for visible TUI changes and call out configuration or protocol compatibility impacts.

## Security & Configuration

Copy `.env.example` to `.env` for local credentials. Never commit API keys, provider tokens, logs, or generated files. Preserve the tool layer's working-directory boundary and cancellation/timeout behavior when changing file or process execution.
