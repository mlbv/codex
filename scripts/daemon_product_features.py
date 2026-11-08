"""Audit the resolved and compiled Cargo features of each product."""

import argparse
import json
from pathlib import Path
import re
import subprocess


CORE_NATIVE_DEPENDENCIES = {"codex-file-watcher", "codex-websocket-client", "sha2"}
PRODUCT_FEATURES = {
    "daemon": {
        "codex-cli": "daemon",
        "codex-core": "claude-code",
        "codex-app-server": "claude-code",
    },
    "messages-api": {
        "codex-cli": "messages-api",
        "codex-core": "messages-api",
        "codex-app-server": "messages-api",
        "codex-protocol": "messages-api",
        "codex-api": "messages-api",
    },
}


def audit_features(features, product):
    for package, required in PRODUCT_FEATURES[product].items():
        assert required in features.get(package, set()), (
            f"{product} requires {package}/{required}"
        )
    if product == "daemon":
        assert "codex-messages-api" not in features, (
            "The daemon must not depend on codex-messages-api"
        )
        forbidden = "messages-api"
    else:
        assert "codex-messages-api" in features, (
            "The API build must include codex-messages-api"
        )
        assert "daemon" not in features["codex-cli"], (
            "The API build must disable the default daemon feature"
        )
        forbidden = "claude-code"
    for package, enabled in features.items():
        assert forbidden not in enabled, f"{product} enables {package}/{forbidden}"
    return {package: sorted(enabled) for package, enabled in sorted(features.items())}


def audit_tree(tree, product):
    features = {}
    core_dependencies = set()
    ancestors = []
    for line in tree.splitlines():
        entry = re.fullmatch(r"(\d+)(\S+) v[^|]+\|([^ ]*)(?: \(\*\))?", line)
        if entry is None:
            continue
        depth, package, enabled = entry.groups()
        depth = int(depth)
        while ancestors and ancestors[-1][0] >= depth:
            ancestors.pop()
        if ancestors and ancestors[-1][1] == "codex-core":
            core_dependencies.add(package)
        ancestors.append((depth, package))
        features.setdefault(package, set()).update(filter(None, enabled.split(",")))
    checked = audit_features(features, product)
    native_dependencies = core_dependencies & CORE_NATIVE_DEPENDENCIES
    if product == "daemon":
        assert native_dependencies == CORE_NATIVE_DEPENDENCIES, (
            f"Missing native core dependencies: {CORE_NATIVE_DEPENDENCIES - native_dependencies}"
        )
    else:
        assert not native_dependencies, (
            f"The API core enables native integration dependencies: {native_dependencies}"
        )
    return {
        "product": product,
        "features": checked,
        "coreDirectDependencies": sorted(core_dependencies),
        "coreNativeDependencies": sorted(native_dependencies),
    }


def audit_feature_graph(source, product, target, output):
    command = [
        "cargo", "tree", "--locked", "--color", "never", "--target", target,
        "-p", "codex-cli", "--no-default-features",
        "--features", f"codex-cli/{product}",
        "--edges", "normal,build", "--prefix", "depth", "--format", "{p}|{f}",
    ]
    print("+ " + " ".join(command), flush=True)
    tree = subprocess.check_output(
        command, cwd=source / "codex-rs", text=True, encoding="utf-8",
    )
    (output / "product-cargo-tree.txt").write_text(tree, encoding="utf-8")
    report = audit_tree(tree, product)
    report["command"] = command
    (output / "product-graph.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8",
    )
    return report


def audit_build_features(event_path, product):
    features = {}
    for line in event_path.read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if event.get("reason") != "compiler-artifact":
            continue
        package = event["target"]["name"].replace("_", "-")
        if not package.startswith("codex-"):
            continue
        features.setdefault(package, set()).update(event["features"])
    return {"product": product, "features": audit_features(features, product)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path("."))
    parser.add_argument("--product", choices=PRODUCT_FEATURES, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--build-log", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    audit_feature_graph(args.source, args.product, args.target, args.output)
    if args.build_log is not None:
        report = audit_build_features(args.build_log, args.product)
        (args.output / "product-build-features.json").write_text(
            json.dumps(report, indent=2) + "\n", encoding="utf-8",
        )


if __name__ == "__main__":
    main()
