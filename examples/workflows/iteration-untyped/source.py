async def demo(items):
    out = []
    for item in items:
        out.append(await echo(item))
    return out
