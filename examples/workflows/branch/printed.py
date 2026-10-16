async def demo(flag):
    if flag:
        t1 = await tools.corpus.echo("yes")
        t2 = await helper_py_reply(t1)
        return t2
    else:
        pass
    return "no"
def main():
    demo = k.absent
    demo = declared.demo
