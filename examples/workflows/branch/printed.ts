/** Answer yes or no */
export async function demo(flag: boolean) {
  /** Check whether the answer is yes */
  if (flag) {
    /** Send back a yes */
    return await corpus.echo("yes");
  }
  /** Otherwise answer no */
  return "no";
}
