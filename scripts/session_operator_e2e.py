#!/usr/bin/env python3
"""Execute each operator law on private live services; retain every run's logs."""
import json
import hashlib
import os
from pathlib import Path
import signal
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent / "ci"))
from restate_suite import RestateServer, RESTATE_VERSION
from check_session_operator_evidence import check


def main():
    binary, root, base, repeats = sys.argv[1:]
    root = Path(root).resolve()
    base, repeats = int(base), int(repeats)
    if repeats < 1:
        raise SystemExit("at least one real execution is required")
    gate = os.environ["KILN_GATE_ID"]
    # The existing service registry owns the PostgreSQL image selection.
    registry = subprocess.check_output(["bash", "scripts/ci/with-service.sh", "--list"], text=True)
    image = next(line.split("\t")[1] for line in registry.splitlines() if line.startswith("pg16\t"))
    subprocess.run(["bash", "scripts/docker-pull-with-retry.sh", image], check=True)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"])
    (root / "source.patch").write_bytes(patch)
    binary_sha = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    passed = 0
    for run in range(1, repeats + 1):
        directory = root / f"run-{run}"
        directory.mkdir(parents=True, exist_ok=False)
        container = f"{gate}-pg-{run}"
        server = RestateServer(f"{gate}-{run}", directory, {
            "RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS": "3",
            "RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS": "pause",
            "RESTATE_DEFAULT_RETRY_POLICY__INITIAL_INTERVAL": "50ms",
            "RESTATE_DEFAULT_RETRY_POLICY__MAX_INTERVAL": "50ms",
            "RESTATE_DEFAULT_RETRY_POLICY__EXPONENTIATION_FACTOR": "1.0",
        }, port_base=base)
        try:
            subprocess.run(["docker", "run", "-d", "--name", container,
                            "--label", os.environ["LASH_GATE_LABEL"],
                            "-p", f"127.0.0.1:{base+3}:5432",
                            "-e", "POSTGRES_USER=lash", "-e", "POSTGRES_PASSWORD=lash",
                            "-e", "POSTGRES_DB=lash", image], check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["bash", "-c", 'source scripts/ci/pg-service.sh; lash_pg_wait operator 60 docker exec "$1"', "--", container], check=True)
            for path in ("crates/lash-postgres-store/schema.sql", "runbooks/session-operator/audit.sql"):
                with open(path, "rb") as schema:
                    subprocess.run(["docker", "exec", "-i", container, "psql", "-U", "lash", "-d", "lash",
                                    "-v", "ON_ERROR_STOP=1", "-q"], stdin=schema, check=True, stdout=subprocess.DEVNULL)
            server.start()
            env = {**os.environ, **server.env(),
                   "RESTATE_AUTHORITY_ID": f"{gate}:{run}",
                   "LASH_POSTGRES_DATABASE_URL": f"postgres://lash:lash@127.0.0.1:{base+3}/lash",
                   "LASH_OPERATOR_ENDPOINT": f"127.0.0.1:{base+4}",
                   "LASH_OPERATOR_ARTIFACT_DIR": str(directory)}
            (directory / "services.json").write_text(json.dumps({
                "source_sha": source, "source_patch_sha256": hashlib.sha256(patch).hexdigest(),
                "binary_sha256": binary_sha, "restate_version": RESTATE_VERSION,
                "postgres_image": image, "gate_id": gate, "run": run,
                "ingress": server.ingress_url, "admin": server.admin_url,
                "endpoint": f"http://127.0.0.1:{base+4}",
                "postgres_port": base+3, "container": container,
            }, indent=2) + "\n")
            with (directory / "cases.jsonl").open("w") as output, (directory / "worker.log").open("w") as errors:
                result = subprocess.run([binary], env=env, stdout=output, stderr=errors, timeout=600)
            if result.returncode:
                print((directory / "worker.log").read_text(), file=sys.stderr)
                raise SystemExit(f"operator run {run} exited {result.returncode}: {directory}")
            rows, failures = check(directory / "cases.jsonl")
            (directory / "verdict.json").write_text(json.dumps({"executed": len(rows), "failures": failures}) + "\n")
            if failures:
                raise SystemExit("; ".join(failures))
            passed += len(rows)
            print(f"run {run}/{repeats}: {len(rows)} passed, 0 failed (executed; {directory}/cases.jsonl)", flush=True)
        finally:
            with (directory / "postgres-evidence.jsonl").open("w") as output:
                for table in ("operator_model_calls", "operator_terminal_writes", "operator_child_cancels",
                              "lash_session_runs", "lash_control_intents", "lash_processes", "lash_process_events", "lash_parent_end_plans"):
                    subprocess.run(["docker", "exec", container, "psql", "-U", "lash", "-d", "lash", "-Atqc",
                                    f"SELECT json_build_object('table', '{table}', 'row', row_to_json(t)) FROM {table} t"],
                                   stdout=output, stderr=subprocess.DEVNULL)
            with (directory / "postgres.log").open("w") as output:
                subprocess.run(["docker", "logs", container], stdout=output, stderr=subprocess.STDOUT)
            subprocess.run(["docker", "rm", "-f", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            server.stop()
    (root / "summary.json").write_text(json.dumps({"source_sha": source, "runs": repeats, "executed": passed, "failed": 0}) + "\n")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    main()
