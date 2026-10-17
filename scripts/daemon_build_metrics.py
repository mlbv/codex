"""Measure hosted Cargo builds and retain bounded, complete output directories."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess


TARGET = "x86_64-pc-windows-msvc"
MANIFEST_BUDGET = 4096
PROFILES = ("release", "ci-test", f"{TARGET}/release", f"{TARGET}/ci-test")
INPUT_ENV = {
    "CARGO_INCREMENTAL", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS", "RUSTC_WRAPPER",
    "VCTOOLSVERSION", "WINDOWSSDKVERSION", "IMAGEOS", "IMAGEVERSION",
    "INCLUDE", "LIB", "LIBPATH", "SOURCE_DIRECTORY", "CARGO_TARGET_DIR", "CARGO_HOME",
    "CC", "CFLAGS", "CXX", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "AR", "LIBSQLITE3_FLAGS",
    "DAEMON_TARGET_CACHE_MAX_BYTES",
}


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")


def target_directory():
    return Path(os.environ["RUNNER_TEMP"]) / "cargo-target"


def cache_directory():
    return Path(os.environ["RUNNER_TEMP"]) / "daemon-cargo-cache"


def cache_key(audit, source_diff, compiler, environment, scripts):
    environment = {key.upper(): value for key, value in environment.items()}
    inputs = {
        "upstream": audit["commit"],
        "sourceDiff": hashlib.sha256(source_diff).hexdigest(),
        "compiler": compiler,
        "target": TARGET,
        "releaseProfile": audit["releaseProfile"],
        "releaseProfileOverrides": audit["releaseProfileOverrides"],
        "environment": {
            key: value for key, value in sorted(environment.items())
            if key in INPUT_ENV or key.startswith(("CARGO_PROFILE_", "CARGO_BUILD_", "CARGO_TARGET_"))
        },
        "scripts": {key: hashlib.sha256(value).hexdigest() for key, value in scripts.items()},
    }
    encoded = json.dumps(inputs, sort_keys=True).encode()
    return "daemon-target-v1-" + hashlib.sha256(encoded).hexdigest(), inputs


def output_sizes(target):
    groups = {name: {"bytes": 0, "files": 0} for name in PROFILES}
    total_bytes = total_files = 0
    for path in target.rglob("*"):
        if not path.is_file():
            continue
        size = path.stat().st_size
        total_bytes += size
        total_files += 1
        relative = path.relative_to(target).as_posix()
        for name in PROFILES:
            if relative.startswith(name + "/"):
                groups[name]["bytes"] += size
                groups[name]["files"] += 1
                break
    return {"bytes": total_bytes, "files": total_files, "profiles": groups}


def select_profiles(sizes, maximum):
    assert maximum > 0, "Cache limit must be positive"
    selected = []
    used = 0
    for name in PROFILES:
        size = sizes["profiles"][name]
        if size["files"] and used + size["bytes"] <= maximum:
            selected.append(name)
            used += size["bytes"]
    return selected, used


def snapshot(target, cache, key, maximum):
    assert not cache.exists() or not any(cache.iterdir()), "Cache staging directory is not empty"
    assert maximum > MANIFEST_BUDGET, "Cache limit must leave room for its manifest"
    sizes = output_sizes(target)
    selected, used = select_profiles(sizes, maximum - MANIFEST_BUDGET)
    report = {
        "key": key, "maximumBytes": maximum, "selectedBytes": used,
        "selectedProfiles": selected,
        "excludedProfiles": {
            name: "budget" if sizes["profiles"][name]["files"] else "not-built"
            for name in PROFILES if name not in selected
        },
        "target": sizes,
    }
    if selected:
        assert len(json.dumps(report, indent=2).encode()) <= MANIFEST_BUDGET
        for name in selected:
            shutil.copytree(
                target / name, cache / "target" / name, copy_function=os.link, symlinks=True,
            )
        save(cache / "manifest.json", report)
    return report


def restore(cache, target, key):
    manifest = json.loads((cache / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["key"] == key, "Cargo cache input key does not match"
    profiles = manifest["selectedProfiles"]
    assert profiles and len(profiles) == len(set(profiles)) and set(profiles) <= set(PROFILES)
    for name in profiles:
        source = (cache / "target" / name).resolve(strict=True)
        destination = (target / name).resolve()
        assert source.is_relative_to(cache.resolve())
        assert destination.is_relative_to(target.resolve())
        assert not destination.exists(), destination
    for name in profiles:
        destination = target / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        os.replace(cache / "target" / name, destination)
    return manifest


def sccache_stats(environment):
    return json.loads(subprocess.check_output(
        ["sccache", "--show-stats", "--stats-format=json"], env=environment, text=True,
        encoding="utf-8",
    ))["stats"]


def stats_delta(before, after):
    result = {}
    for key, value in after.items():
        if isinstance(value, int):
            result[key] = value - before.get(key, 0)
        elif isinstance(value, dict) and "counts" in value:
            previous = before.get(key, {}).get("counts", {})
            result[key] = {
                name: count - previous.get(name, 0) for name, count in value["counts"].items()
            }
        elif isinstance(value, dict) and "secs" in value:
            previous = before.get(key, {"secs": 0, "nanos": 0})
            result[key] = (
                value["secs"] - previous["secs"]
                + (value["nanos"] - previous["nanos"]) / 1_000_000_000
            )
    return result


def record_phase(phase, target, output, environment, before, elapsed):
    after = sccache_stats(environment)
    events = output / ("test-build.jsonl" if phase == "ci-test" else "build.jsonl")
    fresh = rebuilt = 0
    for line in events.read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact":
            if event["fresh"]:
                fresh += 1
            else:
                rebuilt += 1
    report = {
        "phase": phase, "elapsedSeconds": elapsed,
        "cargoArtifacts": {"fresh": fresh, "rebuilt": rebuilt},
        "sccache": stats_delta(before, after), "target": output_sizes(target),
    }
    reports = list(target.rglob("cargo-timing.html"))
    assert reports, "Cargo did not produce the requested timing report"
    for path in reports:
        suffix = path.parent.relative_to(target).as_posix().replace("/", "-")
        destination = output / "timings" / f"{phase}-{suffix}.html"
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, destination)
    save(output / f"{phase}-metrics.json", report)
    print(json.dumps(report), flush=True)


def configure(source, output):
    assert int(os.environ["DAEMON_TARGET_CACHE_MAX_BYTES"]) > MANIFEST_BUDGET
    audit = json.loads((output / "source-audit.json").read_text(encoding="utf-8"))
    compiler = subprocess.check_output(
        ["rustc", "-vV"], cwd=source / "codex-rs", text=True, encoding="utf-8",
    )
    scripts = {
        name: Path(__file__).with_name(name).read_bytes()
        for name in ["daemon_release.py", "daemon_build_metrics.py"]
    }
    environment = dict(os.environ)
    environment["SOURCE_DIRECTORY"] = str(source.resolve())
    environment["CARGO_TARGET_DIR"] = str(target_directory().resolve())
    key, inputs = cache_key(
        audit, (output / "source.diff").read_bytes(), compiler, environment, scripts,
    )
    save(output / "cargo-cache-inputs.json", {
        "key": key, "inputs": inputs, "logicalCpuCount": os.cpu_count(),
        "platform": platform.platform(),
    })
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
        print("key=" + key, file=stream)
        print("path=" + str(cache_directory()), file=stream)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["configure", "restore", "snapshot", "measure"])
    parser.add_argument("--source", type=Path, default=Path("source"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assert os.environ.get("GITHUB_ACTIONS") == "true"
    assert os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
    output = args.output.resolve()
    if args.command == "configure":
        configure(args.source.resolve(), output)
    elif args.command == "measure":
        sizes = output_sizes(target_directory())
        save(output / "cargo-output-sizes.json", sizes)
        print(json.dumps(sizes), flush=True)
    else:
        key = json.loads((output / "cargo-cache-inputs.json").read_text(encoding="utf-8"))["key"]
        if args.command == "restore":
            report = restore(cache_directory(), target_directory(), key)
            save(output / "cargo-cache-restored.json", report)
        else:
            report = snapshot(
                target_directory(), cache_directory(), key,
                int(os.environ["DAEMON_TARGET_CACHE_MAX_BYTES"]),
            )
            save(output / "cargo-cache-snapshot.json", report)
            with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
                print("cacheable=" + str(bool(report["selectedProfiles"])).lower(), file=stream)
        print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
