#!/usr/bin/env python3
"""Cold and differential warm interpreter comparison; pinned release downloads."""
import argparse
import ast
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import resource
import selectors
import signal
import statistics
import subprocess
import textwrap
import time
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[3]
PROGRAMS = Path(__file__).with_name("embedded")
LABEL = "//crates/lash-dialect-typescript:kernel_perf__example"
# Official optimized Linux x86_64 distributions, not workspace dependencies.
RELEASES = {
    "quickjs": (
        "https://bellard.org/quickjs/binary_releases/quickjs-linux-x86_64-2025-09-13.zip",
        "648dc418356b7fa627f78bdf43b12a1cfd94bc220fc36520e1320e36c9328d10",
        "qjs", "upstream QuickJS 2025-09-13",
    ),
    "monty": (
        "https://files.pythonhosted.org/packages/59/60/b2ee672278e291a05fec28f5e50906e2ccfff5899df4cd949693b495175e/pydantic_monty_runtime-1.1.0-py3-none-manylinux_2_28_x86_64.whl",
        "7cee4303924ad44a858ec3efdde4123cadbf032cb23006a2d260c73d68748d60",
        "pydantic_monty_runtime-1.1.0.data/scripts/monty", "Monty 1.1.0",
    ),
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    # TS numeric values can arrive as 1.0, Python's as 1. Only exact integers
    # in the common safe range are admitted; no lossy rounding for equality.
    if isinstance(value, float):
        if not value.is_integer() or abs(value) > 2**53 - 1:
            raise ValueError(f"non-common numeric result: {value!r}")
        return int(value)
    if isinstance(value, int) and not isinstance(value, bool):
        if abs(value) > 2**53 - 1:
            raise ValueError("result exceeds exact JS integer range")
    if isinstance(value, list):
        return [canonical(item) for item in value]
    if isinstance(value, dict):
        if not all(isinstance(key, str) for key in value):
            raise ValueError("result keys must be text")
        return {key: canonical(item) for key, item in value.items()}
    if value is not None and not isinstance(value, (int, str, bool)):
        raise ValueError(f"result outside common JSON domain: {value!r}")
    return value


def digest(value):
    return sha(json.dumps(canonical(value), sort_keys=True, ensure_ascii=False,
                          separators=(",", ":"), allow_nan=False).encode())


def engine_tools(fetch):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("pinned release distributions require Linux x86_64")
    tools = ROOT / "target/kernel-embedded-tools"
    binaries = {}
    for name, (url, checksum, member, _) in RELEASES.items():
        archive = tools / (name + ".zip")
        if not archive.exists():
            if not fetch:
                raise ValueError("use --fetch-engines to opt into pinned release downloads")
            archive.parent.mkdir(parents=True, exist_ok=True)
            archive.write_bytes(urllib.request.urlopen(url, timeout=60).read())
        data = archive.read_bytes()
        if sha(data) != checksum:
            raise ValueError(f"{name}: release archive SHA256 mismatch")
        # Extract only the named executable; never unpack untrusted paths.
        binary = tools / (name + "-bin")
        payload = zipfile.ZipFile(io.BytesIO(data)).read(member)
        binary.write_bytes(payload)
        binary.chmod(0o755)
        binaries[name] = str(binary)
    return binaries


def build(out):
    report = out / "build.json"
    with (out / "build.log").open("w") as log:
        subprocess.run(["kiln", "build", "--config=optimized", "--materializations=final",
                        "--build-report", str(report), LABEL], cwd=ROOT,
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    return subprocess.check_output([
        "python3", "tools/buck2/outputs.py", "--report", str(report),
        "--label", LABEL, "--single"], cwd=ROOT, text=True).strip()


def capped():
    resource.setrlimit(resource.RLIMIT_AS, (8 * 1024**3, 8 * 1024**3))


def measure(command, prefix, timeout, charge="exact"):
    perf = prefix.with_suffix(".perf.tsv")
    resources = prefix.with_suffix(".time.txt")
    invocation = ["perf", "stat", "-x", "\t", "-e", "instructions:u", "-o", str(perf),
                  "--", "/usr/bin/time", "-v", "-o", str(resources), *command]
    load1 = os.getloadavg()[0]
    began = time.perf_counter_ns()
    # The cap covers the workload and measuring wrappers. New sessions let a
    # timeout kill the entire child tree, including a reference engine's threads.
    with prefix.with_suffix(".stderr").open("wb") as err:
        proc = subprocess.Popen(invocation, cwd=ROOT, stdout=subprocess.PIPE,
                                stderr=err, preexec_fn=capped, start_new_session=True,
                                env=dict(os.environ, LC_ALL="C", NO_COLOR="1", KERNEL_PERF_CHARGE=charge))
        output = bytearray()
        first_ns = None
        deadline = time.monotonic() + timeout
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(proc.stdout, selectors.EVENT_READ)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not ready.select(remaining):
                        raise TimeoutError(f"benchmark timeout: {command}")
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    output.extend(chunk)
                    if first_ns is None and b"\n" in output:
                        first_ns = time.perf_counter_ns() - began
            proc.wait(timeout=max(0.01, deadline - time.monotonic()))
        except BaseException:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            raise
        finally:
            proc.stdout.close()
    wall_ns = time.perf_counter_ns() - began
    prefix.with_suffix(".stdout").write_bytes(output)
    if proc.returncode:
        raise ValueError(f"exit {proc.returncode}: {command}; see {prefix}.stderr")
    if first_ns is None:
        raise ValueError(f"missing result line: {command}")
    counters = [line.split("\t") for line in perf.read_text().splitlines()
                if "\tinstructions:u\t" in line]
    if len(counters) != 1 or not counters[0][0].strip().isdigit():
        raise ValueError(f"missing hardware instruction count: {perf}")
    # No silent use of a multiplexed or unsupported primary measurement.
    if float(counters[0][4]) < 99.0:
        raise ValueError(f"instruction counter multiplexed: {perf}")
    rss = re.search(r"Maximum resident set size \(kbytes\): (\d+)", resources.read_text())
    if rss is None:
        raise ValueError(f"missing peak RSS: {resources}")
    return dict(instructions=int(counters[0][0]), wall_ns=wall_ns,
                cold_first_line_ns=first_ns, peak_rss_kib=int(rss[1]), load1=load1,
                command=command), output.decode()


def parse_result(engine, output):
    lines = output.splitlines()
    if engine.startswith("lash"):
        if len(lines) != 1:
            raise ValueError("Lash must produce exactly one receipt")
        receipt = json.loads(lines[0])
        return receipt["result"], receipt
    if len(lines) != 1:
        raise ValueError(f"expected one result line: {output!r}")
    return (ast.literal_eval(lines[0]) if engine in ("cpython", "monty") else json.loads(lines[0])), {}


def validate(receipt):
    expected = {(program, engine, pair, repetitions) for program in receipt['programs']
                for engine in ['lash-ts', 'quickjs', 'lash-py', 'monty', 'node', 'cpython']
                for pair in [1, 2]
                for repetitions in repetition_counts(program, receipt.get('differential_warm', False))}
    actual = [(row['program'], row['engine'], row['pair'], row.get('repetitions', 1))
              for row in receipt['samples']]
    if not expected or len(actual) != len(expected) or set(actual) != expected:
        raise ValueError('incomplete or duplicate sample population')
    failures = [row for row in receipt['samples'] if row.get('failure')]
    if failures:
        raise ValueError('; '.join(f"{row['program']}/{row['engine']}: {row['failure']}" for row in failures))
    results = {}
    for row in receipt['samples']:
        result_sha = digest(row['result'])
        if row['result_sha256'] != result_sha:
            raise ValueError('sample result digest is corrupt')
        if results.setdefault(row['program'], result_sha) != result_sha:
            raise ValueError('cross-engine result digest mismatch')
        if any(row[key] <= 0 for key in ['instructions', 'wall_ns', 'cold_first_line_ns', 'peak_rss_kib']):
            raise ValueError('missing or nonpositive sample measurement')


def charge_readout(rows):
    if all(row["charged"] is None for row in rows):
        return "not metered"
    return f'{statistics.mean(row["charged"] for row in rows):.0f}'


def render(receipt, path):
    validate(receipt)
    if receipt.get('differential_warm'):
        render_warm(receipt, path)
        return
    samples = receipt["samples"]
    def mean(rows, key):
        return statistics.mean(row[key] for row in rows)
    lines = ["# Kernel versus embedded interpreters", "",
             f"Cold-process measurement at base `{receipt['revision']}`.", "",
             "Generated by `python3 scripts/perf/kernel/embedded.py --fetch-engines "
             "--out .kiln/FIG-6675/comparison --report docs/perf/kernel-vs-embedded.md`.", "",
             f"Host: `{receipt['host']}`; CPU: {receipt['cpu']}; kernel: {receipt['kernel']}. "
             f"Lash parent/revision: `{receipt['revision']}` (instrument sources are identified "
             "by SHA-256 in the receipt).", "",
             "Engines: " + "; ".join(f"{name}: {version}" for name, version in receipt['versions'].items()) + ".", "",
             "Two temporally adjacent cold-process pairs per dialect and program, with reversed "
             "order in pair 2; arithmetic means below. Hardware `instructions:u` is primary. "
             "It covers process startup, parsing, compilation, execution, result printing and "
             "teardown, including the small GNU time wrapper. It is not a run-only count. "
             "Wall time is secondary; first result measures spawn to the first complete stdout "
             "line, including wrapper startup and buffering. No warmup or hidden validation runs. "
             "Every child has an 8 GiB address-space cap; peak RSS is GNU time's workload RSS. "
             "Node and CPython are reference rows. Published optimized QuickJS and Monty binaries "
             "are pinned by archive SHA-256; Lash uses Kiln's optimized profile and system+stats_alloc.", "",
             f"All {len(samples)} result digests match across six engines and both pairs. "
             f"Load1 range: {min(s['load1'] for s in samples):.2f}–{max(s['load1'] for s in samples):.2f}. "
             "Shared-host wall times do not certify quiet-host latency.", "",
             "| Program | Engine | Instructions (M) | Wall (ms) | Cold first result (ms) | Peak RSS (MiB) |",
             "|---|---|---:|---:|---:|---:|"]
    groups = {}
    for sample in samples:
        groups.setdefault((sample['program'], sample['engine']), []).append(sample)
    for (program, engine), rows in groups.items():
        lines.append(f"| {program} | {engine} | {mean(rows, 'instructions') / 1e6:.3f} | "
                     f"{mean(rows, 'wall_ns') / 1e6:.3f} | {mean(rows, 'cold_first_line_ns') / 1e6:.3f} | "
                     f"{max(row['peak_rss_kib'] for row in rows) / 1024:.2f} |")
    lines += ["", "## Lash attribution", "",
              "Means in milliseconds. Registry includes both dialect helper sets, as in the existing "
              "kernel instrument; it is assembled once per cold process. Compile is split into library "
              "preparation and document compile plus cell start: the public API compiles the document "
              "lazily inside start, so pure document compilation cannot be isolated here. Run includes "
              "one finish boundary, answered by the existing scripted host. Allocation calls are run-only; "
              "charge is metered work, not a VM statement count. Counters/trace observers are off.", "",
              "| Program | Dialect | Registry | Lower | Library compile | Document compile + start | Run | Run allocations | Charge |",
              "|---|---|---:|---:|---:|---:|---:|---:|---:|"]
    for (program, engine), rows in groups.items():
        if not engine.startswith('lash'):
            continue
        inner = [row['lash'] for row in rows]
        columns = " | ".join(f"{mean(inner, key) / 1e6:.3f}" for key in [
            'registry_ns', 'lower_ns', 'library_compile_ns', 'document_compile_and_start_ns', 'run_ns'])
        lines.append(f"| {program} | {engine} | {columns} | {mean(inner, 'run_allocations'):.0f} | {charge_readout(inner)} |")
    lines += ["", "The workload geometry and fairness limits are documented in "
              "[the instrument](instruments/kernel-vs-embedded.md). Raw receipts retain every result, "
              "source and executable digest, command, pair, load1, timing split, instruction count "
              "and RSS. Quiet-host wall time remains unmeasured.", ""]
    path.write_text("\n".join(lines))


def repetition_counts(program, warm):
    if not warm:
        return [1]
    # Keep the million-iteration/recursive rows practical; amortize preparation
    # noise more strongly for the short heap and workflow bodies.
    n = 2 if program in ('sum1m', 'branch1m', 'fib25') else 100
    return [n, 2 * n]


def repeated_source(source, suffix, repetitions):
    if suffix == 'ts':
        return ('function embedded_body() {\n' + textwrap.indent(source, '  ')
                + '\n  return result;\n}\nlet result = null;\n'
                + f'for (let repeat = 0; repeat < {repetitions}; repeat++) {{\n'
                + '  result = embedded_body();\n}\n')
    return ('def embedded_body():\n' + textwrap.indent(source, '    ')
            + '\n    return result\nresult = None\nrepeat = 0\n'
            + f'while repeat < {repetitions}:\n'
            + '    result = embedded_body()\n    repeat = repeat + 1\n')


def render_warm(receipt, path):
    groups = {}
    for row in receipt['samples']:
        groups.setdefault((row['program'], row['engine'], row['pair']), {})[row['repetitions']] = row
    lines = ['# Differential warm execution', '', f"Base: `{receipt['revision']}`.", '',
             'Each row is (instructions(2N) - instructions(N)) / N; preparation is outside '
             'the incremental work. Results must match every engine, pair and repetition count.', '',
             'A nonpositive Node reference delta is unavailable, with its raw signed difference '
             'shown: adaptive JIT/startup variance defeats that estimate. It is never accepted '
             'as a run cost. Nonpositive primary peer deltas still refuse the report.', '',
             '| Program | Engine | Pair | N | Instructions/run (M) | Peak RSS N / 2N (MiB) | Load1 N / 2N |',
             '|---|---|---:|---:|---:|---:|---:|']
    for (program, engine, pair), rows in groups.items():
        n, twice = repetition_counts(program, True)
        low, high = rows[n], rows[twice]
        delta = high['instructions'] - low['instructions']
        if delta <= 0:
            if engine != 'node':
                raise ValueError(f'nonpositive warm instruction delta: {program}/{engine}/{pair}')
            estimate = f'unavailable (Δinstructions={delta:+d})'
        else:
            estimate = f'{delta / n / 1e6:.6f}'
        lines.append(f'| {program} | {engine} | {pair} | {n} | {estimate} | '
                     f"{low['peak_rss_kib'] / 1024:.2f} / {high['peak_rss_kib'] / 1024:.2f} | "
                     f"{low['load1']:.2f} / {high['load1']:.2f} |")
    path.write_text('\n'.join(lines) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fetch-engines', action='store_true')
    parser.add_argument('--memory', choices=['on', 'off'], default='on',
                        help='kernel memory meter; a metered run past its exact recount is refused')
    parser.add_argument('--memory-witness', action='store_true',
                        help='diagnostic accounting counters; do not use as a timing sample')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--report', type=Path, default=ROOT / 'docs/perf/kernel-vs-embedded.md')
    parser.add_argument('--charge', choices=['exact', 'off'], default='exact')
    parser.add_argument('--kernel', help='reuse an already optimized Kiln artifact')
    parser.add_argument('--differential-warm', action='store_true',
                        help='estimate prepared run cost from paired N/2N repetitions')
    parser.add_argument('--program', action='append', help='exact program; repeat for a partial diagnostic report')
    parser.add_argument('--timeout', type=float, default=1800)
    parser.add_argument('--render', type=Path, help='render an existing complete receipt without running anything')
    args = parser.parse_args()
    os.environ['KERNEL_PERF_MEMORY'] = args.memory
    if args.memory_witness:
        os.environ['KERNEL_PERF_MEMORY_WITNESS'] = '1'
    else:
        os.environ.pop('KERNEL_PERF_MEMORY_WITNESS', None)
    if args.render:
        render(json.loads(args.render.read_text()), args.report)
        return
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / 'receipt.json').exists() or list(out.glob('*.stdout')):
        raise ValueError('output directory already contains samples; use a fresh directory')
    if args.timeout <= 0:
        raise ValueError('timeout must be positive')
    tools = engine_tools(args.fetch_engines)
    tools['lash'] = args.kernel or build(out)
    tools['node'] = subprocess.check_output(['which', 'node'], text=True).strip()
    tools['cpython'] = subprocess.check_output(['which', 'python3'], text=True).strip()
    programs = sorted(path.stem for path in PROGRAMS.glob('*.ts'))
    if set(programs) != {path.stem for path in PROGRAMS.glob('*.py')}:
        raise ValueError('TS/Python program sets differ')
    if args.program:
        if len(set(args.program)) != len(args.program) or not set(args.program) <= set(programs):
            raise ValueError('unknown or duplicate program selection')
        programs = args.program
    versions = {name: data[3] for name, data in RELEASES.items()}
    versions['node'] = subprocess.check_output([tools['node'], '--version'], text=True).strip()
    versions['cpython'] = subprocess.check_output([tools['cpython'], '--version'], text=True).strip()
    versions['lash'] = 'Kiln optimized; compiler/profile in each Lash receipt'
    cpu = next(line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
               if line.startswith('model name'))
    receipt = dict(charge=args.charge, host=platform.node(), cpu=cpu, kernel=platform.release(), versions=versions,
                   revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                   programs=programs, recorded_at=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                   executable_sha256={name: sha(Path(path).read_bytes()) for name, path in tools.items()},
                   release_archives={name: {'url': data[0], 'sha256': data[1]} for name, data in RELEASES.items()},
                   driver_sha256=sha(Path(__file__).read_bytes()), sources={}, samples=[])
    receipt['memory_meter'] = args.memory
    receipt['memory_witness'] = args.memory_witness
    if args.differential_warm:
        receipt['differential_warm'] = True
    for program, repetitions in (
        (program, repetitions) for program in programs
        for repetitions in repetition_counts(program, args.differential_warm)
    ):
        sources = {}
        lash_sources = {}
        for suffix in ['ts', 'py']:
            source = (PROGRAMS / f'{program}.{suffix}').read_text()
            receipt['sources'][f'{program}.{suffix}'] = sha(source.encode())
            stem = f'{program}-n{repetitions}' if args.differential_warm else program
            if args.differential_warm:
                source = repeated_source(source, suffix, repetitions)
                bare = out / f'{stem}-lash.{suffix}'
                bare.write_text(source)
                receipt['sources'][bare.name] = sha(source.encode())
                lash_sources[suffix] = bare
            else:
                lash_sources[suffix] = PROGRAMS / f'{program}.{suffix}'
            path = out / f'{stem}.{suffix}'
            path.write_text(source + ('\nconsole.log(JSON.stringify(result));\n' if suffix == 'ts' else '\nprint(result)\n'))
            sources[suffix] = path
        commands = {
            'lash-ts': [tools['lash'], 'embedded', str(lash_sources['ts'])],
            'quickjs': [tools['quickjs'], str(sources['ts'])],
            'lash-py': [tools['lash'], 'embedded', str(lash_sources['py'])],
            'monty': [tools['monty'], str(sources['py'])],
            'node': [tools['node'], '--max-old-space-size=512', '--disable-wasm-trap-handler', str(sources['ts'])],
            'cpython': [tools['cpython'], '-u', str(sources['py'])],
        }
        # .ts sources deliberately use the JS subset, without annotations.
        # Node 25 accepts .ts directly; QuickJS ignores the file extension.
        expected = None
        for pair, order in enumerate([
            ['lash-ts', 'quickjs', 'lash-py', 'monty', 'node', 'cpython'],
            ['quickjs', 'lash-ts', 'monty', 'lash-py', 'cpython', 'node'],
        ], 1):
            for engine in order:
                prefix = out / f'{stem}-{pair}-{engine}'
                sample, output = measure(commands[engine], prefix, args.timeout, args.charge)
                result, inner = parse_result(engine, output)
                actual = digest(result)
                failure = None
                if engine.startswith('lash') and inner.get('outcome') != 'ok':
                    failure = str(result.get('error', 'non-ok kernel outcome'))
                elif expected is not None and actual != expected:
                    failure = f'result digest mismatch {actual} != {expected}'
                else:
                    expected = actual
                if failure:
                    sample['failure'] = failure
                sample.update(program=program, engine=engine, pair=pair,
                              result=result, result_sha256=actual, lash=inner)
                if args.differential_warm:
                    sample['repetitions'] = repetitions
                receipt['samples'].append(sample)
                with (out / 'samples.jsonl').open('a') as log:
                    log.write(json.dumps(sample, sort_keys=True) + '\n')
                print(f'{program} pair={pair} {engine}: {sample["instructions"]} instructions; digest={actual}', flush=True)
    (out / 'receipt.json').write_text(json.dumps(receipt, indent=2, sort_keys=True) + '\n')
    failures = [row for row in receipt['samples'] if row.get('failure')]
    if failures:
        lines = ['# Kernel comparison failed', '',
                 'Failed samples are excluded from performance averages.', '',
                 '| Program | Engine | Pair | Repetitions | Outcome |',
                 '|---|---|---:|---:|---|']
        for row in receipt['samples']:
            status = 'FAILED: ' + row['failure'] if row.get('failure') else 'ok'
            lines.append(f"| {row['program']} | {row['engine']} | {row['pair']} | "
                         f"{row.get('repetitions', 1)} | {status} |")
        args.report.write_text('\n'.join(lines) + '\n')
        for row in failures:
            print(f"FAIL: {row['program']}/{row['engine']}: {row['failure']}", flush=True)
        raise SystemExit(1)
    render(receipt, args.report)
    print(f'PASS: {len(receipt["samples"])} samples, {len(programs)} programs, six engines, two pairs; all digests match')


if __name__ == '__main__':
    main()
