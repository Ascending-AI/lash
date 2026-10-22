/** Give back the number it is given */
export async function arrow(value: number): Promise<number> {
  return value;
}

/** Answer from the top level */
export async function declared(): Promise<string> {
  return "top";
}

if (true) {
  const local = async () => "block";
  const alsoLocal = async () => "block";
}
