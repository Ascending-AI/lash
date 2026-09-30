#!/usr/bin/env python3
"""Repository build entrypoint using the pinned, unmodified Buck2 release."""
import argparse
import configparser
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from service_policy import needs_local_uncached
from invocation import regular_file, run_command

ROOT = Path(__file__).resolve().parents[2]
DEFAULTS = {'build': '//:workspace_compile', 'check': '//:workspace_check', 'test': '//:dev_tests', 'clippy': '//:workspace_clippy', 'doc': '//:workspace_docs'}
REMOTE_CONFIGURATION = {
    'buck2_re_client': (
        'engine_address', 'cas_address', 'action_cache_address', 'instance_name',
        'tls_ca_certs', 'tls_client_cert',
    ),
    'kiln': ('executor_runtime',),
}
FORK_RECOVERY = (
    'use the current Kiln CLI to create a fresh fork '
    '(`kiln fork lash <name>`), or pass --local'
)


def validate_remote_configuration(path):
    values = configparser.ConfigParser(interpolation=None)
    try:
        values.read_string(regular_file(path))
    except (OSError, UnicodeError, ValueError, configparser.Error):
        raise ValueError(
            'Missing or invalid shared executor configuration; ' + FORK_RECOVERY
        ) from None
    missing = [
        f'{section}.{key}'
        for section, keys in REMOTE_CONFIGURATION.items()
        for key in keys
        if not values.get(section, key, fallback='').strip()
    ]
    if missing:
        raise ValueError(
            'Shared executor configuration is incomplete (' + ', '.join(missing)
            + '); ' + FORK_RECOVERY
        )


def arguments(argv):
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--local', action='store_true')
    mode.add_argument('--shared', action='store_true')
    parser.add_argument('operation', choices=['sync', 'clean', 'analyze', *DEFAULTS, 'run'], nargs='?', default='build')
    parser.add_argument('--jobs', type=int, default=32 if os.environ.get('CI') else 16)
    parser.add_argument('--isolation-dir', default='kiln')
    parser.add_argument('--materializations', choices=['final', 'none'])
    parser.add_argument('--config', choices=['judged', 'optimized'])
    parser.add_argument('--test-report', type=Path)
    parser.add_argument('--test-output-dir', type=Path)
    parser.add_argument('--test_env', action='append', default=[])
    parser.add_argument('--test_arg', action='append', default=[])
    parser.add_argument('--test_timeout', type=int)
    parser.add_argument('--test_output', choices=['errors', 'all'], default='errors')
    parser.add_argument('--no-test-cache', action='store_true')
    parser.add_argument('--local-test-execution', action='store_true')
    options, remaining = parser.parse_known_args(argv)
    if options.jobs <= 0 or (options.test_timeout is not None and options.test_timeout <= 0):
        parser.error('Jobs and test timeout must be positive')
    test_options = options.test_report or options.test_output_dir or options.test_env or options.test_arg or options.test_timeout or options.no_test_cache or options.local_test_execution
    if options.operation != 'test' and test_options:
        parser.error('Test controls require the test operation')
    return options, remaining


def resolve_environment(entries, environment):
    values = {}
    for entry in entries:
        key, separator, value = entry.partition('=')
        if not key or '=' in key or '\0' in key or key.startswith('KILN_ACTION_') or key in ('XML_OUTPUT_FILE', 'TEST_UNDECLARED_OUTPUTS_DIR', 'LASH_TEST_TIMEOUT_SECONDS'):
            raise ValueError(f'Reserved or invalid test environment key: {key}')
        if not separator:
            if key not in environment:
                raise ValueError(f'Test environment variable is unset: {key}')
            value = environment[key]
        values[key] = value
    return values


def expand_labels(tokens, inventory, operation):
    records = [target for package in inventory['packages'] for target in package['targets']]
    records += inventory.get('feature_lane_units', [])
    field = 'build_label' if operation == 'build' else operation + '_label'
    mapping = {target['label']: target[field] for target in records if field in target}
    groups = {
        'build_label': {'//:workspace_compile': 'workspace_build_targets', '//:feature_lane_compile': 'feature_lane_build_targets'},
        'check_label': {'//:workspace_check': 'workspace_check_targets', '//:workspace_compile': 'workspace_check_targets', '//:feature_lane_compile': 'feature_lane_check_targets'},
        'clippy_label': {'//:workspace_clippy': 'workspace_clippy_build_targets', '//:feature_lane_clippy': 'feature_lane_clippy_build_targets'},
        'doc_label': {},
    }[field]
    expanded = []
    skip_value = False
    for token in tokens:
        if skip_value:
            expanded.append(token)
            skip_value = False
            continue
        if token in ('--target-platforms', '--build-report', '--event-log', '--write-build-id', '--command-report-path', '--modifier'):
            expanded.append(token)
            skip_value = True
            continue
        key = token.removeprefix('root') if token.startswith('root//') else token
        if key in groups:
            targets = inventory.get(groups[key])
            if not targets:
                raise ValueError(f'No generated {operation} targets for {key}; run sync')
            expanded.extend(targets)
        elif key in mapping:
            expanded.append(mapping[key])
        elif operation in ('check', 'clippy', 'doc') and key.startswith('//') and key not in mapping.values() and key != '//:workspace_docs':
            raise ValueError(f'No generated {operation} output for {key}; select a target from the inventory')
        else:
            expanded.append(token)
    return expanded


