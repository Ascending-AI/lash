class AppError(Exception):
    pass
class NotFound(AppError):
    pass
def risky(kind):
    if kind == "value":
        raise ValueError("bad value")
    if kind == "found":
        raise NotFound("no such thing", 404)
    if kind == "bare":
        raise RuntimeError
    return "fine"
for kind in ["ok", "value", "found", "bare"]:
    try:
        print(risky(kind))
    except AppError as error:
        print("app", type(error).__name__, error, error.args, repr(error))
    except (ValueError, TypeError) as error:
        print("value", error, error.args)
    except Exception as error:
        print("other", repr(error), str(error) == "")
    else:
        print("no error")
    finally:
        print("finally", kind)
def chained():
    try:
        {}["k"]
    except KeyError as error:
        raise AppError("lookup failed") from error
try:
    chained()
except AppError as error:
    print(error, repr(error.__cause__), type(error.__cause__).__name__)
def reraise():
    try:
        raise ValueError("again")
    except ValueError:
        print("seen")
        raise
try:
    reraise()
except ValueError as error:
    print("outer", error)
def order():
    try:
        return "try"
    finally:
        print("finally runs before the return")
print(order())
def override():
    for attempt in range(3):
        try:
            if attempt < 2:
                continue
            return attempt
        finally:
            print("leaving", attempt)
print(override())
def swallow():
    try:
        raise ValueError("lost")
    finally:
        return "the return in finally wins"
print(swallow())
try:
    try:
        raise NotFound("inner")
    except ValueError:
        print("not here")
except Exception as error:
    print("propagated", error)
try:
    int("x")
except ValueError as error:
    print(error)
try:
    None + 1
except TypeError as error:
    print(error)
try:
    print(defined_later)
except NameError as error:
    print(error)
defined_later = 1
def unbound():
    print(later)
    later = 1
try:
    unbound()
except UnboundLocalError as error:
    print(error)
error = ValueError("made", "not raised")
print(repr(error), error.args, isinstance(error, int))
try:
    await boom("tool failed")
except Exception as error:
    print("tool", type(error).__name__, error)
try:
    raise AppError("first")
except AppError:
    try:
        raise ValueError("second")
    except ValueError as inner:
        print("nested", inner)
raise NotFound("at the end")
