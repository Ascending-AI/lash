#!/usr/bin/env python3
"""Capture simultaneous guest/native cell CPU samples and resolve source lines."""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import json
import hashlib
import os
from pathlib import Path
import re
import resource
import struct
import subprocess
import time

from perf_artifacts import artifacts, VM_WORKER_LABEL, runtime_label
from perf_capture import checked, tool


def loads(path: str) -> list[tuple[int, int, int]]:
    """ELF file offset, virtual address, file size for each load segment."""
    with open(path, "rb") as stream:
        header = stream.read(64)
        if header[:6] != b"\x7fELF\x02\x01":
            raise ValueError(f"expected little-endian ELF64: {path}")
        offset = struct.unpack_from("<Q", header, 32)[0]
        size, count = struct.unpack_from("<HH", header, 54)
        stream.seek(offset)
        rows = []
        for _ in range(count):
            entry = stream.read(size)
            if struct.unpack_from("<I", entry)[0] == 1:
                rows.append(struct.unpack_from("<QQ", entry, 8) + (struct.unpack_from("<Q", entry, 32)[0],))
        return rows


def resolve(raw: dict, out: Path) -> dict[int, list[str]]:
    addresses = {addr for sample in raw["samples"] for addr in sample["native"]}
    capture_hash = hashlib.sha256(json.dumps(raw, sort_keys=True).encode()).hexdigest()
    cache = out / "symbols.json"
    stamp = out / "symbols.capture.sha256"
    if cache.exists() and stamp.exists() and stamp.read_text().strip() == capture_hash:
        cached = {int(key): value for key, value in json.loads(cache.read_text()).items()}
        if cached.keys() == addresses:
            return cached
    maps = []
    for line in raw["maps"].splitlines():
        fields = line.split(maxsplit=5)
        if len(fields) < 6 or "x" not in fields[1] or not fields[5].startswith("/"):
            continue
        low, high = (int(s, 16) for s in fields[0].split("-"))
        maps.append((low, high, int(fields[2], 16), fields[5]))
    grouped = defaultdict(dict)
    translated = {}
    segments = {}
    for addr in addresses:
        for low, high, offset, path in maps:
            if low <= addr < high:
                if path not in segments:
                    segments[path] = loads(path)
                file_offset = addr - low + offset
                virtual = next((virtual + file_offset - offset for offset, virtual, size in segments[path]
                                if offset <= file_offset < offset + size), None)
                if virtual is not None:
                    grouped[path][addr] = virtual
                break
        else:
            translated[addr] = [f"[unmapped 0x{addr:x}]"]
    environment = dict(os.environ, DEBUGINFOD_URLS="")
    for path, addresses in grouped.items():
        unique = sorted(set(addresses.values()))
        result = checked([tool("llvm-addr2line"), "-a", "-f", "-C", "-i", "-e", path],
                         input="".join(f"0x{addr:x}\n" for addr in unique), env=environment,
                         timeout=45).stdout
        by_address = {}
        current = None
        lines = []
        for line in result.splitlines() + ["0x0"]:
            if re.fullmatch(r"0x[0-9a-fA-F]+", line):
                if current is not None:
                    frames = [f"{name} ({location})" for name, location in zip(lines[::2], lines[1::2])]
                    by_address[current] = frames or [f"[{Path(path).name}+0x{current:x}]"]
                current, lines = int(line, 16), []
            else:
                lines.append(line)
        for addr, virtual in addresses.items():
            translated[addr] = by_address[virtual]
    for addr in addresses:
        translated.setdefault(addr, [f"[unresolved 0x{addr:x}]"])
    cache.write_text(json.dumps(translated, indent=2) + "\n")
    stamp.write_text(capture_hash + "\n")
    return translated


def authored_name(code: dict, raw: dict) -> str:
    if code.get("authored_name"):
        return code["authored_name"]
    if not code["site"].get("path", []) and code["name"] == "main":
        return "<cell>"
    if code["offset"] is None:
        return code["name"] + " [library]"
    if code["offset"] is not None:
        candidates = []
        for named in raw["codes"]:
            if not named.get("authored_name"):
                continue
            for entry in raw["source_map"]["sites"]:
                if entry["site"]["unit"] != named["site"]["unit"]:
                    continue
                for span in entry["ranges"]:
                    if span["start"] <= code["offset"] < span["end"]:
                        candidates.append((span["end"] - span["start"], named["authored_name"]))
        if candidates:
            return min(candidates)[1] + " [body]"
    return "<lambda>" if raw["file"].endswith(".py") else "<anonymous>"


