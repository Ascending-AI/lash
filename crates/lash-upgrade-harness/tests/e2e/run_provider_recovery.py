#!/usr/bin/env python3
"""Run one full-path H1 recovery witness in a private Kiln service gate."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--report", type=Path, required=True)
parser.add_argument("--test", required=True)
args = parser.parse_args()
project = Path(__file__).resolve().parents[4]
spec = importlib.util.spec_from_file_location("outputs", project / "tools/buck2/outputs.py")
outputs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(outputs)
report = json.loads(args.report.read_text())

def one(label):
    paths = outputs.resolve(report, label)
    if len(paths) != 1:
        raise SystemExit(f"expected one prebuilt artifact for {label}, got {paths}")
    return paths[0]

gate = os.environ["KILN_GATE_ID"]
base = 61000 + (int(hashlib.sha256(gate.encode()).hexdigest()[:8], 16) % 90) * 50
artifacts = project / ".buck2" / "h1-recovery" / gate / uuid.uuid4().hex
artifacts.mkdir(parents=True, exist_ok=False)
env = os.environ.copy()
env.update({"LASH_E2E_ARTIFACT_DIR": str(artifacts), "LASH_E2E_PORT_BASE": str(base),
            "LASH_E2E_CANDIDATE_SHA": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True, cwd=project).strip(),
            "LASH_RESTATE_SERVER_BIN": str(project / ".buck2/native/restate/restate-server"),
            "LASH_UPGRADE_NODE_N": one("//crates/lash-upgrade-harness:lash-upgrade-node__bin")})
binary = one("//crates/lash-upgrade-harness:h1_recovery__test__rust_test")
listed = subprocess.check_output([binary, "--list"], text=True, env=env, cwd=project)
if f"{args.test}: test" not in listed.splitlines():
    raise SystemExit(f"full-path selector matched zero tests: {args.test}")
print(f"H1 private artifacts: {artifacts}", flush=True)
result = subprocess.run([binary, args.test, "--exact", "--nocapture", "--test-threads=1"],
                        env=env, cwd=project, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180)
(artifacts / "test-output.txt").write_text(result.stdout)
print(result.stdout, end="", flush=True)
if result.returncode == 0 and "1 passed; 0 failed" not in result.stdout:
    raise SystemExit("test process did not report exactly one executed passing witness")
raise SystemExit(result.returncode)
