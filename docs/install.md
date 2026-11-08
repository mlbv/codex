## Installing & building

### System requirements

| Requirement                 | Details                                                         |
| --------------------------- | --------------------------------------------------------------- |
| Operating systems           | macOS 12+, Ubuntu 20.04+/Debian 10+, or Windows 11 **via WSL2** |
| Git (optional, recommended) | 2.23+ for built-in PR helpers                                   |
| RAM                         | 4-GB minimum (8-GB recommended)                                 |

### DotSlash

The GitHub Release also contains a [DotSlash](https://dotslash-cli.com/) file for the Codex CLI named `codex`. Using a DotSlash file makes it possible to make a lightweight commit to source control to ensure all contributors use the same version of an executable, regardless of what platform they use for development.

### Fork products

The fork builds two Windows products from the same commit, directly based on a published upstream release. `master` mirrors upstream `main`; each `patch/<version>` branch contains one combined patch commit directly above the corresponding `rust-v<version>` tag. The active patch branch is the repository default. Compilation and tests run on GitHub-hosted runners in `mlbv/codex`. The package workflows accept a published release tag; an empty dispatch input selects the latest stable release. A branch push selects the release matching the workspace version. Both reject a source revision that is not directly based on that release.

- `daemon-package.yml` produces `codex-<version>-windows-daemon`, including the CLI, app-server, execution server, code-mode host and Windows sandbox helpers. The four [Claude Code integration switches](claude-code.md) are available and default to false.
- `messages-api-package.yml` produces `codex-messages-api-<version>-windows`, containing the CLI executable with the `messages-api` command. Native Claude Code integrations are excluded from this build.

The products use the upstream release profile with `CARGO_PROFILE_RELEASE_LTO=off`. Each package retains the upstream version. Cargo graphs and compiler events are audited for feature isolation. The daemon's full source diff is retained for review; upstream model catalogs and prompts must match the selected release. Before upload, the daemon package is compared with the official binary through the built-in OpenAI provider using fixture ChatGPT credentials, the unmodified official catalog, HTTP and WebSocket requests, and thread continuation after restart.

Run the corresponding package workflow for an installable artifact. The native and API CI workflows also exercise their separate feature sets on Linux and Windows. Package builds and validation evidence are uploaded independently. Building a package does not install it.

### Upstream source development

The following commands describe upstream source development. Use the hosted workflows above for this fork.

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/openai/codex.git
cd codex/codex-rs

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup component add rustfmt
rustup component add clippy
# Install helper tools used by the workspace justfile:
cargo install --locked just
# DotSlash fetches pinned development tools such as buildifier on first use.
cargo install --locked dotslash
# Install nextest for the `just test` helper.
cargo install --locked cargo-nextest

# Build Codex.
cargo build

# Launch the TUI with a sample prompt.
cargo run --bin codex -- "explain this codebase to me"

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p <crate-you-touched>

# Run the relevant tests (project-specific is fastest), for example:
just test -p codex-tui
# `just test` runs the test suite via nextest:
just test
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
```

## Tracing / verbose logging

Codex is written in Rust, so it honors the `RUST_LOG` environment variable to configure its logging behavior.

The TUI records diagnostics in bounded local stores by default. Set `log_dir` explicitly to enable a plaintext TUI log for a run:

```bash
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
