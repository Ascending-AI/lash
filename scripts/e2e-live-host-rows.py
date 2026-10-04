#!/usr/bin/env python3
"""Run one materialized H6 paid row, or record all four as NotRun.

Invoke inside a private Kiln gate with its prebuilt e2e_hosts binary and host
artifact identities. Budget configuration belongs to the operator; the host
refuses a missing or exceeded cap before provider transport.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess

SELECTORS = {
    "S35/file-edit-bugfix": "s35_file_edit_bugfix",
    "S35/missing-helper-file": "s35_missing_helper_file",
    "S35/config-contract-edit": "s35_config_contract_edit",
    "S36/workbench-weather": "s36_workbench_weather",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--row", choices=SELECTORS)
    parser.add_argument("--test-bin", type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    if not os.environ.get("OPENROUTER_API_KEY", "").strip():
        rows = [{"row": row, "selected": 1, "executed": 0,
                 "verdict": "NotRun", "reason": "OPENROUTER_API_KEY is absent"}
                for row in SELECTORS]
        (args.directory / "live-rows.json").write_text(json.dumps(rows, indent=2) + "\n")
        print(json.dumps({"selected": len(SELECTORS), "executed": 0, "verdict": "NotRun"}))
        return 78
    if args.row is None or args.test_bin is None:
        parser.error("funded execution requires --row and --test-bin")
    environment = dict(os.environ)
    environment["LASH_E2E_HOST_ARTIFACTS"] = str(args.directory.resolve())
    return subprocess.call([str(args.test_bin.resolve()), "--ignored", "--exact",
                            SELECTORS[args.row], "--nocapture"], env=environment)


if __name__ == "__main__":
    raise SystemExit(main())
