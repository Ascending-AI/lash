import asyncio


async def branching_approval():
    await display.set_status(key="approval", value="waiting")
    await display.highlight(target="approval")
    await display.show_message(text="Approval requested")
    decision = await host.approval()
    if decision["approved"]:
        await display.set_status(key="approval", value="approved")
        await display.set_light(name="approved", state="green")
        await display.show_message(text="Request approved")
    else:
        await display.set_status(key="approval", value="rejected")
        await display.set_light(name="rejected", state="red")
        await display.show_message(text="Request rejected")
    await asyncio.sleep(0.4)
    await display.highlight(target="result")
    return decision
