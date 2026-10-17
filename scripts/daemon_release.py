"""Build and validate a patched official daemon on GitHub-hosted Windows runners."""

import argparse
import contextlib
from datetime import datetime, timezone
import difflib
from email.utils import parsedate_to_datetime
import hashlib
from http.client import IncompleteRead
import json
import os
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import sys
import tarfile
import time
import tomllib
import traceback
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

import daemon_build_metrics as build_metrics

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"
RELEASE_TAG = re.compile(r"rust-v\d+\.\d+\.\d+(?:-alpha\.\d+(?:\.\d+)*)?")
FIX_PATHS = {
    "token-accounting": {
        "codex-rs/core/src/state/session.rs",
        "codex-rs/core/src/tasks/regular.rs",
        "codex-rs/core/src/session/turn.rs",
        "codex-rs/core/src/compact.rs",
        "codex-rs/app-server/tests/suite/v2/thread_resume.rs",
        "codex-rs/core/src/state/session_tests.rs",
        "codex-rs/thread-store/src/local/mod.rs",
        "codex-rs/core/src/session/mod.rs",
        "codex-rs/protocol/src/protocol.rs",
        "codex-rs/core/src/session/guardian_checkpoint_tests.rs",
        "codex-rs/state/src/runtime/threads.rs",
        "codex-rs/core/src/session/guardian_checkpoint.rs",
        "codex-rs/history/src/rollout_payload.rs",
        "codex-rs/app-server/tests/common/rollout.rs",
        "codex-rs/external-agent-migration/src/sessions/export.rs",
        "codex-rs/core/src/session/tests.rs",
        "codex-rs/thread-store/src/local/thread_history_materialization_tests.rs",
        "codex-rs/app-server/src/request_processors/token_usage_replay.rs",
        "codex-rs/core/src/context_manager/history.rs",
        "codex-rs/app-server/src/bespoke_event_handling.rs",
        "codex-rs/core/src/context_manager/history_tests.rs",
        "codex-rs/rollout/src/recorder_tests.rs",
    },
    "overload-retry": {
        "codex-rs/protocol/src/error.rs",
        "codex-rs/protocol/src/error_tests.rs",
    },
}
ALLOWED = set().union(*FIX_PATHS.values())

TESTS = [
    ("codex_core", "context_manager::history::tests::total_token_usage", 4),
    ("codex_core", "context_manager::history::tests::non_last_reasoning_tokens", 2),
    ("codex_core", "recompute_token_usage", 2),
    ("codex_core", "record_token_usage_info_notifies_extension_contributors", 2),
    ("codex_core", "token_usage_reasoning_coverage", 3),
    ("codex_core", "record_initial_history_seeds_token_info_from_rollout", 1),
    ("codex_protocol", "retryability_preserves_error_details_distinctions", 1),
    (
        "codex_protocol",
        "retry_delay_distinguishes_server_advice_backoff_and_terminal_errors",
        1,
    ),
]


def save(path, value):
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")


def retry_request(operation, description):
    attempts = 4
    for attempt in range(attempts):
        try:
            return operation()
        except (HTTPError, URLError, TimeoutError, IncompleteRead, ConnectionResetError) as error:
            retry_after = None
            if isinstance(error, HTTPError):
                if error.code not in {500, 502, 503, 504}:
                    raise
                retry_after = error.headers.get("Retry-After")
                reason = f"HTTP {error.code}"
            else:
                cause = error.reason if isinstance(error, URLError) else error
                if isinstance(cause, TimeoutError):
                    reason = "request timeout"
                elif isinstance(cause, IncompleteRead):
                    reason = "incomplete response body"
                elif isinstance(cause, ConnectionResetError):
                    reason = "connection reset"
                else:
                    raise
            if attempt == attempts - 1:
                raise
            if isinstance(error, HTTPError):
                error.close()
            delay = 2 ** (attempt + 1)
            if retry_after is not None:
                try:
                    delay = max(0, int(retry_after))
                except ValueError:
                    delay = max(
                        0,
                        (
                            parsedate_to_datetime(retry_after)
                            - datetime.now(timezone.utc)
                        ).total_seconds(),
                    )
            print(
                f"{description}: {reason}; retry {attempt + 2}/{attempts} "
                f"in {delay:g}s",
                flush=True,
            )
            time.sleep(delay)


def github(path):
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "codex-daemon-build",
    }
    token = os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = "Bearer " + token

    def request():
        with urlopen(
            Request(
                "https://api.github.com/repos/openai/codex/" + path, headers=headers
            ),
            timeout=60,
        ) as response:
            return json.load(response)

    return retry_request(request, "GitHub release metadata")


