async def announce(email):
    summary = await llm.query(
        task="Summarize this email",
        inputs={"snippet": email["snippet"]},
    )
    await display.show_message(text=summary)
    return summary


async def summarize_inbox():
    emails = await gmail.list_recent(count=3)
    summaries = []
    for email in emails:
        summaries.append(await announce(email))
    return summaries
