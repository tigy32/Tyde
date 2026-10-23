"""Owned native config; real CLI/provider only, with no credential copies."""
import http.server
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading


def main():
    required = ["TYDE_TEST_CLAUDE_API_KEY", "TYDE_TEST_CLAUDE_BASE_URL", "TYDE_TEST_CLAUDE_MODEL"]
    if not all(os.environ.get(key) for key in required):
        raise RuntimeError("Claude settings regression requires explicit provider credentials, URL and model")
    with tempfile.TemporaryDirectory(prefix="tyde-claude-review-config-") as directory:
        root = Path(directory)
        config = root / "config"
        home = root / "home"
        config.mkdir()
        home.mkdir()
        helper = root / "auth.py"
        helper.write_text(
            'import os\nprint(os.environ["TYDE_TEST_CLAUDE_API_KEY"], end="")\n'
        )
        workspace_helper = root / "workspace_auth.py"
        workspace_helper.write_text(
            'import os, pathlib, sys\npathlib.Path(sys.argv[1]).touch()\n'
            'print(os.environ["TYDE_TEST_CLAUDE_API_KEY"], end="")\n'
        )
        mcp = root / "ambient.py"
        mcp.write_text(
            'import json, pathlib, sys\n'
            'pathlib.Path(sys.argv[1]).touch()\n'
            'for line in sys.stdin:\n'
            ' r = json.loads(line)\n'
            ' if "id" not in r: continue\n'
            ' method = r.get("method")\n'
            ' result = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},'
            '"serverInfo":{"name":"ambient","version":"1"}} if method == "initialize" '
            'else {"tools":[{"name":"ambient_probe","description":"Read an owned marker",'
            '"inputSchema":{"type":"object","properties":{}}}]} if method == "tools/list" '
            'else {"content":[{"type":"text","text":"ambient"}]}\n'
            ' print(json.dumps({"jsonrpc":"2.0","id":r["id"],"result":result}),flush=True)\n'
        )
        settings = {
            "apiKeyHelper": f"{shlex.quote(sys.executable)} {shlex.quote(str(helper))}",
            "env": {
                "ANTHROPIC_BASE_URL": os.environ["TYDE_TEST_CLAUDE_BASE_URL"],
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": os.environ["TYDE_TEST_CLAUDE_MODEL"],
            },
            "hooks": {"SessionStart": [{"hooks": [{
                "type": "command", "command": f"touch {shlex.quote(str(config / 'hook-started'))}"
            }]}]},
        }
        (config / "settings.json").write_text(json.dumps(settings))
        (config / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True,
            "mcpServers": {"ambient": {
                "type": "stdio", "command": sys.executable,
                "args": [str(mcp), str(config / "ambient-started")],
            }},
        }))
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CLAUDE_", "ANTHROPIC_"))}
        env.update(
            HOME=str(home), CLAUDE_CONFIG_DIR=str(config),
            CLAUDE_SECURESTORAGE_CONFIG_DIR=str(config),
            TYDE_CLAUDE_REVIEW_CONFIG_ISOLATED="1",
            TYDE_TEST_CLAUDE_WORKSPACE_HELPER=(
                f"{shlex.quote(sys.executable)} {shlex.quote(str(workspace_helper))} "
                f"{shlex.quote(str(config / 'workspace-helper-ran'))}"
            ),
            CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
        )
        class Unauthorized(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(401)
                self.end_headers()

            def do_GET(self):
                self.send_response(401)
                self.end_headers()

            def log_message(self, *_args):
                pass

        denied = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Unauthorized)
        serving = threading.Thread(target=denied.serve_forever)
        serving.start()
        env["TYDE_TEST_CLAUDE_UNAUTHORIZED_MCP_URL"] = f"http://127.0.0.1:{denied.server_port}/mcp"
        try:
            child = subprocess.run(
                [sys.argv[1], "--exact", "real_claude_review_configuration", "--ignored", "--nocapture"],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            )
        finally:
            denied.shutdown()
            serving.join()
            denied.server_close()
        # Native output can contain private session IDs or conversation text.
        print(json.dumps({"native_config_conformance_completed": True,
                          "exit_code": child.returncode,
                          "not_logged_in": "Not logged in" in child.stdout,
                          "restricted_mcp_error": "restricted MCP" in child.stdout}))
        for line in child.stdout.splitlines():
            location = re.search(r"panicked at ([^:\r\n]+\.rs:\d+:\d+)", line)
            if location:
                print(json.dumps({"panic_location": location[1]}))
            if line.startswith("CLAUDE_REVIEW_CONFIG "):
                print(line)
        if child.returncode:
            raise RuntimeError("Real Claude review configuration regression failed")


if __name__ == "__main__":
    main()
