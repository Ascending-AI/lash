export async function demo(items) {
  let total = 0.0;
  for (const item of items) {
    total = helper_num_add(total, helper_num_to_float(item));
  }
  return k.returnVia(total, helper_ts_promise_adopt);
}
async function main() {
  let demo = declared.demo;
}
