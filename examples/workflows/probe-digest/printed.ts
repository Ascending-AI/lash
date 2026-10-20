/** Pace a digest of topics */
export async function digest(topics) {
  /** Start counting at zero */
  let seen = 0.0;
  /** Go through each topic */
  for (const topic of topics) {
    /** Wait seven seconds */
    ((t2) => (undefined))((await await sleep(7000.0)));
    let t3 = undefined;
    if (helper_same(helper_kind(topic), "record")) {
      if (!(helper_record_contains(topic, "__lash_brand"))) {
        t3 = topic.urgent;
      } else {
        t3 = await helper_ts_read(topic, "urgent");
      }
    } else {
      t3 = await helper_ts_read(topic, "urgent");
    }
    /** Check whether the topic is urgent */
    if (helper_ts_to_boolean(t3)) {
      /** Wait one more second */
      ((t4) => (undefined))((await await sleep(1000.0)));
    }
    /** Not urgent */
    else {
      /** Wait two more seconds */
      ((t5) => (undefined))((await await sleep(2000.0)));
    }
    /** Count the topic as seen */
    seen = seen + 1.0;
  }
  /** Report how many topics were seen */
  return k.returnVia({seen: seen}, helper_ts_promise_adopt);
}
async function main() {
  let digest = declared.digest;
}
