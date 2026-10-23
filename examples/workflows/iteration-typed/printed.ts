/** Add up the numbers */
export async function demo(
  /** The numbers to add */
  items: number[],
): Promise<number> {
  /** Start the total at zero */
  let total = 0;
  /** Go through each number */
  for (const item of items) {
    /** Add it to the total */
    total = num.add(total, num.to_float(item));
  }
  /** Give back the total */
  return total;
}
