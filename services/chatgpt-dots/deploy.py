#!/usr/bin/env python3
"""Deploy the existing Remote MCP Worker without logging credentials."""
import argparse
import fnmatch
import json
import os
import pathlib
import urllib.error
import urllib.parse
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", required=True, choices=("tyde-dots", "tyde-dots-preview"))
    parser.add_argument("--custom-domain", choices=("mcp.tycode.dev",))
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--preflight", action="store_true", help="Read-only checks; no deployment or domain attachment")
    mode.add_argument("--confirm", action="store_true", help="Deploy after all preflight checks pass")
    args = parser.parse_args()
    if args.custom_domain and args.name != "tyde-dots":
        parser.error("mcp.tycode.dev may only be attached to production tyde-dots")
    account = os.environ.get("CLOUDFLARE_ACCOUNT_ID")
    key = os.environ.get("CLOUDFLARE_API_TOKEN")
    if not account or not key:
        raise RuntimeError("Source CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN from ~/.secrets")
    base = "https://api.cloudflare.com/client/v4"
    workers = f"/accounts/{account}/workers"

    def api(path, label, method="GET", data=None, content_type="application/json", missing=False):
        if args.preflight and method != "GET":
            raise RuntimeError("Preflight cannot change Cloudflare configuration")
        request = urllib.request.Request(base + path, data=data, method=method,
                                         headers={"Authorization": f"Bearer {key}", "Content-Type": content_type})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            raise RuntimeError(f"{label}: Cloudflare request failed (HTTP {error.code})") from None
        except (OSError, ValueError):
            raise RuntimeError(f"{label}: Cloudflare transport or response failed") from None
        if not result.get("success"):
            raise RuntimeError(f"{label}: Cloudflare rejected the request")
        if result.get("result_info", {}).get("total_pages", 1) > 1:
            raise RuntimeError(f"{label}: incomplete listing; refusing to change configuration")
        if "result" not in result:
            raise RuntimeError(f"{label}: Cloudflare response is missing its result")
        return result["result"]

    existing = api(f"{workers}/scripts/{args.name}/settings", "Worker settings", missing=True)
    if existing is None and args.name == "tyde-dots":
        raise RuntimeError("Production tyde-dots must already exist; refusing to provision replacement storage")
    binding = {"type": "durable_object_namespace", "name": "BROKER", "class_name": "DotsBroker"}
    metadata = {"main_module": "worker.mjs", "compatibility_date": "2026-07-30",
                "bindings": [binding], "observability": {"enabled": False}, "logpush": False}
    brokers = []
    if existing is None:
        metadata["migrations"] = {"new_tag": "v1", "new_sqlite_classes": ["DotsBroker"]}
    else:
        brokers = [value for value in existing.get("bindings", []) if value.get("name") == "BROKER"]
        if (len(brokers) != 1 or brokers[0].get("type") != "durable_object_namespace"
                or brokers[0].get("class_name") != "DotsBroker" or not brokers[0].get("namespace_id")
                or brokers[0].get("script_name") not in {None, args.name}
                or brokers[0].get("environment") not in {None, "production"}):
            raise RuntimeError("Existing Worker has an unexpected BROKER binding; refusing to replace it")
        retained_settings = {"annotations", "cache_options", "compatibility_date", "compatibility_flags",
                             "limits", "logpush", "observability", "placement", "tags", "tail_consumers", "usage_model"}
        if set(existing) - retained_settings - {"bindings", "migration_tag"}:
            raise RuntimeError("Worker has unsupported settings; refusing to overwrite configuration")
        metadata.update({name: value for name, value in existing.items()
                         if name in retained_settings and value is not None})
        script_settings = api(f"{workers}/scripts/{args.name}/script-settings", "Script-level settings")
        if set(script_settings) - {"logpush", "observability", "tags", "tail_consumers"}:
            raise RuntimeError("Worker has unsupported script-level settings; refusing to overwrite configuration")
        metadata.update({name: value for name, value in script_settings.items() if value is not None})
        metadata["bindings"] = [value if value.get("name") == "BROKER" else
                                {"type": "inherit", "name": value["name"]} for value in existing["bindings"]]

    subdomain = api(f"{workers}/subdomain", "Account subdomain")["subdomain"]
    legacy_route = None
    legacy_domain = None
    if args.name == "tyde-dots":
        if subdomain != "crystal-depths":
            raise RuntimeError("Account does not match the legacy Dots origin; refusing to deploy")
        legacy_route = api(f"{workers}/scripts/{args.name}/subdomain", "Legacy workers.dev route")
        if legacy_route.get("enabled") is not True:
            raise RuntimeError("Legacy workers.dev route must remain enabled for existing grants")
        legacy_domain = api(f"{workers}/domains?hostname=dots.tycode.dev", "Existing dots Custom Domain")
        if (len(legacy_domain) != 1 or legacy_domain[0].get("hostname") != "dots.tycode.dev"
                or legacy_domain[0].get("service") != "tyde-dots"
                or legacy_domain[0].get("environment") != "production"
                or legacy_domain[0].get("zone_name") != "tycode.dev" or not legacy_domain[0].get("zone_id")
                or legacy_domain[0].get("enabled") is not True
                or legacy_domain[0].get("previews_enabled") is not False):
            raise RuntimeError("Existing dots.tycode.dev Custom Domain must remain attached and enabled")

    def domain_preflight():
        query = urllib.parse.urlencode({"name": "tycode.dev", "account.id": account, "per_page": 50})
        zones = api(f"/zones?{query}", "tycode.dev zone")
        if (len(zones) != 1 or zones[0].get("name") != "tycode.dev"
                or zones[0].get("status") != "active" or zones[0].get("account", {}).get("id") != account):
            raise RuntimeError("Exactly one active tycode.dev zone must be accessible in the selected account")
        zone_id = zones[0]["id"]
        if legacy_domain[0]["zone_id"] != zone_id:
            raise RuntimeError("Existing dots Custom Domain belongs to a different zone; refusing to change configuration")
        domains = api(f"{workers}/domains?hostname={args.custom_domain}", "Custom Domain bindings")
        if len(domains) > 1:
            raise RuntimeError("Multiple Custom Domain bindings found; refusing to change configuration")
        if domains:
            domain = domains[0]
            if (domain.get("hostname") != args.custom_domain or domain.get("service") != "tyde-dots"
                    or domain.get("environment") != "production" or domain.get("zone_id") != zone_id
                    or domain.get("enabled") is not True or domain.get("previews_enabled") is not False):
                raise RuntimeError("mcp.tycode.dev has a conflicting Custom Domain binding; refusing to replace it")
        query = urllib.parse.urlencode({"name": args.custom_domain, "per_page": 100})
        records = api(f"/zones/{zone_id}/dns_records?{query}", "mcp.tycode.dev DNS records")
        if records and not domains:
            raise RuntimeError("mcp.tycode.dev already has DNS records; refusing to overwrite them")
        routes = api(f"/zones/{zone_id}/workers/routes", "tycode.dev Worker routes")
        for route in routes:
            hostname = route["pattern"].split("://", 1)[-1].split("/", 1)[0].lower()
            if fnmatch.fnmatchcase(args.custom_domain, hostname):
                raise RuntimeError("A Worker route overlaps mcp.tycode.dev; refusing to change routing")
        print("Custom Domain preflight passed: active tycode.dev zone; no conflicting DNS, domain binding, or Worker route")
        return zone_id, bool(domains)

    if args.custom_domain:
        domain_preflight()
    if existing is None:
        print("Worker preflight passed: new preview storage will be provisioned only with --confirm")
    else:
        print(f"Worker preflight passed: {args.name}; existing bindings and Durable Object storage retained")
    if args.preflight:
        print("Read-only preflight complete; no Worker, DNS, domain, or account changes made")
        return

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
    api(f"{workers}/scripts/{args.name}?bindings_inherit=strict", "Worker upload", "PUT",
        b"".join(parts), f"multipart/form-data; boundary={boundary}")
    if existing is not None:
        deployed = api(f"{workers}/scripts/{args.name}/settings", "Deployed Worker settings")
        deployed_brokers = [value for value in deployed.get("bindings", []) if value.get("name") == "BROKER"]
        if deployed_brokers != brokers:
            raise RuntimeError("Deployed BROKER binding changed; stopping before domain attachment")
    if legacy_route is None:
        api(f"{workers}/scripts/{args.name}/subdomain", "Worker workers.dev route", "POST",
            b'{"enabled":true,"previews_enabled":false}')
    elif api(f"{workers}/scripts/{args.name}/subdomain", "Preserved workers.dev route") != legacy_route:
        raise RuntimeError("Legacy workers.dev configuration changed; stopping before domain attachment")
    if legacy_domain is not None:
        if api(f"{workers}/domains?hostname=dots.tycode.dev", "Preserved dots Custom Domain") != legacy_domain:
            raise RuntimeError("Existing dots.tycode.dev configuration changed; stopping before domain attachment")
    print(f"Deployed https://{args.name}.{subdomain}.workers.dev")
    if args.custom_domain:
        zone_id, attached = domain_preflight()
        if not attached:
            api(f"{workers}/domains", "Custom Domain attachment", "PUT", json.dumps({
                "hostname": args.custom_domain, "service": "tyde-dots", "zone_id": zone_id,
                "enabled": True, "previews_enabled": False,
            }).encode())
            _, attached = domain_preflight()
            if not attached:
                raise RuntimeError("Custom Domain attachment is not visible; verify provisioning before release")
        if api(f"{workers}/domains?hostname=dots.tycode.dev", "Preserved dots Custom Domain") != legacy_domain:
            raise RuntimeError("Existing dots.tycode.dev configuration changed; verify routing before release")
        print("Custom Domain attached: https://mcp.tycode.dev; verify HTTPS provisioning before release")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, KeyError, TypeError, ValueError, OSError) as error:
        if isinstance(error, RuntimeError):
            raise SystemExit(f"Deployment stopped: {error}") from None
        raise SystemExit("Deployment stopped: local files or Cloudflare response are invalid; no sensitive details logged") from None
