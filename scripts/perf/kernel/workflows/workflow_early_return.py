async def demo(value):
    if value < 0:
        return "negative"
    await perf.echo(value)
    return "done"
result = []
for n in range(-64, 64):
    result.append(await demo(n))
