#!/usr/bin/env python3
"""FIG-5125: catalog changes wait for the native MCP Run, beyond its reply."""

import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "mcp_peer_restart", ROOT / "examples/agent-workbench/tests/mcp_peer_restart.py")
ORACLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ORACLE)


class WorkbenchMcpOracleTest(unittest.TestCase):
    def test_catalog_changes_wait_for_native_run_completion(self):
        for operation in ("attach", "detach"):
            with self.subTest(operation=operation):
                journey = ORACLE.Journey.__new__(ORACLE.Journey)
                journey.args = type("Args", (), {"mcp_url": "http://peer/mcp"})()
                pending = [["LashTurn/run (running)"], ["LashTurn/close (running)"], []]
                observations = []

                def controller(request):
                    self.assertEqual(request, {"action": "mcp-quiescence"})
                    receipt = {"open": pending.pop(0)}
                    observations.append(receipt)
                    return receipt

                def api(*args):
                    self.assertFalse(pending,
                                     "catalog changed while the completed reply's native Run still replayed")
                    return {"catalog_changed": True}

                def poll(predicate):
                    for _ in range(3):
                        result = predicate()
                        if result:
                            return result
                    self.fail("native completion was not observed")

                journey.controller = controller
                journey.api = api
                journey.poll = poll
                journey.save = lambda *args: None
                self.assertEqual(getattr(journey, operation)(), {"catalog_changed": True})
                self.assertEqual(observations[-1], {"open": []})


if __name__ == "__main__":
    unittest.main()
