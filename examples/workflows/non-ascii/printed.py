async def demo(name):
    größe = "日本 🦀"
    return (await helper_py_reply((await tools.corpus.echo((await helper_py_add(größe, name))))))
def main():
    demo = declared.demo
