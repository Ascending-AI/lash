#: Say hello
async def demo():
    #: Send a greeting and give back the reply
    return (await helper_py_reply((await tools.corpus.echo("hello"))))
def main():
    demo = declared.demo
