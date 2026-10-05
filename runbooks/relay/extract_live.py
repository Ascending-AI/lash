"""Extract per-step relay prompts, cells, tool calls and provider usage from a workbench trace.

usage: extract_live.py <trace.jsonl> <out-dir> [turn_id ...]
Writes stepN-prompt.json (the full LlmRequest), stepN-response.json, a
readable transcript.md with each step's context blocks, harness message,
reply and tool calls, and usage.md: one row per provider request with its
cache breakpoints and the provider-reported uncached input, cache-read,
cache-write and output tokens. The usage table is also printed.
With several turn ids, steps are numbered across them in trace order.
"""
import json, sys, pathlib
trace, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
turns = sys.argv[3:]
out.mkdir(parents=True, exist_ok=True)
strip = lambda e: {k: v for k, v in e.items() if k not in ("schema_version", "id", "timestamp", "context", "type")}
def texts(message):
    return [b.get("text") for b in message.get("blocks", []) if b.get("kind") == "text"]
def breakpoints(messages):
    return [f"{m}.{b}" for m, message in enumerate(messages)
            for b, block in enumerate(message.get("blocks", [])) if block.get("cache_breakpoint")]
md = []
rows = {}
order = []
prompt_hash = {}
step = 0
for line in trace.open():
    if not line.strip():
        continue
    e = json.loads(line)
    ctx = e.get("context", {})
    if turns and ctx.get("turn_id") not in turns:
        continue
    t = e.get("type")
    call = ctx.get("llm_call_id")
    if t == "prompt_built":
        prompt_hash[(ctx.get("turn_id"), ctx.get("protocol_iteration"))] = e.get("prompt_hash", "")[:12]
    elif t == "llm_call_started":
        step += 1
        req = e["request"]
        (out / f"step{step}-prompt.json").write_text(json.dumps(req, indent=1))
        msgs = req.get("messages", [])
        harness = "".join(texts(msgs[-1])) if msgs else ""
        rows[call] = {
            "step": step,
            "turn": (ctx.get("turn_id") or "")[-8:],
            "iteration": ctx.get("protocol_iteration"),
            "entries": len(texts(msgs[0])) if len(msgs) == 2 else 0,
            "breakpoints": " ".join(breakpoints(msgs)) or "-",
            "system": prompt_hash.get((ctx.get("turn_id"), ctx.get("protocol_iteration")), "?"),
            "harness": harness.split("\n", 1)[0].replace("=== HARNESS · ", "").replace(" ===", ""),
        }
        order.append(call)
        md.append(f"\n## Step {step} (turn {ctx.get('turn_id')}, iteration {ctx.get('protocol_iteration')})\n")
        if len(msgs) == 2:
            md.append("### Context blocks (as committed by the previous `next`)\n")
            for i, text in enumerate(texts(msgs[0])):
                md.append(f"{i}. {text}")
        else:
            md.append("### Context blocks\n(empty)")
        md.append(f"\nCache breakpoints (message.block): {rows[call]['breakpoints']}")
        md.append("\n### Harness message\n```\n" + harness + "\n```")
    elif t == "llm_attempt_completed" and call in rows:
        attempt = e.get("attempt", {})
        usage = attempt.get("usage") or {}
        rows[call].update({
            "model": attempt.get("response_model") or attempt.get("request_model"),
            "input": usage.get("input_tokens", 0),
            "cache_read": usage.get("cache_read_input_tokens", 0),
            "cache_write": usage.get("cache_write_input_tokens", 0),
            "output": usage.get("output_tokens", 0),
            "ms": (attempt.get("ended_at_ms") or 0) - (attempt.get("started_at_ms") or 0),
        })
    elif t == "llm_call_completed":
        body = strip(e)
        (out / f"step{step}-response.json").write_text(json.dumps(body, indent=1))
        resp = body.get("response", body)
        reply = resp.get("text", "") if isinstance(resp, dict) else ""
        md.append("\n### Model reply\n```\n" + (reply or json.dumps(body)[:2000]) + "\n```")
    elif t == "tool_call_completed":
        body = strip(e)
        md.append("\n### Tool call\n```json\n" + json.dumps(body)[:1500] + "\n```")
(out / "transcript.md").write_text("\n".join(md))
header = "| # | turn | iter | harness | ctx entries | breakpoints (msg.block) | system hash | uncached in | cache read | cache write | output | ms |"
table = [header, "|" + "---|" * 12]
for call in order:
    r = rows[call]
    table.append("| {step} | {turn} | {iteration} | {harness} | {entries} | {breakpoints} | {system} | {input} | {cache_read} | {cache_write} | {output} | {ms} |".format(
        **{"input": "-", "cache_read": "-", "cache_write": "-", "output": "-", "ms": "-", **r}))
(out / "usage.md").write_text("\n".join(table) + "\n")
print("\n".join(table))
print(step, "llm calls")
