async def demo(name):
    async def t4(name):
        größe = k.absent
        if name is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        größe = "日本 🦀"
        t1 = await helper_py_add(größe, name)
        t2 = await tools.echo(t1)
        t3 = await helper_py_reply(t2)
        return t3
    t5 = await t4(name)
    return t5
def main():
    demo = k.absent
    demo = declared.demo
