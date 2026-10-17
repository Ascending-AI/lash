async def demo():
    return (await helper_py_reply((await tools.corpus.echo("hello"))))
def main():
    demo = declared.demo
