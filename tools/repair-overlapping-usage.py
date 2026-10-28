#!/usr/bin/env python3
"""Verify scoped accounting evidence; optionally write repaired offline copies."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import sys

FIELDS = ("input_tokens", "output_tokens", "total_tokens", "cached_prompt_tokens",
          "cache_creation_input_tokens", "reasoning_tokens")


class EvidenceError(Exception):
    pass


def require(condition, context):
    if not condition:
        raise EvidenceError(context)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verified_file(spec):
    path = Path(spec["path"]).resolve(strict=True)
    data = path.read_bytes()
    require(digest(data) == spec["sha256"], "Evidence hash mismatch")
    return path, data


def sum_usage(usages):
    total = {}
    for field in FIELDS:
        values = [usage[field] for usage in usages if usage[field] is not None]
        total[field] = sum(values) if values else None
    return total


def prompt(usage):
    return usage["input_tokens"] + (usage["cached_prompt_tokens"] or 0) + (
        usage["cache_creation_input_tokens"] or 0)


def reported(scope):
    if scope["kind"] != "known":
        return None
    usage = scope["usage"]
    require(set(usage) == set(FIELDS), "Unexpected request counter schema")
    require(all(value is None or type(value) is int and value >= 0
                for value in usage.values()), "Invalid request counter")
    require(all(type(usage[field]) is int for field in FIELDS[:3]),
            "Missing mandatory request counter")
    return usage


def recompute(spec):
    path, original = verified_file(spec)
    require(original.endswith(b"\n"), "Unterminated ledger")
    records = [json.loads(line) for line in original.splitlines()]
    requests, event_ids, response_ids = {}, set(), set()
    generation_count = 0
    typing = False
    previous_sequence = -1
    repaired = copy.deepcopy(records)
    for record in repaired:
        require(record["logical_session_id"] == spec["session_id"],
                "Ledger session ownership mismatch")
        require(record["event_id"] not in event_ids, "Duplicate ledger event")
        event_ids.add(record["event_id"])
        require(record["sequence"] > previous_sequence, "Unordered ledger")
        previous_sequence = record["sequence"]
        require(record["accounting_scope"] == "session_own", "Inherited ledger is not auditable")
        event = record["event"]
        kind, data = event["kind"], event.get("data")
        if kind == "TypingStatusChanged":
            if data and not typing:
                generation_count += 1
            typing = data
        if record["visibility"] != "visible":
            continue
        if kind == "StreamEnd":
            message = data["message"]
        elif kind in ("MessageAdded", "MessageMetadataUpdated"):
            message = data
            if kind == "MessageAdded" and not (
                    isinstance(message["sender"], dict) and "Assistant" in message["sender"]):
                continue
        else:
            continue
        usage = message.get("token_usage")
        if usage is None:
            require(kind == "MessageMetadataUpdated" or not message.get("content"),
                    "Assistant response has missing request history")
            continue
        request = reported(usage["request"])
        require(request is not None, "Assistant request counters unavailable")
        message_id = message["message_id"]
        require(message_id, "Request identity missing")
        if kind != "MessageMetadataUpdated":
            require(message_id not in response_ids, "Duplicate response identity")
            response_ids.add(message_id)
        else:
            require(message_id in response_ids, "Metadata has no own response")
        if message_id in requests:
            require(requests[message_id] == request, "Conflicting request revision")
        requests[message_id] = request
        cumulative = usage["cumulative"]
        if cumulative["kind"] == "known":
            cumulative["usage"] = sum_usage(list(requests.values()))
    require(not typing, "Ledger has an unfinished generation")
    require(len(requests) == spec["requests"], "Request coverage mismatch")
    require(generation_count == spec["generations"], "Generation coverage mismatch")
    for field in FIELDS[3:]:
        reported_count = sum(usage[field] is not None for usage in requests.values())
        require(reported_count in (0, len(requests)), "Partial optional request coverage")
    total = sum_usage(list(requests.values()))
    total["prompt_tokens"] = prompt(total)
    require(total == spec["expected"], "Audited request sum mismatch")
    return path, original, repaired, total, len(records)


def run(args):
    manifest = json.loads(args.manifest.read_text())
    require(manifest["version"] == 1, "Unsupported repair manifest")
    specs = manifest["ledgers"]
    require(1 <= len(specs) <= 3, "Repair scope must contain one to three audited ledgers")
    require(len({item["session_id"] for item in specs}) == len(specs), "Duplicate repair target")
    require(len({item["alias"] for item in specs}) == len(specs), "Duplicate repair alias")
    require(all(item["alias"] in ("A", "B", "C") for item in specs), "Unknown audit alias")
    db_path, db_bytes = verified_file(manifest["database"])
    require(not Path(str(db_path) + "-wal").exists(), "Database snapshot has a WAL")
    evidence = [recompute(spec) for spec in specs]
    require(len({entry[0] for entry in evidence}) == len(specs), "Duplicate ledger path")
    aggregate = sum_usage([entry[3] for entry in evidence])
    aggregate["prompt_tokens"] = prompt(aggregate)
    require(aggregate == manifest["expected_aggregate"], "Aggregate request sum mismatch")
    changes = []
    with sqlite3.connect(db_path.as_uri() + "?mode=ro&immutable=1", uri=True) as db:
        for spec, (_, _, _, total, _) in zip(specs, evidence):
            row = db.execute("SELECT record FROM sessions WHERE id=?", (spec["session_id"],)).fetchone()
            # Deleted chat context intentionally retains its own usage archive.
            if row is not None:
                session = json.loads(row[0])
                require(session["backend_kind"] == "antigravity"
                        and session["task_accounting_source"] == "CanonicalMessages"
                        and not session["accounting_history_incomplete"]
                        and not session["accounting_session_shared"], "Uncertified accounting owner")
            rows = db.execute("SELECT agent_id, record FROM task_usage_archive WHERE session_id=?",
                              (spec["session_id"],)).fetchall()
            require(len(rows) == 1, "Accounting archive coverage mismatch")
            agent, raw = rows[0]
            snapshot = json.loads(raw)
            require(snapshot["start"]["backend_kind"] == "antigravity"
                    and snapshot["start"]["session_id"] == spec["session_id"]
                    and snapshot["usage"]["kind"] == "known"
                    and snapshot.get("native_usage") is None, "Uncertified accounting archive")
            before = snapshot["usage"]["usage"]
            require(before == spec["saved"], "Audited archive snapshot mismatch")
            snapshot["usage"]["usage"] = {key: value for key, value in total.items() if value is not None}
            changes.append((agent, snapshot, before))
    report = {"version": 1, "mode": "repaired_copy" if args.output else "dry_run",
              "database_sha256": digest(db_bytes), "aggregate": aggregate, "ledgers": []}
    for spec, (_, original, _, total, count), (_, _, before) in zip(specs, evidence, changes):
        report["ledgers"].append({"alias": spec["alias"], "sha256": digest(original),
                                  "records": count, "requests": spec["requests"],
                                  "generations": spec["generations"], "before": before, "after": total})
    # Reverify immutable input bytes before publication. No originals are written.
    require(db_path.read_bytes() == db_bytes, "Database changed during verification")
    for path, original, _, _, _ in evidence:
        require(path.read_bytes() == original, "Ledger changed during verification")
    if args.output:
        args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
        copied_db = args.output / "sessions.sqlite3"
        shutil.copyfile(db_path, copied_db)
        with sqlite3.connect(copied_db) as db:
            for agent, snapshot, _ in changes:
                db.execute("UPDATE task_usage_archive SET record=? WHERE agent_id=?",
                           (json.dumps(snapshot, separators=(",", ":")), agent))
        for spec, (_, _, records, _, _) in zip(specs, evidence):
            data = b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode()
                            for record in records)
            (args.output / (spec["alias"] + ".jsonl")).write_bytes(data)
        (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--output", type=Path, help="Create a new directory of repaired copies; never install them")
    args = parser.parse_args()
    try:
        run(args)
    except (EvidenceError, OSError, ValueError, KeyError, TypeError, sqlite3.Error) as error:
        # Do not expose session identifiers, transcript content or database rows.
        reason = str(error) if isinstance(error, EvidenceError) else type(error).__name__
        print("Repair refused: " + reason, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
