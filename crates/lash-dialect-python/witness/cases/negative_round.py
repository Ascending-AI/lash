print(round(1234, -2), round(1250, -2), round(1350, -2), round(1234.5, -2))
for value in [1250, 1350, -1250, -1350, 0, 10 ** 30 + 500, 10 ** 30 + 1500]:
    print(round(value, -3), round(value, 0), round(value, 2), round(value, -100))
for value in [1250.0, 1350.0, -1250.0, -1350.0, -0.0, 1e308, 5e-324]:
    print(round(value, -2), round(value, -308), round(value, -400))
for attempt in [lambda: round(1, 1.0), lambda: round(1.0, 'x')]:
    try:
        attempt()
    except TypeError as error:
        print(type(error).__name__)
