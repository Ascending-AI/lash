async def demo(value: int):
    if value < 0:
        return "negative"
    await corpus.echo(value)
    return "done"
