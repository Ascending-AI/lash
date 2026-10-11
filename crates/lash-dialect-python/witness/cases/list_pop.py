xs = [10, 20]
try:
    xs.pop(1.0)
except TypeError as error:
    print("TypeError", error)
print(xs)
try:
    xs.pop(0, 99)
except TypeError as error:
    print("TypeError", error)
print(xs)
print(xs.pop(-2), xs)
