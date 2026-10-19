/** Wait a moment, then report */
export async function demo() {
  /** Pause briefly */
  await sleep(5);
  /** Say it is awake */
  return "awake";
}
