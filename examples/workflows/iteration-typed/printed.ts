/** Add up the numbers */
export async function demo(items) {
  /** Start the total at zero */
  let total = 0.0;
  /** Go through each number */
  for (const item of items) {
    /** Add it to the total */
    total = helper_num_add(total, helper_num_to_float(item));
  }
  /** Give back the total */
  return k.returnVia(total, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
