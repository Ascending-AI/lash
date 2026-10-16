export const arrow = async (value: number) => value;
async function declared() { return "top"; }
if (true) {
  async function local() { return "block"; }
  const alsoLocal = async () => "block";
}
