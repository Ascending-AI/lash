export async function demo(flag: boolean) {
  if (flag) { return await corpus.echo("yes"); }
  return "no";
}
