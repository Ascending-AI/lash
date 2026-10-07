"""Shared live-workbench transport, launch settings and usage accounting."""
import json, os, pathlib, subprocess, time, urllib.error, urllib.request

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
