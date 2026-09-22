"""Offline Bazel 9 diagnostics using the checksum-pinned BuildBuddy decoder."""

import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import urllib.request
from datetime import datetime

BB_VERSION = "5.0.478"
BB_HASHES = {
    (
        "Linux",
        "x86_64",
    ): "223f50ca5b7bb8e8e44d026b103a9a67f85344628eba12625126a7875418ad4c",
    (
        "Linux",
        "aarch64",
    ): "2960d6e0318c067b957cc33365104be3dc1d4c354afca49f563e3202536c6f0d",
    (
        "Darwin",
        "x86_64",
    ): "b44f04faccd4773167cdd328f363ab8f42c6d5b8f74ee1de97f4bf285d4bb382",
    (
        "Darwin",
        "arm64",
    ): "0ca65b967e57ae0d0679568f9db8ac1a14c53acce6d5a11861e2e41071d823a2",
}


def bb_binary():
    host = (platform.system(), platform.machine())
    expected = BB_HASHES.get(host)
    if not expected:
        raise ValueError(f"No pinned BuildBuddy decoder for {host}")
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    path = cache / "lash-diagnostics" / BB_VERSION / "bb"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_file():
        with path.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() == expected:
                return path
    architecture = "arm64" if host[1] == "aarch64" else host[1]
    url = (
        f"https://github.com/buildbuddy-io/bazel/releases/download/{BB_VERSION}/"
        f"bazel-{BB_VERSION}-{host[0].lower()}-{architecture}"
    )
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as target:
        temporary = Path(target.name)
        try:
            with urllib.request.urlopen(url, timeout=60) as source:
                while chunk := source.read(1024 * 1024):
                    target.write(chunk)
            target.close()
            with temporary.open("rb") as source:
                actual = hashlib.file_digest(source, "sha256").hexdigest()
            if actual != expected:
                raise ValueError("BuildBuddy decoder checksum mismatch")
            temporary.chmod(0o755)
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)
    return path


def json_stream(source):
    """Read concatenated protobuf JSON objects without retaining expanded inputs."""
    decoder = json.JSONDecoder()
    buffer = ""
    while chunk := source.read(65536):
        buffer += chunk
        while buffer := buffer.lstrip():
            try:
                value, end = decoder.raw_decode(buffer)
            except json.JSONDecodeError:
                break
            yield value
            buffer = buffer[end:]
        if len(buffer) > 128 * 1024 * 1024:
            raise ValueError("Decoded log entry exceeds 128 MiB")
    if buffer.strip():
        raise ValueError("Truncated or invalid decoded JSON")


def decoded(binary, bundle, filename, flag, errors):
    path = bundle / filename
    if not path.is_file():
        errors.append(f"Missing {filename}")
        return
    with (
        tempfile.TemporaryFile(mode="w+") as output,
        tempfile.TemporaryFile(mode="w+") as stderr,
    ):
        result = subprocess.run(
            [
                str(binary),
                "print",
                f"{flag}={path}",
                *(["--raw"] if flag == "--compact_execution_log" else []),
            ],
            stdout=output,
            stderr=stderr,
        )
        output.seek(0)
        try:
            yield from json_stream(output)
        except ValueError as error:
            errors.append(f"{filename}: {error}")
        if result.returncode:
            stderr.seek(0)
            errors.append(
                f"{filename}: decoder exited {result.returncode}: {stderr.read(2000).strip()}"
            )


def digest_key(digest):
    return (digest.get("hash"), str(digest.get("sizeBytes", "0")))


def phase(metadata, start, end):
    if start not in metadata or end not in metadata:
        return {"seconds": None, "status": "missing"}
    elapsed = (
        datetime.fromisoformat(metadata[end].replace("Z", "+00:00"))
        - datetime.fromisoformat(metadata[start].replace("Z", "+00:00"))
    ).total_seconds()
    return {
        "seconds": elapsed if elapsed >= 0 else None,
        "status": "measured" if elapsed >= 0 else "clock_skew",
    }


