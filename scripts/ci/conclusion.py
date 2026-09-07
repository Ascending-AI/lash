#!/usr/bin/env python3
"""Register build/reuse jobs with the existing strict CI conclusion policy."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci_plan

# Worker artifacts share the workers' trunk/label event policy. A failed
# producer is itself a required failure, even if its consumers are skipped.
ci_plan.UNGATED_JOBS.add("worker-artifacts")
ci_plan.WORKERS_E2E_JOBS.add("worker-artifacts")

ci_plan.GATED_JOBS["nextest-archive"] = "rust"

if __name__ == "__main__":
    raise SystemExit(ci_plan.main())
