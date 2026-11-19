#!/usr/bin/env python3
"""Reject missing checks and unexpected skips, as well as failures/cancellations."""

from __future__ import annotations

import json
import os
import sys


FULL_JOBS = {"lint", "build", "check", "test", "policy"}
# Full-lane jobs that run only when the named `changes` output selects them.
TARGETED_JOBS = {"installer-drift": "installer"}
# Full-lane selection outputs consumed by conditional steps inside `policy`.
FULL_SELECTION_OUTPUTS = ("web",)
ALL_JOBS = FULL_JOBS | set(TARGETED_JOBS) | {"changes", "docs-style"}


def boolean_output(job: dict, name: str) -> bool:
    value = job.get("outputs", {}).get(name)
    if value not in {"true", "false"}:
        raise ValueError(f"missing or invalid {name} output")
    return value == "true"


def validate(needs: dict, event_name: str) -> list[str]:
    if not isinstance(needs, dict) or set(needs) != ALL_JOBS:
        return ["Required gate dependency set is incomplete or unknown"]
    if event_name not in {"pull_request", "push", "merge_group"}:
        return [f"Unsupported event: {event_name}"]
    if needs["changes"].get("result") != "success":
        return ["Change classification, formatting or the source tree gate did not succeed"]
    try:
        changes = needs["changes"]
        full = boolean_output(changes, "full")
        targeted = {
            job for job, output in TARGETED_JOBS.items()
            if full and boolean_output(changes, output)
        }
        if full:
            for output in FULL_SELECTION_OUTPUTS:
                boolean_output(changes, output)
    except (TypeError, ValueError) as error:
        return [str(error)]
    expected = {job: "skipped" for job in ALL_JOBS}
    expected.update({"changes": "success", "docs-style": "success"})
    if full:
        expected.update({job: "success" for job in FULL_JOBS | targeted})
    return [
        f"{job}: expected {wanted}, got {needs[job].get('result', 'missing')}"
        for job, wanted in sorted(expected.items())
        if needs[job].get("result") != wanted
    ]


def main() -> int:
    try:
        errors = validate(json.loads(os.environ["NEEDS_JSON"]), os.environ["GITHUB_EVENT_NAME"])
    except (KeyError, ValueError, TypeError, AttributeError) as error:
        errors = [f"Invalid gate inputs: {error}"]
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("All selected checks succeeded; every skip matches the change scope.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
