"""Check cache invalidation, bounded snapshots, restoration, and phase statistics."""

import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import daemon_build_metrics as metrics


class CargoCacheTests(unittest.TestCase):
    def test_cache_key_tracks_build_inputs_and_ignores_run_identity(self):
        audit = {
            "commit": "official-alpha", "releaseProfile": {"codegen-units": 4},
            "releaseProfileOverrides": {"lto": "off"},
        }
        environment = {
            "RUSTFLAGS": "", "CARGO_PROFILE_RELEASE_LTO": "off",
            "VCToolsVersion": "14.50", "SOURCE_DIRECTORY": "D:/source",
            "CARGO_TARGET_DIR": "D:/target", "GITHUB_RUN_ID": "1", "GH_TOKEN": "first",
            "LIBSQLITE3_FLAGS": "SQLITE_DISABLE_INTRINSIC",
            "DAEMON_TARGET_CACHE_MAX_BYTES": "2147483648",
        }
        scripts = {"build.py": b"build commands"}

        def key(a=audit, diff=b"permitted source changes", compiler="rustc 1.93", env=environment,
                code=scripts):
            return metrics.cache_key(a, diff, compiler, env, code)[0]

        original = key()
        changed_environment = dict(environment, GITHUB_RUN_ID="2", GH_TOKEN="second")
        self.assertEqual(original, key(env=changed_environment))
        self.assertEqual(original, key(env={name.upper(): value for name, value in environment.items()}))
        for name in ["RUSTFLAGS", "CARGO_PROFILE_RELEASE_LTO", "VCToolsVersion",
                     "SOURCE_DIRECTORY", "CARGO_TARGET_DIR", "LIBSQLITE3_FLAGS",
                     "DAEMON_TARGET_CACHE_MAX_BYTES"]:
            with self.subTest(input=name):
                changed_environment = dict(environment)
                changed_environment[name] += "changed"
                self.assertNotEqual(original, key(env=changed_environment))
        changed_audit = copy.deepcopy(audit)
        changed_audit["commit"] = "new-official-alpha"
        for changed in [key(a=changed_audit), key(diff=b"new fix"), key(compiler="new rustc"),
                        key(code={"build.py": b"new build commands"})]:
            self.assertNotEqual(original, changed)

    def test_snapshot_keeps_complete_profiles_within_budget_and_restores_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, cache, restored = root / "target", root / "cache", root / "restored"
            contents = {
                "release/deps/macro.dll": b"h" * 2048,
                "ci-test/build/tool.exe": b"t" * 2048,
                f"{metrics.TARGET}/release/deps/large.rlib": b"r" * 8000,
                f"{metrics.TARGET}/ci-test/deps/large.exe": b"t" * 9000,
            }
            for name, data in contents.items():
                path = target / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
            report = metrics.snapshot(target, cache, "exact-inputs", 10000)
            self.assertEqual(report["selectedProfiles"], ["release", "ci-test"])
            self.assertEqual(report["selectedBytes"], 4096)
            self.assertEqual(report["excludedProfiles"][f"{metrics.TARGET}/release"], "budget")
            self.assertEqual(report["target"]["bytes"], sum(map(len, contents.values())))
            self.assertLessEqual(metrics.output_sizes(cache)["bytes"], 10000)
            metrics.restore(cache, restored, "exact-inputs")
            for name in ["release/deps/macro.dll", "ci-test/build/tool.exe"]:
                self.assertEqual((restored / name).read_bytes(), contents[name])
            self.assertFalse((restored / metrics.TARGET).exists())
            for name, data in contents.items():
                self.assertEqual((target / name).read_bytes(), data)
            skipped = metrics.snapshot(target, root / "too-small", "exact-inputs", 5000)
            self.assertEqual(skipped["selectedProfiles"], [])
            self.assertFalse((root / "too-small").exists())

    def test_wrong_key_and_invalid_profile_fail_before_moving_outputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache, target = root / "cache", root / "target"
            source = cache / "target/release/deps/library.rlib"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"cached")
            manifest = {"key": "original", "selectedProfiles": ["release"]}
            metrics.save(cache / "manifest.json", manifest)
            with self.assertRaisesRegex(AssertionError, "input key"):
                metrics.restore(cache, target, "different")
            self.assertEqual(source.read_bytes(), b"cached")
            self.assertFalse(target.exists())
            manifest["selectedProfiles"] = ["../../outside"]
            metrics.save(cache / "manifest.json", manifest)
            with self.assertRaises(AssertionError):
                metrics.restore(cache, target, "original")
            self.assertEqual(source.read_bytes(), b"cached")

    def test_phase_metrics_preserve_separate_timing_reports_and_cache_deltas(self):
        before = {
            "compile_requests": 5, "cache_hits": {"counts": {"Rust": 3}},
            "cache_misses": {"counts": {"Rust": 2}},
            "cache_read_hit_duration": {"secs": 1, "nanos": 500_000_000},
        }
        after = {
            "compile_requests": 9, "cache_hits": {"counts": {"Rust": 6}},
            "cache_misses": {"counts": {"Rust": 3}},
            "cache_read_hit_duration": {"secs": 2, "nanos": 250_000_000},
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, output = root / "target", root / "artifacts"
            timing = target / "cargo-timings/cargo-timing.html"
            timing.parent.mkdir(parents=True)
            with mock.patch.object(metrics, "sccache_stats", return_value=after):
                for phase in ["ci-test", "release"]:
                    timing.write_text(phase, encoding="utf-8")
                    events = output / ("test-build.jsonl" if phase == "ci-test" else "build.jsonl")
                    events.parent.mkdir(parents=True, exist_ok=True)
                    events.write_text("\n".join(json.dumps(event) for event in [
                        {"reason": "compiler-artifact", "fresh": True},
                        {"reason": "compiler-artifact", "fresh": False},
                        {"reason": "build-finished", "success": True},
                    ]), encoding="utf-8")
                    metrics.record_phase(phase, target, output, {}, before, 12.5)
                    self.assertEqual(
                        (output / "timings" / f"{phase}-cargo-timings.html").read_text(), phase,
                    )
            report = json.loads((output / "release-metrics.json").read_text())
            self.assertEqual(report["sccache"]["compile_requests"], 4)
            self.assertEqual(report["sccache"]["cache_hits"], {"Rust": 3})
            self.assertEqual(report["sccache"]["cache_read_hit_duration"], 0.75)
            self.assertEqual(report["cargoArtifacts"], {"fresh": 1, "rebuilt": 1})


if __name__ == "__main__":
    unittest.main()
