import asyncio
def attempt(which):
    if which == "interrupt":
        raise KeyboardInterrupt()
    if which == "cancel":
        raise asyncio.CancelledError()
    raise ValueError(which)
for which in ["interrupt", "cancel", "value"]:
    try:
        try:
            attempt(which)
        except (Exception, asyncio.CancelledError):
            print("first inner", which)
    except BaseException:
        print("first outer", which)
    try:
        try:
            attempt(which)
        except (Exception, KeyboardInterrupt):
            print("second inner", which)
    except BaseException:
        print("second outer", which)
