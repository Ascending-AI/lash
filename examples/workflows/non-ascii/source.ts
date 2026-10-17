export async function demo(name: string) {
  const größe = "日本 🦀";
  return await corpus.echo(größe + name);
}
