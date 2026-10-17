export async function demo(items) {
  const out = [];
  for (const item of items) { out.push(await corpus.echo(item)); }
  return out;
}
