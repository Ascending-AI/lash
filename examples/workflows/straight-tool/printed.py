async def demo():
    async def t3():
        t1 = await tools.echo("hello")
        t2 = await helper_py_reply(t1)
        return t2
    t4 = await t3()
    return t4
def main():
    demo = k.absent
    demo = declared.demo
