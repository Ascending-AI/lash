#!/usr/bin/env python3
"""Repository build entrypoint using the pinned, unmodified Buck2 release."""
import argparse
import configparser
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

from service_policy import needs_local_uncached
from invocation import regular_file, run_command
from test_selection import skipped_line

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
CONFIGS = ('judged', 'optimized')
BAZEL_CONFIGS = {'local': 'use --local', 'shared': 'use --shared (the default)'}
# Bazel spellings lanes still use, with the control that replaces each here.
# Buck2 and driver flags are kebab-case; any other underscore flag is Bazel's.
BAZEL_FLAGS = {
    'build_tests_only': 'kiln test already builds only what the selected tests need',
    'cache_test_results': 'use --no-test-cache (or --nocache_test_results) to execute instead of reusing a cached verdict',
    'compilation_mode': 'use --config=optimized',
    'flaky_test_attempts': 'the runner never retries; use --runs_per_test=N to reproduce a flake',
    'keep_going': 'use Buck2 --keep-going',
    'platforms': 'use --config=judged or --config=optimized',
    'remote_accept_cached': 'use Buck2 --no-remote-cache for builds, --no-test-cache for test verdicts',
    'remote_download_outputs': 'use --materializations final',
    'spawn_strategy': 'use --local',
    'strategy': 'use --local',
    'test_keep_going': 'use Buck2 --keep-going, or --fail-fast to stop early',
    'test_lang_filters': 'select test labels or package patterns explicitly',
    'test_size_filters': 'select test labels or package patterns explicitly',
    'test_strategy': 'use --local-test-execution',
    'test_tag_filters': 'select test labels or package patterns explicitly; patterns already drop manual targets',
    'test_timeout_filters': 'select test labels or package patterns explicitly',
}
DOCS = 'see docs/agents/hermetic-build.md'
# Policy tags whose test cannot execute under Buck2 at all, with what proves
# the same thing. The other `cargo-*` tags still run here when named:
# `cargo-service-gate` against a live service, `cargo-frontend-assets` after
# the frontend build; `cargo-feature-gate` targets have no Buck2 label.
CARGO_ONLY = {
    'cargo-trybuild': 'trybuild runs Cargo, which the Buck2 sandbox does not provide; test {package}:ui_fixtures, which seals the same .stderr pins, or run `just seal` (cargo test --workspace --locked --test ui)',
}
FEATURE_VARIANT = re.compile(r'__fv_[0-9a-f]+$')


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
    parser.add_argument('--config')
    parser.add_argument('--test-report', type=Path)
    parser.add_argument('--test-output-dir', type=Path)
    parser.add_argument('--test_env', action='append', default=[])
    parser.add_argument('--test_arg', action='append', default=[])
    parser.add_argument('--test_timeout', type=int)
    parser.add_argument('--test_output', choices=['errors', 'all'], default='errors')
    parser.add_argument('--no-test-cache', '--nocache_test_results', action='store_true')
    parser.add_argument('--local-test-execution', action='store_true')
    parser.add_argument('--runs_per_test')
    parser.add_argument('--test_filter')
    parser.add_argument('--test_sharding_strategy')
    options, remaining = parser.parse_known_args(argv)
    if options.jobs <= 0 or (options.test_timeout is not None and options.test_timeout <= 0):
        parser.error('Jobs and test timeout must be positive')
    test_options = options.test_report or options.test_output_dir or options.test_env or options.test_arg or options.test_timeout or options.no_test_cache or options.local_test_execution or options.runs_per_test or options.test_filter or options.test_sharding_strategy
    if options.operation != 'test' and test_options:
        parser.error('Test controls require the test operation')
    if options.config is not None and options.config not in CONFIGS:
        hint = BAZEL_CONFIGS.get(options.config)
        raise ValueError(f'Unknown --config={options.config}; Buck2 configurations are judged and optimized' + (f'; for the Bazel config, {hint}' if hint else ''))
    if options.runs_per_test is not None:
        if not re.fullmatch(r'[1-9][0-9]*', options.runs_per_test):
            raise ValueError(f'--runs_per_test takes a positive run count, not {options.runs_per_test!r}; select the tests to repeat with labels and --test_arg')
        options.runs_per_test = int(options.runs_per_test)
        if options.test_report:
            raise ValueError('--runs_per_test writes one report per run at <test-output-dir>/run-<k>/test-report.json; drop --test-report')
    if options.test_sharding_strategy not in (None, 'disabled'):
        raise ValueError(
            f'--test_sharding_strategy={options.test_sharding_strategy} is not supported; only disabled is accepted, as a no-op. '
            'The runner does not shard: tools/buck2/package-policy.toml alone splits a target into __shard_<k> tests, '
            'and a filter selects across their union'
        )
    reject_bazel_flags(remaining)
    return options, remaining


