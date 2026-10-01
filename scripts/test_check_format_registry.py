#!/usr/bin/env python3
"""Fixture tests for check_format_registry.py."""

from __future__ import annotations

import dataclasses
import importlib.util
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_format_registry.py")
SPEC = importlib.util.spec_from_file_location("check_format_registry", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


REGISTRY = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "migrate"
description = "fixture durable format"
manifest = "Wire"

[[surface]]
constant = "PEER_PROTOCOL_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "coexist"
description = "fixture wire protocol"
outside_manifest = "gates a live peer"

[[surface]]
constant = "KEY_FAMILY_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "coexist"
description = "fixture hash domain"

[[excluded_class]]
suffix = "_FAMILY_VERSION"
reason = "hash-domain tag"

[[unregistered]]
constant = "APP_VERSION"
constant_path = "crates/demo/src/lib.rs"
reason = "mirrors the package version"
"""

SOURCE = """
/// version_guard(items(WireRecord))
pub const WIRE_VERSION: u16 = 3;
/// version_guard(shapes(cover(Hello)))
pub const PEER_PROTOCOL_VERSION: u32 = 1;
/// version_guard(unshaped = "a hash-domain tag with no projected shape")
const KEY_FAMILY_VERSION: u8 = 2;
const OTHER_FAMILY_VERSION: u8 = 1;
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct WireRecord {
    pub id: String,
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
}

#[cfg(test)]
mod tests {
    const PREVIOUS_WIRE_VERSION: u16 = 2;
}
"""

MANIFEST = """
pub struct DurableFormatEntry {
    pub format: DurableFormat,
}

impl DurableFormat {
    pub fn upgrade_policy(self) -> UpgradePolicy {
        match self {
            DurableFormat::Wire => UpgradePolicy::Migrate,
        }
    }
}

pub fn durable_formats() -> &'static [DurableFormatEntry] {
    &[
        DurableFormatEntry {
            format: DurableFormat::Wire,
            version: FormatVersion::Counter(WIRE_VERSION as u32),
            owning_crate: "demo",
            constant: "WIRE_VERSION",
            probe: FormatProbe::Comparable,
        },
    ]
}
"""

GUARDED = """
pub const GUARDED_SURFACES: &[GuardedSurface] = &[
    GuardedSurface {
        constant: "WIRE_VERSION",
        owner: "demo",
        reads: SurfaceReads::History { floor: 1 },
    },
];
"""

LAWS = """
const OWNER: &str = "demo";

#[test]
fn every_guarded_surface_decodes_its_supported_range() {
    laws::every_guarded_surface_decodes_its_supported_range(OWNER, &probes());
}

#[test]
fn unknown_version_is_refused_with_zero_mutation() {
    laws::unknown_version_is_refused_with_zero_mutation(OWNER, &probes());
}

#[test]
fn upcast_preserves_immutable_bytes_and_hashes() {
    laws::upcast_preserves_immutable_bytes_and_hashes(OWNER, &probes());
}
"""

ENGINE_REGISTRY = """
pub struct EngineDurableFormat {
    pub id: &'static str,
}

