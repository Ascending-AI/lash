/** Give back the number it is given */
export const arrow = async (value: number): Promise<number> => value;
/** Answer from the top level */
async function declared(): Promise<string> { return "top"; }
if (true) {
  async function local() { return "block"; }
  const alsoLocal = async () => "block";
}
