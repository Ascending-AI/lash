async def demo(flag):
    async def t3(flag):
        if flag is k.absent:
            await helper_py_arg_missing("demo")
        else:
            pass
        if flag:
            t1 = await tools.corpus.echo("yes")
            t2 = await helper_py_reply(t1)
            return t2
        else:
            pass
        return "no"
    t4 = await t3(flag)
    return t4
def main():
    demo = k.absent
    demo = declared.demo
