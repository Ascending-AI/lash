#!/usr/bin/env python3
"""Read host-observed effect critical paths; run matched SQLite observer controls."""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess
import uuid


def require(condition, message):
    if not condition:
        raise ValueError(message)


def worker(event):
    return event["pid"], event["process_epoch"]


def lease(event):
    return (*worker(event), event["lease"], event["owner_epoch"], event["frame_epoch"])


def read(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines()]
    require(bool(rows), "missing phase header")
    return analyze(rows[0], rows[1:])


def analyze(meta, events):
    """EFFECT-PHASE-IDENTITY/CONSERVATION: incomplete populations refuse claims."""
    for key in ("identity", "experiment", "observer", "backend", "host_pid", "pool_epoch", "clock",
                "terminal", "script_digest", "host_artifact_sha256", "worker_artifact_sha256"):
        require(meta.get(key), f"missing {key}")
    for key in ("commit", "build_profile", "allocator", "host", "started_at"):
        require(meta["identity"].get(key), f"missing experiment identity: {key}")
    require(meta["observer"] == "bounded host-clock effect phases", "phase timestamps disabled")
    geometry = json.loads(meta["experiment"]["configured_geometry"])
    require(meta["experiment"]["backend"] == meta["backend"], "backend identity mismatch")
    require(geometry["worker"] == meta["worker_artifact_sha256"], "worker artifact identity mismatch")
    require(geometry["effect_phases"] == meta["observer"], "observer identity mismatch")
    require(meta["kind"] == "lash.turn-effect-phases", "unknown phase producer")
    require(meta["terminal"] == "all selected turns successful; scripted calls matched",
            "unsuccessful or missing terminal")
    require(meta.get("dropped") == 0, "phase overflow; timings unknown")
    require(not any(e["phase"] == "loss" for e in events), "lost worker; timings incomplete")
    totals = Counter()
    identities = {}
    for event in events:
        require(event.get("pid") and event.get("process_epoch"), "missing PID/epoch")
        require(0 <= event["start_ns"] <= event["end_ns"], "invalid clock interval")
        if event.get("owner") and event.get("document"):
            identities[lease(event)] = event["owner"], event["document"]
        if event["phase"] in ("send", "receive"):
            require(event.get("kind") != "undecodable", "undecodable frame")
            totals[event["direction"] + "_messages"] += 1
            totals[event["direction"] + "_bytes"] += event["bytes"]
    counters = meta["after"]["counters"]
    for direction in ("sent", "received"):
        for unit in ("messages", "bytes"):
            name = direction + "_" + unit
            require(totals[name] == counters["ipc_" + name], f"frame residual: {name}")
    # Every retained turn has an explicit event cut and a successful terminal.
    turn_by_event = {}
    for turn in meta["turns"]:
        require(turn.get("terminal") == "all session outcomes successful; scripted calls matched"
                and turn.get("sessions", 0) > 0, "missing turn terminal/population")
        first, end = turn["first_event"], turn["through_event"]
        require(type(first) is int and 0 <= first < end <= len(events), "missing turn window")
        for index in range(first, end):
            require(index not in turn_by_event, "overlapping turn cuts")
            turn_by_event[index] = turn["turn"]
    indexed = defaultdict(list)
    for index, event in enumerate(events):
        if event.get("effect") is not None:
            require(lease(event) in identities, "missing effect owner/document")
            require(index in turn_by_event, "missing effect turn window")
            indexed[(*lease(event), event["effect"])].append((index, event))
    lines = [f"Effect phases: backend={meta['backend']} observer={meta['observer']}",
             f"identity={json.dumps(meta['identity'], sort_keys=True)}",
             f"replay_experiment={json.dumps(meta['experiment'], sort_keys=True)}",
             f"host={meta['host_artifact_sha256']} worker={meta['worker_artifact_sha256']}",
             f"script_digest={meta['script_digest']}; terminal={meta['terminal']}",
             f"host_pid={meta['host_pid']} pool_epoch={meta['pool_epoch']}; clock={meta['clock']}",
             "Units: ns; one observation per host-read effect. No worker-clock subtraction.",
             "Receive spans include waiting/decoding; progress is receipt time, not worker CPU.",
             "Startup/checkout/document/receive intervals nest: never sum them as elapsed.",
             f"Frame conservation: {sum(totals[d + '_messages'] for d in ('sent', 'received'))} frames, "
             f"{sum(totals[d + '_bytes'] for d in ('sent', 'received'))} bytes; residual=0."]
    complete = 0
    for key, population in indexed.items():
        group = [e for _, e in population]
        requests = [e for e in group if e["phase"] == "receive" and e["kind"] == "host_read"]
        bodies = [e for e in group if e["phase"] == "host_body"]
        answers = [e for e in group if e["phase"] == "send" and e["kind"] == "host_answer"]
        resumes = [e for e in group if e["phase"] == "resume_observed"]
        answer_points = [e for e in group if e["phase"] == "answer"]
        encodes = [e for e in group if e["phase"] == "encode"]
        require(all(len(p) == 1 for p in (requests, bodies, answers, resumes, answer_points, encodes)),
                f"incomplete/duplicate endpoints for effect {key}")
        request, body, answer, resume = requests[0], bodies[0], answers[0], resumes[0]
        require(request["end_ns"] <= body["start_ns"] <= body["end_ns"]
                <= answer_points[0]["start_ns"] <= answer_points[0]["end_ns"]
                <= encodes[0]["start_ns"] <= encodes[0]["end_ns"] <= answer["start_ns"]
                <= answer["end_ns"] <= resume["end_ns"], "effect endpoint order")
        total = resume["end_ns"] - request["end_ns"]
        body_ns = body["end_ns"] - body["start_ns"]
        send_ns = answer["end_ns"] - answer["start_ns"]
        encode_ns = encodes[0]["end_ns"] - encodes[0]["start_ns"]
        residual = total - body_ns - encode_ns - send_ns
        require(residual >= 0, "overlapping effect phase intervals")
        owner, document = identities[lease(request)]
        checkout = [e for e in events if lease(e) == lease(request) and e["phase"] == "checkout"]
        require(len(checkout) == 1, "missing effect lease checkout")
        checkout_ns = checkout[0]["end_ns"] - checkout[0]["start_ns"]
        lines.append(f"turn={turn_by_event[population[0][0]]} lease={key[2:5]} worker={key[:2]} "
                     f"owner={owner} document={document} effect={key[-1]} "
                     f"lease_checkout_ns={checkout_ns} (shared by the lease; excluded from effect turnaround) "
                     f"critical_path_ns={total}: "
                     f"host_body_including_value_codec={body_ns} answer_frame_encode={encode_ns} "
                     f"answer_send={send_ns} dispatch_codec_resume_residual={residual}; "
                     f"frames={sum(e['phase'] in ('send', 'receive') for e in group)} "
                     f"bytes={sum(e.get('bytes') or 0 for e in group)} (subset of reconciled totals)")
        complete += 1
    require(complete > 0, "no complete host-read effects")
    documents = defaultdict(list)
    for index, event in enumerate(events):
        if event["phase"] == "document_start":
            require(lease(event) in identities, "missing document identity")
            owner, document = identities[lease(event)]
            require(index in turn_by_event, "missing document turn window")
            turn = turn_by_event[index]
            # The owner is a session frame. A new turn can execute the same
            # document under that owner again; redrives within a turn stay one.
            if any((row["turn"], row["owner"], row["document"]) == (turn, owner, document)
                   for row in documents[worker(event)]):
                continue
            stops = [e for e in events[index + 1:] if lease(e) == lease(event)
                     and e["phase"] in ("document_park", "document_end")]
            require(bool(stops), "document has no observed first stop")
            stop = stops[0]
            checkout = [e for e in events[:index] if lease(e) == lease(event) and e["phase"] == "checkout"]
            require(len(checkout) == 1, "missing checkout")
            phase_totals = Counter()
            frame_totals = Counter()
            for row in events:
                if (lease(row) == lease(event) and row["start_ns"] >= checkout[0]["end_ns"]
                        and row["end_ns"] <= stop["end_ns"]):
                    if row["phase"] in ("encode", "encode_run", "send", "receive", "host_body"):
                        phase_totals[row["phase"]] += row["end_ns"] - row["start_ns"]
                    if row["phase"] in ("send", "receive"):
                        frame_totals["frames"] += 1
                        frame_totals["bytes"] += row["bytes"]
            documents[worker(event)].append({"owner": owner, "document": document,
                "turn": turn, "first_stop": stop["phase"],
                "phase_totals_ns": dict(phase_totals), "frame_totals": dict(frame_totals),
                "checkout_ns": checkout[0]["end_ns"] - checkout[0]["start_ns"],
                "first_activation_ns": stop["end_ns"] - event["start_ns"],
                "prepare_and_dispatch_ns": event["start_ns"] - checkout[0]["end_ns"]})
    compared = 0
    for subject, docs in documents.items():
        if len(docs) >= 2:
            startup = [e for e in events if worker(e) == subject and e["phase"] == "spawn_to_ready"]
            require(len(startup) == 1, "missing worker startup window")
            lines.append(f"Same-worker first/second document: worker={subject} "
                         f"spawn_to_ready_ns={startup[0]['end_ns'] - startup[0]['start_ns']} "
                         f"documents={json.dumps(docs[:2], sort_keys=True)}")
            compared += 1
    lines.append(f"Population: complete_effects={complete} same_worker_pairs={compared}; "
                 "document windows end at first park/end, not whole-turn elapsed. Diagnostic, no timing gate.")
    require(compared > 0, "no first/second document on the same worker")
    return "\n".join(lines) + "\n"


