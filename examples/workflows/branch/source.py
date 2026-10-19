#: Answer yes or no
async def demo(flag: bool):
    #: Check whether the answer is yes
    if flag:
        #: Send back a yes
        return await corpus.echo("yes")
    #: Otherwise answer no
    return "no"
