export async function demo() {
  const work = async (value: string) => await corpus.echo(value);
  return await Promise.all([work("a"), work("b")]);
}
