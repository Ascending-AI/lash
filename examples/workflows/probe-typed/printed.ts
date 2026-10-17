export async function digest(topics) {
  let seen = 0.0;
  for (const topic of topics) {
    ((t2) => (undefined))((await await sleep(7000.0)));
    let t3 = undefined;
    if (helper_same(helper_kind(topic), "record")) {
      if (!(helper_same(topic.__ts_brand, [] as const))) {
        t3 = topic.urgent;
      } else {
        t3 = await helper_ts_read(topic, "urgent");
      }
    } else {
      t3 = await helper_ts_read(topic, "urgent");
    }
    if (t3) {
      ((t4) => (undefined))((await await sleep(1000.0)));
    } else {
      ((t5) => (undefined))((await await sleep(2000.0)));
    }
    seen = seen + 1.0;
  }
  return k.returnVia({seen: seen}, helper_ts_promise_adopt);
}
async function main() {
  let digest = declared.digest;
}
