#!/usr/bin/env python3
"""Mutation witnesses for the derived conformance mount gate."""

from pathlib import Path
import tempfile
import unittest

from check_conformance_mounts import ROOT, unmounted


class MountGateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(dir=ROOT / ".buck2")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.write("crates/lash-conformance/src/lib.rs", "mod conformance;")
        self.write("crates/lash-conformance/src/conformance/mod.rs", "mod laws;")
        self.write("crates/lash-conformance/src/conformance/laws.rs", "pub async fn law() {}")

    def write(self, relative, source):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source)
        crate = self.root.joinpath(*Path(relative).parts[:2])
        (crate / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "0.1.0"\n')
        if relative.endswith("src/macros.rs"):
            with (crate / "src/lib.rs").open("a") as library:
                library.write("\nmod macros;")

    def missing(self):
        return set(unmounted(self.root)[0])

    def test_unreachable_helper_with_same_name_cannot_mount_a_law(self):
        self.write("crates/backend/src/lib.rs", "fn helper() { law(); }")
        self.write("crates/backend/tests/conformance.rs", """
            fn helper() {}
            #[test] fn mounted() { helper(); }
        """)
        self.assertEqual(self.missing(), {"law"})

    def test_a_declared_macro_is_not_a_mount(self):
        self.write("crates/lash-conformance/src/macros.rs", """
            macro_rules! laws { () => { #[tokio::test] async fn mounted() { law().await; } }; }
        """)
        self.assertEqual(self.missing(), {"law"})
        self.write("crates/backend/tests/conformance.rs", "laws!();")
        self.assertEqual(self.missing(), set())

    def test_removing_the_registration_makes_the_gate_red(self):
        self.write("crates/backend/tests/conformance.rs", "#[tokio::test] async fn mounted() { law().await; }")
        self.assertEqual(self.missing(), set())
        self.write("crates/backend/tests/conformance.rs", "use laws::law; // law().await;")
        self.assertEqual(self.missing(), {"law"})

    def test_helpers_and_callbacks_are_followed(self):
        self.write("crates/backend/tests/conformance.rs", """
            async fn helper() { law().await; }
            #[tokio::test] async fn mounted() { run(helper).await; }
        """)
        self.assertEqual(self.missing(), set())

    def test_a_non_test_macro_is_not_a_mount(self):
        self.write("crates/backend/tests/conformance.rs", """
            macro_rules! helpers { () => { async fn helper() { law().await; } }; }
            helpers!();
        """)
        self.assertEqual(self.missing(), {"law"})

    def test_an_unlinked_test_file_is_not_a_mount(self):
        self.write("crates/backend/src/unlinked.rs", "#[test] fn mounted() { law(); }")
        self.assertEqual(self.missing(), {"law"})

    def test_an_auto_discovered_executable_can_consume_a_helper(self):
        self.write("crates/backend/src/bin/measurement.rs", "fn main() { law(); }")
        self.assertEqual(self.missing(), set())

    def test_a_textually_included_registration_is_a_mount(self):
        self.write("crates/backend/tests/conformance.rs", 'include!("laws.rs");')
        self.write("crates/backend/tests/laws.rs", "#[test] fn mounted() { law(); }")
        manifest = self.root / "crates/backend/Cargo.toml"
        manifest.write_text(manifest.read_text() + 'autotests = false\n[[test]]\nname = "conformance"\n')
        self.assertEqual(self.missing(), set())
        self.write("crates/backend/tests/conformance.rs", '// include!("laws.rs");')
        manifest.write_text('[package]\nname = "fixture"\nversion = "0.1.0"\nautotests = false\n[[test]]\nname = "conformance"\n')
        self.assertEqual(self.missing(), {"law"})

    def test_generated_public_laws_require_backend_registration(self):
        self.write("crates/lash-conformance/src/conformance/laws.rs", """
            pub async fn law() {}
            macro_rules! generate { ($(($name:ident, $point:ident)),*) => {
                $(pub async fn $name() { law().await; })*
            }; }
            generate![(generated_law, Point)];
        """)
        self.assertEqual(self.missing(), {"law", "generated_law"})
        self.write("crates/backend/tests/conformance.rs", "#[test] fn mounted() { generated_law(); }")
        self.assertEqual(self.missing(), set())


if __name__ == "__main__":
    unittest.main()
