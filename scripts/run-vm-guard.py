#!/usr/bin/env python3
"""Run one source guard or its laws with the ordinary test receipt format."""
import argparse
import importlib.util
from pathlib import Path
import sys
import unittest


def load(root, name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), root / 'scripts' / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--guard', choices=['static_state', 'parent_paths', 'laws'], required=True)
    parser.add_argument('--list', action='store_true')
    parser.add_argument('--exact', action='store_true')
    parser.add_argument('--ignored', action='store_true')
    parser.add_argument('--format', default='pretty')
    parser.add_argument('selectors', nargs='*')
    args = parser.parse_intermixed_args()
    root = args.root.resolve()
    static = load(root, 'check-vm-static-state')
    parent = load(root, 'check-vm-parent-paths')
    if args.guard == 'laws':
        laws = load(root, 'test_vm_guards')
        names = unittest.defaultTestLoader.getTestCaseNames(laws.ProductionScopeLaws)
        cases = [(f'ProductionScopeLaws.{name}', lambda name=name: laws.ProductionScopeLaws(name).debug()) for name in names]
    else:
        guard = static if args.guard == 'static_state' else parent
        def check():
            problems = guard.check(root)
            if problems:
                raise AssertionError('\n'.join(problems))
        cases = [(f'vm_guards::{args.guard}', check)]
    selectors = [s for s in args.selectors if s != '--exact']
    if selectors:
        cases = [(name, run) for name, run in cases if name in selectors]
    if args.ignored:
        cases = []
    if args.list:
        for name, _ in cases:
            print(f'{name}: test')
        return 0
    if not cases:
        parser.error('no guard law matched the exact selectors')
    print(f'running {len(cases)} tests', flush=True)
    failed = 0
    for name, run in cases:
        try:
            run()
        except Exception as error:
            failed += 1
            print(str(error), file=sys.stderr, flush=True)
            print(f'test {name} ... FAILED', flush=True)
        else:
            print(f'test {name} ... ok', flush=True)
    print(f'\ntest result: {"FAILED" if failed else "ok"}. {len(cases) - failed} passed; {failed} failed; 0 ignored; 0 measured; 0 filtered out')
    return int(bool(failed))


if __name__ == '__main__':
    raise SystemExit(main())