def compare_pair(paired):
    for field in ("useful_work", "backend", "script_digest", "result_digests"):
        require(paired[0][field] == paired[1][field], f"on/off {field} differs")
    for field in ("commit", "dirty", "build_profile", "allocator", "host", "cpus"):
        require(paired[0]["identity"][field] == paired[1]["identity"][field],
                f"on/off identity differs: {field}")
    experiments = [sample["experiment"] for sample in paired]
    for field in ("host", "compiler", "allocator", "build_profile", "backend",
                  "configured_durability", "workload", "postgres_settings"):
        require(experiments[0][field] == experiments[1][field], f"on/off experiment differs: {field}")
    geometry = [json.loads(experiment["configured_geometry"]) for experiment in experiments]
    for sample, configured in zip(paired, geometry):
        require(configured.pop("effect_phases") == sample["observer"], "observer identity mismatch")
    require(geometry[0] == geometry[1], "on/off experiment geometry differs")


def cap_memory():
    resource.setrlimit(resource.RLIMIT_AS, (8 * 1024**3, 8 * 1024**3))


def instructions(path):
    for line in path.read_text().splitlines():
        fields = line.split(";")
        if len(fields) > 2 and fields[2] == "instructions:u":
            return {"instructions_u": int(fields[0]), "perf_fields": fields[3:]}
    raise ValueError("perf did not count instructions:u")


