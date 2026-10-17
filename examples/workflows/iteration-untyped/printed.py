async def demo(items):
    out = []
    for item in items:
        out.append((await helper_py_reply((await tools.corpus.echo(item)))))
    return out
def main():
    demo = declared.demo
