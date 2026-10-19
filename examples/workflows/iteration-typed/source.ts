/** Add up the numbers */
export async function demo(items: number[]) {
  /** Start the total at zero */
  let total = 0;
  /** Go through each number */
  for (const item of items) {
    /** Add it to the total */
    total += item;
  }
  /** Give back the total */
  return total;
}
