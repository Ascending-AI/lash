#!/usr/bin/env python3
"""Lash external test runner for Buck2's documented v2 test protocol."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import socket
import stat
import sys
import tempfile
import threading
import xml.etree.ElementTree as ET

from junit_xml import libtest_cases
from libtest_selection import ARGUMENT_MARKER, VALUE_FLAGS
from runner_bootstrap import ROOT, activate, safe_directory
from service_policy import needs_local_uncached

activate()
import grpc
import host_sharing_pb2 as host
import test_pb2 as pb
import test_pb2_grpc as rpc


def relay(left, right):
    """Connect inherited Unix streams to gRPC's public Unix-socket interface."""
    def pump(source, destination):
        try:
            while chunk := source.recv(65536):
                destination.sendall(chunk)
        except OSError:
            pass
        finally:
            try:
                destination.shutdown(socket.SHUT_WR)
            except OSError:
                pass
    for source, destination in ((left, right), (right, left)):
        threading.Thread(target=pump, args=(source, destination), daemon=True).start()


class Executor(rpc.TestExecutorServicer):
    def __init__(self):
        self.requests = queue.Queue()

    def ExternalRunnerSpec(self, request, context):
        self.requests.put(request.test_spec)
        return pb.Empty()

    def EndOfTestRequests(self, request, context):
        self.requests.put(None)
        return pb.Empty()

    def Unstable_HeapDump(self, request, context):
        context.abort(grpc.StatusCode.UNIMPLEMENTED, 'Heap dumps are not supported')


def value(spec):
    return pb.ArgValue(content=pb.ArgValueContent(spec_value=spec))


def declared(name, format=None):
    result = pb.ArgValue(content=pb.ArgValueContent(declared_output=pb.OutputName(name=name)))
    if format:
        result.format.format = format
    return result


def parse_env(entries):
    result = {}
    for entry in entries:
        name, separator, supplied = entry.partition('=')
        if not name or name.startswith('KILN_ACTION_') or name in ('XML_OUTPUT_FILE', 'TEST_UNDECLARED_OUTPUTS_DIR', 'LASH_TEST_TIMEOUT_SECONDS'):
            raise ValueError(f'Reserved or invalid test environment key: {name}')
        if separator:
            result[name] = supplied
        elif name in os.environ:
            result[name] = os.environ[name]
        else:
            raise ValueError(f'Test environment variable is unset: {name}')
    return result


def target_directory(root, spec):
    # Every cell occupies its own namespace, including root.
    components = [spec.target.cell, spec.target.package, spec.target.target]
    relative = Path(*components)
    if relative.is_absolute() or '..' in relative.parts:
        raise ValueError('Invalid test label output path')
    destination = root / relative
    safe_directory(destination.parent, private=False)
    if destination.is_symlink():
        raise ValueError(f'Refusing symlinked test output: {destination}')
    return destination


def copy_outputs(root, spec, result, stdout, stderr):
    source = {}
    for entry in result.outputs:
        if entry.output.WhichOneof('value') != 'local_path':
            raise ValueError('A declared test output was not materialized')
        source[entry.declared_output.name] = Path(entry.output.local_path)
    destination = target_directory(root, spec)
    marker = destination / '.lash-test-output'
    if destination.exists() and (marker.is_symlink() or not marker.is_file() or marker.read_text() != '1\n'):
        raise ValueError(f'Refusing to replace unowned test output directory: {destination}')
    with tempfile.TemporaryDirectory(prefix='.result-', dir=destination.parent) as work:
        work = Path(work)
        outputs = {}
        if 'junit' in source:
            xml = source['junit'] / 'test.xml'
            if xml.is_file() and not xml.is_symlink():
                shutil.copyfile(xml, work / 'test.xml')
                outputs['junit_xml'] = str(destination / 'test.xml')
        if 'undeclared' in source and source['undeclared'].is_dir():
            shutil.copytree(source['undeclared'], work / 'undeclared', symlinks=True)
            outputs['undeclared'] = str(destination / 'undeclared')
        (work / 'test.log').write_text(stdout + ('\n---- STDERR ----\n' + stderr if stderr else ''))
        outputs['log'] = str(destination / 'test.log')
        (work / '.lash-test-output').write_text('1\n')
        if destination.exists():
            shutil.rmtree(destination)
        os.replace(work, destination)
    return outputs


