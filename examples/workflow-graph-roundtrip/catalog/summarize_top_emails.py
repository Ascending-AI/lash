async def summarize_top_emails():
    emails = await gmail.list_recent(count=5)
    summaries = []
    for email in emails:
        summary = await llm.query(
            task="Summarize this email in one sentence",
            inputs={
                "sender": email["sender"],
                "subject": email["subject"],
                "snippet": email["snippet"],
            },
        )
        summaries.append(summary)
    digest = await llm.query(
        task="Format these five summaries as a concise numbered email digest",
        inputs={"summaries": summaries},
    )
    await display.show_message(text=digest)
    return digest
