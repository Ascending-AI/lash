async def file_emails():
    emails = await gmail.list_recent(count=3)
    filed = 0
    for list_name in ["today", "this week"]:
        for email in emails:
            await display.add_item(list=list_name, item=email["subject"])
            filed += 1
    return {"filed": filed}
