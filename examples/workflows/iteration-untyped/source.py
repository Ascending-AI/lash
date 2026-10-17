async def demo(items):
    out = []
    for item in items:
        out.append(await corpus.echo(item))
    return out
