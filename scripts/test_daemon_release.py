"""Regression checks for published release selection and interrupted requests."""

from http.client import IncompleteRead
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from unittest import mock
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

import daemon_release


class ReleaseSelectionTests(unittest.TestCase):
    def resolve_release(self, tag, release):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            github_output = output / "github-output"
            with (
                mock.patch.dict(os.environ, {"UPSTREAM_TAG": tag, "GITHUB_OUTPUT": str(github_output)}),
                mock.patch.object(
                    daemon_release,
                    "github",
                    side_effect=[release, {"object": {"type": "commit", "sha": "official-commit"}}],
                ) as github,
            ):
                daemon_release.resolve(output)
            return json.loads((output / "release.json").read_text()), github.call_args_list

    def published_release(self, tag="rust-v0.164.0", prerelease=False):
        return {
            "tag_name": tag,
            "draft": False,
            "prerelease": prerelease,
            "published_at": "2026-10-23T04:48:10Z",
            "html_url": "https://github.com/openai/codex/releases/tag/" + tag,
            "assets": [{
                "name": "codex-package-x86_64-pc-windows-msvc.tar.gz",
                "digest": "sha256:" + "a" * 64,
            }],
        }

    def test_explicit_stable_release_is_accepted(self):
        selected, calls = self.resolve_release("rust-v0.164.0", self.published_release())
        self.assertEqual(selected["tag"], "rust-v0.164.0")
        self.assertEqual(selected["version"], "0.164.0")
        self.assertEqual(selected["commit"], "official-commit")
        self.assertEqual(calls[0], mock.call("releases/tags/rust-v0.164.0"))

    def test_default_selects_latest_stable_release(self):
        selected, calls = self.resolve_release("", self.published_release())
        self.assertEqual(selected["version"], "0.164.0")
        self.assertEqual(calls[0], mock.call("releases/latest"))

    def test_explicit_published_alpha_is_accepted(self):
        tag = "rust-v0.164.0-alpha.12"
        selected, _ = self.resolve_release(tag, self.published_release(tag, prerelease=True))
        self.assertEqual(selected["version"], "0.164.0-alpha.12")

    def test_unreleased_branch_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "published upstream"):
            self.resolve_release("main", self.published_release())

    def test_draft_and_unpublished_releases_are_rejected(self):
        for field, value in [("draft", True), ("published_at", None)]:
            with self.subTest(field=field):
                release = self.published_release()
                release[field] = value
                with self.assertRaisesRegex(AssertionError, "must be published"):
                    self.resolve_release("rust-v0.164.0", release)


class SourceSelectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.source = Path(temporary.name)
        self.git("init")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("config", "core.autocrlf", "false")
        self.fixes = [
            "codex-rs/core/src/state/session.rs",
            "codex-rs/protocol/src/error.rs",
        ]
        self.catalog = "codex-rs/models-manager/models.json"
        self.prompt = "codex-rs/protocol/src/prompts/base_instructions/default.md"
        for path in self.fixes + [self.catalog, self.prompt]:
            self.write(path, "official\n")
        self.commit("Official source")
        self.release = {"commit": self.git("rev-parse", "HEAD")}
        self.product_sources = [
            "codex-rs/Cargo.lock",
            "codex-rs/core/Cargo.toml",
            "codex-rs/core/src/lib.rs",
            "codex-rs/core/src/claude_code/messaging.rs",
            "codex-rs/messages-api/src/request.rs",
            "codex-rs/protocol/src/models.rs",
            "codex-rs/core/src/client.rs",
            "scripts/product_build.py",
        ]
        for path in self.fixes + self.product_sources:
            self.write(path, "fork change\n")
        self.commit("Fork changes")

    def git(self, *arguments):
        return subprocess.check_output(
            ["git", "-C", str(self.source), *arguments], text=True
        ).strip()

    def write(self, path, contents):
        target = self.source / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(contents, encoding="utf-8")

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-m", message)

    def audit(self):
        with mock.patch.object(daemon_release, "ROOT", self.source):
            return daemon_release.unified_source(self.source, self.release)

    def test_full_product_tree_is_audited_without_staging_or_filtering(self):
        before = self.git("rev-parse", "HEAD")
        selected = self.audit()
        self.assertEqual(selected["forkCommit"], before)
        self.assertEqual(selected["upstreamBase"], self.release["commit"])
        self.assertEqual(selected["sourceTree"], self.git("rev-parse", "HEAD^{tree}"))
        self.assertEqual(set(selected["files"]), set(self.fixes + self.product_sources))
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_additional_fork_commit_is_rejected(self):
        self.write("docs/change.md", "extra commit\n")
        self.commit("Additional fork commit")
        with self.assertRaisesRegex(AssertionError, "directly onto"):
            self.audit()

    def test_modified_checkout_is_rejected(self):
        self.write(self.product_sources[0], "uncommitted\n")
        with self.assertRaisesRegex(AssertionError, "local modifications"):
            self.audit()

    def test_model_catalog_and_prompt_changes_are_rejected(self):
        for path in [self.catalog, self.prompt]:
            with self.subTest(path=path):
                self.write(path, "changed official source\n")
                self.git("add", ".")
                self.git("commit", "--amend", "-m", "Fork changes")
                with self.assertRaisesRegex(AssertionError, "match the official release"):
                    self.audit()
                self.write(path, "official\n")
                self.git("add", ".")
                self.git("commit", "--amend", "-m", "Fork changes")


