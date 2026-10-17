export async function summarize_inbox() {
  let announce = ["ts.function", "", "announce", 1.0, async function(this3, args3) {
    return (await helper_ts_promise_start(async function(this2, args2) {
      let t1 = (helper_list_len(args2) < 1.0 ? (await helper_ts_pad(args2, 1.0)) : args2);
      let email = t1[0.0];
      let t3 = undefined;
      if (helper_same(helper_kind(email), "record")) {
        if (!(helper_same(email.__ts_brand, [] as const))) {
          t3 = email.snippet;
        } else {
          t3 = await helper_ts_read(email, "snippet");
        }
      } else {
        t3 = await helper_ts_read(email, "snippet");
      }
      let summary = (await await tools.llm.query((await helper_ts_boundary({task: "Summarize this email", inputs: {snippet: t3}}))));
      await await tools.display.show_message((await helper_ts_boundary({text: summary})));
      return k.returnVia(summary, helper_ts_promise_adopt);
    }, this3, helper_list_copy(args3)));
  }, {}] as const;
  let emails = (await await tools.gmail.list_recent((await helper_ts_boundary({count: 3.0}))));
  let summaries = [];
  for (const email of emails) {
    summaries[helper_list_len(summaries)] = (await ((await (async (t8) => ((await (async (t9) => ((await (async (t10) => ((await (t10)(undefined, t9))))(t8[4.0]))))([email]))))(announce))));
  }
  return k.returnVia(summaries, helper_ts_promise_adopt);
}
async function main() {
  let summarize_inbox = declared.summarize_inbox;
}
