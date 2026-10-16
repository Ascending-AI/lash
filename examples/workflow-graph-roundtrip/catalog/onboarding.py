import asyncio


async def onboarding():
    await display.set_status(key="phase", value="starting")
    await asyncio.sleep(0.4)
    await display.show_message(text="Welcome to the workflow graph")
    await display.set_light(name="ready", state="green")
    await asyncio.sleep(0.4)
    await display.set_progress(pct=35)
    approval = await host.approval()
    await display.highlight(target="checklist")
    await display.add_item(list="steps", item="Approved")
    for _ in range(2):
        await display.add_item(list="steps", item="Loop item")
        await asyncio.sleep(0.25)
    await asyncio.sleep(0.4)
    await display.set_progress(pct=100)
    await display.set_light(name="complete", state="blue")
    return approval
