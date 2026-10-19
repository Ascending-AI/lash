/** Give back the number it is given */
export const arrow = async (value: number) => value;
/** Answer from the top level */
async function declared() { return "top"; }
if (true) {
  async function local() { return "block"; }
  const alsoLocal = async () => "block";
}
