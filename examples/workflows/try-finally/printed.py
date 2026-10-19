#: Try a step, and always finish up
async def demo():
    #: Start with a placeholder result
    result = "start"
    #: Try the echo
    try:
        #: Echo a test message and keep the reply
        result = (await helper_py_reply((await tools.corpus.echo("try"))))
    #: If the echo fails
    except BaseException as t1:
        if (await helper_py_exc_is_exception(t1, ())):
            #: Note that it failed
            result = "caught"
        else:
            raise t1
    #: Whatever happened
    finally:
        #: Send a closing echo
        await helper_py_reply((await tools.corpus.echo("finally")))
    #: Give back the result
    return result
def main():
    demo = declared.demo