def reject_bazel_flags(tokens):
    for index, token in enumerate(tokens):
        if token == '--':
            return
        if token == '-c' and index + 1 < len(tokens) and tokens[index + 1] in ('opt', 'dbg', 'fastbuild'):
            raise ValueError(f'-c {tokens[index + 1]} is a Bazel compilation mode; use --config=optimized or the default profile')
        match = re.fullmatch(r'--([a-z][a-z0-9]*(?:_[a-z0-9]+)+)(?:=.*)?', token)
        if match:
            name = match[1]
            if name not in BAZEL_FLAGS and name.startswith('no') and name[2:] in BAZEL_FLAGS:
                name = name[2:]
            hint = BAZEL_FLAGS.get(name)
            raise ValueError(f'{token} is a Bazel flag the Buck2 driver does not accept; ' + (hint + '; ' if hint else '') + DOCS)


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


TARGET_PATTERN = re.compile(r'^//(?:[A-Za-z0-9_.+-]+(?:/[A-Za-z0-9_.+-]+)*)?(?:/\.\.\.|\.\.\.|:[A-Za-z0-9_.+=,@~*-]*)?$')


def package_pattern(label):
    """Return (package, recursive) for `//pkg/...` or `//pkg:all`-style patterns."""
    if label == '//...':
        return '', True
    if label.endswith('/...'):
        return label[2:-4], True
    if label.endswith((':', ':all', ':*')):
        return label[2:].rsplit(':', 1)[0], False
    return None


def pattern_matches(pattern, label):
    package, recursive = pattern
    owner = label[2:].split(':', 1)[0]
    return owner == package or (recursive and (not package or owner.startswith(package + '/')))


def manual(target):
    return 'manual' in target.get('tags', ())


def reject_cargo_only(labels, inventory):
    """Fail an explicit test of a label only Cargo can execute, naming its replacement."""
    tags = {target['label']: target.get('tags', ()) for package in inventory['packages'] for target in package['targets'] if target.get('label')}
    for label in labels:
        key = label.removeprefix('root')
        key = key if key.startswith('//') else '//' + key.removeprefix('./')
        for tag in tags.get(FEATURE_VARIANT.sub('', key), ()):
            if tag in CARGO_ONLY:
                raise ValueError(f'{key} is Cargo-only ({tag}) and cannot run under Buck2: ' + CARGO_ONLY[tag].format(package=key.split(':', 1)[0]))


def expand_labels(tokens, inventory, operation, skipped=None):
    ordinary = [target for package in inventory['packages'] for target in package['targets']]
    records = ordinary + inventory.get('feature_lane_units', [])
    field = 'build_label' if operation == 'build' else operation + '_label'
    mapping = {target['label']: target[field] for target in records if field in target}
    # A wildcard names the generated Cargo targets beneath it, each mapped to
    # the operation's output exactly as if listed. Passing it to Buck2 instead
    # would build default outputs, so check and clippy would link or skip lint.
    # Feature-lane variants stay behind their explicit //:feature_lane_* groups,
    # and a `manual` target is selected only by its own label, as under Bazel.
    selectable = [target for target in ordinary if target.get('label') and field in target]
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
        pattern = package_pattern(key) if key.startswith('//') else None
        if pattern is not None:
            candidates = [target for target in selectable if pattern_matches(pattern, target['label'])]
            matched = [target[field] for target in candidates if not manual(target)]
            if skipped is not None:
                skipped.extend(target['label'] for target in candidates if manual(target))
            if matched:
                expanded.extend(matched)
            elif candidates:
                raise ValueError(f'No non-manual {operation} targets match {key}; name a manual target explicitly to select it')
            elif operation == 'build':
                # Stock Buck2 builds non-Cargo packages such as tool rules.
                expanded.append(token)
            else:
                raise ValueError(f'No generated {operation} targets match {key}')
        elif key in groups:
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


