"""Laws for identity admission and nonoverlapping effect-path arithmetic."""
import copy
import json
import unittest

from scripts.effect_phases import analyze, compare_pair


def population():
    events = []

    def add(phase, at, duration=0, **extra):
        event = dict(phase=phase, start_ns=at, end_ns=at + duration,
                     pid=7, process_epoch="123", lease=0, owner_epoch=0,
                     frame_epoch=0, owner=None, document=None, effect=None,
                     direction=None, kind=None, bytes=None)
        event.update(extra)
        events.append(event)

    add("receive", 0, 10, direction="received", kind="ready", bytes=10)
    add("spawn_to_ready", 0, 11)
    turns = []
    for ordinal in (1, 2):
        first = len(events)
        base = ordinal * 100
        identity = dict(lease=ordinal, owner_epoch=1, frame_epoch=1,
                        owner=f"cell:{ordinal}", document=f"doc:{ordinal}")
        add("checkout", base, 5, **identity)
        add("document_start", base + 10, **identity)
        effect = dict(identity, effect=0)
        add("receive", base + 15, 5, direction="received", kind="host_read", bytes=10, **effect)
        add("host_body", base + 22, 10, **effect)
        add("answer", base + 34, **effect)
        add("encode", base + 35, 2, **effect)
        add("send", base + 38, 2, direction="sent", kind="host_answer", bytes=10, **effect)
        add("receive", base + 41, 3, direction="received", kind="progress", bytes=10, **effect)
        add("resume_observed", base + 45, **effect)
        add("document_end", base + 50, **identity)
        turns.append(dict(turn=ordinal, first_event=first, through_event=len(events),
                          terminal="all session outcomes successful; scripted calls matched", sessions=1))
    meta = dict(kind="lash.turn-effect-phases", identity={"commit": "abc", "host": "host", "build_profile": "release",
                          "allocator": "system", "started_at": "now", "cpus": 1},
                experiment=dict(host="host", compiler="compiler", allocator="system", build_profile="release",
                    backend="sqlite-file", configured_durability="fixture", workload="script", postgres_settings=None,
                    configured_geometry=json.dumps({"worker": "worker", "window": "fixture turns",
                                                   "effect_phases": "bounded host-clock effect phases"})),
                observer="bounded host-clock effect phases", backend="sqlite-file", host_pid=1,
                pool_epoch=1, clock="host Instant", dropped=0, script_digest="script",
                host_artifact_sha256="host", worker_artifact_sha256="worker", turns=turns,
                terminal="all selected turns successful; scripted calls matched",
                after={"counters": dict(ipc_sent_messages=2, ipc_sent_bytes=20,
                                        ipc_received_messages=5, ipc_received_bytes=50)})
    return meta, events


class EffectPhaseLaws(unittest.TestCase):
    def test_missing_identity_window_terminal_and_loss_refuse_claims(self):
        meta, events = population()
        analyze(meta, events)
        for field in ("identity", "experiment", "observer", "backend", "worker_artifact_sha256", "terminal"):
            invalid = copy.deepcopy(meta)
            invalid[field] = None
            with self.subTest(field=field), self.assertRaises(ValueError):
                analyze(invalid, events)
        for change in (lambda m, e: m.update(dropped=1),
                       lambda m, e: e[2].update(process_epoch=None),
                       lambda m, e: m["turns"][0].update(through_event=2),
                       lambda m, e: e[-1].update(phase="loss")):
            invalid, rows = copy.deepcopy((meta, events))
            change(invalid, rows)
            with self.assertRaises(ValueError):
                analyze(invalid, rows)

    def test_a_second_document_in_the_same_session_frame_is_not_a_redrive(self):
        meta, events = population()
        for event in events:
            if event.get("owner"):
                event["owner"] = "same-session-frame"
                event["document"] = "same-document"
        self.assertIn("same_worker_pairs=1", analyze(meta, events))

    def test_observer_controls_match_results_and_keep_meter_bytes_separate(self):
        first = dict(useful_work={"model_calls": [1, 1], "worker_executions": [1, 1], "ipc_frames": [22, 24]},
                     backend="sqlite-file", script_digest="script", result_digests=[["answer"], ["answer"]],
                     wire_bytes=[10820, 12791], identity=dict(population()[0]["identity"], dirty=False),
                     observer="off; identity receipts only", experiment=population()[0]["experiment"])
        geometry = json.loads(first["experiment"]["configured_geometry"])
        geometry["effect_phases"] = first["observer"]
        first["experiment"]["configured_geometry"] = json.dumps(geometry)
        second = copy.deepcopy(first)
        second["observer"] = "bounded host-clock effect phases"
        geometry["effect_phases"] = second["observer"]
        second["experiment"]["configured_geometry"] = json.dumps(geometry)
        second["wire_bytes"] = [10813, 12789]
        compare_pair([first, second])
        second["result_digests"][0] = ["changed answer"]
        with self.assertRaisesRegex(ValueError, "result_digests differs"):
            compare_pair([first, second])

    def test_missing_codec_endpoint_is_unknown_instead_of_zero(self):
        meta, events = population()
        next(event for event in events if event["phase"] == "encode")["phase"] = "unavailable"
        with self.assertRaisesRegex(ValueError, "incomplete/duplicate endpoints"):
            analyze(meta, events)

    def test_each_wire_frame_is_counted_once_and_residual_refuses(self):
        meta, events = population()
        self.assertIn("7 frames, 70 bytes; residual=0", analyze(meta, events))
        events[0]["bytes"] += 1
        with self.assertRaisesRegex(ValueError, "frame residual"):
            analyze(meta, events)

    def test_host_turnaround_excludes_receive_wait_and_nested_encode(self):
        meta, events = population()
        text = analyze(meta, events)
        self.assertEqual(text.count("critical_path_ns=25"), 2)
        self.assertEqual(text.count("dispatch_codec_resume_residual=11"), 2)
        self.assertIn("same_worker_pairs=1", text)
        # A resume on a replacement PID cannot close the old worker's effect.
        events[10]["pid"] = 8
        with self.assertRaisesRegex(ValueError, "incomplete/duplicate endpoints"):
            analyze(meta, events)
