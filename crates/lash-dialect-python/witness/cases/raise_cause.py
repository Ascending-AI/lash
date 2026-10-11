try:
    raise ValueError("x") from 1
except TypeError as error:
    print("TypeError", error)
except ValueError as error:
    print("ValueError", error.__cause__)
made = KeyError("k")
try:
    raise made from "text"
except TypeError as error:
    print("TypeError", error)
try:
    raise ValueError("y") from None
except ValueError as error:
    print("ValueError", error, error.__cause__)
