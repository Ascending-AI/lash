async def demo(value):
    async def t4(value):
        if value is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        if value < 0:
            return "negative"
        else:
            pass
        t1 = value
        t2 = await tools.echo(t1)
        await helper_py_reply(t2)
        return "done"
    t5 = await t4(value)
    return t5
def main():
    demo = k.absent
    demo = declared.demo
