export async function summarize_inbox() {
  async function announce(email) {
    const summary = await llm.query({
      task: "Summarize this email",
      inputs: { snippet: email.snippet },
    });
    await display.show_message({ text: summary });
    return summary;
  }

  const emails = await gmail.list_recent({ count: 3 });
  const summaries = [];
  for (const email of emails) {
    summaries.push(await announce(email));
  }
  return summaries;
}
