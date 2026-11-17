"""Capture managed-daemon requests through the OpenAI provider path."""

import argparse
import base64
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import shutil
import socket
import tempfile
import threading

import zstandard
from websockets.datastructures import Headers
from websockets.http11 import Response
from websockets.sync.server import serve
from websockets.sync.client import connect
from websockets.exceptions import ConnectionClosed

ROOT = Path(__file__).resolve().parents[1]
Json = dict[str, object]


def sse(events: list[Json]) -> str:
    """Build an SSE body from Responses API event JSON objects."""
    chunks: list[str] = []
    for event in events:
        event_type = event["type"]
        chunks.append(f"event: {event_type}\ndata: {json.dumps(event)}\n")
    return "\n".join(chunks) + "\n"


def ev_response_created(response_id: str) -> Json:
    """Return a minimal `response.created` event."""
    return {"type": "response.created", "response": {"id": response_id}}


def ev_completed(response_id: str) -> Json:
    """Return a minimal `response.completed` event with usage."""
    return {
        "type": "response.completed",
        "response": {
            "id": response_id,
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": None,
                "output_tokens": 1,
                "output_tokens_details": None,
                "total_tokens": 2,
            },
        },
    }


def ev_assistant_message(item_id: str, text: str) -> Json:
    """Return a completed assistant message output item."""
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": item_id,
            "content": [{"type": "output_text", "text": text}],
        },
    }


parser = argparse.ArgumentParser()
parser.add_argument("--package", type=Path, required=True)
parser.add_argument("--transport", choices=["http", "websocket"], required=True)
parser.add_argument("--label", default="corrected")
parser.add_argument("--preflight", action="store_true")
parser.add_argument("--catalog-root", type=Path, default=ROOT)
parser.add_argument("--output-dir", type=Path, required=True)
args = parser.parse_args()
EVIDENCE = args.output_dir.resolve()
EVIDENCE.mkdir(parents=True, exist_ok=True)
requests = []
request_headers = []
lock = threading.Lock()
generated = 0


def respond(body, headers):
    global generated
    assert body["model"] == model, body["model"]
    assert headers["authorization"] == "Bearer fixture-access-token"
    assert headers["chatgpt-account-id"] == "fixture-account"
    with lock:
        requests.append(body)
        request_headers.append(
            {
                key.lower(): "<AUTH>" if key.lower() == "authorization" else value
                for key, value in headers.items()
            }
        )
        index = len(requests)
        response_id = f"metadata-smoke-{index}"
        events = [ev_response_created(response_id)]
        if body.get("generate") is not False:
            generated += 1
            if generated == 1:
                events.append(
                    {
                        "type": "response.output_item.done",
                        "item": {
                            "type": "custom_tool_call",
                            "call_id": "metadata-code-mode",
                            "name": "exec",
                            "input": 'text("metadata-wire-ok-" + (6 * 7))',
                        },
                    }
                )
            else:
                events.append(ev_assistant_message(f"msg-{index}", "Done"))
        events.append(ev_completed(response_id))
        return events


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("upgrade", "").lower() == "websocket":
            # Exercise the official session-scoped HTTP fallback.
            self.send_error(426)
            return
        path = self.path.split("?", 1)[0]
        if path == "/backend-api/wham/accounts/check":
            value = {"accounts": [{
                "id": "fixture-account",
                "workspace_backend_origin": "https://chatgpt.com",
                "account_routing_override": "NO_CONSTRAINT",
            }]}
        elif path == "/backend-api/wham/config/bundle":
            value = {}
        elif path == "/backend-api/wham/settings/user":
            value = {"commit_attribution_enabled": False}
        elif path == "/backend-api/codex/models":
            value = catalog
        else:
            self.send_error(404)
            return
        payload = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        if self.headers.get("content-encoding") == "zstd":
            body = zstandard.ZstdDecompressor().decompressobj().decompress(body)
        body = json.loads(body)
        if self.path != "/backend-api/codex/responses":
            self.send_error(404)
            return
        events = respond(body, self.headers)
        payload = sse(events).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def websocket_handler(connection):
    assert connection.request.path == "/backend-api/codex/responses"
    try:
        for message in connection:
            body = json.loads(message)
            assert body["type"] == "response.create", body["type"]
            for event in respond(body, connection.request.headers):
                connection.send(json.dumps(event))
    except ConnectionClosed:
        pass


def websocket_http_request(connection, request):
    if request.headers.get("upgrade", "").lower() == "websocket":
        return None
    if request.path.split("?", 1)[0] == "/backend-api/codex/models":
        payload = json.dumps(catalog).encode()
        return Response(200, "OK", Headers({
            "content-type": "application/json", "content-length": str(len(payload)),
        }), payload)
    return connection.respond(404, "Not found\n")


