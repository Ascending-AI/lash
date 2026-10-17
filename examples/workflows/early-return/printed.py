async def demo(value):
    if value < 0:
        return "negative"
    else:
        pass
    await helper_py_reply((await tools.corpus.echo(value)))
    return "done"
def main():
    demo = declared.demo
