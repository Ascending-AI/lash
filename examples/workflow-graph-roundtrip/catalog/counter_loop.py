import asyncio


async def counter_loop():
    await display.set_status(key="counter", value="running")
    await display.set_progress(pct=5)
    count = 0
    reached = 20
    while count < 3:
        await display.add_item(list="counts", item=count)
        await display.set_progress(pct=reached)
        count += 1
        reached += 20
        await asyncio.sleep(0.25)
    for pct in [70, 85, 100]:
        await display.set_progress(pct=pct)
        await asyncio.sleep(0.3)
    await display.highlight(target="progress")
    await display.set_status(key="counter", value="complete")
    await display.show_message(text="Counter complete")
    return count
