async def main():
    with k.finish():
        with k.bind("t1"):
            (await control.finish("done"))
            None
    await helper_py_reply((await corpus.echo("unreachable", result="json")))
