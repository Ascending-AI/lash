"""Drive the tic-tac-toe memory scenario on a live workbench and score it.

usage: ttt_live.py --out DIR [--games N] [--ask K] [--told] [--seed S]
                   [--schedule mixed|perfect|random|perfect,random,...]
                   [--first agent|opponent] [--policy relay|rlm|standard] [--channel native|cell]
                   [--model M] [--upstream SLUGS] [--port P] [--env-file F]
                   [--budget USD]

Run it in a Kiln fork with env.sh sourced and OPENROUTER_API_KEY exported (or
named in --env-file). It launches a fresh workbench (its own data directory
under DIR, so a fresh session) with the tic-tac-toe world on, plays N games,
with at most five continuation turns per game, asks one final memory question
about the first K games,
scores the answer against the world's game log, and stops the workbench.

It writes under DIR:
- results.json: the settings, per game the result, misplays, refused moves,
  turn outcome, requests, work steps, tokens, cache reads and cost, the final
  turn's usage and answer, the totals and the score;
- game-<g>/ and final/: extract_live.py's per-request files and usage.md;
- log.json: the world's game log, the answer key's source;
- wb-data/ (with trace.jsonl) and wb-run/: the workbench's own files.
"""
import argparse, json, os, pathlib, subprocess, sys, time, urllib.error, urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from extract_live import extract  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parents[2]
POLICIES = {
    "relay": {"AGENT_WORKBENCH_PROTOCOL": "rlm", "AGENT_WORKBENCH_RLM_POLICY": "relay"},
    "rlm": {"AGENT_WORKBENCH_PROTOCOL": "rlm", "AGENT_WORKBENCH_RLM_POLICY": "chronological"},
    "standard": {"AGENT_WORKBENCH_PROTOCOL": "standard"},
}


def settings():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--out", required=True, type=pathlib.Path)
    p.add_argument("--games", type=int, default=10)
    p.add_argument("--ask", type=int, default=5)
    p.add_argument("--told", action="store_true")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--schedule", default="mixed")
    p.add_argument("--first", choices=("agent", "opponent"), default="agent")
    p.add_argument("--policy", choices=sorted(POLICIES), default="relay")
    p.add_argument("--channel", choices=("native", "cell"), default="native")
    p.add_argument("--model", default="z-ai/glm-5.3-flash")
    p.add_argument("--upstream", default="z-ai", help="OpenRouter upstream pin; empty for none")
    p.add_argument("--port", type=int, default=4491)
    p.add_argument("--env-file", type=pathlib.Path, help="KEY=VALUE lines, e.g. the OpenRouter key")
    p.add_argument("--budget", type=float, default=0.5, help="stop after a game once spend passes this (USD)")
    args = p.parse_args()
    if not 1 <= args.ask < args.games:
        p.error(f"--ask K must satisfy 1 <= K < N (--games); got K={args.ask}, N={args.games}")
    if args.policy == "relay" and args.channel != "native":
        p.error("relay requires --channel native")
    return args


def game_prompt(args, game):
    text = f"Game {game}: you are X. Play it to the end."
    if args.told and game == 1:
        text += f" After the games I will ask you who won each of the first {args.ask} games."
    return text


def final_prompt(args):
    return (f"List who won each of the first {args.ask} games, in order. Reply with exactly a JSON array "
            'whose items are "ASSISTANT", "USER" or "DRAW" '
            '("ASSISTANT" = you, the agent, won; "USER" = we, your opponent, won).')


def environment(args):
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
        "AGENT_WORKBENCH_TTT": "on",
        "AGENT_WORKBENCH_TTT_SEED": str(args.seed),
        "AGENT_WORKBENCH_TTT_SCHEDULE": args.schedule,
        "AGENT_WORKBENCH_TTT_FIRST": args.first,
        "OPENROUTER_MODEL": args.model,
        "AGENT_WORKBENCH_OPENROUTER_PROVIDER": args.upstream,
        "AGENT_WORKBENCH_DATA_DIR": str(args.out / "wb-data"),
        "AGENT_WORKBENCH_RUN_DIR": str(args.out / "wb-run"),
    })
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


def main():
    args = settings()
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    env = environment(args)
    if not env.get("OPENROUTER_API_KEY"):
        sys.exit("OPENROUTER_API_KEY is not set (export it or pass --env-file)")
    trace = args.out / "wb-data" / "trace.jsonl"
    wb = Workbench(args.port)
    games, final, stopped = [], None, None
    launcher(args, env, "up")
    try:
        spent = 0.0
        for game in range(1, args.games + 1):
            started = wb.call("/api/ttt/games", {})
            turns = [wb.turn(game_prompt(args, game))]
            record = wb.call("/api/ttt")["games"][game - 1]
            while record["status"] == "ongoing" and len(turns) <= 5:
                turns.append(wb.turn(f"Game {game} is not finished. Continue playing it to the end."))
                record = wb.call("/api/ttt")["games"][game - 1]
            played = turns[-1]
            turn_usage = usage(extract(trace, args.out / f"game-{game}", [turn["turn_id"] for turn in turns]))
            games.append({"game": game, "opponent": record["opponent"], "result": record["result"],
                          "moves": len(record["moves"]), "misplays": record["misplays"],
                          "illegal_moves": record["illegal_moves"], "picture": record["picture"],
                          "started": started, "turns": turns, "continuation_turns": len(turns) - 1,
                          **played, **turn_usage})
            spent += turn_usage["cost"]
            print(f"game {game}: {record['result']} vs {record['opponent']}; {turn_usage['requests']} requests, "
                  f"${turn_usage['cost']:.5f}; reply: {(played['reply'] or '')[:120]!r}", flush=True)
            if record["status"] == "ongoing":
                stopped = f"invalid run: game {game} is ongoing after 5 continuation turns"
                break
            if spent > args.budget:
                stopped = f"spend ${spent:.4f} passed the ${args.budget} budget after game {game}"
                break
        if stopped is None:
            answered = wb.turn(final_prompt(args))
            final = {**answered, **usage(extract(trace, args.out / "final", [answered["turn_id"]]))}
            final["score"] = wb.call("/api/ttt/score", {"answer": answered["reply"] or "", "ask": args.ask})
        log = wb.call("/api/ttt")
    finally:
        launcher(args, env, "down")
    (args.out / "log.json").write_text(json.dumps(log, indent=1))
    turns = games + ([final] if final else [])
    totals = {key: sum(turn[key] for turn in turns)
              for key in ("requests", "steps", "input_uncached", "cache_read", "cache_write", "output")}
    totals["cost"] = round(sum(turn["cost"] for turn in turns), 6)
    all_turns = [turn for game in games for turn in game["turns"]] + ([final] if final else [])
    totals["failed_turns"] = [turn["turn_id"] for turn in all_turns if turn["failed"]]
    totals["continuation_turns"] = sum(game["continuation_turns"] for game in games)
    results = {
        "settings": {key: (str(value) if isinstance(value, pathlib.Path) else value)
                     for key, value in vars(args).items() if key not in ("env_file", "channel")},
        "channel": args.channel if args.policy != "standard" else None,
        "prompts": {"game_1": game_prompt(args, 1), "game_n": game_prompt(args, 2), "final": final_prompt(args)},
        "games": games,
        "final": final,
        "totals": totals,
        "score": final["score"] if final else None,
        "stopped": stopped,
    }
    (args.out / "results.json").write_text(json.dumps(results, indent=1))
    score = results["score"]
    print(json.dumps({"totals": totals, "score": score, "stopped": stopped}, indent=1))


if __name__ == "__main__":
    main()
