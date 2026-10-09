// The asynchronous witnesses. Each case is a cell that calls the scripted
// tools `echo(x)` (answers `x`), `boom(x)` (fails with an error named
// `boom` whose message is `x`) and `sleep(ms)`, and reports through
// `console.log`, which writes an object as compact JSON. `deliveries` scripts the arrivals: each entry is the batch
// of outcomes that arrive together, by the label of its request. A case
// with a `deviation` is one the kernel runs differently from Node, under
// the row of that name in `deviations.md`.
//
// `record.mjs` runs every case in Node and writes `recorded.json`; the laws
// in `src/tests/async_fn.rs` run the same cases on the kernel machine.
export const cases = [
  {
    name: "fan_out_shares_a_counter",
    source: `
let counter = 0;
const step = async (name) => {
  const first = await echo(name + "1");
  counter = counter + 1;
  console.log(\`\${name} first \${first} \${counter}\`);
  const second = await echo(name + "2");
  counter = counter * 2;
  console.log(\`\${name} second \${second} \${counter}\`);
  return first + second;
};
const all = await Promise.all([step("a"), step("b"), step("c")]);
console.log(\`done \${all[0]} \${all[1]} \${all[2]} \${counter}\`);
`,
    deliveries: [["echo:b1"], ["echo:a1", "echo:c1"], ["echo:c2"], ["echo:a2"], ["echo:b2"]],
  },
  {
    name: "fan_out_other_arrival_order",
    source: `
let counter = 0;
const step = async (name) => {
  const first = await echo(name + "1");
  counter = counter + 1;
  console.log(\`\${name} first \${first} \${counter}\`);
  const second = await echo(name + "2");
  counter = counter * 2;
  console.log(\`\${name} second \${second} \${counter}\`);
  return first + second;
};
const all = await Promise.all([step("a"), step("b"), step("c")]);
console.log(\`done \${all[0]} \${all[1]} \${all[2]} \${counter}\`);
`,
    deliveries: [["echo:c1", "echo:b1", "echo:a1"], ["echo:a2"], ["echo:b2", "echo:c2"]],
  },
  {
    name: "operand_read_before_an_await",
    source: `
let total = 0;
const add = async (n) => {
  total = total + (await echo(n));
  console.log(\`added \${n} total \${total}\`);
};
await Promise.all([add(1), add(10)]);
console.log(\`total \${total}\`);
`,
    deliveries: [["echo:10"], ["echo:1"]],
  },
  {
    name: "callbacks_of_a_map_run_concurrently",
    source: `
const mapOf = (items, f) => {
  let out = [];
  for (const item of items) {
    out = [...out, f(item)];
  }
  return out;
};
const results = await Promise.all(mapOf(["a", "b", "c"], async (item) => {
  console.log(\`start \${item}\`);
  const value = await echo(item);
  console.log(\`end \${value}\`);
  return value + value;
}));
console.log(\`results \${results[0]} \${results[1]} \${results[2]}\`);
`,
    deliveries: [["echo:c"], ["echo:a"], ["echo:b"]],
  },
  {
    name: "promise_created_then_awaited_later",
    source: `
const work = async () => {
  console.log("work start");
  await null;
  console.log("work middle");
  return 7;
};
const promise = work();
let x = 1;
x = x + 1;
console.log(\`between \${x}\`);
await null;
console.log("after one tick");
const first = await promise;
const second = await promise;
console.log(\`got \${first} \${second}\`);
`,
    deliveries: [],
  },
  {
    name: "race_loser_keeps_running_and_fails",
    source: `
const slow = async () => {
  const value = await echo("slow");
  console.log(\`slow got \${value}\`);
  await boom("late");
  console.log("not reached");
};
const fast = async () => {
  const value = await echo("fast");
  return value;
};
const winner = await Promise.race([slow(), fast()]);
console.log(\`winner \${winner}\`);
const more = await echo("more");
console.log(\`more \${more}\`);
const again = await echo("again");
console.log(\`again \${again}\`);
`,
    deliveries: [["echo:fast"], ["echo:slow"], ["boom:late"], ["echo:more"], ["echo:again"]],
  },
  {
    name: "all_fails_fast",
    source: `
const ok = async (name) => {
  const value = await echo(name);
  console.log(\`ok \${value}\`);
  return value;
};
const bad = async () => {
  await boom("x");
};
try {
  await Promise.all([ok("a"), bad(), ok("b")]);
  console.log("not reached");
} catch (error) {
  console.log(\`caught \${error}\`);
}
const tail = await echo("tail");
console.log(\`tail \${tail}\`);
`,
    deliveries: [["echo:a"], ["boom:x"], ["echo:b"], ["echo:tail"]],
  },
  {
    name: "all_settled",
    source: `
const thrower = async () => {
  throw "thrown";
};
const failing = async () => {
  try {
    await boom("b");
  } catch (error) {
    throw \`\${error}\`;
  }
};
const results = await Promise.allSettled([echo("a"), failing(), 3, thrower()]);
for (const result of results) {
  console.log(result);
}
`,
    deliveries: [["boom:b"], ["echo:a"]],
  },
  {
    name: "rejection_caught_after_its_task_ended",
    source: `
const failing = async () => {
  await null;
  throw "bang";
};
const promise = failing();
await null;
await null;
await null;
console.log("the task has ended");
try {
  await promise;
} catch (error) {
  console.log(\`caught \${error}\`);
}
`,
    deliveries: [],
  },
  {
    name: "await_inside_finally",
    source: `
const run = async () => {
  try {
    const value = await echo("body");
    console.log(\`body \${value}\`);
    throw "failed";
  } finally {
    const cleaned = await echo("cleanup");
    console.log(\`cleanup \${cleaned}\`);
  }
};
const other = async () => {
  const value = await echo("other");
  console.log(\`other \${value}\`);
};
const background = other();
try {
  await run();
} catch (error) {
  console.log(\`caught \${error}\`);
}
await background;
`,
    deliveries: [["echo:body"], ["echo:other", "echo:cleanup"]],
  },
  {
    name: "microtask_order_without_tools",
    source: `
const log = (line) => console.log(line);
const a = async () => {
  log("a1");
  await null;
  log("a2");
  await null;
  log("a3");
};
const b = async () => {
  log("b1");
  await Promise.resolve(1);
  log("b2");
  return Promise.resolve(2);
};
const pa = a();
const pb = b();
const seen = pb.then((value) => log(\`then \${value}\`));
const chain = Promise.resolve()
  .then(() => log("r1"))
  .then(() => log("r2"))
  .then(() => log("r3"))
  .then(() => log("r4"))
  .then(() => log("r5"));
log("sync");
await pa;
log("pa done");
await pb;
log("pb done");
await seen;
await chain;
log("end");
`,
    deliveries: [],
  },
  {
    name: "then_catch_finally_over_tools",
    source: `
const p = echo("a")
  .then((value) => {
    console.log(\`then \${value}\`);
    return boom("b");
  })
  .catch((error) => {
    console.log(\`catch \${error}\`);
    return "recovered";
  })
  .finally(() => {
    console.log("finally");
  });
const q = echo("q");
console.log(\`q \${await q}\`);
console.log(\`p \${await p}\`);
`,
    deliveries: [["echo:a"], ["echo:q", "boom:b"]],
  },
  {
    name: "finally_keeps_the_outcome_and_waits",
    source: `
const log = (line) => console.log(line);
const failed = Promise.reject("no").finally(() => echo("f"));
const fine = Promise.resolve("yes").finally(() => { log("fine finally"); });
const ticks = Promise.resolve()
  .then(() => log("t1"))
  .then(() => log("t2"))
  .then(() => log("t3"))
  .then(() => log("t4"))
  .then(() => log("t5"));
log(\`fine \${await fine}\`);
try {
  await failed;
} catch (error) {
  log(\`failed \${error}\`);
}
await ticks;
`,
    deliveries: [["echo:f"]],
  },
  {
    name: "arrivals_in_one_batch_keep_their_order",
    source: `
const viaPromise = async () => {
  const promise = echo("p");
  const value = await promise;
  console.log(\`promise \${value}\`);
};
const inPlace = async () => {
  const value = await echo("i");
  console.log(\`in place \${value}\`);
};
const viaAll = async () => {
  const values = await Promise.all([echo("x")]);
  console.log(\`all \${values[0]}\`);
};
const failing = async () => {
  try {
    await boom("e");
  } catch (error) {
    console.log(\`failed \${error}\`);
  }
};
await Promise.all([viaPromise(), inPlace(), viaAll(), failing()]);
`,
    deliveries: [["echo:x", "echo:p", "boom:e", "echo:i"]],
  },
  {
    name: "arrivals_in_one_batch_reversed",
    source: `
const viaPromise = async () => {
  const promise = echo("p");
  const value = await promise;
  console.log(\`promise \${value}\`);
};
const inPlace = async () => {
  const value = await echo("i");
  console.log(\`in place \${value}\`);
};
const viaAll = async () => {
  const values = await Promise.all([echo("x")]);
  console.log(\`all \${values[0]}\`);
};
const failing = async () => {
  try {
    await boom("e");
  } catch (error) {
    console.log(\`failed \${error}\`);
  }
};
await Promise.all([viaPromise(), inPlace(), viaAll(), failing()]);
`,
    deliveries: [["echo:i", "boom:e", "echo:p", "echo:x"]],
  },
  {
    name: "race_and_any_with_a_sleep",
    source: `
const timer = sleep(50);
const first = await Promise.race([timer, echo("r")]);
console.log(\`race \${first}\`);
const found = await Promise.any([boom("n1"), echo("y"), boom("n2")]);
console.log(\`any \${found}\`);
try {
  await Promise.any([boom("z1"), boom("z2")]);
} catch (error) {
  console.log(\`none \${error}\`);
}
const settled = await Promise.race([1, echo("unused")]);
console.log(\`settled \${settled}\`);
await sleep(5);
console.log("slept");
`,
    deliveries: [
      ["sleep:50"],
      ["boom:n1"],
      ["echo:y"],
      ["boom:z2"],
      ["boom:z1"],
      ["sleep:5"],
    ],
  },
  {
    name: "aggregates_decided_when_called",
    source: `
const log = (line) => console.log(line);
const ticks = Promise.resolve()
  .then(() => log("t1"))
  .then(() => log("t2"))
  .then(() => log("t3"))
  .then(() => log("t4"));
const all = await Promise.all([1, Promise.resolve(2)]);
log(\`all \${all[0]} \${all[1]}\`);
const none = await Promise.all([]);
log("empty all");
const raced = await Promise.race([Promise.resolve("first"), 2]);
log(\`race \${raced}\`);
await ticks;
`,
    deliveries: [],
  },
  {
    name: "single_and_list_waiters_on_one_promise",
    deviation: "TS_JOIN_WAKE_ORDER",
    source: `
const member = async () => {
  await echo("m");
};
const promise = member();
const all = Promise.all([promise]);
const single = async () => {
  await promise;
  console.log("single 1");
  await null;
  console.log("single 2");
};
const waiting = single();
await all;
console.log("all");
await waiting;
`,
    deliveries: [["echo:m"]],
  },
];
