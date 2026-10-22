/** Echo every item */
export async function demo(items: string[]): Promise<string[]> {
  /** Start an empty list of replies */
  const out = [];
  /** Go through each item */
  for (const item of items) {
    /** Echo the item and keep the reply */
    out.push(await corpus.echo(item));
  }
  /** Give back the replies */
  return out;
}