def report(raw: dict, out: Path, top: int = 10) -> dict:
    at = time.monotonic()
    symbols = resolve(raw, out)
    if not any("crates/lash-" in frame and re.search(r":[1-9]\d*", frame)
               for frames in symbols.values() for frame in frames):
        raise ValueError("native capture has no Lash source lines; use the symbolized profiling build")
    codes = {code["token"]: code for code in raw["codes"]}
    names = {token: authored_name(code, raw) for token, code in codes.items()}
    filename = raw["file"]
    guest, native, joined = Counter(), Counter(), Counter()
    guest_self, native_self, joined_self = Counter(), Counter(), Counter()
    folded = {"guest": Counter(), "rust": Counter(), "joined": Counter()}
    def clean(frame):
        return frame.replace(";", ":").replace("\n", " ")
    for sample in raw["samples"]:
        frames = []
        sites = []
        for frame in sample["guest"]:
            token, statement = frame & 0xffffffff, frame >> 32
            code = codes[token]
            statement_line = raw["statements"][statement] if statement < len(raw["statements"]) else None
            line = code["line"] or statement_line
            location = f"{filename}:{line}" if line else "library"
            frames.append(clean(f"{names[token]} ({location})"))
            site_line = statement_line or line
            site = f"{filename}:{site_line}" if site_line else "library"
            sites.append(clean(f"{names[token]} ({site})"))
        if sample["truncated_guest"]:
            frames.append("[guest truncated]")
            sites.append("[guest truncated]")
        if sample["phase"].startswith(("Native(", "Formula(")):
            token = int(re.search(r"\((\d+)\)", sample["phase"])[1])
            name = next(row["name"] for row in raw["libs"] if row["token"] == token)
            phase = clean(f"{sample['phase'].split('(')[0]}:{name} [library]")
            frames.append(phase)
            sites.append(phase)
        elif sample["phase"] == "Collect":
            frames.append("[collect]")
            sites.append("[collect]")
        frames = frames or ["[machine]"]
        sites = sites or ["[machine]"]
        # addr2line emits inner inline frame first, as does the raw native walk.
        rust = [clean(frame) for addr in sample["native"] for frame in symbols[addr]]
        if sample["truncated_native"]:
            rust.append("[native truncated]")
        rust = list(reversed(rust))
        for key, stack in (("guest", frames), ("rust", rust), ("joined", sites + rust)):
            folded[key][";".join(stack)] += 1
        guest.update(set(frames)); guest_self.update(frames[-1:])
        # Keep full inline/native chains in the folded artifacts. Text rows
        # charge inline std/dependency work to its innermost Lash source frame.
        application = [frame for frame in rust if "crates/lash-" in frame] or rust
        native.update(set(application)); native_self.update(application[-1:])
        # Each guest site gets the native stack observed in the same interrupt.
        for site in set(sites):
            joined.update((site, frame) for frame in set(application))
            joined_self.update((site, frame) for frame in application[-1:])
    tables = {}
    for label, inclusive, selves in (("guest", guest, guest_self), ("rust", native, native_self),
                                      ("joined", joined, joined_self)):
        tables[label] = [{"site": key if isinstance(key, str) else " -> ".join(key),
                          "self": selves[key], "inclusive": inclusive[key]}
                         for key in sorted(inclusive, key=lambda k: (-inclusive[k], -selves[k], k) if label == "guest"
                                             else (-selves[k], -inclusive[k], k))]
    totals = {key: raw[key] for key in ("instructions", "running_fraction", "allocations", "charge", "wall_seconds",
                                       "sample_hz", "refused", "overflow", "overruns", "pid", "window")}
    for key in ("sampler_instructions", "sampler_counter_failures", "guest_accuracy"):
        if key in raw:
            totals[key] = raw[key]
    totals["samples"] = len(raw["samples"])
    totals["truncated_guest"] = sum(s["truncated_guest"] for s in raw["samples"])
    totals["truncated_native"] = sum(s["truncated_native"] for s in raw["samples"])
    totals["unresolved_native_addresses"] = sum(not any(re.search(r":[1-9]\d*", frame) for frame in frames)
                                                for frames in symbols.values())
    totals["symbolize_seconds"] = time.monotonic() - at
    lines = [f"Cell {filename}", "| Total | Value |", "|---|---:|",
             f"| instructions:u | {totals['instructions']['instructions']} |",
             f"| PMU time enabled / running (ns) | {totals['instructions']['time_enabled']} / {totals['instructions']['time_running']} |",
             f"| PMU running fraction | {totals['running_fraction']:.6f} |",
             f"| Allocation / reallocation calls | {totals['allocations']} |",
             f"| Exact charge | {totals['charge']} |",
             f"| Observed wall seconds | {totals['wall_seconds']:.6f} |",
             f"| CPU samples / refused / overruns / overflow | {totals['samples']} / {totals['refused']} / {totals['overruns']} / {totals['overflow']} |",
             f"| Truncated guest / native samples | {totals['truncated_guest']} / {totals['truncated_native']} |",
             f"| Native addresses without source lines | {totals['unresolved_native_addresses']} |",
             f"| Source resolution seconds | {totals['symbolize_seconds']:.3f} |"]
    if "sampler_instructions" in totals:
        lines.append(f"Sampler handler instructions:u: {totals['sampler_instructions']}; "
                     f"counter failures: {totals['sampler_counter_failures']}.")
    if "guest_accuracy" in totals:
        lines.append(f"Guest accuracy: {totals['guest_accuracy']}.")
    lines.append("Rust text self samples include inlined std/dependency work at the innermost Lash source frame; folded stacks retain every frame.")
    for label, rows in tables.items():
        lines += [f"\n{label}: self / inclusive CPU samples", "| Self | Inclusive | Function (file:line) |",
                  "|---:|---:|---|"]
        lines += [f"| {row['self']} | {row['inclusive']} | {row['site']} |" for row in rows[:top]]
    for label, stacks in folded.items():
        path = out / f"{label}.folded"
        path.write_text("".join(f"{stack} {count}\n" for stack, count in sorted(stacks.items())))
        svg = checked([tool("inferno-flamegraph"), "--title", f"Cell {label} CPU", str(path)]).stdout
        (out / f"{label}.svg").write_text(svg)
    receipt = {"tables": tables, "totals": totals}
    (out / "report.json").write_text(json.dumps(receipt, indent=2) + "\n")
    text = "\n".join(lines) + "\n"
    (out / "tables.txt").write_text(text)
    print(text, end="")
    return receipt


