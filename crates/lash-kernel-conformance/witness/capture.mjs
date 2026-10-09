// Trusted witness source; this is not a guest runtime or sandbox.
import {readFile, writeFile} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
import {setImmediate as tick} from 'node:timers/promises';

function datum(value) {
  if (value === null) return 'null';
  if (value === undefined) return 'absent';
  if (typeof value === 'boolean') return {bool: value};
  if (typeof value === 'bigint') return {int: String(value)};
  if (typeof value === 'number') {
    let text = Number.isNaN(value) ? 'nan' : value === Infinity ? 'inf'
      : value === -Infinity ? '-inf' : Object.is(value, -0) ? '-0.0' : String(value);
    if (/^-?\d+$/.test(text)) text += '.0';
    return {float: text};
  }
  if (typeof value === 'string') return {text: value};
  if (value instanceof Uint8Array) return {bytes: Buffer.from(value).toString('hex')};
  if (Array.isArray(value)) return {list: value.map(datum)};
  if (value instanceof Map) return {map: [...value].map(([key, item]) => [datum(key), datum(item)])};
  if (value instanceof Set) return {set: [...value].map(datum)};
  if (typeof value === 'object') return {record: Object.entries(value).map(([key, item]) => [key, datum(item)])};
  throw new TypeError(`unsupported witness datum ${typeof value}`);
}

const [source, scriptPath, output] = process.argv.slice(2);
const script = JSON.parse(await readFile(scriptPath, 'utf8'));
const program = await import(pathToFileURL(source));
const pending = [], trace = [], prints = [], delivered = new Set();
function tool(name, ...args) {
  const id = pending.length, row = script.tools[id];
  if (!row || row.name !== name || JSON.stringify(row.args) !== JSON.stringify(args)) {
    throw new Error(`unscripted tool ${id}: ${name}`);
  }
  trace.push({phase: 'requested', id, tool: name, args: args.map(datum)});
  return new Promise((resolve, reject) => pending.push({resolve, reject}));
}
let ended = false, result;
const completion = Promise.resolve().then(() => program.main({tool, print: value => prints.push(datum(value))}))
  .then(value => { ended = true; result = {returned: datum(value)}; }, error => {
    ended = true;
    result = {raised: {kind: error?.kind ?? error?.name ?? 'throw', message: error?.message ?? String(error), data: datum(error?.data ?? null)}};
  });
for (const batch of script.deliveries) {
  await tick();
  if (ended) throw new Error('program ended before delivery script was consumed');
  for (const id of batch) {
    if (!pending[id] || delivered.has(id)) throw new Error(`unknown or repeated delivery ${id}`);
    delivered.add(id);
    trace.push({phase: 'delivered', id});
    const row = script.tools[id];
    if (row.error) {
      const error = Object.assign(new Error(row.error.message), row.error);
      pending[id].reject(error);
    } else pending[id].resolve(row.value);
  }
}
await tick();
if (!ended) throw new Error('program waits beyond its script');
await completion;
if (pending.length !== script.tools.length || delivered.size !== script.tools.length) {
  throw new Error('unconsumed tools or outcomes');
}
await writeFile(output, JSON.stringify({end: result, prints, trace}, null, 2) + '\n');
