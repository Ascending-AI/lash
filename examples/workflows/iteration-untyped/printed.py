async def demo(items):
    async def t8(items):
        item = k.absent
        out = k.absent
        if items is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        out = []
        for t2 in items:
            item = t2
            t6 = out
            t3 = item
            t4 = await tools.corpus.echo(t3)
            t5 = await helper_py_reply(t4)
            t6.append(t5)
        return out
    t9 = await t8(items)
    return t9
def main():
    demo = k.absent
    demo = declared.demo