def action_metadata(entries):
    """Join only protocol action digests; never guess from timestamps or labels."""
    results = {}
    operations = {}
    waiting = []
    for entry in entries:
        details = entry.get("details", {})
        if "getActionResult" in details:
            call = details["getActionResult"]
            result = call.get("response", {})
            if result:
                results.setdefault(
                    digest_key(call.get("request", {}).get("actionDigest", {})), []
                ).append((True, result))
        for method in ("execute", "waitExecution"):
            if method not in details:
                continue
            call = details[method]
            key = (
                digest_key(call.get("request", {}).get("actionDigest", {}))
                if method == "execute"
                else None
            )
            for operation in call.get("responses", []):
                name = operation.get("name")
                if key and key[0] and name:
                    operations[name] = key
                response = operation.get("response", {})
                if operation.get("done") and "result" in response:
                    waiting.append((key, name, response))
    for key, name, response in waiting:
        key = key or operations.get(name)
        if key:
            results.setdefault(key, []).append(
                (response.get("cachedResult", False), response["result"])
            )
    return results


def summarize(binary, bundle):
    errors = []
    manifest = bundle / "manifest.json"
    if manifest.is_file():
        capture = json.loads(manifest.read_text())
        state = capture.get("state")
        if capture.get("source_identity_error"):
            errors.append(f"Source identity unavailable: {capture['source_identity_error']}")
        if state in ("running", "interrupted"):
            errors.append(
                f"Build {state}; unfinished actions may be absent from the logs"
            )
    metadata = action_metadata(
        decoded(binary, bundle, "grpc.bin", "--grpc_log", errors)
    )
    actions = []
    for entry in decoded(
        binary, bundle, "execution.zst", "--compact_execution_log", errors
    ):
        if "spawn" not in entry:
            continue
        spawn = entry["spawn"]
        digest = spawn.get("digest", {})
        cached = spawn.get("cacheHit", False)
        matches = metadata.get(digest_key(digest), [])
        # An Execute cache hit is historical attribution even if the client did
        # not mark the compact spawn as a cache hit.
        fresh = [result for hit, result in matches if not hit]
        historical = [result for hit, result in matches if hit]
        candidates = historical if cached else fresh
        unique = {
            json.dumps(result.get("executionMetadata", {}), sort_keys=True)
            for result in candidates
        }
        detail = json.loads(next(iter(unique))) if len(unique) == 1 else {}
        status = "cache_hit" if cached else "unavailable"
        if not cached and len(unique) > 1:
            status = "ambiguous_attempts"
        elif detail.get("worker"):
            status = "historical_cache_worker" if cached else "executed"
        elif not cached and historical and not fresh:
            status = "cache_hit"
        phases = {
            name: phase(detail, start, end)
            for name, start, end in (
                ("queue", "queuedTimestamp", "workerStartTimestamp"),
                (
                    "input_fetch",
                    "inputFetchStartTimestamp",
                    "inputFetchCompletedTimestamp",
                ),
                ("execution", "executionStartTimestamp", "executionCompletedTimestamp"),
                (
                    "output_upload",
                    "outputUploadStartTimestamp",
                    "outputUploadCompletedTimestamp",
                ),
            )
        }
        actions.append(
            {
                "label": spawn.get("targetLabel"),
                "mnemonic": spawn.get("mnemonic"),
                "digest": digest,
                "runner": spawn.get("runner"),
                "attribution": status,
                "worker": detail.get("worker"),
                "phases": phases,
                "properties": spawn.get("platform", {}).get("properties", []),
                "metrics": spawn.get("metrics", {}),
            }
        )
    summary = {"complete": not errors, "errors": errors, "actions": actions}
    (bundle / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    fresh = [a for a in actions if a["attribution"] == "executed"]
    print(
        f"{len(actions)} actions; {len(fresh)} with fresh worker attribution; "
        f"{sum(a['attribution'] == 'unavailable' for a in actions)} unavailable; "
        f"capture {'complete' if not errors else 'incomplete'}"
    )
    for action in sorted(
        fresh, key=lambda a: a["phases"]["execution"]["seconds"] or 0, reverse=True
    )[:10]:
        durations = " ".join(
            f"{name}={v['seconds']}s"
            if v["seconds"] is not None
            else f"{name}={v['status']}"
            for name, v in action["phases"].items()
        )
        print(f"{action['label']} {action['mnemonic']} {action['worker']}: {durations}")
    return summary
