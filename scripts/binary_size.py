#!/usr/bin/env python3
"""Inspect an existing Kiln ELF by section, demangled symbol and crate."""
from __future__ import annotations

import argparse
from collections import defaultdict
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tools.buck2.outputs import resolve

SECTION = re.compile(
    r"\[\s*(\d+)\]\s+(\S+)\s+(\S+)\s+([0-9a-fA-F]+)\s+"
    r"([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+"
    r"(.*?)\s+(\d+)\s+(\d+)\s+(\d+)\s*$"
)
SYMBOL = re.compile(r"^([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+(\S)\s+(.+)$")
RUST_HASH = re.compile(r"::h[0-9a-f]{16}$")
CRATE = re.compile(r"^<?([A-Za-z_][A-Za-z_0-9]*)::")
MERGED = re.compile(r"(?:\.llvm\.|\.lto(?:\.|$)|\.constprop\.|\.isra\.|^LLVM)")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def command(tool: str, *args: str) -> str:
    return subprocess.run([tool, *args], check=True, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          env={**os.environ, "LC_ALL": "C"}).stdout


def tool_identity(name: str) -> dict:
    path = shutil.which(name)
    if path is None:
        raise ValueError(f"GNU {name} is required on PATH")
    version = command(path, "--version").splitlines()[0]
    if not version.startswith(f"GNU {name}"):
        raise ValueError(f"expected GNU {name}, found {version}")
    return {"path": str(Path(path).resolve()), "version": version}


def parse_sections(output: str) -> list[dict]:
    sections = []
    for line in output.splitlines():
        match = SECTION.search(line)
        if not match:
            continue
        index, name, kind, address, offset, size, _, flags, *_ = match.groups()
        sections.append({"index": int(index), "name": name, "type": kind,
                         "address": int(address, 16), "offset": int(offset, 16),
                         "bytes": int(size, 16), "flags": flags.strip()})
    if not sections:
        raise ValueError("readelf reported no named sections")
    return sections


def attribution(name: str) -> tuple[str, str | None]:
    """BINARY-SIZE-OWNERSHIP: only an unambiguous Rust path prefix owns bytes."""
    if MERGED.search(name):
        return "unattributed", "lto_merged"
    match = CRATE.match(name)
    if match:
        return match[1], None
    return "unattributed", "unknown"


def parse_symbols(output: str) -> list[dict]:
    symbols = []
    for line in output.splitlines():
        match = SYMBOL.match(line.strip())
        if not match:
            continue  # Undefined and unsized symbols have no measured extent.
        address, size, kind, name = match.groups()
        if int(size, 16) == 0:
            continue
        crate, reason = attribution(name)
        symbols.append({"address": int(address, 16), "bytes": int(size, 16),
                        "type": kind, "name": name, "key": RUST_HASH.sub("", name),
                        "crate": crate, "unattributed_reason": reason})
    return symbols


def crate_sizes(sections: list[dict], symbols: list[dict]) -> tuple[dict, dict]:
    """Partition every allocated section; overlaps and gaps never gain an owner."""
    allocated = [s for s in sections if "A" in s["flags"] and s["bytes"]]
    events = {s["index"]: defaultdict(list) for s in allocated}
    for index, symbol in enumerate(symbols):
        start, end = symbol["address"], symbol["address"] + symbol["bytes"]
        matches = [s for s in allocated if "T" not in s["flags"]
                   and s["address"] <= start and end <= s["address"] + s["bytes"]]
        if len(matches) == 1:
            event = events[matches[0]["index"]]
            event[start].append((True, index))
            event[end].append((False, index))
    sizes = defaultdict(int, {"unattributed": 0})
    reasons = dict.fromkeys(("unknown", "shared", "lto_merged"), 0)
    for section in allocated:
        event = events[section["index"]]
        start = section["address"]
        end = start + section["bytes"]
        event[start]
        event[end]
        previous = start
        active = set()
        for position in sorted(event):
            length = position - previous
            if length:
                crate, reason = "unattributed", "unknown"
                if len(active) > 1:
                    reason = "shared"
                elif active:
                    symbol = symbols[next(iter(active))]
                    crate, reason = symbol["crate"], symbol["unattributed_reason"]
                sizes[crate] += length
                if reason:
                    reasons[reason] += length
            for entering, index in event[position]:
                if entering:
                    active.add(index)
                else:
                    active.remove(index)
            previous = position
    return dict(sorted(sizes.items())), reasons


