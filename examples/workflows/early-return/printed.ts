/** Echo a value unless it is negative */
export async function demo(value) {
  /** Check for a negative value */
  if (helper_num_to_float(value) < 0.0) {
    /** Stop and report that it is negative */
    return k.returnVia("negative", helper_ts_promise_adopt);
  }
  /** Echo the value */
  await await corpus.echo((await helper_ts_boundary(value))) as KernelType<"json">;
  /** Report that it is done */
  return k.returnVia("done", helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
