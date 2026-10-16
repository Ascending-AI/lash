export async function demo(items: number[]) {
  let total = 0;
  for (const item of items) { total += item; }
  return total;
}
