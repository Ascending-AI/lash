#: Try a step, and always finish up
async def demo():
    #: Start with a placeholder result
    result = "start"
    #: Try the echo
    try:
        #: Echo a test message and keep the reply
        result = await corpus.echo("try")
    #: If the echo fails
    except Exception:
        #: Note that it failed
        result = "caught"
    #: Whatever happened
    finally:
        #: Send a closing echo
        await corpus.echo("finally")
    #: Give back the result
    return result