def selection_variant(test_args, runtime_env, timeout, local, uncached):
    """Name the Buck2 output root after everything this runner adds to a test.

    Buck2 places a test's declared outputs under a directory named by its
    target and by the stage's variant, never by its command. Without a variant,
    every execution of one target shares that directory, so two invocations
    with different filters read each other's `test.xml`. The name is a
    function of the selection alone, which keeps the remote action, and so the
    cached verdict, the same for the same selection in every checkout.
    """
    selection = json.dumps([test_args, sorted(runtime_env.items()), timeout, local, uncached])
    return 'lash-' + hashlib.sha256(selection.encode()).hexdigest()[:16]


@contextmanager
def output_lease(directory, label, configuration, variant):
    """Hold one target's output root for one selection across invocations.

    Invocations that run the same selection of the same target still share an
    output root. The lease runs them one after another, from execution until
    the outputs are copied out. It ends with the process that holds it.
    """
    name = hashlib.sha256('\0'.join((label, configuration, variant)).encode()).hexdigest()
    fd = os.open(directory / (name + '.lock'), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def libtest_arguments(spec, test_args):
    """Return the libtest arguments this runner can read, and whether that is all of them."""
    baked = []
    complete = True
    marker = False
    for arg in spec.command:
        if marker:
            if arg.WhichOneof('value') == 'verbatim':
                baked.append(arg.verbatim)
            else:
                complete = False
        elif arg.WhichOneof('value') == 'verbatim' and arg.verbatim == ARGUMENT_MARKER:
            marker = True
    return baked + list(test_args), complete


def selection_filters(args):
    """Split libtest arguments into positional filters and skipped patterns."""
    filters, skips = [], []
    arguments = iter(args)
    for arg in arguments:
        if arg == '--skip':
            skips.append(next(arguments, ''))
        elif arg.startswith('--skip='):
            skips.append(arg.removeprefix('--skip='))
        elif arg in VALUE_FLAGS:
            next(arguments, None)
        elif arg and not arg.startswith('-'):
            filters.append(arg)
    return filters, skips


def report_mismatch(xml, stdout, args, complete):
    """Say how a JUnit report differs from the selection that was executed.

    A report belongs to its execution when it names the cases that execution
    printed and no case outside the requested filters. Anything else is another
    execution's report, whatever put it there, and must not pass.
    """
    reported = {
        case.get('name') for case in ET.parse(xml).getroot().iter('testcase')
        # A suite-named case records the binary's exit, not a test.
        if case.get('name') != case.get('classname')
    }
    printed = set(libtest_cases(stdout)[0])
    if printed and printed != reported:
        strays = sorted(printed ^ reported)
        return f'the report and the test output disagree on {len(strays)} cases, e.g. {strays[0]}'
    filters, skips = selection_filters(args)
    exact = '--exact' in args

    def matches(pattern, name):
        return name == pattern if exact else pattern in name

    for name in sorted(reported):
        if any(matches(skip, name) for skip in skips):
            return f'the report names {name}, which the selection skips'
        if filters and complete and not any(matches(selector, name) for selector in filters):
            return f'the report names {name}, which is outside the selection {" ".join(filters)}'
    return None


def execute_test(client, spec, options, runtime_env):
    label = f'{spec.target.cell}//{spec.target.package}:{spec.target.target}'
    target_timeouts = [int(label.split('=', 1)[1]) for label in spec.labels if label.startswith('lash.timeout_seconds=')]
    if len(target_timeouts) > 1 or any(t <= 0 for t in target_timeouts):
        raise ValueError('Invalid target timeout policy')
    timeout = options.timeout if options.timeout is not None else (target_timeouts[0] if target_timeouts else 300)
    command = [value(arg) for arg in spec.command]
    command += [value(pb.ExternalRunnerSpecValue(verbatim=arg)) for arg in options.test_arg]
    env = {key: value(val) for key, val in spec.env.items()}
    env.update({key: value(pb.ExternalRunnerSpecValue(verbatim=val)) for key, val in runtime_env.items()})
    env['LASH_TEST_TIMEOUT_SECONDS'] = value(pb.ExternalRunnerSpecValue(verbatim=str(timeout)))
    env['XML_OUTPUT_FILE'] = declared('junit', '{}/test.xml')
    env['TEST_UNDECLARED_OUTPUTS_DIR'] = declared('undeclared')
    service = needs_local_uncached(set(spec.env) | set(runtime_env), spec.labels)
    local = bool(options.local_test_execution or service)
    uncached = bool(options.no_test_cache or service)
    variant = selection_variant(options.test_arg, runtime_env, timeout, local, uncached)
    executable = pb.TestExecutable(
        target=spec.target.handle,
        stage=pb.TestStage(testing=pb.Testing(suite=spec.target.target, variant=variant)),
        cmd=command,
        env=[pb.EnvironmentVariable(key=key, value=val) for key, val in sorted(env.items())],
        pre_create_dirs=[pb.DeclaredOutput(name=name, supports_remote=False) for name in ('junit', 'undeclared')],
    )
    request = pb.ExecuteRequest2(
        test_executable=executable,
        host_sharing_requirements=host.HostSharingRequirements(shared=host.HostSharingRequirements.Shared(weight_class=host.WeightClass(permits=1))),
        disable_test_execution_caching=uncached,
    )
    request.timeout.seconds = timeout + 15
    if local:
        request.executor_override.name = 'local'
    with output_lease(options.leases, label, spec.target.configuration, variant):
        return report_execution(client, spec, options, label, request, service)


def report_execution(client, spec, options, label, request, service):
    response = client.Execute2(request)
    if response.WhichOneof('response') == 'cancelled':
        report = {'label': label, 'status': 'OMITTED', 'exit_code': None, 'outputs': {}, 'cache': None, 'stdout': '', 'stderr': '', 'configuration': spec.target.configuration}
        client.ReportTestResult(pb.ReportTestResultRequest(result=pb.TestResult(name=label, target=spec.target.handle, status=pb.OMITTED, details='Execution cancelled')))
        return report
    result = response.result
    stdout = getattr(result.stdout, 'inline').decode('utf-8', errors='replace')
    stderr = getattr(result.stderr, 'inline').decode('utf-8', errors='replace')
    timed_out = result.status.WhichOneof('status') == 'timed_out'
    code = None if timed_out else result.status.finished
    status = 'TIMEOUT' if timed_out else ('PASS' if code == 0 else 'FAIL')
    outputs = copy_outputs(options.test_output_dir, spec, result, stdout, stderr)
    output_errors = []
    xml = outputs.get('junit_xml')
    if xml:
        try:
            mismatch = report_mismatch(xml, stdout, *libtest_arguments(spec, options.test_arg))
            if mismatch:
                output_errors.append(f'Test report does not match its selection: {mismatch}')
        except (ET.ParseError, OSError) as error:
            output_errors.append(f'Invalid per-case XML report: {error}')
    else:
        output_errors.append('Test did not produce its declared per-case XML report')
    metadata_path = Path(outputs['undeclared']) / '_lash_runner' / 'execution.json' if 'undeclared' in outputs else None
    timeout_metadata = None
    if metadata_path is not None and metadata_path.is_file() and not metadata_path.is_symlink() and not metadata_path.parent.is_symlink():
        try:
            timeout_metadata = json.loads(metadata_path.read_text())
            if not isinstance(timeout_metadata, dict):
                raise ValueError('Watchdog metadata must be an object')
        except (ValueError, OSError) as error:
            output_errors.append(f'Invalid watchdog metadata: {error}')
            timeout_metadata = None
    if timeout_metadata:
        if timeout_metadata.get('interrupted_signal'):
            status = 'FAIL'
        elif timeout_metadata.get('timed_out'):
            status = 'TIMEOUT'
        elif timeout_metadata.get('cleanup_complete') is False:
            output_errors.append('Test process group cleanup did not complete')
    output_error = '; '.join(output_errors) or None
    if status == 'PASS' and output_error:
        status = 'INFRA_FAILURE'
    kind = result.execution_details.execution_kind
    executor = kind.WhichOneof('command')
    remote = kind.remote_command if executor == 'remote_command' else None
    report = {
        'label': label, 'configuration': spec.target.configuration,
        'status': status, 'exit_code': code, 'outputs': outputs,
        'stdout': stdout, 'stderr': stderr,
        'duration_seconds': result.execution_time.seconds + result.execution_time.nanos / 1e9,
        'max_memory_used_bytes': result.max_memory_used_bytes if result.HasField('max_memory_used_bytes') else None,
        'executor': executor, 'cache': remote.cache_hit if remote is not None else False,
        'action_digest': remote.action_digest if remote is not None else None,
        'output_error': output_error, 'execution': timeout_metadata, 'service_local_uncached': service,
    }
    details = f'---- STDOUT ----\n{stdout}\n---- STDERR ----\n{stderr}\n' + (output_error or '')
    if status == 'PASS' and getattr(options, 'test_output', 'errors') != 'all':
        details = ''
    verdict = pb.TestResult(name=label, target=spec.target.handle, status=getattr(pb, status), duration=result.execution_time, details=details)
    if result.HasField('max_memory_used_bytes'):
        verdict.max_memory_used_bytes = result.max_memory_used_bytes
    client.ReportTestResult(pb.ReportTestResultRequest(result=verdict))
    return report


def run_test(client, spec, options, runtime_env):
    try:
        return execute_test(client, spec, options, runtime_env)
    except Exception as error:
        label = f'{spec.target.cell}//{spec.target.package}:{spec.target.target}'
        client.ReportTestResult(pb.ReportTestResultRequest(result=pb.TestResult(name=label, target=spec.target.handle, status=pb.INFRA_FAILURE, details=str(error))))
        return {'label': label, 'configuration': spec.target.configuration, 'status': 'INFRA_FAILURE', 'exit_code': None, 'outputs': {}, 'cache': None, 'output_error': str(error)}


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--executor-fd', type=int, required=True)
    parser.add_argument('--orchestrator-fd', type=int, required=True)
    parser.add_argument('--buck-trace-id')
    parser.add_argument('--config-entry', action='append', default=[])
    transport, remaining = parser.parse_known_args(sys.argv[1:])
    if '--' in remaining:
        remaining = remaining[remaining.index('--') + 1:]
    if remaining and remaining[0] == 'ignored':
        remaining.pop(0)
    options = argparse.ArgumentParser()
    options.add_argument('--buck-test-info')
    options.add_argument('--test-report', type=Path, required=True)
    options.add_argument('--test-output-dir', type=Path, required=True)
    options.add_argument('--timeout', type=int)
    options.add_argument('--max-concurrency', type=int, default=2)
    options.add_argument('--test-arg', action='append', default=[])
    options.add_argument('--test-env', action='append', default=[])
    options.add_argument('--test-env-file', type=Path)
    options.add_argument('--test-output', choices=['errors', 'all'], default='errors')
    options.add_argument('--no-test-cache', action='store_true')
    options.add_argument('--local-test-execution', action='store_true')
    run = options.parse_args(remaining)
    if (run.timeout is not None and run.timeout <= 0) or run.max_concurrency <= 0:
        options.error('Timeout and concurrency must be positive')
    run.test_report = run.test_report.absolute()
    run.test_output_dir = safe_directory(run.test_output_dir.absolute(), private=False)
    safe_directory(run.test_report.parent, private=False)
    run.leases = safe_directory(ROOT / '.buck2' / 'test-leases')
    return transport, run


class Reports:
    def __init__(self, path, trace_id):
        self.path = path
        self.lock = threading.Lock()
        self.value = {'schema': 1, 'trace_id': trace_id, 'session_complete': False, 'results': {}, 'infrastructure_errors': []}
        self.write()

    def write(self):
        with tempfile.NamedTemporaryFile(mode='w', dir=self.path.parent, delete=False) as output:
            temporary = Path(output.name)
            json.dump(self.value, output, indent=2)
            output.write('\n')
        os.replace(temporary, self.path)

    def started(self, spec):
        label = f'{spec.target.cell}//{spec.target.package}:{spec.target.target}'
        with self.lock:
            if label in self.value['results']:
                raise ValueError(f'Duplicate test label/configuration: {label}')
            self.value['results'][label] = {'label': label, 'status': 'RUNNING', 'configuration': spec.target.configuration, 'outputs': {}}
            self.write()

    def finished(self, future):
        with self.lock:
            try:
                result = future.result()
                self.value['results'][result['label']] = result
            except Exception as error:
                self.value['infrastructure_errors'].append(str(error))
                label = getattr(future, 'lash_label', None)
                if label in self.value['results']:
                    self.value['results'][label].update(status='INFRA_FAILURE', output_error=str(error))
            self.write()

    def exit_code(self):
        with self.lock:
            return 32 if self.value['infrastructure_errors'] or any(t['status'] != 'PASS' for t in self.value['results'].values()) else 0

    def complete(self):
        with self.lock:
            self.value['session_complete'] = True
            self.write()
            return 32 if self.value['infrastructure_errors'] or any(t['status'] != 'PASS' for t in self.value['results'].values()) else 0


def main():
    transport, options = arguments()
    runtime_env = parse_env(options.test_env)
    if options.test_env_file:
        info = options.test_env_file.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError('The runtime environment file must be a private regular file')
        supplied = json.loads(options.test_env_file.read_text())
        runtime_env.update(parse_env([key + '=' + val for key, val in supplied.items()]))
    reports = Reports(options.test_report, transport.buck_trace_id)
    with tempfile.TemporaryDirectory(prefix='lash-tests-') as work:
        executor_path, orchestrator_path = Path(work) / 'executor', Path(work) / 'orchestrator'
        executor = Executor()
        server = grpc.server(ThreadPoolExecutor(max_workers=2), options=[('grpc.max_receive_message_length', 64 * 1024 * 1024)])
        rpc.add_TestExecutorServicer_to_server(executor, server)
        server.add_insecure_port(f'unix:{executor_path}')
        server.start()
        upstream_executor = socket.socket(fileno=transport.executor_fd)
        upstream_executor.setblocking(True)
        downstream_executor = socket.socket(socket.AF_UNIX)
        downstream_executor.connect(str(executor_path))
        relay(upstream_executor, downstream_executor)
        upstream_orchestrator = socket.socket(fileno=transport.orchestrator_fd)
        upstream_orchestrator.setblocking(True)
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(orchestrator_path))
        listener.listen(1)
        def connect_orchestrator():
            connection, _ = listener.accept()
            relay(connection, upstream_orchestrator)
        threading.Thread(target=connect_orchestrator, daemon=True).start()
        channel = grpc.insecure_channel(f'unix:{orchestrator_path}', options=[('grpc.default_authority', 'localhost'), ('grpc.max_receive_message_length', 64 * 1024 * 1024)])
        client = rpc.TestOrchestratorStub(channel)
        try:
            grpc.channel_ready_future(channel).result(timeout=30)
        except grpc.FutureTimeoutError:
            reports.value['infrastructure_errors'].append('Buck2 test orchestrator connection was not ready within 30 seconds')
            reports.write()
            raise RuntimeError(reports.value['infrastructure_errors'][-1])
        with ThreadPoolExecutor(max_workers=options.max_concurrency) as pool:
            while (spec := executor.requests.get()) is not None:
                reports.started(spec)
                future = pool.submit(run_test, client, spec, options, runtime_env)
                future.lash_label = f'{spec.target.cell}//{spec.target.package}:{spec.target.target}'
                future.add_done_callback(reports.finished)
        code = reports.exit_code()
        client.EndOfTestResults(pb.EndOfTestResultsRequest(exit_code=code))
        reports.complete()
        channel.close()
        server.stop(0).wait()
        listener.close()
        upstream_executor.close()
        downstream_executor.close()
        upstream_orchestrator.close()
        if reports.value['infrastructure_errors']:
            print('\n'.join(reports.value['infrastructure_errors']), file=sys.stderr)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
