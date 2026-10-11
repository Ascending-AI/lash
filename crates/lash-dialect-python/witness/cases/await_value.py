try:
    print("got", await 1)
except TypeError as error:
    print("TypeError", error)
def plain():
    return 2
try:
    print("got", await plain())
except TypeError as error:
    print("TypeError", error)
