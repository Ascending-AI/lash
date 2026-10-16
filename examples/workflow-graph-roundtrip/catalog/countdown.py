async def countdown():
    count = 0
    while True:
        count += 1
        await display.set_progress(pct=count * 25)
        if count >= 4:
            break
        await display.show_message(text=f"Tick {count}")
    await display.set_status(key="countdown", value="done")
    return {"count": count}
