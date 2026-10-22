/** Measure each word */
export async function demo(items: string[]): Promise<number[]> {
  /** Describe how to measure one word */
  const size = (item: string) => item.length;
  /** Measure every word in the list */
  return items.map(size);
}