pub fn durable_formats() -> &'static [EngineDurableFormat] {
    &[
        EngineDurableFormat {
            id: "demo.engine_wire",
            name: "demo engine wire",
            version: ENGINE_WIRE_VERSION as u32,
            constant: "ENGINE_WIRE_VERSION",
            upgrade_policy: UpgradePolicy::Drain,
            unwalkable_reason: "engine state",
        },
    ]
}
"""


class FormatRegistryTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)
        self.write("crates/demo/src/lib.rs", SOURCE)
        self.write("crates/demo/tests/old.rs", "const FIXTURE_V1_VERSION: u32 = 1;\n")
        self.write(str(gate.GUARDED_REGISTRY), GUARDED)
        self.write("crates/demo/src/guarded_surface_tests.rs", LAWS)
        self.registry_text = REGISTRY
        self.manifest = MANIFEST

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")

    def problems(self) -> list[str]:
        config = self.repo / "registry.toml"
        config.write_text(self.registry_text, encoding="utf-8")
        return gate.check(self.repo, gate.load_registry(config), self.manifest)

    def test_a_consistent_registry_passes(self) -> None:
        self.assertEqual(self.problems(), [])

    def test_an_unknown_version_constant_is_named(self) -> None:
        self.write(
            "crates/demo/src/extra.rs",
            "pub(crate) const LEASE_ENCODING_VERSION: u8 = 3;\n"
            "const INDEX_IDENTITY_EPOCH: u8 = 6;\n",
        )
        problems = self.problems()
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("crates/demo/src/extra.rs:LEASE_ENCODING_VERSION", problems[1])
        self.assertIn("crates/demo/src/extra.rs:INDEX_IDENTITY_EPOCH", problems[0])

    def test_a_stale_exclusion_fails(self) -> None:
        self.write("crates/demo/src/lib.rs", SOURCE.replace("APP_VERSION", "APP_NAME"))
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("names no swept constant", problems[0])

    def test_an_exclusion_needs_a_reason(self) -> None:
        self.registry_text = REGISTRY.replace(
            'reason = "mirrors the package version"', 'reason = " "'
        )
        with self.assertRaises(gate.RegistryError):
            self.problems()

    def test_a_surface_without_a_manifest_disposition_fails(self) -> None:
        self.registry_text = REGISTRY.replace(
            'outside_manifest = "gates a live peer"\n', ""
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("PEER_PROTOCOL_VERSION must have exactly one", problems[0])

    def test_a_surface_with_two_dispositions_fails(self) -> None:
        self.registry_text = REGISTRY.replace(
            'manifest = "Wire"\n', 'manifest = "Wire"\noutside_manifest = "both"\n'
        )
        problems = self.problems()
        self.assertTrue(
            any("WIRE_VERSION must have exactly one" in problem for problem in problems),
            problems,
        )

    def test_a_manifest_row_no_surface_claims_fails(self) -> None:
        self.registry_text = REGISTRY.replace(
            'manifest = "Wire"', 'outside_manifest = "not reported"'
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("no registered surface claims", problems[0])

    def test_a_claim_on_the_wrong_variant_fails(self) -> None:
        self.registry_text = REGISTRY.replace('manifest = "Wire"', 'manifest = "Other"')
        problems = self.problems()
        self.assertTrue(
            any("reports WIRE_VERSION as DurableFormat::Wire" in p for p in problems),
            problems,
        )

    def test_a_claim_without_a_manifest_row_fails(self) -> None:
        self.manifest = MANIFEST.split("pub fn durable_formats")[0]
        problems = self.problems()
        self.assertTrue(
            any("has no row reporting WIRE_VERSION" in p for p in problems),
            problems,
        )

    def test_a_row_reporting_another_symbol_is_refused(self) -> None:
        self.manifest = MANIFEST.replace("Counter(WIRE_VERSION", "Counter(PEER_VERSION")
        with self.assertRaises(gate.RegistryError):
            self.problems()

    def test_a_surface_without_an_upgrade_policy_fails(self) -> None:
        self.registry_text = REGISTRY.replace('upgrade = "migrate"\n', "")
        with self.assertRaises(gate.RegistryError):
            self.problems()

    def test_an_unknown_upgrade_policy_fails(self) -> None:
        self.registry_text = REGISTRY.replace('upgrade = "migrate"', 'upgrade = "ignore"')
        with self.assertRaises(gate.RegistryError):
            self.problems()

    def test_a_surface_policy_must_match_the_rust_arm(self) -> None:
        self.registry_text = REGISTRY.replace('upgrade = "migrate"', 'upgrade = "drain"')
        problems = self.problems()
        self.assertTrue(
            any("upgrade_policy() answers" in problem for problem in problems),
            problems,
        )

    def test_a_claim_on_a_variant_without_an_arm_fails(self) -> None:
        self.registry_text = REGISTRY.replace('manifest = "Wire"', 'manifest = "Other"')
        problems = self.problems()
        self.assertTrue(
            any("upgrade_policy() has no arm" in problem for problem in problems),
            problems,
        )

    def test_an_arm_without_a_manifest_row_fails(self) -> None:
        self.manifest = MANIFEST.replace(
            "DurableFormat::Wire => UpgradePolicy::Migrate,",
            "DurableFormat::Wire => UpgradePolicy::Migrate,\n            "
            "DurableFormat::Unlisted => UpgradePolicy::Drain,",
        )
        problems = self.problems()
        self.assertTrue(
            any(
                "DurableFormat::Unlisted has an upgrade_policy() arm but no "
                "manifest row" in problem
                for problem in problems
            ),
            problems,
        )

    def test_an_engine_registered_row_satisfies_a_manifest_claim(self) -> None:
        self.write("crates/lash-restate/src/formats.rs", ENGINE_REGISTRY)
        self.write(
            "crates/demo/src/engine.rs",
            '/// version_guard(unshaped = "fixture engine wire")\n'
            "pub const ENGINE_WIRE_VERSION: u8 = 1;\n",
        )
        self.registry_text = REGISTRY + textwrap.dedent(
            """
            [[surface]]
            constant = "ENGINE_WIRE_VERSION"
            constant_path = "crates/demo/src/engine.rs"
            upgrade = "drain"
            description = "engine-registered fixture format"
            manifest = "engine:demo.engine_wire"
            """
        )
        self.assertEqual(self.problems(), [])

    def test_an_engine_row_no_surface_claims_fails(self) -> None:
        self.write("crates/lash-restate/src/formats.rs", ENGINE_REGISTRY)
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("engine format `demo.engine_wire`", problems[0])
        self.assertIn("no registered surface claims", problems[0])

    def test_a_migrate_surface_outside_the_guarded_table_fails(self) -> None:
        self.write(str(gate.GUARDED_REGISTRY), GUARDED.split("    GuardedSurface {")[0] + "];\n")
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("WIRE_VERSION is a migrate surface outside GUARDED_SURFACES", problems[0])

    def test_a_stated_unguarded_reason_admits_a_migrate_surface(self) -> None:
        self.write(str(gate.GUARDED_REGISTRY), GUARDED.split("    GuardedSurface {")[0] + "];\n")
        self.registry_text = REGISTRY.replace(
            'upgrade = "migrate"\n', 'upgrade = "migrate"\nunguarded = "a DDL stamp"\n'
        )
        self.assertEqual(self.problems(), [])

    def test_a_guarded_row_cannot_also_be_unguarded(self) -> None:
        self.registry_text = REGISTRY.replace(
            'upgrade = "migrate"\n', 'upgrade = "migrate"\nunguarded = "a DDL stamp"\n'
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("also states unguarded", problems[0])

    def test_a_guarded_row_must_name_a_migrate_surface(self) -> None:
        self.write(str(gate.GUARDED_REGISTRY), GUARDED.replace("WIRE_VERSION", "PEER_PROTOCOL_VERSION"))
        problems = self.problems()
        self.assertTrue(
            any("row PEER_PROTOCOL_VERSION must name exactly one" in p for p in problems),
            problems,
        )

    def test_a_guarded_owner_must_run_every_law(self) -> None:
        self.write(
            "crates/demo/src/guarded_surface_tests.rs",
            LAWS.replace("laws::upcast_preserves_immutable_bytes_and_hashes", "skipped"),
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("owned by demo, which does not run the guarded-surface laws", problems[0])

    def test_a_guarded_owner_must_run_the_laws_as_itself(self) -> None:
        self.write(str(gate.GUARDED_REGISTRY), GUARDED.replace('owner: "demo"', 'owner: "other"'))
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("owned by other", problems[0])


    def test_a_surface_whose_constant_declares_no_guard_fails(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            SOURCE.replace("/// version_guard(items(WireRecord))\n", ""),
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("WIRE_VERSION declares no version_guard marker", problems[0])

    def test_a_stated_reason_admits_a_surface_without_a_guard(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            SOURCE.replace("items(WireRecord)", 'unshaped = "a manual epoch"'),
        )
        self.assertEqual(self.problems(), [])

    def test_an_empty_reason_is_not_a_reason(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            SOURCE.replace("items(WireRecord)", 'unshaped = " "'),
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("unshaped must state a reason", problems[0])

    def test_a_guard_naming_what_the_tree_lacks_fails(self) -> None:
        self.write(
            "crates/demo/src/lib.rs", SOURCE.replace("items(WireRecord)", "items(Gone)")
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("has a guard that cannot be evaluated", problems[0])
        self.assertIn("does not find Gone", problems[0])

    def test_a_reachable_type_the_tree_cannot_resolve_fails(self) -> None:
        derived = SOURCE.replace("items(WireRecord)", "roots(WireRecord)").replace(
            "pub struct WireRecord {\n    pub id: String,",
            "#[derive(Serialize)]\npub struct WireRecord {\n    pub id: Missing,",
        )
        self.write("crates/demo/src/lib.rs", derived)
        # The file's sweep reaches the same shape, so both surfaces fail.
        problems = [problem for problem in self.problems() if ":WIRE_VERSION " in problem]
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("WIRE_VERSION has a guard that cannot be evaluated", problems[0])
        self.assertIn("roots(crates/demo/src/lib.rs) reaches 1 type(s) it cannot resolve", problems[0])
        self.assertIn("WireRecord (crates/demo/src/lib.rs) names Missing", problems[0])

        self.write(
            "crates/demo/src/lib.rs",
            derived + "\n#[derive(Serialize)]\npub struct Missing(u8);\n",
        )
        self.assertEqual(self.problems(), [])

    def test_a_swept_shape_whose_field_cannot_be_resolved_fails(self) -> None:
        self.write(
            "crates/demo/src/lib.rs", SOURCE.replace("pub name: String,", "pub name: Missing,")
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("Hello (crates/demo/src/lib.rs) names Missing", problems[0])

    def test_a_ddl_guard_without_its_catalog_fails(self) -> None:
        self.write("crates/demo/schema.sql", "CREATE TABLE demo (id TEXT);\n")
        ddl = 'items(WireRecord), file(path = "crates/demo/schema.sql")'
        self.write("crates/demo/src/lib.rs", SOURCE.replace("items(WireRecord)", ddl))
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("guards SQL DDL and declares no migration catalog", problems[0])

        named = ddl + ', catalog(path = "crates/demo/src/migrate.rs", MIGRATIONS)'
        self.write("crates/demo/src/lib.rs", SOURCE.replace("items(WireRecord)", named))
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("MIGRATIONS (crates/demo/src/migrate.rs) cannot be read", problems[0])

        self.write("crates/demo/src/migrate.rs", "static MIGRATIONS: &[Step] = &[];\n")
        self.assertEqual(self.problems(), [])

    def test_a_malformed_marker_fails(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            SOURCE.replace("shapes(cover(Hello))", "shapes(cover(Hello)"),
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("PEER_PROTOCOL_VERSION has a guard that cannot be evaluated", problems[0])


class RealRepositoryTests(unittest.TestCase):
    def test_the_repository_registry_is_exhaustive(self) -> None:
        registry = gate.load_registry(gate.DEFAULT_CONFIG)
        manifest = (gate.ROOT / gate.MANIFEST).read_text(encoding="utf-8")
        self.assertEqual(gate.check(gate.ROOT, registry, manifest), [])

    def test_the_gate_names_the_constants_fig_3521_found_unregistered(self) -> None:
        # The three surviving version constants that were in neither the registry nor the
        # manifest before FIG-3521. Dropping their entries must fail the gate by
        # name; a sweep that stopped seeing them would pass silently instead.
        missing = {
            "crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:"
            "SCOPE_STORAGE_PAYLOAD_VERSION",
            "crates/lash-restate/src/controller/process_command.rs:"
            "PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION",
            "crates/lash-restate/src/durable_wait.rs:DURABLE_WAIT_REGISTRY_FORMAT_VERSION",
        }
        registry = gate.load_registry(gate.DEFAULT_CONFIG)
        pruned = dataclasses.replace(
            registry,
            surfaces={
                key: value
                for key, value in registry.surfaces.items()
                if key not in missing
            },
            unregistered={
                key: value
                for key, value in registry.unregistered.items()
                if key not in missing
            },
        )
        manifest = (gate.ROOT / gate.MANIFEST).read_text(encoding="utf-8")
        problems = gate.check(gate.ROOT, pruned, manifest)
        unknown = {
            problem.split(" ", 1)[0]
            for problem in problems
            if "the registry does not know" in problem
        }
        self.assertEqual(unknown, missing)


if __name__ == "__main__":
    unittest.main()