class RetryRequestTests(unittest.TestCase):
    def test_metadata_request_retries_a_truncated_http_body(self):
        payload = json.dumps({"tag_name": "rust-v1.2.3-alpha.4"}).encode()

        class Handler(BaseHTTPRequestHandler):
            requests = 0

            def do_GET(self):
                Handler.requests += 1
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload[:5] if Handler.requests == 1 else payload)

            def log_message(self, *_args):
                pass

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                local_url = f"http://127.0.0.1:{server.server_port}/releases"
                with (
                    mock.patch.object(
                        daemon_release,
                        "urlopen",
                        side_effect=lambda _request, timeout: urlopen(local_url, timeout=timeout),
                    ),
                    mock.patch.object(daemon_release.time, "sleep") as sleep,
                ):
                    self.assertEqual(
                        daemon_release.github("releases?per_page=100"),
                        json.loads(payload),
                    )
                    self.assertEqual(Handler.requests, 2)
                    sleep.assert_called_once_with(2)
            finally:
                server.shutdown()
                worker.join()

    def test_connection_resets_retry_within_the_same_budget(self):
        for error in [ConnectionResetError("reset"), URLError(ConnectionResetError("reset"))]:
            with self.subTest(error=type(error).__name__):
                operation = mock.Mock(side_effect=[error, "complete"])
                with mock.patch.object(daemon_release.time, "sleep") as sleep:
                    self.assertEqual(daemon_release.retry_request(operation, "download"), "complete")
                    self.assertEqual(operation.call_count, 2)
                    sleep.assert_called_once_with(2)

    def test_truncated_reads_stop_after_the_retry_budget(self):
        error = IncompleteRead(b"partial", 10)
        operation = mock.Mock(side_effect=error)
        with mock.patch.object(daemon_release.time, "sleep") as sleep:
            with self.assertRaises(IncompleteRead) as raised:
                daemon_release.retry_request(operation, "metadata")
            self.assertIs(raised.exception, error)
            self.assertEqual(operation.call_count, 4)
            self.assertEqual(sleep.call_args_list, [mock.call(2), mock.call(4), mock.call(8)])

    def test_retry_after_is_preserved_across_an_interrupted_read(self):
        unavailable = HTTPError("https://example.invalid", 503, "busy", {"Retry-After": "7"}, None)
        operation = mock.Mock(side_effect=[unavailable, IncompleteRead(b"partial", 10), "complete"])
        with mock.patch.object(daemon_release.time, "sleep") as sleep:
            self.assertEqual(daemon_release.retry_request(operation, "metadata"), "complete")
            self.assertEqual(sleep.call_args_list, [mock.call(7), mock.call(4)])

    def test_permanent_errors_are_not_retried(self):
        for error in [
            HTTPError("https://example.invalid", 403, "forbidden", {}, None),
            URLError("certificate verification failed"),
            json.JSONDecodeError("invalid JSON", "", 0),
        ]:
            with self.subTest(error=type(error).__name__):
                operation = mock.Mock(side_effect=error)
                with mock.patch.object(daemon_release.time, "sleep") as sleep:
                    with self.assertRaises(type(error)):
                        daemon_release.retry_request(operation, "metadata")
                    operation.assert_called_once_with()
                    sleep.assert_not_called()


if __name__ == "__main__":
    unittest.main()
