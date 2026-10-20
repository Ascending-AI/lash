/** Search the web, counting failures */
export async function resilient_search() {
  /** Choose the result limits to try */
  let limits = [2.0, 50.0, 1.0];
  /** Start counting failures at zero */
  let failures = 0.0;
  /** Try each limit */
  for (const limit of limits) {
    /** Search and show the top result */
    try {
      /** Search the web for workflow engines */
      let found = (await await tools.web.search((await helper_ts_boundary({query: "workflow engines", limit: limit}))));
      let t2 = undefined;
      if (helper_same(helper_kind(found), "record")) {
        if (!(helper_record_contains(found, "__lash_brand"))) {
          t2 = found.results;
        } else {
          t2 = await helper_ts_read(found, "results");
        }
      } else {
        t2 = await helper_ts_read(found, "results");
      }
      let t3 = undefined;
      if (helper_same(helper_kind(t2), "list")) {
        if (0.0 < helper_list_len(t2)) {
          t3 = t2[0.0];
          if (helper_same(t3, [] as const)) {
            t3 = undefined;
          }
        } else {
          t3 = await helper_ts_get_computed(t2, 0.0);
        }
      } else {
        t3 = await helper_ts_get_computed(t2, 0.0);
      }
      let t4 = undefined;
      if (helper_same(helper_kind(t3), "record")) {
        if (!(helper_record_contains(t3, "__lash_brand"))) {
          t4 = t3.title;
        } else {
          t4 = await helper_ts_read(t3, "title");
        }
      } else {
        t4 = await helper_ts_read(t3, "title");
      }
      /** Show the top result's title */
      await await tools.display.show_message((await helper_ts_boundary({text: t4})));
    }
    /** If the search fails */
    catch (error) {
      /** Count the failure */
      failures = failures + 1.0;
      /** Say that a search failed */
      await await tools.display.set_status((await helper_ts_boundary({key: "search", value: "A search failed"})));
    }
  }
  /** Report how many searches failed */
  return k.returnVia({failures: failures}, helper_ts_promise_adopt);
}
async function main() {
  let resilient_search = declared.resilient_search;
}
