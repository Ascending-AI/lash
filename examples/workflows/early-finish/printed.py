async def main():
    with k.finish():
        with k.bind("t1"):
            (await tools.finish("done"))
            None
    await helper_py_reply((await tools.corpus.echo("unreachable", result="json")))
