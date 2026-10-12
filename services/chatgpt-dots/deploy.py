#!/usr/bin/env python3
"""Deploy the explicitly selected Dots Worker without logging credentials."""
import argparse
import json
import os
import pathlib
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", required=True)
    parser.add_argument("--confirm", action="store_true")
    args = parser.parse_args()
    if not args.confirm or args.name not in {"tyde-dots", "tyde-dots-preview"}:
        parser.error("Choose tyde-dots or tyde-dots-preview and supply --confirm")
    account = os.environ["CLOUDFLARE_ACCOUNT_ID"]
    key = os.environ["CLOUDFLARE_API_TOKEN"]
    base = f"https://api.cloudflare.com/client/v4/accounts/{account}/workers"

    def api(path, method="GET", data=None, content_type="application/json", missing=False):
        request = urllib.request.Request(base + path, data=data, method=method,
                                         headers={"Authorization": f"Bearer {key}", "Content-Type": content_type})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            raise RuntimeError(f"Cloudflare request failed (HTTP {error.code})") from None
        if not result.get("success"):
            raise RuntimeError("Cloudflare did not accept the deployment")
        return result.get("result")

    existing = api(f"/scripts/{args.name}/settings", missing=True)
    binding = {"type": "durable_object_namespace", "name": "BROKER", "class_name": "DotsBroker"}
    metadata = {"main_module": "worker.mjs", "compatibility_date": "2026-07-30",
                "bindings": [binding], "observability": {"enabled": False}, "logpush": False}
    if existing is None:
        metadata["migrations"] = {"new_tag": "v1", "new_sqlite_classes": ["DotsBroker"]}
    else:
        brokers = [value for value in existing.get("bindings", []) if value.get("name") == "BROKER"]
        if len(brokers) != 1 or brokers[0].get("type") != "durable_object_namespace":
            raise RuntimeError("Existing Worker has an unexpected BROKER binding; refusing to replace it")
        metadata["bindings"] = brokers
    boundary = uuid.uuid4().hex
    parts = []
    for name, content_type, content in [
        ("metadata", "application/json", json.dumps(metadata).encode()),
        ("worker.mjs", "application/javascript+module", pathlib.Path(__file__).with_name("worker.mjs").read_bytes()),
        ("codec.mjs", "application/javascript+module", pathlib.Path(__file__).with_name("codec.mjs").read_bytes()),
    ]:
        filename = f'; filename="{name}"' if name != "metadata" else ""
        parts.extend([f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"{filename}\r\nContent-Type: {content_type}\r\n\r\n".encode(), content, b"\r\n"])
    parts.append(f"--{boundary}--\r\n".encode())
    api(f"/scripts/{args.name}", "PUT", b"".join(parts), f"multipart/form-data; boundary={boundary}")
    api(f"/scripts/{args.name}/subdomain", "POST", b'{"enabled":true,"previews_enabled":false}')
    subdomain = api("/subdomain")["subdomain"]
    print(f"Deployed https://{args.name}.{subdomain}.workers.dev")


if __name__ == "__main__":
    main()
