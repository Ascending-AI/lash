"""Local perf capture with explicit success/failure receipts beside workloads."""
from __future__ import annotations

from collections import Counter
import os
from decimal import Decimal
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys


def tool(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise RuntimeError(f"required profiling tool is missing from PATH: {name}")
    return path


def checked(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    proc = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if proc.returncode:
        raise RuntimeError(f"{' '.join(cmd)} exited {proc.returncode}: {proc.stderr.strip()}")
    return proc


def blocked_stacks(text: str, window: tuple[int, int] | None = None) -> tuple[Counter, int]:
    """Pair a sleeping sched_switch stack with that TID's next SWITCH IN.

    Per-task context-switch records supply switch-ins even when the task is not
    running; this avoids system-wide sampling and other processes' stacks.
    Runnable/preempted tasks are excluded. Counts are nanoseconds until resumed
    (including wakeup-to-run latency). Incomplete intervals are counted, omitted.
    """
    header = re.compile(r"^\s*(.*?)\s+(\d+)/\s*(\d+)\s+(\d+\.\d+):\s+(.*)$")
    frame = re.compile(r"^\s+[0-9a-f]+\s+(.+?)\s+\([^)]*\)\s*$")
    sleeping = {}
    totals = Counter()
    pending = None

    def finish():
        nonlocal pending
        if pending:
            tid, stamp, comm, frames = pending
            sleeping[tid] = (stamp, ";".join([comm, *reversed(frames or ["[unknown]"])]))
            pending = None

    for line in text.splitlines():
        if "PERF_RECORD_LOST" in line:
            raise RuntimeError("lost scheduler records; blocked-time profile is incomplete")
        match = header.match(line)
        if match:
            finish()
            comm, _, tid, seconds, event = match.groups()
            stamp = int(Decimal(seconds) * 1_000_000_000)
            if "PERF_RECORD_SWITCH" in event and re.search(r"\bIN\b", event):
                if tid in sleeping:
                    start, stack = sleeping.pop(tid)
                    if stamp < start:
                        raise RuntimeError("off-CPU timestamps are out of order")
                    elapsed = stamp-start if window is None else max(0, min(stamp,window[1])-max(start,window[0]))
                    if elapsed:
                        totals[stack] += elapsed
            elif "sched:sched_switch:" in event:
                state = re.search(r"prev_state=(\S+)", event)
                prev = re.search(r"prev_pid=(\d+)", event)
                if not state or not prev:
                    pretty = re.search(r":(\d+)\s+\[\d+\]\s+(\S+)\s+==>", event)
                    if not pretty:
                        raise RuntimeError(f"unrecognized sched_switch record: {line}")
                    prev = (None, pretty[1])
                    state = (None, pretty[2])
                if state[1] not in ("R", "R+", "0"):
                    pending = (prev[1], stamp, comm.strip(), [])
                else:
                    sleeping.pop(prev[1], None)
        elif pending and line.strip() and re.search(r"[^ ]+:\d+", line.strip()) and not frame.match(line):
            if pending[3]:
                pending[3][-1] += " @ " + line.strip().replace(";", ":")
        elif pending and (match := frame.match(line)):
            pending[3].append(match[1].replace(";", ":"))
    finish()
    return totals, len(sleeping)


def resolve_blocked_sources(raw: str, status: dict) -> str:
    """Translate perf's file offsets through ELF LOAD segments, then symbolize in batches."""
    frame = re.compile(r"^(\s+[0-9a-f]+\s+.+?)\s+\((.+)\+0x([0-9a-f]+)\)\s*$")
    requests = {}
    for line in raw.splitlines():
        if match := frame.match(line):
            requests.setdefault(match[2], set()).add(int(match[3], 16))
    resolved = {}
    counts = {"resolved_frames": 0, "unmapped_frames": 0}
    for name, offsets in requests.items():
        path = Path(name)
        if not path.is_file():
            continue
        with path.open('rb') as source:
            header = source.read(64)
            if header[:6] != b'\x7fELF\x02\x01':
                continue
            phoff = struct.unpack_from('<Q', header, 32)[0]
            size, number = struct.unpack_from('<HH', header, 54)
            segments = []
            for index in range(number):
                source.seek(phoff + index*size)
                program = source.read(size)
                if struct.unpack_from('<I', program)[0] == 1:
                    offset, address = struct.unpack_from('<QQ', program, 8)
                    length = struct.unpack_from('<Q', program, 32)[0]
                    segments.append((offset, address, length))
        addresses = {}
        for offset in sorted(offsets):
            for start, address, length in segments:
                if start <= offset < start+length:
                    addresses[offset] = address + offset-start
                    break
        if not addresses:
            continue
        result = checked([tool('llvm-symbolizer'), '--output-style=JSON', '--obj='+str(path)],
                         input=''.join(f'0x{address:x}\n' for address in addresses.values())).stdout
        symbols = [json.loads(line) for line in result.splitlines() if line.strip()]
        if len(symbols) != len(addresses):
            raise RuntimeError('LLVM symbolizer returned an incomplete address batch')
        for offset, record in zip(addresses, symbols):
            sites = [f"{symbol['FileName']}:{symbol['Line']}:{symbol['Column']}" for symbol in record['Symbol']
                     if symbol.get('FileName') and symbol.get('Line')]
            resolved[(name, offset)] = list(dict.fromkeys(sites))
    lines = []
    for line in raw.splitlines():
        if match := frame.match(line):
            sites = resolved.get((match[2], int(match[3], 16)), [])
            lines.append(f'{match[1]} ({match[2]})')
            if sites:
                lines.append('  ' + ' | '.join(sites))
                counts['resolved_frames'] += 1
            else:
                counts['unmapped_frames'] += 1
        else:
            lines.append(line)
    status['source_resolution'] = counts
    return '\n'.join(lines)+'\n'


def write_blocked(directory: Path, text: str, status: dict) -> None:
    marks = directory / "turn-windows.jsonl"
    totals, incomplete = blocked_stacks(text)
    if not totals:
        raise RuntimeError("no complete blocked intervals; off-CPU capture cannot certify blocked time")
    (directory / "off-cpu.folded").write_text("".join(
        f"{stack} {value}\n" for stack, value in sorted(totals.items())))
    (directory / "off-cpu.top.txt").write_text("blocked_ns stack (top 30)\n" + "".join(
        f"{value} {stack}\n" for stack, value in totals.most_common(30)))
    status["incomplete_intervals_omitted"] = incomplete
    if marks.exists():
        boundaries = [json.loads(line) for line in marks.read_text().splitlines()]
        windows = []
        start = None
        for boundary in boundaries:
            if boundary["command"] == "enable":
                if start is not None:
                    raise RuntimeError("overlapping off-CPU turn windows")
                start = boundary["monotonic_ns"]
            elif boundary["command"] == "disable" and start is not None:
                windows.append((start,boundary["monotonic_ns"]))
                start = None
            else:
                raise RuntimeError("unpaired off-CPU turn window")
        if start is not None:
            raise RuntimeError("unfinished off-CPU turn window")
        status["turn_windows_monotonic_ns"] = windows
        for index, window in enumerate(windows):
            selected, _ = blocked_stacks(text, window)
            (directory / f"turn-{index+1}.off-cpu.top.txt").write_text(
                "blocked_ns stack; clipped to this turn; overlapping thread waits\n" + "".join(
                    f"{value} {stack}\n" for stack,value in selected.most_common(30)))


def run_profiled(cmd: list[str], *, receipt: Path, cpu: bool = False,
                 off_cpu: bool = False, population: str | None = None, kernel: bool = False,
                 **kwargs) -> subprocess.CompletedProcess:
    """Run once; capture both populations in the same perf.data when requested.

    `kernel` also samples the workload's kernel cycles (its syscalls and
    faults), which `perf_event_paranoid` <= 1 permits for its own processes.
    """
    if not cpu and not off_cpu:
        return subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    profile_path = receipt.with_suffix(".profiles")
    if population:
        profile_path = profile_path.with_name(f"{receipt.stem}-{population}.profiles")
    directory = profile_path.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    status_path = directory / "capture.json"
    status = {"status": "failed", "cpu": cpu, "off_cpu": off_cpu, "kernel": kernel, "workload": cmd,
              "workload_receipt": str(receipt.resolve())}
    # Never reuse old successful artifacts after a failed capture.
    for name in ("perf.data", "perf.script", "cpu.folded", "cpu.top.txt",
                 "off-cpu.folded", "off-cpu.top.txt"):
        (directory / name).unlink(missing_ok=True)
    for path in directory.glob("turn-*.off-cpu.top.txt"):
        path.unlink()
    status_path.write_text(json.dumps(status, indent=2) + "\n")
    try:
        perf = tool("perf")
        demangler = tool("c++filt")
        if cpu:
            collapse = tool("inferno-collapse-perf")
        record = [perf, "record", "-g", "--call-graph", "fp", "--no-buildid-cache", "-c", "1",
                  "-o", str(directory / "perf.data")]
        if cpu:
            record += ["-e", "cycles/name=cycles,freq=999/" + ("" if kernel else "u")]
        if off_cpu:
            record += ["-e", "sched:sched_switch", "--switch-events", "--clockid", "mono"]
            status["source_resolver"] = tool("llvm-symbolizer")
            marks = directory / "turn-windows.jsonl"
            marks.unlink(missing_ok=True)
            kwargs["env"] = dict(kwargs.get("env", os.environ), LASH_OFF_CPU_MARKS=str(marks))
        proc = subprocess.run([*record, "--", *cmd], capture_output=True, text=True, **kwargs)
        (directory / "record.stderr.txt").write_text(proc.stderr)
        if proc.returncode:
            raise RuntimeError(f"perf record/workload exited {proc.returncode}: {proc.stderr.strip()}")
        script = [perf, "script", "--ns", "--demangle", "-i", str(directory / "perf.data")]
        script += ["-F", "comm,pid,tid,time,event,ip,sym,dso" + (",dsoff,trace" if off_cpu else "")]
        if off_cpu:
            script += ["--show-switch-events", "--show-lost-events"]
        raw = checked(script, cwd=kwargs.get("cwd"), env=kwargs.get("env")).stdout
        if off_cpu:
            raw = resolve_blocked_sources(raw, status)
        text = checked([demangler, "-s", "rust"], input=raw).stdout
        (directory / "perf.script").write_text(text)
        if cpu:
            folded = checked([collapse, "--event-filter", "cycles",
                              str(directory / "perf.script")]).stdout
            if not folded.strip():
                raise RuntimeError("CPU capture contains no folded samples; increase workload size")
            (directory / "cpu.folded").write_text(folded)
            report = checked([perf, "report", "--stdio", "--no-children", "--demangle",
                              "--sort", "dso,symbol", "--percent-limit", "0",
                              "-g", "none", "-i", str(directory / "perf.data")]).stdout
            report = checked([demangler, "-s", "rust"], input=report).stdout
            # Keep perf's headers and the first 30 rows of the CPU event table.
            rows, count = [], 0
            for line in report.splitlines():
                if "sched:sched_switch" in line:
                    break
                if re.match(r"\s*\d+\.\d+%", line):
                    count += 1
                if count <= 30:
                    rows.append(re.sub(r"\s{2,}", "  ", line.rstrip()))
            (directory / "cpu.top.txt").write_text("\n".join(rows) + "\n")
        if off_cpu:
            write_blocked(directory, text, status)
        status["status"] = "ok"
        status["units"] = {"cpu": "samples", "off_cpu": "blocked nanoseconds"}
        return proc
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        status["reason"] = str(error)
        if off_cpu:
            status["privilege_note"] = (
                "sched:sched_switch needs readable tracefs and tracepoint perf permissions; "
                "perf_event_paranoid=1 alone may not permit it. No privilege escalation attempted.")
        raise SystemExit(f"error: profiling failed ({status_path}): {error}") from error
    finally:
        status_path.write_text(json.dumps(status, indent=2) + "\n")
        print(f"Profile capture receipt: {status_path}", file=sys.stderr)


def instruction_window(cmd: list[str], directory: Path, *, env: dict, cwd: Path) -> dict:
    """Capture an acknowledged instructions:u window, with raw perf/time evidence.

    The producer must send enable/disable and retain acknowledgements in its
    own receipt. GNU time's RSS covers the whole producer process, not its window.
    """
    import os
    import resource
    import signal

    directory.mkdir(parents=True, exist_ok=False)
    control, ack = directory / "control.fifo", directory / "ack.fifo"
    os.mkfifo(control)
    os.mkfifo(ack)
    environment = dict(env, LASH_PERF_CONTROL=str(control), LASH_PERF_ACK=str(ack))
    status = {"status": "failed", "event": "instructions:u", "command": cmd,
              "load1_before": os.getloadavg()[0]}
    status_path = directory / "instructions.json"
    status_path.write_text(json.dumps(status, indent=2) + "\n")

    def cap():
        ceiling = 8 * 1024**3
        _, hard = resource.getrlimit(resource.RLIMIT_AS)
        if hard != resource.RLIM_INFINITY:
            ceiling = min(ceiling, hard)
        resource.setrlimit(resource.RLIMIT_AS, (ceiling, ceiling))

    try:
        command = [tool("perf"), "stat", "-e", "instructions:u", "--delay=-1",
                   f"--control=fifo:{control},{ack}", "-x", ";", "-o",
                   str(directory / "perf-stat.txt"), "--", "/usr/bin/time", "-v", "-o",
                   str(directory / "resources.txt"), *cmd]
        proc = subprocess.Popen(command, cwd=cwd, env=environment, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                start_new_session=True, preexec_fn=cap)
        try:
            stdout, stderr = proc.communicate(timeout=300)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            raise RuntimeError("instruction capture exceeded 300 seconds")
        (directory / "stdout.txt").write_text(stdout)
        (directory / "stderr.txt").write_text(stderr)
        if proc.returncode:
            raise RuntimeError(f"perf/workload exited {proc.returncode}: {stderr.strip()}")
        raw = (directory / "perf-stat.txt").read_text()
        rows = [line.split(";") for line in raw.splitlines()
                if ";instructions:u;" in line]
        if len(rows) != 1:
            raise ValueError("expected exactly one instructions:u event")
        row = rows[0]
        instructions, runtime, fraction = int(row[0]), float(row[3]), float(row[4]) / 100
        if instructions <= 0 or runtime <= 0 or not 0 < fraction <= 1:
            raise ValueError("unsupported or empty instruction population")
        resources = (directory / "resources.txt").read_text()
        rss = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", resources)
        if not rss:
            raise ValueError("missing GNU time peak RSS")
        status.update(status="ok", instructions=instructions, event_runtime_ns=runtime,
                      running_fraction=fraction, peak_rss_kib=int(rss[1]))
        return status
    except (OSError, ValueError, RuntimeError) as error:
        status["reason"] = str(error)
        raise
    finally:
        status["load1_after"] = os.getloadavg()[0]
        status_path.write_text(json.dumps(status, indent=2) + "\n")
        control.unlink(missing_ok=True)
        ack.unlink(missing_ok=True)


def reprocess_off_cpu(directory: Path) -> None:
    """Resolve and summarize already-recorded scheduler data without running work."""
    directory = directory.resolve()
    path = directory / 'capture.json'
    status = json.loads(path.read_text())
    if not status.get('off_cpu'):
        raise SystemExit('reprocessing requires an off-CPU capture')
    status.update(status='failed', reprocessed_without_rerunning_workload=True)
    status.pop('reason', None)
    for output in ['off-cpu.folded','off-cpu.top.txt']:
        (directory/output).unlink(missing_ok=True)
    for output in directory.glob('turn-*.off-cpu.top.txt'):
        output.unlink()
    path.write_text(json.dumps(status,indent=2)+'\n')
    try:
        status['source_resolver'] = tool('llvm-symbolizer')
        raw = checked([tool('perf'),'script','--ns','--demangle','-i',str(directory/'perf.data'),
            '-F','comm,pid,tid,time,event,ip,sym,dso,dsoff,trace',
            '--show-switch-events','--show-lost-events'],env=dict(os.environ)).stdout
        raw = resolve_blocked_sources(raw, status)
        text = checked([tool('c++filt'),'-s','rust'],input=raw).stdout
        (directory/'perf.script').write_text(text)
        write_blocked(directory,text,status)
        status.update(status='ok',units={'off_cpu':'blocked nanoseconds'})
    except (OSError,RuntimeError) as error:
        status['reason'] = str(error)
        raise
    finally:
        path.write_text(json.dumps(status,indent=2)+'\n')
    print(f'Profile capture receipt: {path}')