def git(source, *args, index=None):
    environment = dict(os.environ)
    if index is not None:
        environment["GIT_INDEX_FILE"] = str(index)
    return subprocess.check_output(
        ["git", "-c", "core.safecrlf=false", "-C", str(source), *args],
        env=environment,
    )


def daemon_source_fix(output):
    fork_commit = git(ROOT, "rev-parse", "HEAD").decode().strip()
    parents = git(ROOT, "show", "-s", "--format=%P", fork_commit).decode().split()
    assert len(parents) == 1, "Expected one fork commit above upstream"
    base = parents[0]
    github("commits/" + base)
    changed = set(
        git(ROOT, "diff", "--name-only", base, fork_commit).decode().splitlines()
    )
    files = sorted(changed & ALLOWED)
    assert all(set(files) & paths for paths in FIX_PATHS.values()), (
        "Expected token-accounting and overload-retry source changes"
    )
    index = output / "daemon-source.index"
    git(ROOT, "read-tree", base, index=index)
    for path in files:
        entry = git(ROOT, "ls-tree", fork_commit, "--", path).decode().strip()
        metadata, name = entry.split("\t", 1)
        mode, kind, blob = metadata.split()
        assert mode == "100644" and kind == "blob" and name == path, path
        git(ROOT, "update-index", "--cacheinfo", f"{mode},{blob},{path}", index=index)
    tree = git(ROOT, "write-tree", index=index).decode().strip()
    author = git(ROOT, "show", "-s", "--format=%an", fork_commit).decode().strip()
    email = git(ROOT, "show", "-s", "--format=%ae", fork_commit).decode().strip()
    integration_commit = (
        git(
            ROOT,
            "-c",
            f"user.name={author}",
            "-c",
            f"user.email={email}",
            "commit-tree",
            tree,
            "-p",
            base,
            "-m",
            "Integrate permitted daemon source changes",
        )
        .decode()
        .strip()
    )
    return {
        "forkCommit": fork_commit,
        "upstreamBase": base,
        "integrationCommit": integration_commit,
        "integrationTree": tree,
        "files": files,
    }


def run_parity_fixture(
    source, output, package, transport, label, *, startup_only=False
):
    from daemon_parity_process import run_fixture

    parity = output / "parity"
    log = parity / f"{label}-{transport}.worker.log"
    arguments = [
        "--package",
        str(package),
        "--transport",
        transport,
        "--label",
        label,
        "--catalog-root",
        str(source),
        "--output-dir",
        str(parity),
    ]
    if startup_only:
        arguments.append("--preflight")
    config = parity / f"{label}-{transport}.worker.json"
    save(
        config,
        {
            "arguments": arguments,
            "log": str(log),
            "temporaryRoot": os.environ["DAEMON_PARITY_TEMP"],
        },
    )
    run_fixture(config, log)


def fixture_worker(config_path):
    import ctypes

    config = json.loads(config_path.read_text(encoding="utf-8"))
    with (
        Path(config["log"]).open("w", encoding="utf-8") as log,
        contextlib.redirect_stdout(log),
        contextlib.redirect_stderr(log),
    ):
        try:
            assert not ctypes.windll.shell32.IsUserAnAdmin(), (
                "Parity fixtures require a standard user token"
            )
            os.environ["RUNNER_TEMP"] = config["temporaryRoot"]
            temporary = Path(config["temporaryRoot"]) / "temp"
            temporary.mkdir(exist_ok=True)
            os.environ["TEMP"] = str(temporary)
            os.environ["TMP"] = str(temporary)
            fixture = ROOT / "scripts/daemon_wire_fixture.py"
            sys.argv = [str(fixture), *config["arguments"]]
            runpy.run_path(str(fixture), run_name="__main__")
        except BaseException:
            traceback.print_exc()
            raise


def preflight(source, output, release):
    for transport in ["http", "websocket"]:
        run_parity_fixture(source, output, output / "official", transport, "preflight")


