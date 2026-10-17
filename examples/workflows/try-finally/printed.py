async def demo():
    result = "start"
    try:
        result = (await helper_py_reply((await tools.corpus.echo("try"))))
    except BaseException as t1:
        if (await helper_py_exc_is_exception(t1, ())):
            result = "caught"
        else:
            raise t1
    finally:
        await helper_py_reply((await tools.corpus.echo("finally")))
    return result
def main():
    demo = declared.demo
