async def demo(name):
    größe = k.absent
    größe = "日本 🦀"
    t1 = await helper_py_add(größe, name)
    t2 = await tools.corpus.echo(t1)
    t3 = await helper_py_reply(t2)
    return t3
def main():
    demo = k.absent
    demo = declared.demo
