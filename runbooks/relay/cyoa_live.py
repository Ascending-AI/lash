"""Drive and programmatically judge a seeded story-memory journey."""
import argparse
import json
import pathlib
import sys

from extract_live import extract
from live_common import POLICIES, Workbench, environment, launcher, collect_usage

KINDS = ("fact", "negative", "order")
DEFAULT_TYPES = ("fact", "fact", "negative", "order")
WARNING = "At the end I will quiz you on details of your journey: ages, codes, objects, pets and the order of people."


def settings():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", required=True, type=pathlib.Path)
    p.add_argument("--rounds", type=int, default=12)
    p.add_argument("--questions", type=int, default=12)
    p.add_argument("--types", default=",".join(DEFAULT_TYPES), help="cyclic type mix; repeat a type for greater weight")
    p.add_argument("--told", action="store_true")
    p.add_argument("--delivery", choices=("tool", "message"), default="tool")
    p.add_argument("--policy", choices=sorted(POLICIES), default="relay")
    p.add_argument("--channel", choices=("native", "cell"), default="native")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--branching", type=int, default=3)
    p.add_argument("--vocabulary-seed", type=int, default=1)
    p.add_argument("--model", default="z-ai/glm-5.3-flash")
    p.add_argument("--upstream", default="z-ai")
    p.add_argument("--port", type=int, default=4492)
    p.add_argument("--env-file", type=pathlib.Path)
    p.add_argument("--budget", type=float, default=0.15, help="maximum USD before stopping; reserve this per run")
    args = p.parse_args()
    args.types = [kind.strip() for kind in args.types.split(",")]
    if not args.types or any(kind not in KINDS for kind in args.types):
        p.error("--types must be a comma list of fact,negative,order")
    if args.rounds < 1 or args.questions < 1 or not 2 <= args.branching <= 26:
        p.error("rounds/questions must be positive; branching must be 2..26")
    if not all(0 <= n < 2**64 for n in (args.seed, args.vocabulary_seed)):
        p.error("seeds must be unsigned 64-bit integers")
    capacities = {"fact": 2 * args.rounds, "order": args.rounds * (args.rounds - 1) // 2,
                  "negative": 2 * args.rounds + (args.branching > 2)}
    for kind, capacity in capacities.items():
        count = sum(args.types[i % len(args.types)] == kind for i in range(args.questions))
        if count > capacity:
            p.error(f"more {kind} questions than distinct facts or pairs ({capacity})")
    if args.policy == "relay" and args.channel != "native":
        p.error("relay requires --channel native")
    if args.budget <= 0:
        p.error("budget must be positive")
    return args


def round_prompt(args, number, passage):
    text = f"Round {number}: read where you are and choose one option."
    if args.told and number == 1:
        text += " " + WARNING
    if args.delivery == "message":
        text += "\nCurrent passage and choices:\n" + json.dumps(passage)
    return text


def final_prompt(questions):
    return ('Answer these questions about your journey. Reply with exactly a JSON object '
            'keyed q1..qQ (use each question\'s id). For order and negative questions use "YES" or "NO". '
            'For ages and gate codes use integers; for other facts use strings. Copy stated values exactly. '
            '\n' +
            "\n".join(f"{q['id']}: {q['prompt']}" for q in questions))


def run(args):
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    env = environment(args, {
        "AGENT_WORKBENCH_TTT": "off",
        "AGENT_WORKBENCH_STORY": "on",
        "AGENT_WORKBENCH_STORY_SEED": str(args.seed),
        "AGENT_WORKBENCH_STORY_BRANCHING": str(args.branching),
        "AGENT_WORKBENCH_STORY_VOCABULARY_SEED": str(args.vocabulary_seed),
    })
    if not env.get("OPENROUTER_API_KEY"):
        sys.exit("OPENROUTER_API_KEY is not set (export it or pass --env-file)")
    trace = args.out / "wb-data" / "trace.jsonl"
    wb = Workbench(args.port)
    rounds, final, stopped, questions = [], None, None, []
    launcher(args, env, "up")
    try:
        spent = 0.0
        for number in range(1, args.rounds + 1):
            passage = wb.call("/api/story/rounds", {})
            turns = [wb.turn(round_prompt(args, number, passage))]
            record = wb.call("/api/story")["rounds"][-1]
            while record["choice"] is None and len(turns) <= 5:
                turns.append(wb.turn(f"Round {number} is not finished. Choose one option."))
                record = wb.call("/api/story")["rounds"][-1]
            metering = collect_usage(trace, args.out / f"round-{number}", [t["turn_id"] for t in turns], env, extract)
            rounds.append({"round": number, "record": record, "turns": turns,
                           "continuation_turns": len(turns) - 1, **metering})
            spent += metering["cost"]
            print(f"round {number}: {record['choice']}; {metering['requests']} requests, ${metering['cost']:.6f}", flush=True)
            if record["choice"] is None:
                stopped = f"invalid run: round {number} unfinished after 5 continuation turns"
                break
            # Stop while enough budget remains for one similarly priced round/final.
            if spent + max(metering["cost"], spent / number) >= args.budget:
                stopped = f"budget: ${spent:.6f} spent; next turn may exceed ${args.budget}"
                break
        if stopped is None:
            quiz = {"seed": args.seed, "questions": args.questions, "types": args.types}
            questions = wb.call("/api/story/questions", quiz)
            answered = wb.turn(final_prompt(questions))
            final = {**answered, **collect_usage(trace, args.out / "final", [answered["turn_id"]], env, extract)}
            final["score"] = wb.call("/api/story/score", {**quiz, "answer": answered["reply"] or ""})
        log = wb.call("/api/story")
    finally:
        launcher(args, env, "down")
    (args.out / "log.json").write_text(json.dumps(log, indent=2))
    (args.out / "questions.json").write_text(json.dumps(questions, indent=2))
    usages = rounds + ([final] if final else [])
    totals = {key: sum(row[key] for row in usages)
              for key in ("requests", "steps", "input_uncached", "cache_read", "cache_write", "output", "cost")}
    totals["cost"] = round(totals["cost"], 6)
    totals["billing_generations"] = [g for row in usages for g in row.get("billing_generations", [])]
    totals["unreported_generations"] = [g for row in usages for g in row.get("unreported_generations", [])]
    turns = [turn for row in rounds for turn in row["turns"]] + ([final] if final else [])
    totals["failed_turns"] = [turn for turn in turns if turn["failed"]]
    totals["continuation_turns"] = sum(row["continuation_turns"] for row in rounds)
    results = {
        "settings": {key: str(v) if isinstance(v, pathlib.Path) else v
                     for key, v in vars(args).items() if key != "env_file"},
        "rounds": rounds, "final": final, "totals": totals,
        "score": final["score"] if final else None, "stopped": stopped,
        "valid": stopped is None,
    }
    (args.out / "results.json").write_text(json.dumps(results, indent=2))
    print(json.dumps({"totals": totals, "score": results["score"], "stopped": stopped}, indent=2))


if __name__ == "__main__":
    run(settings())
