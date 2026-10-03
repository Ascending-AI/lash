import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { SURFACES, verifyTranscriptSurface, allKindRecords } from "./transcript_projection_harness.mjs";

const html = readFileSync(new URL("../assets/index.html", import.meta.url), "utf8");
const durableToolTranscript = JSON.parse(
  process.env.LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT ?? "null",
);
assert.ok(durableToolTranscript, "canonical records must come from the Rust committed-row fixture");

test("all registered production surfaces preserve canonical records", () => {
  const assets = {
    workbench: html,
    service: process.env.LASH_TRANSCRIPT_SERVICE_ASSET,
    slack: process.env.LASH_TRANSCRIPT_SLACK_ASSET,
  };
  assert.deepEqual(Object.keys(assets).sort(), [...SURFACES].sort());
  for (const [surface, asset] of Object.entries(assets)) {
    assert.ok(asset, `${surface} must supply its production asset`);
    verifyTranscriptSurface(surface, asset, durableToolTranscript);
    const corpus = allKindRecords();
    verifyTranscriptSurface(surface, asset, [...corpus, {...corpus[0], row_id:"hidden", suppressed:"protocol_internal"}]);
  }
});