def resolve(output):
    tag = os.environ.get("UPSTREAM_TAG", "").strip()
    if tag:
        if not RELEASE_TAG.fullmatch(tag):
            raise ValueError("Expected a published upstream rust-v release tag")
        release = github("releases/tags/" + tag)
    else:
        release = github("releases/latest")
        assert not release["prerelease"], "Default release must be stable"
    assert not release["draft"] and release["published_at"], "Release must be published"
    assert RELEASE_TAG.fullmatch(release["tag_name"]), "Expected an upstream rust-v release tag"
    tag = release["tag_name"]
    version = tag.removeprefix("rust-v")
    asset_name = f"codex-package-{TARGET}.tar.gz"
    for asset in release["assets"]:
        if asset["name"] == asset_name:
            break
    else:
        raise RuntimeError(
            f"Published upstream release {release['tag_name']} has no {asset_name}. "
            "An official Windows package is required for daemon validation; "
            "select an upstream release that publishes this asset. "
            f"Release: {release['html_url']}"
        )
    assert re.fullmatch(r"sha256:[a-f0-9]{64}", asset["digest"]), (
        "Official SHA-256 digest is required"
    )
    revision = github("git/ref/tags/" + tag)["object"]
    while revision["type"] == "tag":
        revision = github("git/tags/" + revision["sha"])["object"]
    assert revision["type"] == "commit", "Release tag must resolve to a commit"
    save(
        output / "release.json",
        {
            "version": version,
            "tag": release["tag_name"],
            "commit": revision["sha"],
            "publishedAt": release["published_at"],
            "url": release["html_url"],
            "asset": asset,
        },
    )
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
        stream.write(f"tag={tag}\nversion={version}\ncommit={revision['sha']}\n")
    print(f"Selected {release['tag_name']} published {release['published_at']}")


def prepare(source, output, release):
    manifest = tomllib.loads(
        (source / "codex-rs/Cargo.toml").read_text(encoding="utf-8")
    )
    assert manifest["workspace"]["package"]["version"] == release["version"]
    assert git(source, "rev-parse", "HEAD").decode().strip() == release["commit"]
    assert not git(source, "status", "--porcelain"), "Official source must start clean"
    fix = daemon_source_fix(output)
    assert fix["upstreamBase"] == release["commit"], (
        "Rebase the fork onto the selected published upstream release tag"
    )
    git(source, "fetch", "--no-tags", str(ROOT), fix["integrationCommit"])
    git(source, "cherry-pick", "--no-commit", fix["integrationCommit"])
    lock = source / "codex-rs/Cargo.lock"
    before = tomllib.loads(lock.read_text(encoding="utf-8"))
    run_logged(
        ["cargo", "update", "--workspace"],
        output / "lockfile.log",
        source,
        runner_environment(),
    )
    after = tomllib.loads(lock.read_text(encoding="utf-8"))
    assert len(before["package"]) == len(after["package"])
    for a, b in zip(before["package"], after["package"]):
        if "source" in a:
            assert a == b, a["name"]
        else:
            assert b["version"] == release["version"]
            assert {k: v for k, v in a.items() if k != "version"} == {
                k: v for k, v in b.items() if k != "version"
            }
    changed = set(git(source, "diff", "HEAD", "--name-only").decode().splitlines())
    expected = set(fix["files"])
    assert expected <= changed <= expected | {"codex-rs/Cargo.lock"}, changed
    (output / "source.diff").write_bytes(git(source, "diff", "HEAD", "--binary"))
    save(
        output / "source-audit.json",
        {
            "tag": release["tag"],
            "version": release["version"],
            "commit": git(source, "rev-parse", "HEAD").decode().strip(),
            "changedFiles": sorted(changed),
            "externalDependenciesUnchanged": True,
            "wireSourcesMatchOfficial": True,
            "releaseProfile": manifest["profile"]["release"],
            "releaseProfileOverrides": {"lto": os.environ["CARGO_PROFILE_RELEASE_LTO"]}
            if "CARGO_PROFILE_RELEASE_LTO" in os.environ
            else {},
            "sourceFix": fix,
        },
    )
    archive = output / release["asset"]["name"]

    def download():
        with (
            urlopen(release["asset"]["browser_download_url"], timeout=120) as response,
            archive.open("wb") as stream,
        ):
            shutil.copyfileobj(response, stream)

    retry_request(download, "Official package download")
    with archive.open("rb") as stream:
        digest = "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()
    assert digest == release["asset"]["digest"], "Official package digest mismatch"
    with tarfile.open(archive) as stream:
        stream.extractall(output / "official", filter="data")


def runner_environment():
    if (
        os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
    ):
        raise RuntimeError("Compilation is restricted to GitHub-hosted runners")
    return dict(os.environ)


def build_environment(source):
    env = runner_environment()
    env["CODEX_REPO_ROOT"] = str(source)
    env["CARGO_TARGET_DIR"] = str(build_metrics.target_directory())
    env["STABLE_GIT_COMMIT"] = git(source, "rev-parse", "HEAD").decode().strip()
    # Resolve V8 with the selected release's own checksum manifests and downloader.
    os.environ["CODEX_REPO_ROOT"] = str(source)
    sys.path.insert(0, str(source / "scripts"))
    from codex_package.targets import TargetSpec
    from codex_package.v8 import resolve_codex_v8_cargo_env

    env.update(
        retry_request(
            lambda: resolve_codex_v8_cargo_env(
                TargetSpec(TARGET, True, False, "windows-x86_64")
            ),
            "Official V8 artifact download",
        )
    )
    return env


