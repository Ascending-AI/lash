from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("check-postgres-payload-shape-version.py")
SPEC = importlib.util.spec_from_file_location("postgres_payload_shape_version", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


PAYLOAD_DIFF = """\
diff --git a/crates/lash-postgres-store/schema-shape.txt b/crates/lash-postgres-store/schema-shape.txt
--- a/crates/lash-postgres-store/schema-shape.txt
+++ b/crates/lash-postgres-store/schema-shape.txt
@@ -1 +1 @@
-    shape /properties/old/type string
+    shape /properties/new/type string
"""

SHARED_PAYLOAD_DIFF = PAYLOAD_DIFF.replace(
    "/properties/new/type",
    "/persisted-by/sqlite/session_meta.relation_json/properties/new/type",
)

POSTGRES_FINGERPRINT_DIFF = """\
diff --git a/crates/lash-postgres-store/payload-schema-fingerprints.txt b/crates/lash-postgres-store/payload-schema-fingerprints.txt
--- a/crates/lash-postgres-store/payload-schema-fingerprints.txt
+++ b/crates/lash-postgres-store/payload-schema-fingerprints.txt
@@ -1 +1 @@
-payload-fingerprint postgres lash_session_meta.meta_json SessionMeta sha256:old
+payload-fingerprint postgres lash_session_meta.meta_json SessionMeta sha256:new
"""

SHARED_FINGERPRINT_DIFF = POSTGRES_FINGERPRINT_DIFF + """\
diff --git a/crates/lash-postgres-store/payload-schema-fingerprints.txt b/crates/lash-postgres-store/payload-schema-fingerprints.txt
--- a/crates/lash-postgres-store/payload-schema-fingerprints.txt
+++ b/crates/lash-postgres-store/payload-schema-fingerprints.txt
@@ -2 +2 @@
-payload-fingerprint sqlite session_meta.relation_json SessionRelation sha256:old
+payload-fingerprint sqlite session_meta.relation_json SessionRelation sha256:new
"""


def version_diff(source: str, before: int, after: int, visibility: str = "") -> str:
    declaration = f"{visibility}const SCHEMA_VERSION: i32 ="
    return f"""\
diff --git a/{source} b/{source}
--- a/{source}
+++ b/{source}
@@ -1 +1 @@
-{declaration} {before};
+{declaration} {after};
"""


class PostgresPayloadShapeVersionTest(unittest.TestCase):
    def test_initial_exact_fingerprint_registration_does_not_force_a_bump(self) -> None:
        additions = "\n".join(
            f"+{line}" for line in sorted(MODULE.FINGERPRINT_REGISTRATION_BASELINES)
        )
        patch = f"""\
diff --git a/{MODULE.FINGERPRINT_ARTIFACT} b/{MODULE.FINGERPRINT_ARTIFACT}
new file mode 100644
--- /dev/null
+++ b/{MODULE.FINGERPRINT_ARTIFACT}
@@ -0,0 +1,2 @@
{additions}
"""
        valid, _ = MODULE.validate_patch(patch)
        self.assertTrue(valid)

    def test_unapproved_fingerprint_registration_requires_a_bump(self) -> None:
        patch = POSTGRES_FINGERPRINT_DIFF.replace("-payload-fingerprint", " payload-fingerprint")
        valid, _ = MODULE.validate_patch(patch)
        self.assertFalse(valid)

    def test_fingerprint_change_without_component_bump_fails(self) -> None:
        valid, _ = MODULE.validate_patch(POSTGRES_FINGERPRINT_DIFF)
        self.assertFalse(valid)

    def test_postgres_fingerprint_change_with_forward_bump_passes(self) -> None:
        patch = POSTGRES_FINGERPRINT_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        valid, _ = MODULE.validate_patch(patch)
        self.assertTrue(valid)

    def test_shared_fingerprint_change_requires_both_backend_bumps(self) -> None:
        patch = SHARED_FINGERPRINT_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        valid, message = MODULE.validate_patch(patch)
        self.assertFalse(valid)
        self.assertIn("SQLite SCHEMA_VERSION", message)

    def test_shared_fingerprint_change_with_both_backend_bumps_passes(self) -> None:
        patch = SHARED_FINGERPRINT_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        patch += version_diff(MODULE.SQLITE_VERSION_SOURCE, 30, 31, "pub(crate) ")
        valid, _ = MODULE.validate_patch(patch)
        self.assertTrue(valid)

    def test_payload_change_without_component_bump_fails(self) -> None:
        valid, _ = MODULE.validate_patch(PAYLOAD_DIFF)
        self.assertFalse(valid)

    def test_payload_change_with_both_backend_bumps_passes(self) -> None:
        patch = SHARED_PAYLOAD_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        patch += version_diff(MODULE.SQLITE_VERSION_SOURCE, 30, 31, "pub(crate) ")
        valid, _ = MODULE.validate_patch(patch)
        self.assertTrue(valid)

    def test_payload_change_with_postgres_downgrade_fails(self) -> None:
        patch = SHARED_PAYLOAD_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 42)
        patch += version_diff(MODULE.SQLITE_VERSION_SOURCE, 30, 31, "pub(crate) ")
        valid, _ = MODULE.validate_patch(patch)
        self.assertFalse(valid)

    def test_payload_change_with_sqlite_downgrade_fails(self) -> None:
        patch = SHARED_PAYLOAD_DIFF
        patch += version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        patch += version_diff(MODULE.SQLITE_VERSION_SOURCE, 30, 29, "pub(crate) ")
        valid, _ = MODULE.validate_patch(patch)
        self.assertFalse(valid)

    def test_postgres_only_bump_does_not_cover_shared_shape(self) -> None:
        patch = SHARED_PAYLOAD_DIFF + version_diff(
            MODULE.POSTGRES_VERSION_SOURCE, 43, 44
        )
        valid, message = MODULE.validate_patch(patch)
        self.assertFalse(valid)
        self.assertIn("SQLite SCHEMA_VERSION", message)

    def test_postgres_only_shape_does_not_force_sqlite_bump(self) -> None:
        patch = PAYLOAD_DIFF + version_diff(MODULE.POSTGRES_VERSION_SOURCE, 43, 44)
        valid, _ = MODULE.validate_patch(patch)
        self.assertTrue(valid)

    def test_registering_a_preexisting_payload_does_not_force_a_bump(self) -> None:
        registration = PAYLOAD_DIFF.replace(
            "-    shape /properties/old/type string\n",
            "+  payload-shape lash_session_meta.meta_json SessionMeta\n",
        )
        identity = "lash_session_meta.meta_json SessionMeta"
        baseline = MODULE.shape_fingerprint(
            {"shape /properties/new/type string"}
        )
        with mock.patch.dict(
            MODULE.REGISTRATION_BASELINES, {identity: baseline}, clear=True
        ):
            valid, _ = MODULE.validate_patch(registration)
        self.assertTrue(valid)

    def test_registration_with_unapproved_shape_is_not_exempt(self) -> None:
        registration = PAYLOAD_DIFF.replace(
            "-    shape /properties/old/type string\n",
            "+  payload-shape lash_session_meta.meta_json SessionMeta\n",
        )
        valid, _ = MODULE.validate_patch(registration)
        self.assertFalse(valid)

    def test_registration_does_not_hide_an_existing_payload_addition(self) -> None:
        mixed_change = """\
diff --git a/crates/lash-postgres-store/schema-shape.txt b/crates/lash-postgres-store/schema-shape.txt
--- a/crates/lash-postgres-store/schema-shape.txt
+++ b/crates/lash-postgres-store/schema-shape.txt
@@ -1,2 +1,5 @@
   payload-shape old_json Old
     shape /properties/old/type string
+    shape /properties/new/type string
+  payload-shape new_json New
+    shape /properties/id/type string
"""
        identity = "lash_trigger_subscriptions.record_json TriggerSubscriptionRecord"
        baseline = MODULE.shape_fingerprint({"shape /properties/id/type string"})
        with mock.patch.dict(
            MODULE.REGISTRATION_BASELINES, {identity: baseline}, clear=True
        ):
            valid, _ = MODULE.validate_patch(mixed_change)
        self.assertFalse(valid)

    def test_same_column_name_registration_does_not_hide_existing_payload_change(self) -> None:
        mixed_change = """\
diff --git a/crates/lash-postgres-store/schema-shape.txt b/crates/lash-postgres-store/schema-shape.txt
--- a/crates/lash-postgres-store/schema-shape.txt
+++ b/crates/lash-postgres-store/schema-shape.txt
@@ -1,5 +1,8 @@
 table lash_processes
   column record_json text not-null
  payload-shape record_json ProcessRecord
     shape /properties/id/type string
+    shape /properties/status/type string
 table lash_trigger_subscriptions
   column record_json text not-null
+  payload-shape record_json TriggerSubscriptionRecord
+    shape /properties/id/type string
"""
        valid, _ = MODULE.validate_patch(mixed_change)
        self.assertFalse(valid)

    def test_structural_column_change_alone_does_not_use_payload_gate(self) -> None:
        column_diff = PAYLOAD_DIFF.replace(
            "    shape /properties/old/type string",
            "  column old text not-null",
        ).replace(
            "    shape /properties/new/type string",
            "  column new text not-null",
        )
        valid, _ = MODULE.validate_patch(column_diff)
        self.assertTrue(valid)


if __name__ == "__main__":
    unittest.main()
