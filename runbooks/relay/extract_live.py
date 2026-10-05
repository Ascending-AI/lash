"""Extract per-step relay prompts, cells and tool calls from a workbench trace.

usage: extract_live.py <trace.jsonl> <out-dir> [turn_id]
Writes stepN-prompt.json (the full LlmRequest), stepN-response.json and a
readable transcript.md with each step's context blocks, harness message,
reply and tool calls.
"""
import json, sys, pathlib
trace, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
turn = sys.argv[3] if len(sys.argv) > 3 else None
out.mkdir(parents=True, exist_ok=True)
strip = lambda e: {k: v for k, v in e.items() if k not in ("schema_version", "id", "timestamp", "context", "type")}
def texts(message):
    return [b.get("text") for b in message.get("blocks", []) if b.get("kind") == "text"]
md = []
step = 0
for line in trace.open():
    if not line.strip():
        continue
    e = json.loads(line)
    if turn and e.get("context", {}).get("turn_id") != turn:
        continue
    t = e.get("type")
    if t == "llm_call_started":
        step += 1
        req = e["request"]
        (out / f"step{step}-prompt.json").write_text(json.dumps(req, indent=1))
        msgs = req.get("messages", [])
        md.append(f"\n## Step {step} (turn {e['context'].get('turn_id')}, iteration {e['context'].get('protocol_iteration')})\n")
        if len(msgs) == 2:
            md.append("### Context blocks (as committed by the previous `next`)\n")
            for i, text in enumerate(texts(msgs[0])):
                md.append(f"{i}. {text}")
        else:
            md.append("### Context blocks\n(empty)")
        md.append("\n### Harness message\n```\n" + "".join(texts(msgs[-1])) + "\n```")
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
print(step, "llm calls")
