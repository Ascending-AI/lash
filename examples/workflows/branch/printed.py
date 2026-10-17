async def demo(flag):
    if flag:
        return (await helper_py_reply((await tools.corpus.echo("yes"))))
    else:
        pass
    return "no"
def main():
    demo = declared.demo