def capture(args):
    from perf_artifacts import boundary_artifacts, runtime_label
    root = Path(__file__).resolve().parents[1]
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binary, helper = boundary_artifacts(root, runtime_label([]), build=not args.no_build,
        report=args.build_report, optimized=True)
    source = {"format": "lash-perf-replay/1", "name": "effect-phase-pair",
        "description": "Two equal documents with one clock host read each on a local SQLite session.",
        "protocol": "typescript", "sessions": 1,
        "turns": [{"repeat": 2, "user": "Read the clock, return its type.", "replies": [{"cell": [
            'const stamp = Date.now();', 'await control.finish({ kind: typeof stamp });']}]}]}
    workload = out / "workload.json"
    workload.write_text(json.dumps(source, indent=2) + "\n")
    experiment = {"id": str(uuid.uuid4()), "backend": "sqlite-file", "window": "two successful turns per process; startup included in instructions",
                  "host_sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
                  "worker_sha256": hashlib.file_digest(helper.open("rb"), "sha256").hexdigest(),
                  "workload_sha256": hashlib.sha256(workload.read_bytes()).hexdigest(),
                  "observer_modes": ["off", "bounded host-clock effect phases"], "samples": []}
    env = dict(os.environ, LASH_VM_WORKER=str(helper))
    for pair in range(args.pairs):
        for mode in (("off", "on") if pair % 2 == 0 else ("on", "off")):
            run = out / f"pair-{pair}-{mode}"
            run.mkdir()
            scratch = run / "tmp"
            scratch.mkdir()
            sample_env = dict(env, TMPDIR=str(scratch))
            cmd = [str(binary), "replay", str(workload), "--store-dir", str(run / "stores"),
                   "--out", str(run / "replay.json")]
            phases = run / "turn-effect-phases.jsonl"
            cmd += ["--effect-phases", str(phases)]
            if mode == "off":
                cmd += ["--effect-phases-off"]
            load = os.getloadavg()[0]
            with (run / "run.log").open("w") as log:
                subprocess.run(["/usr/bin/time", "-v", "-o", str(run / "resources.txt"),
                    "perf", "stat", "--no-big-num", "-x", ";", "-e", "instructions:u", "-o",
                    str(run / "perf.csv"), "--", *cmd], cwd=root, env=sample_env,
                    stdout=log, stderr=subprocess.STDOUT, preexec_fn=cap_memory, check=True)
            replay = json.loads((run / "replay.json").read_text())
            receipt = replay["workloads"][0]
            header = json.loads(phases.open().readline())
            require(header["host_artifact_sha256"] == experiment["host_sha256"] and
                    header["worker_artifact_sha256"] == experiment["worker_sha256"], "artifact mismatch")
            subjects = header["after"]["executions"]
            require(subjects and all(s.get("pid") and s.get("process_epoch") for s in subjects),
                    "missing actual worker population")
            require(header["after"]["counters"]["receipts_dropped"] == 0, "worker receipt overflow")
            useful = {key: [r["metrics"][key] for r in receipt["rounds"]]
                      for key in ("model_calls", "worker_executions", "ipc_frames")}
            rss_line = next(line for line in (run / "resources.txt").read_text().splitlines()
                            if "Maximum resident set size" in line)
            sample = {"pair": pair, "mode": mode, "store_dir": str(run / "stores"),
                      "temporary_directory": str(scratch), "load1": load, "peak_rss_kib": int(rss_line.split(":")[-1]),
                      "useful_work": useful, "wire_bytes": [r["metrics"]["ipc_bytes"] for r in receipt["rounds"]],
                      "result_digests": [turn["result_digests"] for turn in header["turns"]],
                      "identity": header["identity"], "experiment": header["experiment"], "workers": subjects,
                      "backend": header["backend"], "script_digest": header["script_digest"],
                      "observer": header["observer"], "turns": header["turns"], "terminal": "replay succeeded; all scripted calls matched",
                      **instructions(run / "perf.csv")}
            experiment["samples"].append(sample)
            (out / "observer-overhead.json").write_text(json.dumps(experiment, indent=2) + "\n")
            if mode == "on":
                text = read(phases)
                (run / "critical-path.txt").write_text(text)
                if pair == 0:
                    print(text, end="")
        paired = [s for s in experiment["samples"] if s["pair"] == pair]
        compare_pair(paired)
    text = overhead(experiment)
    (out / "observer-overhead.txt").write_text(text)
    print(text, end="")


