/** Echo two values at once */
export async function demo(): Promise<string[]> {
  /** Describe how to echo one value */
  const work = async (value: string) => await corpus.echo(value);
  /** Echo "a" and "b" side by side and wait for both */
  return await Promise.all([work("a"), work("b")]);
}
