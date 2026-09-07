#!/usr/bin/env python3
"""Run the shared strict CI conclusion policy."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci_plan

if __name__ == "__main__":
    raise SystemExit(ci_plan.main())
