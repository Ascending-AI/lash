# examples/workflows/early-return: pure echo replaces corpus.echo/await.
def pure_echo(value):
    return value
def demo(value):
    if value < 0:
        return "negative"
    pure_echo(value)
    return "done"
result = []
for n in range(-64, 64):
    result.append(demo(n))
