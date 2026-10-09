#!/usr/bin/env python3
"""Embed a shard tree in its owning crate, without runtime filesystem I/O."""

import argparse
import json
import os
import re
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--tests-output", type=Path,
                        help="emit one executable machine law per document shard")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    lines = ["// Regenerate with crates/lash-kernel-conformance/index.py; owned with this shard tree.",
             "const CORPUS_FILES: &[(&str, &str)] = &["]
    paths = sorted(args.root.rglob("*.json"))
    for path in paths:
        relative = os.path.relpath(path.resolve(), output.parent)
        lines.append(f"    ({json.dumps(path.name)}, include_str!({json.dumps(relative)})),")
    lines += ["];", ""]
    content = "\n".join(lines)
    if args.check:
        pattern = r'\(\s*("[^"\n]+")\s*,\s*include_str!\(("[^"\n]+")\)\s*,?\s*\)'
        if re.findall(pattern, output.read_text()) != re.findall(pattern, content):
            raise SystemExit("corpus embedding index is stale")
    else:
        output.write_text(content)
    if args.tests_output:
        tests_output = args.tests_output.resolve()
        lines = ["// One executable law per rule shard, owned with this corpus."]
        for path in paths:
            rule = json.loads(path.read_text())["rule"]
            if not re.fullmatch(r"K-[A-Z]+-[0-9]{3}", rule):
                raise SystemExit(f"invalid rule id in {path}")
            relative = os.path.relpath(path.resolve(), tests_output.parent)
            lines.extend(["#[test]", f"fn {rule.lower().replace('-', '_')}() {{",
                          f"    super::run_rule({json.dumps(rule)}, include_str!({json.dumps(relative)}));",
                          "}", ""])
        content = "\n".join(lines)
        if args.check:
            pattern = r'super::run_rule\(\s*("[^"\n]+")\s*,\s*include_str!\(("[^"\n]+")\)\s*,?\s*\)'
            if re.findall(pattern, tests_output.read_text()) != re.findall(pattern, content):
                raise SystemExit("machine law index is stale")
        else:
            tests_output.write_text(content)


if __name__ == "__main__":
    main()
