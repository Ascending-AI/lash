#!/usr/bin/env python3
"""Identity ADR 0068 inventory pins against the shipping workspace."""
from pathlib import Path
import re
import sys
import unittest

import check_outcome_suffixes as gate

REPO = Path(__file__).resolve().parents[1]


def public_outcome_inventory_matches_adr_0068():
    found = gate.violations(REPO)
    if found:
        raise AssertionError(found)


def public_outcome_names_follow_declared_suffix_roles():
    witnesses = [
        ("crates/lash-core-execution/src/triggers/report.rs", "TriggerEmitReport", "struct", "pub deliveries: Vec<TriggerDeliveryEmitReceipt>"),
        ("crates/lash-core-execution/src/triggers/report.rs", "TriggerDeliveryEmitReceipt", "struct", "pub outcome: TriggerDeliveryEmitOutcome"),
        ("crates/lash-core-execution/src/runtime/process/model.rs", "ProcessRegistrationOutcome", "enum", "Existing"),
        ("crates/lash-sansio/src/tool_output.rs", "ToolRetryStatus", "enum", "Exhausted"),
    ]
    for path, name, kind, member in witnesses:
        text = gate.blank_noncode((REPO / path).read_text())
        match = re.search(r"\bpub\s+" + kind + r"\s+" + name + r"\s*\{", text)
        if match is None:
            raise AssertionError(f"{name} no longer names its declared {kind} role")
        end = text.find("\n}", match.end())
        if member not in text[match.end():end]:
            raise AssertionError(f"{name} lost its role witness {member}")
    if gate.RESULT_ALIASES != frozenset({"Result", "MaintenanceResult", "TriggerEffectResult", "TriggerOccurrenceReclamationResult"}):
        raise AssertionError("the Result exception inventory changed without a role ruling")


if __name__ == "__main__":
    pins = [public_outcome_inventory_matches_adr_0068, public_outcome_names_follow_declared_suffix_roles]
    selected = sys.argv[1:]
    suite = unittest.TestSuite(unittest.FunctionTestCase(pin) for pin in pins if not selected or pin.__name__ in selected)
    if suite.countTestCases() == 0:
        raise SystemExit("no named identity inventory pin selected")
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
