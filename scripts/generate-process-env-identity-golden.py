#!/usr/bin/env python3
"""Write the process-environment identity golden from the Rust generator's log.

Run the ignored runtime::process::testing::identity generator through kiln test
with --exact --ignored --nocapture, then pass its test.log here.
"""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "crates/lash-core-execution/src/runtime/process/testing/fixtures/process_execution_env_identity.json"
MARKER = "PROCESS_ENV_IDENTITY_GOLDEN "


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    args = parser.parse_args()
    log = args.log.read_text()
    if log.count(MARKER) != 1:
        parser.error("expected exactly one Rust identity corpus in the log")
    corpus, _ = json.JSONDecoder().raw_decode(log.split(MARKER, 1)[1])
    OUTPUT.write_text(json.dumps(corpus, indent=2) + "\n")


if __name__ == "__main__":
    main()
