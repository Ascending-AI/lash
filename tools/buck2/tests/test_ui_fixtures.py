import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[3]
RUNNER = ROOT / 'tools/buck2/ui_fixtures_runner.py'
RUSTC = ROOT / 'tools/buck2/toolchains/rust-files/bin/rustc'
spec = importlib.util.spec_from_file_location('ui_fixture_runner', RUNNER)
ui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui)

PIN = '''error[E0624]: associated function `hidden` is private
 --> tests/ui/hidden.rs:1:31
  |
1 | fn main() { surface::Surface::hidden(); }
  |                               ^^^^^^ private associated function
  |
 ::: $WORKSPACE/crates/dependency/src/lib.rs
  |
  | impl Surface { pub(crate) fn hidden() {} }
  |                ---------------------- private associated function defined here
'''


class NormalizationTests(unittest.TestCase):
    def test_existing_stderr_pins_are_preserved(self):
        pins = list((ROOT / 'crates/lash/tests/ui').glob('*.stderr'))
        self.assertGreater(len(pins), 50)
        for pin in pins:
            with self.subTest(pin=pin.name):
                self.assertEqual(ui.normalize(pin.read_text(), 'tests/ui/' + pin.stem + '.rs', 'crates/lash'), pin.read_text())

    def test_registry_paths_and_external_snippet_lines_match_trybuild(self):
        raw = 'error[E0277]: trait bound\n  --> third-party/rust/serde_json-1.0.145.crate/src/ser.rs:12:4\n   |\n12 | pub fn to_string() {}\n   |    ^^^^^^^^^^^^^ required here\n\nerror: aborting due to 1 previous error\n\nFor more information about this error, try `rustc --explain E0277`.\n'
        result = ui.normalize(raw, 'tests/ui/fixture.rs', 'crates/lash')
        self.assertIn('$CARGO/serde_json-$VERSION/src/ser.rs\n', result)
        self.assertNotIn('12 |', result)
        self.assertNotIn('aborting', result)
        self.assertNotIn('--explain', result)

    def test_staging_keeps_directories_private_and_sources_untouched(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'input'
            (source / 'tests/ui').mkdir(parents=True)
            original = source / 'tests/ui/fixture.rs'
            original.write_text('original')
            stage = root / 'stage'
            stage.mkdir()
            ui.overlay_sources(stage, stage / 'crates/lash', source)
            destination = stage / 'crates/lash/tests/ui/fixture.rs'
            self.assertFalse(destination.parent.is_symlink())
            destination.unlink()
            destination.write_text('replacement')
            self.assertEqual(original.read_text(), 'original')
            unsafe = stage / 'unsafe'
            unsafe.symlink_to(source, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, 'Unsafe'):
                ui.overlay_sources(stage, unsafe, source)


# CI provisions the toolchain before this suite, so there a missing compiler fails the probes instead of skipping them.
@unittest.skipUnless(RUSTC.is_file() or os.environ.get('GITHUB_ACTIONS') == 'true', 'Prepare the repository-pinned Rust toolchain to run real compiler fixture probes')
class CompilerFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        dep = self.root / 'crates/dependency/src'
        dep.mkdir(parents=True)
        (dep / 'lib.rs').write_text('pub struct Surface;\nimpl Surface { pub(crate) fn hidden() {} }\n')
        subprocess.run([str(RUSTC), '--crate-name=surface', '--crate-type=rlib', '--edition=2024', '-Adead_code', 'crates/dependency/src/lib.rs', '-o', str(self.root / 'libsurface.rlib')], cwd=self.root, check=True)
        self.fixtures = self.root / 'crates/lash/tests/ui'
        self.fixtures.mkdir(parents=True)
        (self.fixtures / 'hidden.rs').write_text('fn main() { surface::Surface::hidden(); }\n')
        (self.fixtures / 'hidden.stderr').write_text(PIN)
        self.manifest = self.root / 'manifest.json'
        self.manifest.write_text(json.dumps({
            'schema': 1, 'compiler': [str(RUSTC)], 'sysroot': str(RUSTC.parent.parent),
            'edition': '2024', 'features': [], 'package': 'crates/lash',
            'libraries': ['libsurface.rlib'], 'externs': {'surface': 'libsurface.rlib'},
            'sources': [{'package': 'crates/dependency', 'root': 'crates/dependency'}],
            'fixtures': [{'name': 'hidden', 'source': 'crates/lash/tests/ui/hidden.rs', 'expected': 'crates/lash/tests/ui/hidden.stderr'}],
        }))

    def invoke(self, *arguments):
        return subprocess.run([sys.executable, str(RUNNER), '--manifest', str(self.manifest), *arguments], cwd=self.root, env=dict(os.environ, UI_JOBS='1', TEST_UNDECLARED_OUTPUTS_DIR=str(self.root / 'receipts')), capture_output=True, text=True)

    def result(self):
        return json.loads((self.root / 'receipts/ui-fixtures/results.json').read_text())['results'][0]

    def test_real_private_api_failure_matches_exact_pin_with_dependency_snippet(self):
        outcome = self.invoke()
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        self.assertIn('test hidden ... ok', outcome.stdout)
        self.assertTrue(self.result()['passed'])
        self.assertEqual((self.root / 'receipts/ui-fixtures/hidden/stderr.actual').read_text(), PIN)

    def test_real_launcher_preserves_ui_case_xml_without_libtest_rediscovery(self):
        for fixture, expected_code in [('fn main() { surface::Surface::hidden(); }\n', 0), ('fn main() {}\n', 1)]:
            with self.subTest(expected_code=expected_code):
                (self.fixtures / 'hidden.rs').write_text(fixture)
                outputs = self.root / f'launch-{expected_code}'
                xml = outputs / 'test.xml'
                environment = dict(os.environ, UI_JOBS='1', TEST_TARGET='//fixture:ui', TEST_BINARY='//fixture:ui', LASH_TEST_TIMEOUT_SECONDS='30', XML_OUTPUT_FILE=str(xml), TEST_UNDECLARED_OUTPUTS_DIR=str(outputs / 'undeclared'))
                outcome = subprocess.run(['/usr/bin/bash', str(ROOT / 'tools/buck2/test_launcher.sh'), sys.executable, str(RUNNER), '--manifest', str(self.manifest)], cwd=self.root, env=environment, capture_output=True, text=True)
                self.assertEqual(outcome.returncode, expected_code, outcome.stdout + outcome.stderr)
                report = ET.parse(xml).getroot()
                self.assertEqual([case.attrib['name'] for case in report.iter('testcase')], ['hidden'])
                self.assertEqual(len(list(report.iter('failure'))), expected_code)
                self.assertIn('1 passed' if expected_code == 0 else '1 failed', report.find('.//system-out').text)
                receipt = json.loads((outputs / 'undeclared/_lash_runner/execution.json').read_text())
                self.assertEqual(receipt['exit_code'], expected_code)
                self.assertTrue(receipt['cleanup_complete'])

    def test_declared_proc_macro_loads_with_the_pinned_compiler_sysroot(self):
        macro = self.root / 'macro.rs'
        macro.write_text('extern crate proc_macro;\n#[proc_macro] pub fn empty(_: proc_macro::TokenStream) -> proc_macro::TokenStream { proc_macro::TokenStream::new() }\n')
        subprocess.run([str(RUSTC), '--crate-name=ui_macro', '--crate-type=proc-macro', '--edition=2024', str(macro), '-o', str(self.root / 'libui_macro.so')], cwd=self.root, check=True)
        fixture = self.fixtures / 'hidden.rs'
        fixture.write_text(fixture.read_text() + 'ui_macro::empty!();\n')
        manifest = json.loads(self.manifest.read_text())
        manifest['externs']['ui_macro'] = 'libui_macro.so'
        manifest['libraries'].append('libui_macro.so')
        self.manifest.write_text(json.dumps(manifest))
        outcome = self.invoke()
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        self.assertTrue(self.result()['passed'])
        self.assertEqual((self.root / 'receipts/ui-fixtures/hidden/stderr.actual').read_text(), PIN)

    def test_unexpected_compile_success_fails_the_test(self):
        (self.fixtures / 'hidden.rs').write_text('fn main() {}\n')
        outcome = self.invoke()
        self.assertEqual(outcome.returncode, 1, outcome.stdout + outcome.stderr)
        self.assertEqual(self.result()['rustc_exit_code'], 0)
        self.assertFalse(self.result()['passed'])
        self.assertIn('rustc succeeded unexpectedly', outcome.stdout)

    def test_diagnostic_drift_fails_and_retains_raw_actual_expected_and_diff(self):
        (self.fixtures / 'hidden.stderr').write_text('wrong diagnostic\n')
        outcome = self.invoke()
        self.assertEqual(outcome.returncode, 1, outcome.stdout + outcome.stderr)
        self.assertGreater(self.result()['rustc_exit_code'], 0)
        case = self.root / 'receipts/ui-fixtures/hidden'
        for filename in ['stderr.raw', 'stderr.actual', 'stderr.expected', 'stderr.diff']:
            self.assertTrue((case / filename).is_file())
        self.assertIn('-wrong diagnostic', (case / 'stderr.diff').read_text())

    def test_bless_preserves_the_testing_pin_when_production_help_differs(self):
        expected = self.fixtures / 'hidden.stderr'
        expected.write_text('testing diagnostic\n')
        outcome = self.invoke('--bless', '--output-dir', str(self.root / 'bless'))
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        self.assertEqual(expected.read_text(), 'testing diagnostic\n')
        self.assertEqual(expected.with_suffix('.stderr.no-testing').read_text(), PIN)
        self.assertIn('-testing diagnostic', (self.root / 'bless/hidden/stderr.diff').read_text())

    def test_bless_cannot_accept_unexpected_compile_success(self):
        (self.fixtures / 'hidden.rs').write_text('fn main() {}\n')
        outcome = self.invoke('--bless', '--output-dir', str(self.root / 'bless'))
        self.assertEqual(outcome.returncode, 1, outcome.stdout + outcome.stderr)
        self.assertEqual((self.fixtures / 'hidden.stderr').read_text(), PIN)
        self.assertFalse((self.fixtures / 'hidden.stderr.no-testing').exists())
        self.assertIn('rustc succeeded unexpectedly', outcome.stdout)

    def test_parallel_mixed_verdicts_keep_each_case_and_failure_receipts(self):
        (self.fixtures / 'drift.rs').write_text((self.fixtures / 'hidden.rs').read_text())
        (self.fixtures / 'drift.stderr').write_text('wrong diagnostic\n')
        manifest = json.loads(self.manifest.read_text())
        manifest['fixtures'].append({'name': 'drift', 'source': 'crates/lash/tests/ui/drift.rs', 'expected': 'crates/lash/tests/ui/drift.stderr'})
        self.manifest.write_text(json.dumps(manifest))
        environment = dict(os.environ, TEST_UNDECLARED_OUTPUTS_DIR=str(self.root / 'receipts'))
        environment.pop('UI_JOBS', None)
        outcome = subprocess.run([sys.executable, str(RUNNER), '--manifest', str(self.manifest)], cwd=self.root, env=environment, capture_output=True, text=True)
        self.assertEqual(outcome.returncode, 1, outcome.stdout + outcome.stderr)
        self.assertIn('test hidden ... ok', outcome.stdout)
        self.assertIn('test drift ... FAILED', outcome.stdout)
        self.assertIn('---- drift stdout ----\ndiagnostic drift\n', outcome.stdout)
        self.assertNotIn('---- hidden stdout ----', outcome.stdout)
        self.assertIn('1 passed; 1 failed', outcome.stdout)
        results = json.loads((self.root / 'receipts/ui-fixtures/results.json').read_text())['results']
        self.assertEqual({result['name']: result['passed'] for result in results}, {'hidden': True, 'drift': False})
        for name in ['hidden', 'drift']:
            case = self.root / 'receipts/ui-fixtures' / name
            self.assertIn('private associated function', (case / 'stderr.raw').read_text())
            self.assertTrue((case / 'stderr.diff').is_file())


if __name__ == '__main__':
    unittest.main()
