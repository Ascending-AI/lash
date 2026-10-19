/** Send a paced inbox digest */
export async function digest(items) {
  /** Start counting at zero */
  let sent = 0.0;
  /** Go through each item */
  for (const item of items) {
    /** Wait six seconds */
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
    /** Show the item's title */
    await await tools.display.show_message((await helper_ts_boundary({text: t3})));
    /** Count the item as sent */
    sent = sent + 1.0;
  }
  /** Report how many items were sent */
  return k.returnVia({sent: sent}, helper_ts_promise_adopt);
}
async function main() {
  let digest = declared.digest;
}