def overhead(experiment):
    for key in ("id", "host_sha256", "worker_sha256", "workload_sha256", "window"):
        require(experiment.get(key), f"missing overhead identity: {key}")
    samples = experiment["samples"]
    require(bool(samples), "missing overhead population")
    lines = [f"Experiment={experiment['id']} host={experiment['host_sha256']} "
             f"worker={experiment['worker_sha256']} workload={experiment['workload_sha256']}",
             f"Window={experiment['window']}",
             f"Identity={json.dumps(samples[0]['identity'], sort_keys=True)}",
             f"Replay experiment={json.dumps(samples[0]['experiment'], sort_keys=True)}"]
    ratios = []
    for pair in sorted({s["pair"] for s in samples}):
        paired = [s for s in samples if s["pair"] == pair]
        require(len(paired) == 2 and {s["mode"] for s in paired} == {"off", "on"},
                "incomplete observer pair")
        compare_pair(paired)
        for sample in paired:
            require(sample.get("result_digests") and all(sample["result_digests"]), "missing result identity")
            require(sample.get("workers") and all(w.get("pid") and w.get("process_epoch")
                    for w in sample["workers"]), "missing worker identity")
            require(sample.get("terminal") == "replay succeeded; all scripted calls matched", "missing terminal")
            require(sample["instructions_u"] > 0, "instructions unavailable")
            require(sample.get("load1") is not None and sample.get("peak_rss_kib") is not None,
                    "missing resource observation")
            lines.append(f"pair={pair} mode={sample['mode']} observer={sample['observer']} instructions:u={sample['instructions_u']} "
                         f"load1={sample['load1']} peak_rss_kib={sample['peak_rss_kib']} "
                         f"workers={[(w['pid'], w['process_epoch']) for w in sample['workers']]} "
                         f"wire_bytes_per_turn={sample['wire_bytes']}")
        modes = {s["mode"]: s for s in paired}
        ratios.append(modes["on"]["instructions_u"] / modes["off"]["instructions_u"] - 1)
    lines.append(f"Observer overhead: pairs={len(ratios)} instructions:u fractional deltas={ratios}; "
                 f"median={statistics.median(ratios):.6f}. "
                 "Instruction counts include host and inherited worker tasks, startup and export. "
                 "Resource observations are per command; RSS is not summed. "
                 "Shared-host wall time unqualified; no threshold or deployed reach claim.")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    reader = commands.add_parser("read")
    reader.add_argument("path", type=Path)
    overhead_reader = commands.add_parser("overhead")
    overhead_reader.add_argument("path", type=Path)
    run = commands.add_parser("capture")
    run.add_argument("--out", type=Path, required=True, help="New evidence directory")
    run.add_argument("--pairs", type=int, default=3)
    run.add_argument("--no-build", action="store_true")
    run.add_argument("--build-report", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "read":
            print(read(args.path), end="")
        elif args.command == "overhead":
            print(overhead(json.loads(args.path.read_text())), end="")
        else:
            require(args.pairs > 0, "pairs must be positive")
            capture(args)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"effect-phases: refused: {error}\n")


if __name__ == "__main__":
    main()
