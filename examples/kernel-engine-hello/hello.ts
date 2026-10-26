export async function main(name: string): Promise<string> {
  return await host.greet(name);
}
