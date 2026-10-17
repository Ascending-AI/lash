export async function demo(value: number) {
  if (value < 0) { return "negative"; }
  await corpus.echo(value);
  return "done";
}
