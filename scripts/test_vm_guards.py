#!/usr/bin/env python3
"""Laws for ADR 0123's production-only source inventories."""
import importlib.util
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('vm_static_laws', ROOT / 'scripts/check-vm-static-state.py')
static = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = static
spec.loader.exec_module(static)


class ProductionScopeLaws(unittest.TestCase):
    def test_test_only_items_are_excluded_but_production_branches_remain(self):
        source = '''
#[cfg(test)]
mod laws { static GUEST: u64 = 1; }
#[cfg(all(test, unix))]
fn fixture() { static GUEST: u64 = 2; }
#[cfg(feature = "testing")]
thread_local! { static COUNTER: Cell<u64> = Cell::new(0); }
#[cfg(any(test, feature = "shipping"))]
static PRODUCTION: Mutex<u64> = Mutex::new(0);
static ALWAYS: Mutex<u64> = Mutex::new(0);
'''
        production = static.production(source)
        self.assertNotIn('GUEST', production)
        self.assertNotIn('COUNTER', production)
        self.assertIn('PRODUCTION', production)
        self.assertIn('ALWAYS', production)
        self.assertEqual(source.count('\n'), production.count('\n'))

    def test_test_cfg_cannot_be_spelled_in_a_string_to_hide_production(self):
        source = 'const TEXT: &str = "#[cfg(test)]";\nstatic GUEST: Mutex<u64> = Mutex::new(0);\n'
        self.assertIn('GUEST', static.production(source))

    def test_test_only_external_modules_are_not_scanned(self):
        source = '#[cfg(test)]\nmod tests;\n#[cfg(all(test, unix))]\nmod turn_laws;\nmod worker;\n'
        modules = static.module_declarations(static.production(source))
        self.assertEqual([(None, 'worker')], modules)

    def test_inline_module_children_remain_production(self):
        source = 'mod host { mod nested; }\n#[cfg(test)] mod laws { mod fixture; }\n'
        self.assertEqual([(None, 'host/nested')], static.module_declarations(static.production(source)))


if __name__ == '__main__':
    unittest.main()
