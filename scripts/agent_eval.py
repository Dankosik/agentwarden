#!/usr/bin/env python3
"""Validate evaluation cases, emit a prompt, or summarize an existing Codex JSONL trace.

Offline only: this module never runs a model, a shell, or a recorded command.
A completed trace is execution evidence, not a passing behavioral evaluation.
"""

import argparse
from collections import Counter
import json
from pathlib import Path, PurePosixPath
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
CATALOG = ROOT / "evals/agent-cases.json"
USAGE_KEYS = ("input_tokens", "cached_input_tokens", "output_tokens")
KNOWN_EVENTS = {
    "thread.started", "turn.started", "turn.completed", "turn.failed", "error",
    "item.started", "item.updated", "item.completed",
}


def load_cases(path=CATALOG):
    """Reject malformed/ambiguous cases; leave semantic grading to the reviewer."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict) or set(data) != {"version", "cases"}:
        raise ValueError("catalog requires exactly version and cases")
    if type(data["version"]) is not int or data["version"] != 1:
        raise ValueError("unsupported catalog version")
    cases = data["cases"]
    if not isinstance(cases, list) or not cases:
        raise ValueError("cases must be a nonempty list")
    seen = set()
    keys = {"id", "title", "mode", "route", "prompt", "fixture", "checks"}
    for case in cases:
        if not isinstance(case, dict) or set(case) != keys:
            raise ValueError("invalid case fields")
        for key in ("id", "title", "mode", "route", "prompt"):
            if not isinstance(case[key], str) or not case[key].strip():
                raise ValueError("case field must be nonempty text: " + key)
        name = case["id"]
        if not re.fullmatch(r"T[0-9]{2}", name) or name in seen:
            raise ValueError("invalid or duplicate case id: " + name)
        seen.add(name)
        if case["mode"] not in {"explicit", "implicit", "negative"}:
            raise ValueError("invalid activation mode: " + name)
        if not re.fullmatch(r"rust-[a-z-]+", case["route"]):
            raise ValueError("invalid routing target: " + name)
        if case["mode"] == "explicit" and "$" + case["route"] not in case["prompt"]:
            raise ValueError("explicit prompt must name its skill: " + name)
        fixture = case["fixture"]
        if not isinstance(fixture, dict) or set(fixture) != {"kind", "paths", "setup"}:
            raise ValueError("invalid fixture: " + name)
        if fixture["kind"] not in {"repository", "controlled"}:
            raise ValueError("invalid fixture kind: " + name)
        if not isinstance(fixture["setup"], str):
            raise ValueError("fixture setup must be text: " + name)
        if fixture["kind"] == "controlled" and not fixture["setup"].strip():
            raise ValueError("controlled fixture requires setup: " + name)
        paths = fixture["paths"]
        if not isinstance(paths, list) or not paths:
            raise ValueError("fixture paths must be a nonempty list: " + name)
        for value in paths:
            if not isinstance(value, str) or not value or "\\" in value or ":" in value:
                raise ValueError("nonportable fixture path: " + name)
            path = PurePosixPath(value)
            if path.is_absolute() or ".." in path.parts or path.as_posix() != value or value == ".":
                raise ValueError("unsafe fixture path: " + name)
        checks = case["checks"]
        if not isinstance(checks, list) or not checks or any(
            not isinstance(item, str) or not item.strip() for item in checks
        ):
            raise ValueError("checks must be nonempty text: " + name)
    return cases


def prompt_for(cases, case_id):
    """Emit only the request; never concatenate the fixture or grading rubric."""
    for case in cases:
        if case["id"] == case_id:
            return case["prompt"]
    raise ValueError("unknown case: " + case_id)


def summarize(lines, exit_code):
    """Summarize observed events without executing or interpreting command strings."""
    if type(exit_code) is not int:
        raise ValueError("exit code must be an integer")
    event_counts = Counter()
    commands = []
    completed_ids = set()
    open_items = set()
    usage = []
    active_turn = False
    completed_turns = 0
    failures = 0
    lifecycle_gaps = []
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except (ValueError, TypeError) as error:
            raise ValueError("invalid JSON at trace line " + str(number)) from error
        if not isinstance(event, dict) or not isinstance(event.get("type"), str):
            raise ValueError("invalid event at trace line " + str(number))
        kind = event["type"]
        event_counts[kind] += 1
        if kind == "turn.started":
            if active_turn:
                lifecycle_gaps.append("overlapping turn start")
            active_turn = True
        elif kind == "turn.completed":
            if not active_turn:
                lifecycle_gaps.append("completion without turn start")
            active_turn = False
            completed_turns += 1
            values = event.get("usage")
            if values is not None and not isinstance(values, dict):
                raise ValueError("invalid usage at trace line " + str(number))
            values = values or {}
            for key in USAGE_KEYS:
                value = values.get(key)
                if value is not None and (type(value) is not int or value < 0):
                    raise ValueError("invalid token count: " + key)
            usage.append(values)
        elif kind in {"turn.failed", "error"}:
            failures += 1
            if kind == "turn.failed":
                active_turn = False
        elif kind in {"item.started", "item.updated", "item.completed"}:
            item = event.get("item")
            if (not isinstance(item, dict) or not isinstance(item.get("id"), str)
                    or not item["id"]):
                raise ValueError("invalid item at trace line " + str(number))
            if not active_turn:
                lifecycle_gaps.append("item outside active turn")
            identity = item["id"]
            if kind == "item.started":
                open_items.add(identity)
            if kind != "item.completed":
                continue
            if identity in completed_ids:
                raise ValueError("duplicate completed item: " + identity)
            completed_ids.add(identity)
            open_items.discard(identity)
            if item.get("type") == "command_execution":
                command = item.get("command")
                status = item.get("exit_code")
                if not isinstance(command, str) or (status is not None and type(status) is not int):
                    raise ValueError("invalid command evidence: " + identity)
                commands.append({"id": identity, "command": command, "exit_code": status})
    if exit_code != 0 or failures:
        status = "failed"
    elif not completed_turns or active_turn or open_items or lifecycle_gaps:
        status = "incomplete"
    else:
        status = "completed"
    counts = Counter(item["command"] for item in commands)
    totals = {
        key: sum(value[key] for value in usage)
        if usage and all(value.get(key) is not None for value in usage) else None
        for key in USAGE_KEYS
    }
    return {
        "trace_status": status,
        "behavioral_grade": "NOT_GRADED",
        "process_exit_code": exit_code,
        "event_counts": dict(sorted(event_counts.items())),
        "unknown_event_types": sorted(set(event_counts) - KNOWN_EVENTS),
        "completed_turns": completed_turns,
        "unfinished_items": sorted(open_items),
        "lifecycle_gaps": lifecycle_gaps,
        "commands": commands,
        "command_count": len(commands),
        "failed_command_count": sum(item["exit_code"] not in (0, None) for item in commands),
        "unknown_command_exit_count": sum(item["exit_code"] is None for item in commands),
        "repeated_command_strings": {key: value for key, value in counts.items() if value > 1},
        "usage": totals,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", type=Path, default=CATALOG)
    actions = parser.add_subparsers(dest="action", required=True)
    actions.add_parser("validate", help="validate the case catalog without running models")
    prompt = actions.add_parser("prompt", help="print only one natural-language request")
    prompt.add_argument("case_id")
    trace = actions.add_parser("summarize", help="summarize an existing Codex JSONL trace")
    trace.add_argument("trace", type=Path)
    trace.add_argument("--exit-code", type=int, required=True)
    args = parser.parse_args(argv)
    try:
        if args.action == "summarize":
            with args.trace.open(encoding="utf-8") as stream:
                result = summarize(stream, args.exit_code)
            print(json.dumps(result, indent=2, ensure_ascii=True))
            return 0 if result["trace_status"] == "completed" else 1
        cases = load_cases(args.catalog)
        if args.action == "prompt":
            print(prompt_for(cases, args.case_id))
        else:
            print("Validated " + str(len(cases)) + " evaluation cases; no model runs performed.")
    except (OSError, ValueError, TypeError) as error:
        print("Agent evaluation error: " + str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
