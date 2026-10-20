#: Say hello
async def demo():
    #: Send a greeting and give back the reply
    return (await helper_py_reply((await corpus.echo("hello", result="json"))))
def main():
    demo = declared.demo
