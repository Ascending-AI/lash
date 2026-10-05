import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { SURFACES, verifyTranscriptSurface, allKindRecords } from "./transcript_projection_harness.mjs";

const timeline = readFileSync(new URL("../assets/timeline.js", import.meta.url), "utf8");
const durableToolTranscript = JSON.parse(
  process.env.LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT ?? "null",
);
assert.ok(durableToolTranscript, "canonical records must come from the Rust committed-row fixture");

test("all registered production surfaces preserve canonical records", () => {
  const assets = {
    workbench: timeline,
  };
  assert.deepEqual(Object.keys(assets).sort(), [...SURFACES].sort());
  for (const [surface, asset] of Object.entries(assets)) {
    assert.ok(asset, `${surface} must supply its production asset`);
    verifyTranscriptSurface(surface, asset, durableToolTranscript);
    const corpus = allKindRecords();
    // Display order follows the turn's lanes, not source recording order:
    // the early reply in the corpus follows thinking and execution in the UI.
    const displayOrder = ['user', 'reasoning', 'tool_call', 'code_block', 'attachment', 'event', 'assistant_reply']
      .map(kind => corpus.find(row => row.kind === kind).row_id);
    verifyTranscriptSurface(surface, asset, [...corpus, {...corpus[0], row_id:"hidden", suppressed:"protocol_internal"}], displayOrder);
  }
});
