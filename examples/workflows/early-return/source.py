async def demo(value: int):
    if value < 0:
        return "negative"
    await echo(value)
    return "done"