def run_logged(command, log_path, source, env, event_path=None):
    print("+ " + " ".join(command), flush=True)
    with log_path.open("w", encoding="utf-8") as log:
        events = event_path.open("w", encoding="utf-8") if event_path else None
        try:
            with subprocess.Popen(
                command,
                cwd=source / "codex-rs",
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
            ) as process:
                for line in process.stdout:
                    log.write(line)
                    if events is None:
                        print(line, end="", flush=True)
                        continue
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        print(line, end="", flush=True)
                        continue
                    events.write(line)
                    if event.get("reason") == "compiler-message":
                        print(event["message"]["rendered"] or line, end="", flush=True)
                    elif event.get("reason") == "compiler-artifact" and event.get(
                        "executable"
                    ):
                        print("Test executable: " + event["executable"], flush=True)
                returncode = process.wait()
                if returncode:
                    raise subprocess.CalledProcessError(returncode, command)
        finally:
            if events is not None:
                events.close()


def build(source, output, release):
    env = build_environment(source)
    cache_before = build_metrics.sccache_stats(env)
    started = time.monotonic()
    package = output / "package"
    binaries = [
        "codex",
        "codex-code-mode-host",
        "codex-command-runner",
        "codex-windows-sandbox-setup",
    ]
    build_args = [argument for binary in binaries for argument in ["--bin", binary]]
    binaries_dir = Path(env["CARGO_TARGET_DIR"]) / TARGET / "release"
    run_logged(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--timings",
            "--message-format=json-render-diagnostics",
            "--target",
            TARGET,
            *build_args,
        ],
        output / "build.log",
        source,
        env,
        event_path=output / "build.jsonl",
    )
    build_metrics.record_phase(
        "release", Path(env["CARGO_TARGET_DIR"]), output, env, cache_before,
        time.monotonic() - started,
    )
    run_logged(
        [
            sys.executable,
            str(source / "scripts/build_codex_package.py"),
            "--target",
            TARGET,
            "--cargo-profile",
            "release",
            "--strip",
            "none",
            "--package-version",
            release["version"],
            "--package-dir",
            str(package),
            "--entrypoint-bin",
            str(binaries_dir / "codex.exe"),
            "--code-mode-host-bin",
            str(binaries_dir / "codex-code-mode-host.exe"),
            "--codex-command-runner-bin",
            str(binaries_dir / "codex-command-runner.exe"),
            "--codex-windows-sandbox-setup-bin",
            str(binaries_dir / "codex-windows-sandbox-setup.exe"),
            "--rg-bin",
            str(output / "official/codex-path/rg.exe"),
        ],
        output / "package.log",
        source,
        env,
    )
    voice = output / "official/codex-resources/voice"
    if voice.exists():
        shutil.copytree(voice, package / "codex-resources/voice")
    version = (
        subprocess.check_output([str(package / "bin/codex.exe"), "--version"])
        .decode()
        .strip()
    )
    assert version == "codex-cli " + release["version"], version


def test(source, output, release):
    env = build_environment(source)
    cache_before = build_metrics.sccache_stats(env)
    started = time.monotonic()
    run_logged(
        [
            "cargo",
            "test",
            "--locked",
            "--profile",
            "ci-test",
            "--timings",
            "--target",
            TARGET,
            "--lib",
            "-p",
            "codex-core",
            "-p",
            "codex-protocol",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ],
        output / "test-build.log",
        source,
        env,
        event_path=output / "test-build.jsonl",
    )
    build_metrics.record_phase(
        "ci-test", Path(env["CARGO_TARGET_DIR"]), output, env, cache_before,
        time.monotonic() - started,
    )
    executables = {}
    for line in (output / "test-build.jsonl").read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact" and event.get("executable"):
            executables[event["target"]["name"]] = event["executable"]
    results = []
    for crate, test_filter, expected in TESTS:
        log_path = output / ("test-" + test_filter.rsplit("::", 1)[-1] + ".log")
        print("Running " + test_filter, flush=True)
        result = subprocess.run(
            [executables[crate], test_filter, "--nocapture"],
            cwd=source / "codex-rs",
            env=env,
            capture_output=True,
            timeout=120,
        )
        text = (result.stdout + result.stderr).decode(errors="replace")
        log_path.write_text(text, encoding="utf-8")
        print(text, end="", flush=True)
        result.check_returncode()
        match = re.search(r"test result: ok\. (\d+) passed; (\d+) failed", text)
        assert match and int(match[1]) == expected, text
        results.append({"filter": test_filter, "passed": expected})
        save(output / "targeted-tests.json", results)


