"""Exercise the packaged Windows CLI with disposable credentials and a loopback auth server."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading


def check(binary: Path) -> None:
    with tempfile.TemporaryDirectory() as temp:
        home = Path(temp) / "codex"
        home.mkdir()
        auth_dir = Path(temp) / "messages-auth"
        original = home / "auth.json"
        original.write_text(json.dumps({"auth_mode": "apikey", "OPENAI_API_KEY": "ordinary-offline-test"}), encoding="utf-8")
        before = original.read_bytes()
        env = dict(os.environ, CODEX_HOME=str(home), CODEX_API_KEY="environment-offline-test", CODEX_ACCESS_TOKEN="invalid-offline-test")
        command = [str(binary), "-c", 'cli_auth_credentials_store="file"', "messages-api", "--auth-dir", str(auth_dir)]

        def auth_check(args, code, stdin=None):
            result = subprocess.run(command + args, input=stdin, capture_output=True, text=True, env=env, timeout=30)
            if result.returncode != code:
                raise RuntimeError(f"Independent login check failed: {args}: {result.returncode}: {result.stderr}")
            if original.read_bytes() != before:
                raise RuntimeError("Messages API changed ordinary Codex credentials")

        auth_check(["login", "status"], 1)
        auth_check(["login", "--with-api-key"], 0, "gateway-offline-test\n")
        auth_check(["login", "status"], 0)
        auth_check(["logout"], 0)
        auth_check(["login", "status"], 1)
        nested = auth_dir / "messages-api" / "auth"
        nested.mkdir(parents=True)
        (nested / "auth.json").write_text(json.dumps({"auth_mode": "apikey", "OPENAI_API_KEY": "gateway-offline-test"}), encoding="utf-8")
        (home / "config.toml").write_text("[env]\nCODEX_HOME = " + json.dumps(str(auth_dir)) + "\n", encoding="utf-8")
        inherited = dict(os.environ, CODEX_HOME=str(home))
        probe = subprocess.run([str(binary), "-c", 'cli_auth_credentials_store="file"', "messages-api", "login", "status"], capture_output=True, text=True, env=inherited, timeout=30)
        if probe.returncode != 0 or original.read_bytes() != before:
            raise RuntimeError("User config environment did not override inherited CODEX_HOME before configuration/auth loading")

        requests = []

        class AuthServer(BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization")))
                body = json.dumps({"email": "test@example.test", "chatgpt_user_id": "user-test", "chatgpt_account_id": "account-test", "chatgpt_plan_type": "pro", "chatgpt_account_is_fedramp": False}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), AuthServer)
        worker = threading.Thread(target=server.serve_forever)
        worker.start()
        try:
            configured = f"http://127.0.0.1:{server.server_port}"
            (home / "config.toml").write_text("[env]\nCODEX_AUTHAPI_BASE_URL = " + json.dumps(configured) + "\nCODEX_ACCESS_TOKEN = 'at-config-test'\n", encoding="utf-8")
            inherited.update(CODEX_AUTHAPI_BASE_URL="http://127.0.0.1:1", CODEX_ACCESS_TOKEN="at-inherited-test")
            inherited.pop("CODEX_API_KEY", None)
            result = subprocess.run([str(binary), "login", "status"], capture_output=True, text=True, env=inherited, timeout=30)
            if result.returncode != 0 or not requests or any(row != ("/v1/user-auth-credential/whoami", "Bearer at-config-test") for row in requests):
                raise RuntimeError("Configured auth endpoint/access token did not override inherited variables at the real auth consumer")
            requests.clear()
            env.update(CODEX_AUTHAPI_BASE_URL="http://127.0.0.1:1", CODEX_ACCESS_TOKEN="at-inherited-test")
            auth_check(["login", "--with-access-token"], 0, "at-gateway-test\n")
            auth_check(["login", "status"], 0)
            if not requests or any(row != ("/v1/user-auth-credential/whoami", "Bearer at-gateway-test") for row in requests):
                raise RuntimeError("Messages API failed to load its stored access token independently of process credentials")
            auth_check(["logout"], 0)
            auth_check(["login", "status"], 1)
        finally:
            server.shutdown()
            server.server_close()
            worker.join()
        if original.read_bytes() != before:
            raise RuntimeError("Auth probes changed ordinary stored credentials")
    print("PASS: independent login/logout, stored credential preservation, config-home override, and configured auth endpoint/token precedence")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    check(parser.parse_args().binary.resolve())
