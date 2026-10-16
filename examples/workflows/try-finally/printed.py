async def demo():
    result = k.absent
    result = "start"
    try:
        t4 = await tools.corpus.echo("try")
        result = await helper_py_reply(t4)
    except BaseException as t1:
        t6 = ()
        t7 = await helper_py_exc_is_exception(t1, t6)
        if t7:
            result = "caught"
        else:
            raise t1
    finally:
        t2 = await tools.corpus.echo("finally")
        await helper_py_reply(t2)
    return result
def main():
    demo = k.absent
    demo = declared.demo
