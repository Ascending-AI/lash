/** Echo every item */
export async function demo(items) {
  /** Start an empty list of replies */
  let out = [];
  /** Go through each item */
  for (const item of items) {
    /** Echo the item and keep the reply */
    out[helper_list_len(out)] = (await await corpus.echo((await helper_ts_boundary(item))) as KernelType<"json">);
  }
  /** Give back the replies */
  return k.returnVia(out, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
