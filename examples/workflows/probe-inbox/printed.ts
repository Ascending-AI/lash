export async function digest(items) {
  let sent = 0.0;
  for (const item of items) {
    ((t2) => (undefined))((await await sleep(6000.0)));
    let t3 = undefined;
    if (helper_same(helper_kind(item), "record")) {
      if (!(helper_same(item.__ts_brand, [] as const))) {
        t3 = item.title;
      } else {
        t3 = await helper_ts_read(item, "title");
      }
    } else {
      t3 = await helper_ts_read(item, "title");
    }
    await await tools.display.show_message((await helper_ts_boundary({text: t3})));
    sent = sent + 1.0;
  }
  return k.returnVia({sent: sent}, helper_ts_promise_adopt);
}
async function main() {
  let digest = declared.digest;
}
