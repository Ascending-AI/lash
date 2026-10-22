/** Grüße aus 日本 🦀 */
export async function demo(name: string): Promise<string> {
  /** Pick the greeting: 日本 🦀 */
  const größe = "日本 🦀";
  /** Echo the greeting with the name */
  return await corpus.echo(größe + name);
}
