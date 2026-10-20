#: Grüße aus 日本 🦀
async def demo(name):
    #: Pick the greeting: 日本 🦀
    größe = "日本 🦀"
    #: Echo the greeting with the name
    return (await helper_py_reply((await tools.corpus.echo(größe + name, result="json"))))
def main():
    demo = declared.demo
