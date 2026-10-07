"""Extract per-step relay prompts, programs, tool calls and provider usage from a workbench trace.

usage: extract_live.py <trace.jsonl> <out-dir> [turn_id ...]
Writes stepN-prompt.json (the full LlmRequest), stepN-response.json, a
readable transcript.md with each step's context entries, step message,
reply (an execute_code program or the plain-text answer) and tool calls, and
usage.md: one row per provider request with its step, its reply shape, its
cache breakpoints and the provider-reported uncached input, cache-read,
cache-write and output tokens, and the upstream provider OpenRouter routed it
to. The usage table is also printed.
With several turn ids, steps are numbered across them in trace order.
"""
import json, re, sys, pathlib
strip = lambda e: {k: v for k, v in e.items() if k not in ("schema_version", "id", "timestamp", "context", "type")}
def texts(message):
    return [b.get("text") for b in message.get("blocks", []) if b.get("kind") == "text"]
def breakpoints(messages):
    return [f"{m}.{b}" for m, message in enumerate(messages)
            for b, block in enumerate(message.get("blocks", [])) if block.get("cache_breakpoint")]


def extract(trace, out, turns=(), billing=None):
    """Write the per-step files and usage.md under `out`; return the usage rows, in trace order."""
    trace, out, turns = pathlib.Path(trace), pathlib.Path(out), list(turns)
    billing = billing or {}
    out.mkdir(parents=True, exist_ok=True)
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
            # A relay request: the context message (its constant header, then one
            # `[i]` block per entry), then the step message.
            context = texts(msgs[0])[1:] if len(msgs) == 2 else []
            message = "".join(texts(msgs[-1])) if msgs else ""
            n = re.match(r'<step n="(\d+)">', message)
            rows[call] = {
                "call_id": call,
                "step": step,
                "turn": (ctx.get("turn_id") or "")[-8:],
                "iteration": ctx.get("protocol_iteration"),
                "entries": len(context),
                "breakpoints": " ".join(breakpoints(msgs)) or "-",
                "system": prompt_hash.get((ctx.get("turn_id"), ctx.get("protocol_iteration")), "?"),
                "harness": n[1] if n else "?",
                "tools": ",".join(t.get("name", "?") for t in req.get("tools", [])) or "-",
            }
            order.append(call)
            md.append(f"\n## Request {step} (turn {ctx.get('turn_id')}, iteration {ctx.get('protocol_iteration')})\n")
            md.append("### Context (as committed by the previous `next`)\n")
            md.append("\n".join(context) if context else "(empty)")
            md.append(f"\nCache breakpoints (message.block): {rows[call]['breakpoints']}")
            md.append("\n### Step message\n```\n" + message + "\n```")
        elif t == "provider_stream_event" and call in rows:
            # OpenRouter names the upstream that served the call on its chunks.
            for upstream in re.findall(r'"provider":\s*"([^"]+)"', json.dumps(e.get("event", {})).replace('\\"', '"')):
                if upstream != "openai_compatible":
                    rows[call].setdefault("upstreams", set()).add(upstream)
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
            parts = resp.get("parts", []) if isinstance(resp, dict) else []
            prose = "".join(p.get("text", "") for p in parts if p.get("type") == "text")
            calls = [p for p in parts if p.get("type") == "tool_call"]
            programs = []
            for p in calls:
                try:
                    programs.append(json.loads(p.get("input_json", "{}")).get("code", ""))
                except ValueError:
                    programs.append(p.get("input_json", ""))
            if call in rows:
                # A cell-channel reply works by writing a `<typescript>` cell.
                rows[call]["reply"] = "work" if calls or "<typescript>" in prose else "answer"
                rows[call]["cost"] = (body.get("provider_usage") or {}).get("cost")
            if calls:
                if prose.strip():
                    md.append("\n### Text beside the call (dropped)\n```\n" + prose + "\n```")
                for program in programs:
                    md.append("\n### execute_code program\n```typescript\n" + program + "\n```")
            else:
                md.append("\n### Answer\n```\n" + (prose or json.dumps(body)[:2000]) + "\n```")
        elif t == "llm_call_failed" and call in rows:
            body = strip(e)
            (out / f"step{rows[call]['step']}-response.json").write_text(json.dumps(body, indent=1))
            rows[call].update(reply="failed", input=None, cache_read=None, cache_write=None, output=None)
            receipt = billing.get(call)
            if receipt and "total_cost" in receipt:
                cached = receipt.get("native_tokens_cached") or 0
                rows[call].update(
                    input=receipt["native_tokens_prompt"] - cached,
                    cache_read=cached, cache_write=0, output=receipt["native_tokens_completion"],
                    cost=receipt["total_cost"], billing_generation=receipt["id"],
                )
            md.append("\n### Failed model call\n```json\n" + json.dumps(body, indent=1) + "\n```")
        elif t == "tool_call_completed":
            body = strip(e)
            md.append("\n### Tool call\n```json\n" + json.dumps(body)[:1500] + "\n```")
    (out / "transcript.md").write_text("\n".join(md))
    header = "| # | turn | iter | step | reply | ctx entries | breakpoints (msg.block) | system hash | tools | upstream | uncached in | cache read | cache write | output | ms | cost $ |"
    table = [header, "|" + "---|" * 16]
    for call in order:
        r = rows[call]
        r = {**r, "upstream": ",".join(sorted(r.get("upstreams", ()))) or "?"}
        table.append("| {step} | {turn} | {iteration} | {harness} | {reply} | {entries} | {breakpoints} | {system} | {tools} | {upstream} | {input} | {cache_read} | {cache_write} | {output} | {ms} | {cost} |".format(
            **{"input": "-", "cache_read": "-", "cache_write": "-", "output": "-", "ms": "-", "reply": "?", **r, "cost": "-" if r.get("cost") is None else f"{r['cost']:.5f}"}))
    total = sum(rows[call].get("cost") or 0 for call in order)
    table.append(f"\nProvider-reported cost: ${total:.5f}")
    for call in order:
        row = rows[call]
        if row.get("billing_generation"):
            table.append(f"\nFailed request {row['step']}: tokens and cost reconciled from OpenRouter generation receipt `{row['billing_generation']}` (billing.json).")
        elif row.get("reply") == "failed":
            table.append(f"\nFailed request {row['step']}: usage is unreported; omitted amounts are unknown, not zero.")
    (out / "usage.md").write_text("\n".join(table) + "\n")
    return [{**rows[call], "upstreams": sorted(rows[call].get("upstreams", ()))} for call in order]


if __name__ == "__main__":
    rows = extract(sys.argv[1], sys.argv[2], sys.argv[3:])
    print((pathlib.Path(sys.argv[2]) / "usage.md").read_text(), end="")
    print(len(rows), "llm calls")
