export async function demo(items: string[]) {
  const size = (item: string) => item.length;
  return items.map(size);
}
