# examples/workflows/iteration-untyped: pure echo replaces corpus.echo/await.
def pure_echo(item):
    return item
def demo(items):
    out = []
    for item in items:
        out.append(pure_echo(item))
    return out
items = []
for n in range(128):
    items.append("item-" + str(n))
result = demo(items)
