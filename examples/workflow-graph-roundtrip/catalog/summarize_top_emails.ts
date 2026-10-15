export async function summarize_top_emails() {
  const emails = await gmail.list_recent({ count: 5 });
  const summaries = [];
  for (const email of emails) {
    const summary = await llm.query({
      task: "Summarize this email in one sentence",
      inputs: { sender: email.sender, subject: email.subject, snippet: email.snippet },
    });
    summaries.push(summary);
  }
  const digest = await llm.query({
    task: "Format these five summaries as a concise numbered email digest",
    inputs: { summaries },
  });
  await display.show_message({ text: digest });
  return digest;
}
