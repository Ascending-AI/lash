// Records, from Node, what each case of cases.mjs does, and writes
// recorded.json beside it.
//
// A run is a list of epochs. An epoch is the stretch between two arrivals:
// the outcomes delivered into it, the tool calls and sleeps it made and the
// lines it logged. Each batch of a case's `deliveries` is settled in one
// macrotask, in the order listed, and the microtask queue then drains
// before the next batch: that is how a kernel run sees arrivals, which are
// delivered only when no task is ready (`K-TASK-024`). When the script is
// used up, the oldest pending call is answered.
//
// Run from this directory with Node: `node record.mjs`.
import { writeFileSync } from "node:fs";
import { cases } from "./cases.mjs";

const AsyncFunction = (async () => {}).constructor;
const drained = () => new Promise((resolve) => setImmediate(resolve));

const unhandled = [];
process.on("unhandledRejection", (reason) => unhandled.push(String(reason)));

async function record({ name, source, deliveries, deviation }) {
  const epochs = [];
  let epoch = { delivered: [], asked: [], logged: [] };
  const pending = [];
  const ask = (label, settle) => {
    epoch.asked.push(label);
    return new Promise((resolve, reject) => {
      pending.push({ label, settle: () => settle(resolve, reject) });
    });
  };
  const echo = (x) => ask(`echo:${x}`, (resolve) => resolve(x));
  const boom = (x) =>
    ask(`boom:${x}`, (_, reject) => {
      const error = new Error(String(x));
      error.name = "boom";
      reject(error);
    });
  const sleep = (ms) => ask(`sleep:${ms}`, (resolve) => resolve(undefined));
  // The dialect's `console.log`: a plain object or an array as compact JSON,
  // anything else as its string.
  const spelled = (part) =>
    part !== null && typeof part === "object" && !(part instanceof Error)
      ? JSON.stringify(part)
      : String(part);
  const console = { log: (...parts) => epoch.logged.push(parts.map(spelled).join(" ")) };

  let end = null;
  new AsyncFunction("echo", "boom", "sleep", "console", source)(echo, boom, sleep, console).then(
    () => (end = "ok"),
    (error) => (end = `error ${error}`),
  );
  const script = deliveries.slice();
  for (;;) {
    await drained();
    if (end !== null) break;
    epochs.push(epoch);
    const batch = script.shift() ?? (pending.length > 0 ? [pending[0].label] : null);
    if (batch === null) {
      end = "stuck";
      break;
    }
    epoch = { delivered: batch, asked: [], logged: [] };
    for (const label of batch) {
      const index = pending.findIndex((call) => call.label === label);
      if (index < 0) throw new Error(`${name}: no pending call ${label}`);
      pending.splice(index, 1)[0].settle();
    }
  }
  epochs.push(epoch);
  await drained();
  if (unhandled.length > 0) {
    throw new Error(`${name}: unhandled rejection ${unhandled.join(", ")}`);
  }
  return { name, ...(deviation ? { deviation } : {}), source, deliveries, epochs, end };
}

const recorded = [];
for (const witness of cases) {
  recorded.push(await record(witness));
}
writeFileSync(
  new URL("recorded.json", import.meta.url),
  `${JSON.stringify({ node: process.version, cases: recorded }, null, 1)}\n`,
);