def normalize(capture):
    identities = {}
    uuid = re.compile(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
    )

    def visit(value, key=""):
        if key in {
            "create_time",
            "turn_started_at_unix_ms",
            "x-codex-ws-stream-request-start-ms",
        }:
            return "<timestamp>"
        if key in {"sec-websocket-key", "content-length"}:
            return "<" + key + ">"
        if isinstance(value, dict):
            return {k: visit(value[k], k) for k in sorted(value)}
        if isinstance(value, list):
            return [visit(v) for v in value]
        if isinstance(value, str):
            if key == "host":
                assert re.fullmatch(r"127\.0\.0\.1:\d+", value), value
                return "<fixture-backend>"
            if key == "x-codex-turn-metadata":
                return visit(json.loads(value))
            value = value.replace(capture["scratch"], "<fixture>").replace(
                capture["scratch"].replace("\\", "/"), "<fixture>"
            )
            value = re.sub(
                r"Wall time \d+(?:\.\d+)? seconds", "Wall time <elapsed> seconds", value
            )
            return uuid.sub(
                lambda m: identities.setdefault(m[0], f"<id-{len(identities)}>"), value
            )
        return value

    return visit(capture)


def verify(source, output, release):
    audit = json.loads((output / "source-audit.json").read_text(encoding="utf-8"))
    assert set(git(source, "diff", "HEAD", "--name-only").decode().splitlines()) == set(
        audit["changedFiles"]
    )
    assert (
        git(source, "diff", "HEAD", "--binary") == (output / "source.diff").read_bytes()
    ), "Source changed after its audit"
    results = []
    for transport in ["http", "websocket"]:
        captures = []
        for label, package in [
            ("official", output / "official"),
            ("patched", output / "package"),
        ]:
            run_parity_fixture(source, output, package, transport, label)
            capture = json.loads(
                (
                    output
                    / "parity"
                    / f"metadata-wire-{transport}-{label}.requests.json"
                ).read_text(encoding="utf-8")
            )
            captures.append(normalize(capture))
        difference = "\n".join(
            difflib.unified_diff(
                json.dumps(captures[0], indent=2).splitlines(),
                json.dumps(captures[1], indent=2).splitlines(),
                fromfile="official",
                tofile="patched",
            )
        )
        (output / (transport + "-request-parity.diff")).write_text(
            difference, encoding="utf-8"
        )
        assert not difference, (
            f"{transport} request parity failed; see the diff artifact"
        )
        results.append(
            {
                "transport": transport,
                "requests": len(captures[0]["requests"]),
                "payloadsAndHeadersEqual": True,
                "restartAndResume": True,
            }
        )
        save(output / "request-parity.json", results)
    package = output / "package"
    hashes = {}
    for path in package.rglob("*"):
        if path.is_file():
            with path.open("rb") as stream:
                hashes[path.relative_to(package).as_posix()] = hashlib.file_digest(
                    stream, "sha256"
                ).hexdigest()
    save(output / "package-hashes.json", hashes)
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as stream:
        stream.write(
            f"Built {release['tag']} at {release['commit']} with release LTO "
            f"{audit['releaseProfileOverrides'].get('lto', audit['releaseProfile']['lto'])}.\n\n"
            f"The source audit, {sum(expected for _, _, expected in TESTS)} focused accounting/retry tests, and official-binary "
            "HTTP/WebSocket request comparisons passed, including restart/resume. "
            "No live model requests were made. Installation is separate.\n"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=[
            "resolve",
            "prepare",
            "preflight",
            "build",
            "test",
            "verify",
            "fixture-worker",
        ],
    )
    parser.add_argument("--source", type=Path, default=Path("source"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--worker-config", type=Path)
    args = parser.parse_args()
    output, source = args.output.resolve(), args.source.resolve()
    output.mkdir(parents=True, exist_ok=True)
    try:
        if args.command == "fixture-worker":
            fixture_worker(args.worker_config)
        elif args.command == "resolve":
            resolve(output)
        else:
            release = json.loads((output / "release.json").read_text(encoding="utf-8"))
            {
                "prepare": prepare,
                "preflight": preflight,
                "build": build,
                "test": test,
                "verify": verify,
            }[args.command](source, output, release)
    except Exception as error:
        save(
            output / "failure.json",
            {
                "stage": args.command,
                "errorType": type(error).__name__,
                "error": str(error),
            },
        )
        raise


if __name__ == "__main__":
    main()