@contextmanager
def http_server():
    instance = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=instance.serve_forever, daemon=True)
    thread.start()
    try:
        yield instance.server_address[1]
    finally:
        instance.shutdown()
        instance.server_close()
        thread.join()


@contextmanager
def server():
    # A separate account-service origin keeps workspace routing on the fixture inference URL.
    with http_server() as account_port:
        if args.transport == "http":
            with http_server() as port:
                yield port, account_port
        else:
            with serve(
                websocket_handler, "127.0.0.1", 0, max_size=16 * 1024 * 1024,
                process_request=websocket_http_request,
            ) as instance:
                thread = threading.Thread(target=instance.serve_forever, daemon=True)
                thread.start()
                try:
                    yield instance.socket.getsockname()[1], account_port
                finally:
                    instance.shutdown()
                    thread.join()


fixture_root = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())) / "dp"
fixture_root.mkdir(exist_ok=True)
scratch = Path(
    tempfile.mkdtemp(prefix=f"{args.label[0]}{args.transport[0]}-", dir=fixture_root)
)
scratch.mkdir(parents=True, exist_ok=True)
config_dir = scratch / "config"
config_dir.mkdir(exist_ok=True)
catalog_path = args.catalog_root.resolve() / "codex-rs/models-manager/models.json"
catalog_bytes = catalog_path.read_bytes()
catalog = json.loads(catalog_bytes)
model = catalog["models"][0]["slug"]


def jwt_part(value):
    return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")


id_token = ".".join([
    jwt_part({"alg": "none", "typ": "JWT"}),
    jwt_part({
        "sub": "fixture-user",
        "https://api.openai.com/auth": {
            "chatgpt_user_id": "fixture-user",
            "chatgpt_account_id": "fixture-account",
            "chatgpt_plan_type": "pro",
        },
    }),
    base64.urlsafe_b64encode(b"fixture-signature").decode().rstrip("="),
])
(config_dir / "auth.json").write_text(json.dumps({
    "auth_mode": "chatgpt",
    "tokens": {
        "id_token": id_token,
        "access_token": "fixture-access-token",
        "refresh_token": "fixture-refresh-token",
        "account_id": "fixture-account",
    },
    "last_refresh": datetime.now(timezone.utc).isoformat(),
}), encoding="utf-8")
environment = dict(os.environ)
environment["CODEX_HOME"] = str(config_dir)
environment["NO_PROXY"] = "127.0.0.1,localhost"
failure = None
daemon_version = None