def command(options, remaining, executable, root, inventory=None, skipped=None):
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
        universe = [arg.removeprefix('root') if arg.startswith('root//') else arg for arg in args] or ['//:workspace_compile']
        invalid = [arg for arg in universe if not TARGET_PATTERN.match(arg)]
        if invalid:
            raise ValueError('analyze accepts only target labels and patterns: ' + ' '.join(invalid))
        terms = []
        for label in universe:
            pattern = package_pattern(label)
            if pattern is None:
                terms.append(f'deps({label})')
                continue
            # Buck2 tests carry policy tags as `labels`; the fixture rules as `tags`.
            terms.append(f'deps({label} - attrfilter(labels, manual, {label}) - attrfilter(tags, manual, {label}))')
            if inventory is not None and skipped is not None:
                skipped.extend(target['label'] for package in inventory['packages'] for target in package['targets'] if target.get('label') and manual(target) and pattern_matches(pattern, target['label']))
        args = ['--show-providers', ' + '.join(terms)]
    elif operation == 'run':
        if not labels:
            raise ValueError('run requires an explicit target')
    elif not labels:
        args.insert(0, DEFAULTS[operation])
    if operation in ('build', 'check', 'clippy', 'doc'):
        if inventory is None:
            raise ValueError('Missing generated target inventory; run sync')
        args = expand_labels(args, inventory, operation, skipped)
    if operation == 'test':
        if inventory is not None:
            reject_cargo_only([args[index] for index in labels], inventory)
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
        result += ['--test-arg=' + arg for arg in options.test_arg + ([options.test_filter] if options.test_filter else [])]
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
    if options.operation in ('analyze', 'build', 'check', 'clippy', 'doc', 'test'):
        inventory = json.loads((ROOT / 'tools/buck2/target-inventory.json').read_text())
    skipped = []
    argv = command(options, remaining, executable, ROOT, inventory, skipped)
    if skipped:
        print(skipped_line(dict.fromkeys(skipped)), file=sys.stderr, flush=True)
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
        extra = ['--test-env-file', environment.name]

        def execute(args):
            return run_command(ROOT, options.jobs, options.isolation_dir, args, planner=lambda args: plan_test_command(args, ROOT))
        if options.runs_per_test is not None:
            return repeat_tests(options, remaining, executable, ROOT, extra, execute)
        return execute(argv + extra)


def run_outcome(report_path):
    """Return the executed and failed case counts of one run, with its problems."""
    try:
        report = json.loads(Path(report_path).read_text())
    except (OSError, ValueError):
        return 0, 0, ['no test report']
    problems = [] if report.get('session_complete') is True else ['incomplete test report']
    results = report.get('results') or {}
    executed = failed = 0
    for label, result in sorted(results.items()):
        if result.get('status') != 'PASS':
            problems.append(f'{label} {result.get("status")}')
        if result.get('cache'):
            problems.append(f'{label} reused a cached verdict')
        xml = (result.get('outputs') or {}).get('junit_xml')
        try:
            cases = ET.parse(xml).getroot().iter('testcase') if xml else ()
        except (ET.ParseError, OSError):
            problems.append(f'{label} has an unreadable test.xml')
            continue
        for case in cases:
            # A suite-named error records the binary's exit, not a test case:
            # an unmatched selector or a crash before any case completed.
            if case.find('skipped') is None and not (case.find('error') is not None and case.get('name') == case.get('classname')):
                executed += 1
                failed += case.find('failure') is not None or case.find('error') is not None
    if not executed:
        problems.append('executed zero test cases')
    return executed, failed, problems


def repeat_tests(options, remaining, executable, root, extra, execute):
    """Execute the selection once per run, never from the verdict cache."""
    runs = options.runs_per_test
    base = (options.test_output_dir or root / '.buck2/test-results').absolute()
    failed_runs = []
    counts = set()
    for run in range(1, runs + 1):
        directory = base / f'run-{run}'
        run_options = argparse.Namespace(**{**vars(options), 'test_output_dir': directory, 'test_report': directory / 'test-report.json', 'no_test_cache': True})
        run_options.test_report.unlink(missing_ok=True)
        code = execute(command(run_options, remaining, executable, root) + extra)
        executed, failed, problems = run_outcome(run_options.test_report)
        if code and not problems:
            problems.append(f'exit code {code}')
        counts.add(executed)
        verdict = 'FAIL: ' + '; '.join(problems) if problems else 'PASS'
        print(f'runs_per_test: run {run}/{runs}: {executed - failed} passed, {failed} failed - {verdict} ({directory})', flush=True)
        if problems:
            failed_runs.append(run)
        if code not in (0, 32) or not executed:
            # An incomplete run or an empty selection repeats deterministically.
            print(f'runs_per_test: stopped after run {run}/{runs}', flush=True)
            return code if code not in (0, 32) else 32
    if failed_runs:
        print(f'runs_per_test: {len(failed_runs)} of {runs} runs failed (runs {", ".join(map(str, failed_runs))})', flush=True)
        return 32
    print(f'runs_per_test: {runs}/{runs} runs passed, {" or ".join(map(str, sorted(counts)))} cases per run', flush=True)
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f'hermetic-build: {error}')
