/** Answer yes or no */
export async function demo(flag) {
  /** Check whether the answer is yes */
  if (flag) {
    /** Send back a yes */
    return k.returnVia((await await corpus.echo("yes") as KernelType<"json">), helper_ts_promise_adopt);
  }
  /** Otherwise answer no */
  return k.returnVia("no", helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