def lifecycle(*arguments):
    completed = subprocess.run(
        [str(args.package / "bin/codex.exe"), "app-server", "daemon", *arguments],
        capture_output=True,
        env=environment,
        timeout=100,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    log = EVIDENCE / f"daemon-{args.label}-{args.transport}-{'-'.join(arguments)}.log"
    log.write_bytes(completed.stdout + completed.stderr)
    if completed.returncode:
        raise RuntimeError(
            (completed.stdout + completed.stderr).decode(errors="replace")
        )
    return completed.stdout


@contextmanager
def daemon_connection():
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    proxy = subprocess.Popen(
        [str(args.package / "bin/codex.exe"), "app-server", "proxy"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )

    def bridge():
        peer, _ = listener.accept()

        def incoming():
            try:
                while data := peer.recv(65536):
                    proxy.stdin.write(data)
                    proxy.stdin.flush()
            except (OSError, BrokenPipeError):
                pass
            finally:
                proxy.stdin.close()

        reader = threading.Thread(target=incoming, daemon=True)
        reader.start()
        try:
            while data := os.read(proxy.stdout.fileno(), 65536):
                peer.sendall(data)
        except OSError:
            pass
        finally:
            peer.close()
            reader.join(timeout=2)

    thread = threading.Thread(target=bridge, daemon=True)
    thread.start()
    try:
        with connect(
            f"ws://127.0.0.1:{listener.getsockname()[1]}/", proxy=None, compression=None
        ) as connection:
            yield RpcSession(connection)
    finally:
        listener.close()
        thread.join(timeout=3)
        if proxy.poll() is None:
            proxy.kill()
        proxy.wait(timeout=5)


class RpcSession:
    def __init__(self, connection):
        self.connection = connection
        self.next_id = 0
        self.pending = []
        self.rpc(
            "initialize",
            {
                "clientInfo": {
                    "name": "codex_python_sdk",
                    "title": "Codex Python SDK",
                    "version": "0.0.0",
                },
                "capabilities": {"experimentalApi": True},
            },
        )
        connection.send(json.dumps({"method": "initialized"}))

    def rpc(self, method, params):
        self.next_id += 1
        request_id = self.next_id
        self.connection.send(
            json.dumps({"id": request_id, "method": method, "params": params})
        )
        while True:
            message = json.loads(self.connection.recv(timeout=30))
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message["result"]
            self.pending.append(message)

    def turn(self, thread_id, text):
        self.rpc(
            "turn/start",
            {"threadId": thread_id, "input": [{"type": "text", "text": text}]},
        )
        messages = []
        while True:
            message = (
                self.pending.pop(0)
                if self.pending
                else json.loads(self.connection.recv(timeout=30))
            )
            messages.append(message)
            if message.get("method") == "turn/completed":
                assert message["params"]["turn"]["status"] == "completed", message
                assert "Done" in json.dumps(messages), messages
                return


with server() as (port, account_port):
    (config_dir / "config.toml").write_text(
        f"""
model = {json.dumps(model)}
model_provider = "openai"
openai_base_url = "http://127.0.0.1:{port}/backend-api/codex"
chatgpt_base_url = "http://127.0.0.1:{account_port}/backend-api"
model_catalog_json = {json.dumps(str(catalog_path))}
cli_auth_credentials_store = "file"
approval_policy = "never"
sandbox_mode = "workspace-write"
suppress_unstable_features_warning = true
[features]
code_mode_only = true
code_mode_host = true
memories = false
apps = false
plugins = false
[analytics]
enabled = false
[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"
""",
        encoding="utf-8",
    )
    try:
        release = (
            config_dir
            / "packages/app-server-daemon/releases"
            / f"local-parity-{args.label}-{args.transport}-x86_64-pc-windows-msvc"
        )
        shutil.copytree(args.package, release)
        current = release.parent.parent / "current"

        def quote(value):
            return "'" + str(value).replace("'", "''") + "'"

        subprocess.run(
            [
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                f"New-Item -ItemType Junction -Path {quote(current)} -Target {quote(release)} | Out-Null",
            ],
            check=True,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        lifecycle("start")
        daemon_version = json.loads(lifecycle("version"))
        assert daemon_version["status"] == "running", daemon_version
        if not args.preflight:
            with daemon_connection() as client:
                thread_id = client.rpc(
                    "thread/start",
                    {
                        "cwd": str(scratch),
                        "approvalPolicy": "never",
                        "sandbox": "workspace-write",
                    },
                )["thread"]["id"]
                client.turn(thread_id, "Run the metadata wire smoke")
                client.turn(thread_id, "Continue the same thread")
            lifecycle("restart")
            with daemon_connection() as client:
                client.rpc("thread/resume", {"threadId": thread_id})
                client.turn(thread_id, "Continue after restarting app-server")
    except Exception as error:
        failure = repr(error)
    finally:
        lifecycle("stop")
result = {
    "transport": args.transport,
    "package": str(args.package),
    "scratch": str(scratch),
    "requests": len(requests),
    "generatedResponses": generated,
    "failure": failure,
    "liveModelRequests": False,
    "daemonVersion": daemon_version,
    "provider": "openai",
    "authMode": "chatgpt",
    "model": model,
    "officialCatalogSha256": hashlib.sha256(catalog_bytes).hexdigest(),
}
label = args.label
(EVIDENCE / f"metadata-wire-{args.transport}-{label}.requests.json").write_text(
    json.dumps(
        {"requests": requests, "headers": request_headers, "scratch": str(scratch)},
        indent=2,
    ),
    encoding="utf-8",
)
(EVIDENCE / f"metadata-wire-{args.transport}-{label}.json").write_text(
    json.dumps(result, indent=2), encoding="utf-8"
)
assert failure is None, result
if args.preflight:
    assert daemon_version["status"] == "running" and generated == 0, result
else:
    assert generated == 4, result
    items = [item for request in requests for item in request.get("input", [])]
    assert any((item["type"] == "additional_tools" for item in items)), (
        "RLite request path was not exercised"
    )
    assert any(
        (
            (item.get("internal_chat_message_metadata_passthrough") or {}).get(
                "turn_id"
            )
            for item in items
        )
    ), "OpenAI metadata path was not exercised"
    assert any(
        (
            "metadata-wire-ok-42" in json.dumps(item)
            for item in items
            if item["type"] == "custom_tool_call_output"
        )
    ), "code-mode host did not run"
result["status"] = "passed"
(EVIDENCE / f"metadata-wire-{args.transport}-{label}.json").write_text(
    json.dumps(result, indent=2), encoding="utf-8"
)
print(json.dumps(result, indent=2))
