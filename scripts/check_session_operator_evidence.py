#!/usr/bin/env python3
"""Refuse missing, duplicated or incorrect public operator case evidence."""
import json
from pathlib import Path
import sys

CASES = ("withdrawal", "running_cancel", "parked_redrive", "parked_cancel", "parked_fork", "lost_reply_repeat")


def check(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.startswith("{")]
    failures = []
    for case in CASES:
        found = [row for row in rows if row.get("case") == case]
        if len(found) != 1:
            failures.append(f"{case}: expected one executed case, got {len(found)}")
            continue
        row = found[0]
        d = row["detail"]
        valid = row.get("passed") is True
        if case == "withdrawal":
            valid &= d["input_terminal"] is True and d["model_calls"] == 0
        elif case == "lost_reply_repeat":
            valid &= d["repeats"] == 3 and d["cancel_model_calls"] == 2 and d["fork_model_calls"] == 4 and d["duplicate_model_effects"] == 0
            valid &= d["receipts_preserved"] is True and d["stale_requests_refused"] == 6
        else:
            valid &= d["terminal_writes"] == d["child_cancels"] == d["scope_closes"] == d["child_scope_closes"] == 1
            valid &= d["child_cancel_request"]["origin"] == "parent_ended" and d["child_cancel_request"]["requester"] == d["scope"]
            child_ended = d["child_status"] == "cancelled"
            if case in ("parked_cancel", "parked_fork") and d["child_status"] == "abandoned":
                child_ended = d["child_outcome"]["evidence"]["writer"] == {"resume_refused": {"reason": "substrate_lost"}}
            valid &= child_ended
            valid &= d["terminal_kind"] == ("answered" if case == "parked_redrive" else "cancelled")
            if case == "parked_redrive":
                valid &= d["same_admission"] is True and d["model_calls"] == 3 and d["same_journal_prefix"] is True and d["recorded_commands"] > 0
            if case == "parked_fork":
                valid &= bool(d["successor"]) and d["successor"] != d["original"] and d["successor_scope_open"] is True
        if not valid:
            failures.append(f"{case}: invalid evidence {d}")
    if len(rows) != len(CASES):
        failures.append(f"expected {len(CASES)} cases, executed {len(rows)}")
    return rows, failures


if __name__ == "__main__":
    rows, failures = check(sys.argv[1])
    for failure in failures:
        print(failure, file=sys.stderr)
    print(f"executed {len(rows)} cases, {len(failures)} failures")
    sys.exit(bool(failures))