def capture(root: Path, file: Path, out: Path, *, worker: bool = False,
            build: bool = True, build_report: Path | None = None) -> dict:
    if not file.is_file() or file.suffix not in (".ts", ".py"):
        raise ValueError(f"expected an existing .ts or .py source file: {file}")
    out.mkdir(parents=True, exist_ok=True)
    if (out / "raw.json").exists():
        raise ValueError(f"output already contains a capture: {out}")
    label = VM_WORKER_LABEL if worker else runtime_label([])
    binary = artifacts(root, [label], build=build, report=build_report or out / "build.json",
                       optimized=True, symbolized=True)[label]
    def limit():
        resource.setrlimit(resource.RLIMIT_AS, (8 * 1024**3, 8 * 1024**3))
    checked([str(binary), "--profile-cell", str(file), str(out)], cwd=root, timeout=90, preexec_fn=limit)
    raw = json.loads((out / "raw.json").read_text())
    with binary.open("rb") as stream:
        artifact_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    identity = {"checkout_commit": checked(["git", "rev-parse", "HEAD"], cwd=root).stdout.strip(),
                "tracked_tree_dirty": bool(checked(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root).stdout),
                "source_sha256": hashlib.sha256(raw["source"].encode()).hexdigest(),
                "artifact_sha256": artifact_hash, "worker": worker,
                "build_report": str(build_report or out / "build.json")}
    (out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    return report(raw, out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--build-report", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    file = args.file.resolve()
    if file.suffix not in (".ts", ".py"):
        parser.error("cell must be a .ts or .py file")
    out = (args.out or file.with_name(file.name + ".profile")).resolve()
    started = time.monotonic()
    try:
        capture(root, file, out, worker=args.worker, build=not args.no_build, build_report=args.build_report)
        print(f"Start to tables: {time.monotonic() - started:.3f}s; artifacts: {out}")
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(f"cell profile failed: {error}") from error


if __name__ == "__main__":
    main()
