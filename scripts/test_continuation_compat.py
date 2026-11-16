"""Contract §7 requires complete cross-build evidence selected by witnesses."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import continuation_compat as compat


class ContinuationLaws(unittest.TestCase):
    def test_export_and_observation_readers_follow_the_runner_version(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(compat, 'EXCHANGE_VERSION', 7):
            root = Path(directory)
            (root / 'image.json').write_text('{}')
            (root / 'manifest.json').write_text(json.dumps(dict(version=7, cases=[
                dict(id='case', name='case', pauses=[dict(id='0', kind='start', image='image.json')])
            ])))
            pauses = compat.inventory(root, {'case': 'case'})
            path = root / 'runs.json'
            path.write_text(json.dumps(dict(version=7, runs=[dict(case='case', pause='0')])))
            self.assertEqual(set(compat.runs(path, pauses, True)), pauses)

    def test_missing_exported_pause_observation_cannot_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'runs.json'
            path.write_text(json.dumps(dict(version=2, runs=[])))
            with self.assertRaisesRegex(ValueError, 'incomplete observations'):
                compat.observations(path, {('case', '0')}, True)

    def test_heap_dependency_change_triggers_compatibility(self):
        path = 'crates/lash-kernel-state/src/paged/tree.rs'
        self.assertEqual(compat.triggered([path], {path}), [path])

    def test_changed_format_must_refuse_an_image_that_opens(self):
        before = {name: 1 for name in compat.FORMAT_NAMES}
        after = dict(before, KERNEL_PARKED_STATE_VERSION=2)
        run = dict(case='case', pause='0', opened=True, refusal=None,
                   mutations=[], executed_steps=0)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'runs.json'
            path.write_text(json.dumps(dict(version=2, runs=[run])))
            with self.assertRaisesRegex(ValueError, 'predecessor opened'):
                compat.refusals(path, {('case', '0')}, before, after)
            run.update(opened=False, refusal=dict(error='OpenError', kind='format',
                       surface='KERNEL_PARKED_STATE_VERSION', expected=2, observed=1))
            path.write_text(json.dumps(dict(version=2, runs=[run])))
            compat.refusals(path, {('case', '0')}, before, after)

    def changed(self, directory, host_pin=False, receipt=None, **run):
        """The changed-format branch against one base image the candidate
        refuses as the contract requires, unless `run` overrides it."""
        before = {name: 1 for name in compat.FORMAT_NAMES}
        after = dict(before, KERNEL_PARKED_STATE_VERSION=2)
        refused = dict(case='case', pause='0', opened=False, mutations=[], executed_steps=0,
                       refusal=dict(error='OpenError', kind='format',
                                    surface='KERNEL_PARKED_STATE_VERSION', expected=2, observed=1))
        path = Path(directory) / 'runs.json'
        path.write_text(json.dumps(dict(version=2, runs=[dict(refused, **run)])))
        inputs = {} if receipt is None else dict(deployment_receipt=str(receipt))
        return compat.changed_format(path, {('case', '0')}, before, after, inputs, 'base', 'head', host_pin)

    def test_pre_1_0_changed_format_passes_on_refusal_alone_and_declares_it(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(compat.check_version_bumps, 'STRICT_VERSION_BUMPS', False):
            self.assertEqual(self.changed(directory),
                             dict(deployment='not required', reason='pre-1.0, no host pin'))
            with self.assertRaisesRegex(ValueError, 'predecessor opened'):
                self.changed(directory, opened=True)
            with self.assertRaises(ValueError):
                self.changed(directory, mutations=['session_root'])

    def test_strict_version_bumps_require_the_deployment_receipt(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(compat.check_version_bumps, 'STRICT_VERSION_BUMPS', True):
            with self.assertRaisesRegex(ValueError, 'deployment receipt'):
                self.changed(directory)

    def test_host_pin_requires_the_deployment_receipt(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(compat.check_version_bumps, 'STRICT_VERSION_BUMPS', False):
            with self.assertRaisesRegex(ValueError, 'deployment receipt'):
                self.changed(directory, host_pin=True)

    def test_supplied_receipt_is_verified_before_1_0(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(compat.check_version_bumps, 'STRICT_VERSION_BUMPS', False):
            receipt = Path(directory) / 'deployment.json'
            receipt.write_text(json.dumps(dict(version=2, base_commit='base',
                                               ticket_url='https://example.com/FIG-1')))
            with self.assertRaisesRegex(ValueError, 'not linked'):
                self.changed(directory, receipt=receipt)


if __name__ == '__main__':
    unittest.main()