def input_binary(args: argparse.Namespace) -> tuple[Path, dict | None]:
    if args.elf:
        return args.elf.resolve(), None
    payload = json.loads(args.build_report.read_text())
    paths = [Path(p) for p in resolve(payload, args.label)]
    elfs = []
    for path in paths:
        if path.is_file():
            with path.open("rb") as stream:
                if stream.read(4) == b"\x7fELF":
                    elfs.append(path)
    if len(elfs) != 1:
        raise ValueError(f"expected one materialized ELF for {args.label}, found {len(elfs)}")
    label = "root" + args.label if args.label.startswith("//") else args.label
    return elfs[0], {"path": str(args.build_report.resolve()),
                     "sha256": digest(args.build_report), "label": label,
                     "metadata": {k: v for k, v in payload.items() if k != "results"},
                     "result": payload["results"][label]}


def inspect(binary: Path, build: dict | None) -> dict:
    with binary.open("rb") as stream:
        if stream.read(4) != b"\x7fELF":
            raise ValueError(f"not an ELF: {binary}")
    tools = {name: tool_identity(name) for name in ("nm", "readelf")}
    nm, readelf = tools["nm"]["path"], tools["readelf"]["path"]
    header = command(readelf, "-W", "-h", str(binary))
    if not re.search(r"Type:\s+(?:EXEC|DYN)\b", header):
        raise ValueError("expected a linked ELF executable or shared object")
    notes = command(readelf, "-W", "-n", str(binary))
    build_id = re.search(r"Build ID:\s*(\S+)", notes)
    sections = parse_sections(command(readelf, "-W", "-S", str(binary)))
    symbols = parse_symbols(command(nm, "--defined-only", "--print-size",
                                    "--demangle", "--radix=x", str(binary)))
    sizes, reasons = crate_sizes(sections, symbols)
    file_bytes = binary.stat().st_size
    file_sections = sum(s["bytes"] for s in sections if s["type"] != "NOBITS")
    return {"format": "lash-binary-size", "version": 1,
            "identity": {"path": str(binary), "sha256": digest(binary),
                         "gnu_build_id": build_id[1] if build_id else None,
                         "elf_header": header, "build_report": build},
            "tools": tools, "sections": sections, "symbols": symbols,
            "by_crate": sizes, "unattributed": reasons,
            "totals": {"file_bytes": file_bytes, "file_section_bytes": file_sections,
                       "file_headers_and_padding_bytes": file_bytes - file_sections,
                       "allocated_bytes": sum(sizes.values()),
                       "debug_bytes": sum(s["bytes"] for s in sections
                                          if s["name"].startswith((".debug", ".zdebug")))}}


def table(title: str, values: dict, top: int | None = None, signed: bool = False) -> None:
    print(f"\n{title} (bytes)")
    rows = sorted(values.items(), key=lambda row: (-row[1], row[0]))
    for name, size in rows[:top]:
        print(f"{size:+12d}  {name}" if signed else f"{size:12d}  {name}")


