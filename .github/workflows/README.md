# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - Windows ARM64 nextest archives cross-compiled on Windows x64, then replayed on native Windows ARM64 shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`
  - Linux remote-env tests

## Manual daemon package

`daemon-package.yml` is the manual Windows package build for the public fork. It uses
GitHub-hosted runners, selects the latest published stable upstream release unless a tag
is supplied, and derives its version and source commit from that tag. The fork's
development manifest keeps upstream's placeholder version. Builds use the upstream
release profile with LTO disabled through `CARGO_PROFILE_RELEASE_LTO=off` by default.
Focused tests use upstream's `ci-test` profile before the package build. Both steps
stream output to the Actions log and save it in the evidence artifact. The fork keeps
one commit above a published upstream release tag, rather than tracking upstream
`main`. Explicitly selected prerelease tags must also have a published release.
The selected release commit must match the fork commit's direct parent.
The workflow isolates its permitted daemon Rust changes in an unreferenced
integration commit, then cherry-picks that onto the official release
checkout. Source audits exclude unrelated fork changes and fail on merge conflicts.
It does not maintain or apply a patch file.

Cargo registry/Git downloads and verified V8 artifacts are cached between runs.
`sccache` stores compiled Rust libraries in the GitHub Actions cache as they are
built; linking and tests still run. Cargo incremental compilation is disabled so
compiler results can be cached. The compiler-cache namespace includes embedded
skills and SQL migrations to invalidate procedural-macro inputs when they change.
Download caches are saved even after a non-cancelled build or test failure. A first
run without a matching cache still builds from scratch.
Compiler-cache idle shutdown is disabled for long compilations. Server logs are
saved as `artifacts/sccache.log` in the evidence artifact.

Cargo output caching retains complete profile directories under a 2 GiB snapshot
limit, including its manifest. Host release and test outputs take priority because
they contain procedural macros and build scripts that sccache cannot retain.
Directories exceeding the remaining budget are excluded without pruning the live
build directory. Snapshots use hard links so staging does not duplicate the build
outputs on the runner's disk. Exact cache keys cover the audited upstream source
and permitted changes, compiler, SDK/image, profile overrides, build flags, paths,
and build scripts. There are no prefix restores across different inputs.
Cargo still validates freshness after restoration; a cache hit does not guarantee
that every workspace crate can be reused after a fresh checkout.

Both compilation phases collect Cargo HTML timing reports, phase-specific sccache
counters, and Cargo's fresh/rebuilt artifact counts. The evidence artifact includes
`timings/*.html`, `ci-test-metrics.json`, `release-metrics.json`, final output sizes,
and the cache selection/restoration reports. The first instrumented run measures
the output sizes and seeds eligible profiles; a matching subsequent run is needed
to measure the benefit of restoring them.
Release-metadata reads retry truncated responses and connection resets within the
same bounded retry policy. Retry regression checks run before release selection.

Daemon fixtures run through Task Scheduler as a standard Windows user. Official-binary
HTTP and WebSocket fixtures run before compilation so fixture problems fail early. Package upload
requires the source audit, focused tests, and official-binary HTTP/WebSocket
request comparisons to pass, including restart/resume. Evidence is uploaded even
when a build fails. The workflow does not install or publish a release.

Run it from the Actions tab, or use
`gh workflow run daemon-package.yml --repo mlbv/codex` after the workflow is published.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.
