import asyncio


async def digest(items):
    sent = 0
    for item in items:
        await asyncio.sleep(6)
        await display.show_message(text=item["title"])
        sent = sent + 1
    return {"sent": sent}