def show(receipt: dict, top: int) -> None:
    print(f"ELF: {receipt['identity']['path']}")
    print(f"SHA-256: {receipt['identity']['sha256']}")
    print(f"Build ID: {receipt['identity']['gnu_build_id'] or 'absent'}")
    sections = {s["name"]: s["bytes"] for s in receipt["sections"]}
    table("Primary sections", {name: sections.get(name, 0)
                              for name in (".text", ".rodata", ".data", ".bss")})
    table("Debug sections (separate from crate attribution)", {
        name: size for name, size in sections.items() if name.startswith((".debug", ".zdebug"))})
    table("Other sections", {name: size for name, size in sections.items()
                             if name not in (".text", ".rodata", ".data", ".bss")
                             and not name.startswith((".debug", ".zdebug"))})
    table("Totals (.bss/NOBITS occupies memory, not file contents)", receipt["totals"])
    print(f"\nTop {top} symbols (declared sizes; aliases may overlap)")
    for symbol in sorted(receipt["symbols"], key=lambda s: (-s["bytes"], s["name"]))[:top]:
        print(f"{symbol['bytes']:12d}  {symbol['name']}")
    table("Crates (all allocated section bytes, counted once)", receipt["by_crate"])
    table("Unattributed breakdown", receipt["unattributed"])
    if not receipt["symbols"]:
        print("No sized symbols found; stripped/unsized bytes remain unattributed.")


def symbol_totals(receipt: dict) -> dict:
    totals = defaultdict(int)
    for symbol in receipt["symbols"]:
        totals[symbol["key"]] += symbol["bytes"]
    return dict(totals)


def show_diff(current: dict, baseline: dict, top: int) -> None:
    if baseline.get("format") != "lash-binary-size" or baseline.get("version") != 1:
        raise ValueError("baseline is not a supported binary-size receipt")
    print(f"\nBaseline SHA-256: {baseline['identity']['sha256']}")
    for name in current["tools"]:
        if current["tools"][name]["version"] != baseline["tools"][name]["version"]:
            print(f"Tool version differs for {name}; interpret attribution changes cautiously.")
    for title, now, before in (
        ("Symbols (hash-normalized declared sizes)", symbol_totals(current), symbol_totals(baseline)),
        ("Crates (allocated bytes)", current["by_crate"], baseline["by_crate"]),
    ):
        delta = {name: now.get(name, 0) - before.get(name, 0) for name in now.keys() | before.keys()}
        table(f"{title}: top growth", {n: d for n, d in delta.items() if d > 0}, top, signed=True)
        # Sort greatest shrink first, then print the original negative delta.
        print(f"\n{title}: top shrinkage (bytes)")
        for name, size in sorted(((n, d) for n, d in delta.items() if d < 0),
                                 key=lambda row: (row[1], row[0]))[:top]:
            print(f"{size:+12d}  {name}")
    table("Total deltas", {n: v - baseline["totals"][n] for n, v in current["totals"].items()},
          signed=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--elf", type=Path, help="Existing linked ELF (never rebuilt).")
    source.add_argument("--build-report", type=Path, help="Kiln report with final materialization.")
    parser.add_argument("--label", help="Label in --build-report, e.g. //crates/lash-perf:lash-perf__bin.")
    parser.add_argument("--out", type=Path, default=Path("binary-size.receipt.json"))
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--top", type=int, default=20)
    args = parser.parse_args()
    if bool(args.build_report) != bool(args.label):
        parser.error("--label is required with --build-report and only valid with it")
    if args.top < 1:
        parser.error("--top must be positive")
    try:
        binary, build = input_binary(args)
        inputs = [binary, args.build_report, args.baseline]
        if args.out.resolve() in [p.resolve() for p in inputs if p]:
            raise ValueError("receipt output must not overwrite an input")
        baseline = json.loads(args.baseline.read_text()) if args.baseline else None
        receipt = inspect(binary, build)
        show(receipt, args.top)
        if baseline is not None:
            show_diff(receipt, baseline, args.top)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n")
        print(f"\nReceipt: {args.out}")
        return 0
    except subprocess.CalledProcessError as error:
        parser.exit(1, f"binary-size: {error.cmd[0]} failed: {error.stderr.strip()}\n")
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"binary-size: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
