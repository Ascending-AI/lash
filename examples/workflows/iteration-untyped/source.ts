export async function demo(items) {
  const out = [];
  for (const item of items) { out.push(await echo(item)); }
  return out;
}
