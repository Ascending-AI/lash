/** Summarize the inbox */
export async function summarize_inbox() {
  /** Describe how to summarize and show one email */
  async function announce(email) {
    const summary = await llm.query({
      task: "Summarize this email",
      inputs: { snippet: email.snippet },
    });
    await display.showMessage({ text: summary });
    return summary;
  }

  /** Get the three newest emails */
  const emails = await gmail.listRecent({ count: 3 });
  /** Start an empty list of summaries */
  const summaries = [];
  /** Go through each email */
  for (const email of emails) {
    /** Summarize it, show it and keep the summary */
    summaries.push(await announce(email));
  }
  /** Give back the summaries */
  return summaries;
}
