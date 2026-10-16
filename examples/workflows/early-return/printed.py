async def demo(value):
    if value < 0:
        return "negative"
    else:
        pass
    t1 = value
    t2 = await tools.corpus.echo(t1)
    await helper_py_reply(t2)
    return "done"
def main():
    demo = k.absent
    demo = declared.demo
