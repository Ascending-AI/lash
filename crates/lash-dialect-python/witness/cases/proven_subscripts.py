d = {'a': 1, 'b': 2}
print(d['a'])
d['a'] = 3
print(d['a'])
d['a'] += 4
print(d['a'])
alias = d
del alias['a']
try:
    print(d['a'])
except KeyError as error:
    print(type(error).__name__, error.args)
d['a'] = await host.echo(8)
print(d['a'])

xs = [10, 20]
print(xs[0], xs[-1])
xs[0] = await host.echo(30)
xs[0] += 1
print(xs)
xs.clear()
for action in [0, 1]:
    try:
        if action == 0:
            print(xs[0])
        else:
            xs[0] = 99
    except IndexError:
        print('bounds')
try:
    print(xs[9223372036854775808])
except IndexError:
    print('huge bounds')

print((1, 2)[0])
try:
    print((1, 2)[3])
except IndexError:
    print('tuple bounds')

def annotated(xs: dict):
    return xs[0]
print(annotated([42]))

print({'x': 5}[await host.echo('x')])

# An empty tuple is a Python value, including through an alias and a shifted index.
values = [(), (1,), [2], None, False]
print(values[0], values[1], values[2], values[3], values[4])
alias = values
alias[0] = (3,)
print(values[0])
del alias[0]
print(values[0], values[1])
try:
    print(values[0.0])
except TypeError:
    print('float index')
