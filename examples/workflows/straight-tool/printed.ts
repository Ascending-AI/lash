/** Say hello */
export async function demo(): Promise<string> {
  /** Send a greeting and give back the reply */
  return await corpus.echo("hello");
}
