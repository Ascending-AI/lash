// The asynchronous witnesses. Each case is a cell that calls the scripted
// tools `host.echo(x)` (answers `x`), `host.boom(x)` (fails with an error named
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
    name: "promise_aggregates_follow_reaction_order",
    source: `
console.log(await Promise.race([Promise.resolve(0).then(x => x + 1), Promise.resolve(2)]));
console.log(await Promise.any([Promise.resolve(0).then(x => x + 1), Promise.resolve(2)]));
console.log(await Promise.race([Promise.resolve(1), Promise.resolve(2)]));
console.log(await Promise.any([Promise.resolve(1), Promise.resolve(2)]));
`,
    deliveries: [],
  },
  {
    name: "for_initializer_closure_keeps_its_binding",
    source: `
const fns = [];
for (let i = 0, read = () => i; i < 3; i++) fns.push(() => read());
console.log(fns.map(f => f()));
const passes = [];
for (let j = 0; j < 3; j++) { if (j === 1) continue; passes.push(() => j); }
console.log(passes.map(f => f()));
`,
    deliveries: [],
  },
  {
    name: "computed_rest_excludes_canonical_keys",
    source: `
const object = {1: 11, b: 22};
const {[1]: picked, ...rest} = object;
console.log(picked, Object.keys(rest));
let calls = 0;
const key = {toString() { calls++; return "1"; }};
const {[key]: other, ...remaining} = object;
console.log(other, Object.keys(remaining), calls);
`,
    deliveries: [],
  },
  {
    name: "array_from_steps_growing_collections",
    source: `
const map = new Map([[1, 1]]);
console.log(Array.from(map, entry => { if (entry[0] === 1) map.set(2, 2); return entry[1]; }));
const set = new Set([1]);
console.log(Array.from(set, item => { if (item === 1) set.add(2); return item; }));
`,
    deliveries: [],
  },
  {
    name: "destructuring_steps_live_iterators",
    source: `
const growing = [undefined];
const [a = (() => { growing.push(8); return 1; })(), b, ...tail] = growing;
console.log([a, b, tail]);
const shrinking = [undefined, 8, 9];
const [c = (() => { shrinking.length = 0; return 1; })(), d, ...rest] = shrinking;
console.log([c, d, rest]);
`,
    deliveries: [],
  },
  {
    name: "error_conversion_reads_current_properties",
    source: `
const error = new Error("old");
error.name = "Changed"; error.message = "new";
console.log(String(error), \`\${error}\`, error.toString());
const typed = new TypeError("m"); typed.name = "Custom";
console.log(String(typed), \`\${typed}\`);
`,
    deliveries: [],
  },
  {
    name: "null_base_assignment_precedes_key_conversion",
    source: `
for (const base of [null, undefined]) {
  let order = "";
  const key = {toString() { order += "k"; return "x"; }};
  const value = () => { order += "v"; return 1; };
  try { base[key] = value(); } catch (error) { order += "E"; }
  console.log(order);
}
`,
    deliveries: [],
  },
  {
    name: "fan_out_shares_a_counter",
    source: `
let counter = 0;
const step = async (name) => {
  const first = await host.echo(name + "1");
  counter = counter + 1;
  console.log(\`\${name} first \${first} \${counter}\`);
  const second = await host.echo(name + "2");
  counter = counter * 2;
  console.log(\`\${name} second \${second} \${counter}\`);
  return first + second;
};
const all = await Promise.all([step("a"), step("b"), step("c")]);
console.log(\`done \${all[0]} \${all[1]} \${all[2]} \${counter}\`);
`,
    deliveries: [["host.echo:b1"], ["host.echo:a1", "host.echo:c1"], ["host.echo:c2"], ["host.echo:a2"], ["host.echo:b2"]],
  },
  {
    name: "fan_out_other_arrival_order",
    source: `
let counter = 0;
const step = async (name) => {
  const first = await host.echo(name + "1");
  counter = counter + 1;
  console.log(\`\${name} first \${first} \${counter}\`);
  const second = await host.echo(name + "2");
  counter = counter * 2;
  console.log(\`\${name} second \${second} \${counter}\`);
  return first + second;
};
const all = await Promise.all([step("a"), step("b"), step("c")]);
console.log(\`done \${all[0]} \${all[1]} \${all[2]} \${counter}\`);
`,
    deliveries: [["host.echo:c1", "host.echo:b1", "host.echo:a1"], ["host.echo:a2"], ["host.echo:b2", "host.echo:c2"]],
  },
  {
    name: "operand_read_before_an_await",
    source: `
let total = 0;
const add = async (n) => {
  total = total + (await host.echo(n));
  console.log(\`added \${n} total \${total}\`);
};
await Promise.all([add(1), add(10)]);
console.log(\`total \${total}\`);
`,
    deliveries: [["host.echo:10"], ["host.echo:1"]],
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
  const value = await host.echo(item);
  console.log(\`end \${value}\`);
  return value + value;
}));
console.log(\`results \${results[0]} \${results[1]} \${results[2]}\`);
`,
    deliveries: [["host.echo:c"], ["host.echo:a"], ["host.echo:b"]],
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
  const value = await host.echo("slow");
  console.log(\`slow got \${value}\`);
  await host.boom("late");
  console.log("not reached");
};
const fast = async () => {
  const value = await host.echo("fast");
  return value;
};
const winner = await Promise.race([slow(), fast()]);
console.log(\`winner \${winner}\`);
const more = await host.echo("more");
console.log(\`more \${more}\`);
const again = await host.echo("again");
console.log(\`again \${again}\`);
`,
    deliveries: [["host.echo:fast"], ["host.echo:slow"], ["host.boom:late"], ["host.echo:more"], ["host.echo:again"]],
  },
  {
    name: "all_fails_fast",
    source: `
const ok = async (name) => {
  const value = await host.echo(name);
  console.log(\`ok \${value}\`);
  return value;
};
const bad = async () => {
  await host.boom("x");
};
try {
  await Promise.all([ok("a"), bad(), ok("b")]);
  console.log("not reached");
} catch (error) {
  console.log(\`caught \${error}\`);
}
const tail = await host.echo("tail");
console.log(\`tail \${tail}\`);
`,
    deliveries: [["host.echo:a"], ["host.boom:x"], ["host.echo:b"], ["host.echo:tail"]],
  },
  {
    name: "all_settled",
    source: `
const thrower = async () => {
  throw "thrown";
};
const failing = async () => {
  try {
    await host.boom("b");
  } catch (error) {
    throw \`\${error}\`;
  }
};
const results = await Promise.allSettled([host.echo("a"), failing(), 3, thrower()]);
for (const result of results) {
  console.log(result);
}
`,
    deliveries: [["host.boom:b"], ["host.echo:a"]],
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
    const value = await host.echo("body");
    console.log(\`body \${value}\`);
    throw "failed";
  } finally {
    const cleaned = await host.echo("cleanup");
    console.log(\`cleanup \${cleaned}\`);
  }
};
const other = async () => {
  const value = await host.echo("other");
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
    deliveries: [["host.echo:body"], ["host.echo:other", "host.echo:cleanup"]],
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
const p = host.echo("a")
  .then((value) => {
    console.log(\`then \${value}\`);
    return host.boom("b");
  })
  .catch((error) => {
    console.log(\`catch \${error}\`);
    return "recovered";
  })
  .finally(() => {
    console.log("finally");
  });
const q = host.echo("q");
console.log(\`q \${await q}\`);
console.log(\`p \${await p}\`);
`,
    deliveries: [["host.echo:a"], ["host.echo:q", "host.boom:b"]],
  },
  {
    name: "finally_keeps_the_outcome_and_waits",
    source: `
const log = (line) => console.log(line);
const failed = Promise.reject("no").finally(() => host.echo("f"));
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
    deliveries: [["host.echo:f"]],
  },
  {
    name: "arrivals_in_one_batch_keep_their_order",
    source: `
const viaPromise = async () => {
  const promise = host.echo("p");
  const value = await promise;
  console.log(\`promise \${value}\`);
};
const inPlace = async () => {
  const value = await host.echo("i");
  console.log(\`in place \${value}\`);
};
const viaAll = async () => {
  const values = await Promise.all([host.echo("x")]);
  console.log(\`all \${values[0]}\`);
};
const failing = async () => {
  try {
    await host.boom("e");
  } catch (error) {
    console.log(\`failed \${error}\`);
  }
};
await Promise.all([viaPromise(), inPlace(), viaAll(), failing()]);
`,
    deliveries: [["host.echo:x", "host.echo:p", "host.boom:e", "host.echo:i"]],
  },
  {
    name: "arrivals_in_one_batch_reversed",
    source: `
const viaPromise = async () => {
  const promise = host.echo("p");
  const value = await promise;
  console.log(\`promise \${value}\`);
};
const inPlace = async () => {
  const value = await host.echo("i");
  console.log(\`in place \${value}\`);
};
const viaAll = async () => {
  const values = await Promise.all([host.echo("x")]);
  console.log(\`all \${values[0]}\`);
};
const failing = async () => {
  try {
    await host.boom("e");
  } catch (error) {
    console.log(\`failed \${error}\`);
  }
};
await Promise.all([viaPromise(), inPlace(), viaAll(), failing()]);
`,
    deliveries: [["host.echo:i", "host.boom:e", "host.echo:p", "host.echo:x"]],
  },
  {
    name: "race_and_any_with_a_sleep",
    source: `
const timer = sleep(50);
const first = await Promise.race([timer, host.echo("r")]);
console.log(\`race \${first}\`);
const found = await Promise.any([host.boom("n1"), host.echo("y"), host.boom("n2")]);
console.log(\`any \${found}\`);
try {
  await Promise.any([host.boom("z1"), host.boom("z2")]);
} catch (error) {
  console.log(\`none \${error}\`);
}
const settled = await Promise.race([1, host.echo("unused")]);
console.log(\`settled \${settled}\`);
await sleep(5);
console.log("slept");
`,
    deliveries: [
      ["sleep:50"],
      ["host.boom:n1"],
      ["host.echo:y"],
      ["host.boom:z2"],
      ["host.boom:z1"],
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
    source: `
const member = async () => {
  await host.echo("m");
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
    deliveries: [["host.echo:m"]],
  },
];
