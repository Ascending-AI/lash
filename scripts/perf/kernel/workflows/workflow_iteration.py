async def demo(items):
    out = []
    for item in items:
        out.append(await perf.echo(item))
    return out
items = []
for n in range(128):
    items.append("item-" + str(n))
result = await demo(items)
