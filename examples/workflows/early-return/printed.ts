/** Echo a value unless it is negative */
export async function demo(value: number) {
  /** Check for a negative value */
  if (value < 0) {
    /** Stop and report that it is negative */
    return "negative";
  }
  /** Echo the value */
  await corpus.echo(value);
  /** Report that it is done */
  return "done";
}
