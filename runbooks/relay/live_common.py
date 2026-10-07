"""Shared live-workbench transport, launch settings and usage accounting."""
import json, os, pathlib, subprocess, time, urllib.error, urllib.request, urllib.parse

REPO = pathlib.Path(__file__).resolve().parents[2]
POLICIES = {
    "relay": {"AGENT_WORKBENCH_PROTOCOL": "rlm", "AGENT_WORKBENCH_RLM_POLICY": "relay"},
    "rlm": {"AGENT_WORKBENCH_PROTOCOL": "rlm", "AGENT_WORKBENCH_RLM_POLICY": "chronological"},
    "standard": {"AGENT_WORKBENCH_PROTOCOL": "standard"},
}


def environment(args, world):
    env = dict(os.environ)
    if args.env_file:
        for line in args.env_file.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                key, value = line.split("=", 1)
                env[key.removeprefix("export ").strip()] = value.strip().strip('"').strip("'")
    env.pop("LASH_RLM_CHANNEL", None)
    env.pop("AGENT_WORKBENCH_RLM_POLICY", None)
    env.update(POLICIES[args.policy])
    if args.policy != "standard":
        env["LASH_RLM_CHANNEL"] = args.channel
    env.update({
        "OPENROUTER_MODEL": args.model,
        "AGENT_WORKBENCH_OPENROUTER_PROVIDER": args.upstream,
        "AGENT_WORKBENCH_DATA_DIR": str(args.out / "wb-data"),
        "AGENT_WORKBENCH_RUN_DIR": str(args.out / "wb-run"),
    })
    env.update(world)
    return env


def launcher(args, env, verb):
    with open(args.out / f"workbench-{verb}.log", "w") as log:
        subprocess.run([str(REPO / "scripts/agent-workbench-dev.sh"), verb, "--port", str(args.port)],
                       cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)


class Workbench:
    def __init__(self, port):
        self.base = f"http://127.0.0.1:{port}"

    def call(self, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data,
                                         headers={"content-type": "application/json"})
        try:
            return json.load(urllib.request.urlopen(request, timeout=60))
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{path}: HTTP {error.code}: {error.read().decode()[:500]}") from None

    def turn(self, text, timeout_s=900):
        sent = self.call("/api/turn", {"text": text})
        turn_id = sent["turn_id"]
        deadline = time.time() + timeout_s
        while True:
            time.sleep(3)
            state = self.call("/api/state")
            if not state["active_turns"] and not state["queued_work"]:
                break
            if time.time() > deadline:
                raise RuntimeError(f"turn {turn_id} did not settle in {timeout_s}s")
        replies = [row["content"].get("text") or "" for row in state["transcript"]
                   if row["kind"] == "assistant_reply" and row["provenance"].get("turn_id") == turn_id
                   and row["provenance"].get("is_turn_reply")]
        failures = [f for f in state.get("turn_failure_settlements") or []
                    if turn_id in json.dumps(f)]
        return {"turn_id": turn_id, "reply": replies[-1] if replies else None,
                "failed": bool(failures) or not replies, "failure": failures or None}


def usage(rows):
    """One turn's totals from its extract_live rows."""
    total = lambda key: sum(row.get(key) or 0 for row in rows)
    return {
        "requests": len(rows),
        "steps": sum(1 for row in rows if row.get("reply") == "work"),
        "input_uncached": total("input"),
        "cache_read": total("cache_read"),
        "cache_write": total("cache_write"),
        "output": total("output"),
        "cost": round(total("cost"), 6),
        "upstreams": sorted({u for row in rows for u in row.get("upstreams", [])}),
    }


def collect_usage(trace, out, turns, env, extractor):
    """Reconcile billed failed calls whose trace has no terminal usage receipt."""
    rows = extractor(trace, out, turns)
    if not rows:
        return usage(rows)
    receipts = {}
    missing = []
    failures = []
    for line in pathlib.Path(trace).open():
        event = json.loads(line)
        if event.get("type") == "llm_call_failed" and event.get("context", {}).get("turn_id") in turns:
            failures.append(event)
    for event in failures:
        call = event["context"]["llm_call_id"]
        ids = [a.get("detail", {}).get("execution_evidence", {}).get("provider_response_id")
               for a in event.get("attempts", [])]
        # A terminal failed call is reconciled only when its one attempt has
        # an identifiable OpenRouter receipt. Multi-attempt billing stays explicit.
        if len(ids) != 1 or not isinstance(ids[0], str) or not ids[0].startswith("gen-"):
            missing.append({"call_id": call, "reason": "no single OpenRouter generation receipt"})
            continue
        generation = ids[0]
        key = env.get("OPENROUTER_API_KEY")
        if not key:
            missing.append({"call_id": call, "generation": generation, "reason": "no OpenRouter key"})
            continue
        request = urllib.request.Request(
            "https://openrouter.ai/api/v1/generation?id=" + urllib.parse.quote(generation),
            headers={"Authorization": "Bearer " + key},
        )
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                data = json.load(response)["data"]
            receipt = {k: data.get(k) for k in ("id", "model", "provider_name", "total_cost",
                "native_tokens_prompt", "native_tokens_completion", "native_tokens_cached", "cancelled")}
            if receipt["id"] != generation:
                raise ValueError("generation receipt identity differs from the failed call")
            if not all(isinstance(receipt[k], (int, float)) for k in
                       ("total_cost", "native_tokens_prompt", "native_tokens_completion")):
                raise ValueError("generation receipt lacks numeric usage")
            receipts[call] = receipt
        except (urllib.error.URLError, ValueError, KeyError) as error:
            missing.append({"call_id": call, "generation": generation, "reason": str(error)})
    if failures:
        pathlib.Path(out, "billing.json").write_text(json.dumps({"receipts": receipts, "missing": missing}, indent=2))
    if receipts:
        rows = extractor(trace, out, turns, billing=receipts)
    result = usage(rows)
    result["billing_generations"] = [r["id"] for r in receipts.values()]
    result["unreported_generations"] = missing
    return result
