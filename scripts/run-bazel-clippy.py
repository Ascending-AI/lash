#!/usr/bin/env python3
"""Require a completed Clippy output group for every requested Bazel target."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile


ASPECT = "//tools/bazel:clippy.bzl%lash_clippy_aspect"


def validate_events(path: Path) -> None:
    requested = set()
    groups = {}
    file_sets = {}
    with path.open(encoding="utf-8") as events:
        for line in events:
            event = json.loads(line)
            identity = event.get("id", {})
            configured = identity.get("targetConfigured")
            if configured and not configured.get("aspect"):
                for child in event.get("children", []):
                    completed = child.get("targetCompleted")
                    if completed:
                        requested.add((completed["label"], completed.get("configuration", {}).get("id")))
            named = identity.get("namedSet")
            if named:
                file_sets[named["id"]] = event.get("namedSetOfFiles", {})
            completed = identity.get("targetCompleted")
            if not completed:
                continue
            result = event.get("completed", {})
            key = (completed["label"], completed.get("configuration", {}).get("id"))
            if result.get("success"):
                for group in result.get("outputGroup", []):
                    if group.get("name") == "clippy_checks" and not group.get("incomplete"):
                        groups.setdefault(key, []).extend(group.get("fileSets", []))

    def has_marker(roots):
        pending = [root["id"] for root in roots]
        seen = set()
        while pending:
            identifier = pending.pop()
            if identifier in seen:
                continue
            seen.add(identifier)
            files = file_sets.get(identifier, {})
            if any(file.get("name", "").endswith(".lash-clippy.ok") for file in files.get("files", [])):
                return True
            pending.extend(child["id"] for child in files.get("fileSets", []))
        return False

    missing = {label for label, configuration in requested if not has_marker(groups.get((label, configuration), []))}
    if not requested or missing:
        raise ValueError("no Clippy verdict for: " + ", ".join(sorted(missing or {"<no targets>"})))


def run(bazel: str, options: list[str]) -> int:
    with tempfile.TemporaryDirectory(prefix="lash-clippy-") as temporary:
        events = Path(temporary) / "events.json"
        extra = ["--aspects=" + ASPECT]
        separator = options.index("--") if "--" in options else len(options)
        flags = options[:separator]
        extra.append("--output_groups=" + (
            "+clippy_checks" if any(flag.startswith("--output_groups") for flag in flags)
            else "clippy_checks"
        ))
        for index, flag in enumerate(flags):
            if flag.startswith("--build_event_json_file="):
                events = Path(flag.partition("=")[2])
            elif flag == "--build_event_json_file" and index + 1 < len(flags):
                events = Path(flags[index + 1])
        if events == Path(temporary) / "events.json":
            extra.append("--build_event_json_file=" + str(events))
        arguments = [bazel, "build", *flags, *extra, *options[separator:]]
        result = subprocess.run(arguments, check=False)
        if result.returncode:
            return result.returncode
        try:
            validate_events(events)
        except (OSError, ValueError, KeyError) as error:
            print("hermetic-build: " + str(error), file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(run(sys.argv[1], sys.argv[2:]))
