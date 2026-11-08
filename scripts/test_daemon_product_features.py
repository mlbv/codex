"""Exercise product graph and compiler-feature isolation without compiling Rust."""

import json
from pathlib import Path
import tempfile
import unittest

from daemon_product_features import audit_build_features
from daemon_product_features import audit_tree


DAEMON_TREE = """0codex-cli v0.167.0 (D:\\source\\cli)|daemon
1codex-app-server v0.167.0 (D:\\source\\app-server)|claude-code
2codex-core v0.167.0 (D:\\source\\core)|claude-code
3codex-file-watcher v0.167.0|
3codex-websocket-client v0.167.0|
3sha2 v0.10.9|default,std
3codex-protocol v0.167.0|
3codex-api v0.167.0|
1codex-core v0.167.0 (D:\\source\\core)|claude-code (*)
"""

API_TREE = """0codex-cli v0.167.0 (D:\\source\\cli)|messages-api
1codex-app-server v0.167.0|messages-api
2codex-file-watcher v0.167.0|
2codex-core v0.167.0|messages-api
3codex-protocol v0.167.0|messages-api
3codex-api v0.167.0|messages-api
4codex-websocket-client v0.167.0|
1codex-messages-api v0.167.0|
2sha2 v0.10.9|default,std
"""


class ProductFeatureTests(unittest.TestCase):
    def test_native_dependencies_are_checked_on_core_edges(self):
        daemon = audit_tree(DAEMON_TREE, "daemon")
        self.assertEqual(
            daemon["coreNativeDependencies"],
            ["codex-file-watcher", "codex-websocket-client", "sha2"],
        )
        api = audit_tree(API_TREE, "messages-api")
        self.assertEqual(api["coreNativeDependencies"], [])
        self.assertIn("codex-file-watcher", api["features"])
        self.assertIn("sha2", api["features"])

    def test_daemon_rejects_api_package_or_transitive_feature(self):
        for contamination in [
            "1codex-messages-api v0.167.0|\n",
            "1codex-protocol v0.167.0|messages-api\n",
        ]:
            with self.subTest(contamination=contamination):
                with self.assertRaises(AssertionError):
                    audit_tree(DAEMON_TREE + contamination, "daemon")

    def test_api_rejects_native_feature_or_optional_core_dependency(self):
        for contaminated in [
            API_TREE.replace("|messages-api\n3codex-protocol", "|claude-code,messages-api\n3codex-protocol"),
            API_TREE.replace("3codex-protocol", "3sha2 v0.10.9|default,std\n3codex-protocol"),
            API_TREE.replace("|messages-api\n1codex-app-server", "|daemon,messages-api\n1codex-app-server"),
        ]:
            with self.subTest(tree=contaminated):
                with self.assertRaises(AssertionError):
                    audit_tree(contaminated, "messages-api")

    def test_missing_feature_or_empty_graph_cannot_pass(self):
        for tree in ["", API_TREE.replace("codex-api v0.167.0|messages-api", "codex-api v0.167.0|")]:
            with self.subTest(tree=tree):
                with self.assertRaises(AssertionError):
                    audit_tree(tree, "messages-api")

    def test_compiler_events_detect_feature_unification(self):
        features = audit_tree(DAEMON_TREE, "daemon")["features"]
        with tempfile.TemporaryDirectory() as temporary:
            events = Path(temporary) / "build.jsonl"

            def write_events():
                rows = [{
                    "reason": "compiler-artifact",
                    "target": {"name": package.replace("-", "_")},
                    "features": enabled,
                } for package, enabled in features.items()]
                rows.append({"reason": "build-finished", "success": True})
                events.write_text("\n".join(map(json.dumps, rows)), encoding="utf-8")

            write_events()
            audit_build_features(events, "daemon")
            features["codex-protocol"] = ["messages-api"]
            write_events()
            with self.assertRaisesRegex(AssertionError, "codex-protocol/messages-api"):
                audit_build_features(events, "daemon")


if __name__ == "__main__":
    unittest.main()
