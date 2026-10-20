/** Summarize the inbox */
export async function summarize_inbox() {
  /** Describe how to summarize and show one email */
  let announce = ["ts.function", "", "announce", 1.0, async function(this3, args3) {
    return (await helper_ts_promise_start(async function(this2, args2) {
      let t1 = (helper_list_len(args2) < 1.0 ? (await helper_ts_pad(args2, 1.0)) : args2);
      let email = t1[0.0];
      let t3 = undefined;
      if (helper_same(helper_kind(email), "record")) {
        if (!(helper_record_contains(email, "__lash_brand"))) {
          t3 = email.snippet;
        } else {
          t3 = await helper_ts_read(email, "snippet");
        }
      } else {
        t3 = await helper_ts_read(email, "snippet");
      }
      let summary = (await await llm.query((await helper_ts_boundary({task: "Summarize this email", inputs: {snippet: t3}}))));
      await await display.showMessage((await helper_ts_boundary({text: summary})));
      return k.returnVia(summary, helper_ts_promise_adopt);
    }, this3, helper_list_copy(args3)));
  }, {}] as const;
  /** Get the three newest emails */
  let emails = (await await gmail.listRecent((await helper_ts_boundary({count: 3.0}))));
  /** Start an empty list of summaries */
  let summaries = [];
  /** Go through each email */
  for (const email of emails) {
    /** Summarize it, show it and keep the summary */
    summaries[helper_list_len(summaries)] = (await ((await (async (t8) => ((await (async (t9) => ((await (async (t10) => ((await (t10)(undefined, t9))))(t8[4.0]))))([email]))))(announce))));
  }
  /** Give back the summaries */
  return k.returnVia(summaries, helper_ts_promise_adopt);
}
async function main() {
  let summarize_inbox = declared.summarize_inbox;
}
