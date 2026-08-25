#!/usr/bin/env python3
"""Unit tests for check_facade_external_types.py."""

from __future__ import annotations

import contextlib
import io
from pathlib import Path
import sys
import unittest
from unittest import mock

import check_facade_external_types as gate


def external_export() -> dict[str, object]:
    return {
        "id": 1,
        "name": "Foreign",
        "visibility": "public",
        "inner": {"use": {"id": 9, "source": "dependency::Foreign"}},
    }


class ResolvedExportTests(unittest.TestCase):
    def test_unavailable_dependency_rustdoc_is_a_hard_error(self) -> None:
        document = {
            "index": {},
            "paths": {"9": {"path": ["dependency", "Foreign"], "kind": "struct"}},
        }
        with mock.patch.object(gate.api_surface, "external_target", return_value=None):
            with self.assertRaisesRegex(
                RuntimeError,
                "cannot resolve external facade export dependency::Foreign",
            ):
                gate.resolved_export(external_export(), document, False)

    def test_missing_export_path_is_a_hard_error(self) -> None:
        document = {"index": {}, "paths": {}}
        with self.assertRaisesRegex(RuntimeError, "has no rustdoc path"):
            gate.resolved_export(external_export(), document, False)


class ExternalModuleTests(unittest.TestCase):
    def test_glob_reexported_external_module_leak_fails_gate(self) -> None:
        facade = {
            "root": "0",
            "index": {
                "0": {
                    "id": "0",
                    "visibility": "public",
                    "inner": {"module": {"items": ["1"]}},
                },
                "1": {
                    "id": "1",
                    "name": "suite",
                    "visibility": "public",
                    "inner": {
                        "use": {
                            "id": "9",
                            "source": "dependency::suite",
                            "is_glob": True,
                        }
                    },
                },
            },
            "paths": {
                "9": {"path": ["dependency", "suite"], "kind": "module"}
            },
        }
        dependency = {
            "index": {
                "90": {
                    "id": "90",
                    "visibility": "public",
                    "inner": {"module": {"items": ["91"]}},
                },
                "91": {
                    "id": "91",
                    "name": "leak",
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "sig": {
                                "inputs": [
                                    [
                                        "value",
                                        {
                                            "resolved_path": {
                                                "path": "foreign::Leaked",
                                                "id": "99",
                                                "args": None,
                                            }
                                        },
                                    ]
                                ],
                                "output": None,
                            },
                            "generics": {},
                        }
                    },
                },
            },
            "paths": {
                "90": {"path": ["dependency", "suite"], "kind": "module"},
                "91": {
                    "path": ["dependency", "suite", "leak"],
                    "kind": "function",
                },
                "99": {"path": ["foreign", "Leaked"], "kind": "struct"},
            },
        }

        with (
            mock.patch.object(gate.api_surface, "rustdoc", return_value=facade),
            mock.patch.object(
                gate.api_surface,
                "external_target",
                return_value=(dependency["index"]["90"], dependency),
            ),
            mock.patch.object(
                gate.api_coverage, "crate_directories", return_value=[]
            ),
            mock.patch.object(gate, "configured_allowlist", return_value=set()),
            mock.patch.object(sys, "argv", [str(Path(gate.__file__))]),
            contextlib.redirect_stderr(io.StringIO()) as errors,
        ):
            status = gate.main()

        self.assertEqual(1, status)
        self.assertIn("foreign::Leaked", errors.getvalue())

    def test_external_module_with_missing_child_is_a_hard_error(self) -> None:
        facade = {
            "index": {},
            "paths": {
                "9": {"path": ["dependency", "suite"], "kind": "module"}
            },
        }
        dependency = {
            "index": {},
            "paths": {},
        }
        module = {
            "id": "90",
            "name": "suite",
            "visibility": "public",
            "inner": {"module": {"items": ["missing"]}},
        }

        with mock.patch.object(
            gate.api_surface,
            "external_target",
            return_value=(module, dependency),
        ):
            with self.assertRaisesRegex(RuntimeError, "missing child item missing"):
                list(gate.exposed_references(external_export(), facade, False))


class AllowlistDriftTests(unittest.TestCase):
    def run_gate(self, actual: set[str], allowed: set[str]) -> tuple[int, str]:
        with (
            mock.patch.object(gate, "external_types", side_effect=[actual, set()]),
            mock.patch.object(gate, "configured_allowlist", return_value=allowed),
            mock.patch.object(sys, "argv", [str(Path(gate.__file__))]),
            contextlib.redirect_stderr(io.StringIO()) as errors,
        ):
            status = gate.main()
        return status, errors.getvalue()

    def test_missing_allowlist_entry_fails(self) -> None:
        status, errors = self.run_gate({"foreign::New"}, set())

        self.assertEqual(1, status)
        self.assertIn("Unallowlisted external types", errors)
        self.assertIn("foreign::New", errors)

    def test_stale_allowlist_entry_fails(self) -> None:
        status, errors = self.run_gate(set(), {"foreign::Retired"})

        self.assertEqual(1, status)
        self.assertIn("Stale facade external-type allowlist entries", errors)
        self.assertIn("foreign::Retired", errors)


if __name__ == "__main__":
    unittest.main()
