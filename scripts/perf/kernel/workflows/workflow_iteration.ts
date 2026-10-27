async function demo(items) {
  const out = [];
  for (const item of items) { out.push(await perf.echo(item)); }
  return out;
}
const items = [];
for (let n = 0; n < 128; n++) { items.push("item-" + String(n)); }
const result = await demo(items);
