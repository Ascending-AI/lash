def helper():
    raise
try:
    try:
        raise ValueError("v")
    except ValueError:
        helper()
except ValueError as error:
    print("helper ValueError", error)
except RuntimeError as error:
    print("helper RuntimeError", error)
try:
    try:
        raise ValueError("w")
    finally:
        try:
            raise
        except RuntimeError:
            print("RuntimeError in finally")
except ValueError as error:
    print("finally ValueError", error)
try:
    helper()
except RuntimeError as error:
    print("no active exception:", error)
