saved = "old"
try:
    raise ValueError("x")
except ValueError as saved:
    print("inside", saved)
try:
    print(saved)
except NameError as error:
    print("NameError", error)
def leave():
    try:
        raise KeyError("k")
    except KeyError as caught:
        print("caught", caught)
    try:
        return caught
    except UnboundLocalError as error:
        return "UnboundLocalError " + str(error)
print(leave())
