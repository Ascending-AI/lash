async def unread_subjects():
    emails = await gmail.list_recent(count=6)
    subjects = [email["subject"] for email in emails if email["unread"]]
    for subject in subjects:
        await display.add_item(list="unread", item=subject)
    return {"count": len(subjects), "subjects": subjects}