def command(options, remaining, executable, root, inventory=None):
    operation = options.operation
    actual = 'build' if operation in ('check', 'clippy', 'doc') else 'cquery' if operation == 'analyze' else operation
    result = [str(executable), '--isolation-dir', options.isolation_dir, actual]
    if operation == 'clean':
        if remaining:
            raise ValueError('clean accepts no arguments')
        return result
    if operation != 'analyze':
        result += ['--num-threads', '8']
    result += ['-c', 'kiln.execution_mode=' + ('local' if options.local else 'remote')]
    if options.local and operation != 'analyze':
        result.append('--local-only')
    if options.config:
        result += ['--target-platforms', '//tools/buck2:' + options.config]
    if options.config == 'optimized':
        result += ['-c', 'kiln.rust_profile=optimized']
    materializations = options.materializations or ('final' if operation in ('doc', 'run') else 'none')
    if operation not in ('test', 'analyze', 'run'):
        result += ['--materializations', 'all' if materializations == 'final' else 'none']
    elif operation == 'test' and materializations != 'none':
        raise ValueError('test materializes declared reports automatically; use build --materializations final for executable artifacts')
    args = list(remaining)
    labels = [arg for arg in args if arg.startswith('//') or arg.startswith('root//')]
    if operation == 'test':
        from test_selection import target_positions
        labels = target_positions(args)
    if operation == 'analyze':
        if args:
            raise ValueError('analyze accepts no arguments')
        args = ['--show-providers', 'deps(//:workspace_compile)']
    elif operation == 'run':
        if not labels:
            raise ValueError('run requires an explicit target')
    elif not labels:
        args.insert(0, DEFAULTS[operation])
    if operation in ('build', 'check', 'clippy', 'doc'):
        if inventory is None:
            raise ValueError('Missing generated target inventory; run sync')
        args = expand_labels(args, inventory, operation)
    if operation == 'test':
        result += ['--exclude=lash.internal_test_binary', '--always-exclude']
        result += ['-c', f'test.v2_test_executor={root / "tools/buck2/test_runner.py"}']
        if '--' in args:
            raise ValueError('Use --test_arg for test arguments')
        result += args + ['--', '--test-report', str((options.test_report or root / '.buck2/test-report.json').absolute()), '--test-output-dir', str((options.test_output_dir or root / '.buck2/test-results').absolute()), '--max-concurrency', str(options.jobs), '--test-output', options.test_output]
        if options.test_timeout is not None:
            result += ['--timeout', str(options.test_timeout)]
        service = needs_local_uncached(entry.partition('=')[0] for entry in options.test_env)
        if options.no_test_cache or service:
            result.append('--no-test-cache')
        if options.local_test_execution or options.local or service:
            result.append('--local-test-execution')
        result += ['--test-arg=' + arg for arg in options.test_arg]
    else:
        result += args
    return result


def main(argv=None):
    options, remaining = arguments(sys.argv[1:] if argv is None else argv)
    invocation_directory = os.environ.get('LASH_BUILD_WORKING_DIRECTORY', str(Path.cwd()))
    os.chdir(ROOT)
    sync = ROOT / 'tools/buck2/sync.py'
    executable = ROOT / '.buck2/bin/buck2'
    bootstrap = [sys.executable, str(ROOT / 'tools/buck2/bootstrap.py')]
    if options.operation == 'clean':
        bootstrap.append('--client-only')
    subprocess.run(bootstrap, check=True, stdout=subprocess.DEVNULL)
    if options.operation == 'sync':
        if remaining:
            raise ValueError('sync accepts no arguments')
        return subprocess.call([sys.executable, str(sync)])
    if options.operation != 'clean':
        if not options.local:
            validate_remote_configuration(ROOT / '.buckconfig.local')
        subprocess.run([sys.executable, str(sync), '--check'], check=True)
    inventory = None
    if options.operation in ('build', 'check', 'clippy', 'doc'):
        inventory = json.loads((ROOT / 'tools/buck2/target-inventory.json').read_text())
    argv = command(options, remaining, executable, ROOT, inventory)
    if options.operation != 'test':
        environment = None
        if options.operation == 'run':
            environment = dict(os.environ, BUILD_WORKING_DIRECTORY=invocation_directory, BUILD_WORKSPACE_DIRECTORY=str(ROOT))
            environment.pop('LASH_BUILD_WORKING_DIRECTORY', None)
        return run_command(ROOT, options.jobs, options.isolation_dir, argv, env=environment, stdout=subprocess.DEVNULL if options.operation == 'analyze' else None)
    from runner_bootstrap import ensure_runtime, safe_directory
    from test_selection import plan_test_command
    ensure_runtime()
    values = resolve_environment(options.test_env, os.environ)
    state = safe_directory(ROOT / '.buck2/runtime-env')
    with tempfile.NamedTemporaryFile(mode='w', prefix='test-', suffix='.json', dir=state) as environment:
        json.dump(values, environment)
        environment.flush()
        return run_command(ROOT, options.jobs, options.isolation_dir, argv + ['--test-env-file', environment.name], planner=lambda args: plan_test_command(args, ROOT))


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f'hermetic-build: {error}')
