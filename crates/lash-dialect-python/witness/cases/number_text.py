big = 2 ** 63
print(big, big + 1, big * big, -big - 1)
print(2 ** 64 - 1, (2 ** 64) // 3, (2 ** 64) % 1000, 10 ** 30 // 7)
print(7 // 2, -7 // 2, 7 // -2, -7 // -2)
print(7 % 3, -7 % 3, 7 % -3, -7 % -3)
print(7 / 2, -7 / 2, 1 / 3, 10 / 5, 2 ** 53 + 1, (2 ** 53 + 1) / 1)
print(7.5 // 2, -7.5 // 2, 7.5 % 2, -7.5 % 2, 2 ** 0.5, 2 ** -1, 10 ** -2)
print(divmod(-7, 2), divmod(7.5, 2), abs(-big), int(7.9), int(-7.9), round(2.5), round(3.5), round(-0.5), round(2.675, 2))
print(1 == 1.0, 2 ** 53 == 2.0 ** 53, 2 ** 53 + 1 == 2.0 ** 53, 0.1 + 0.2, 1e16, 1.5e-5, 123456789012345678.0, 0.0001, -0.0)
attempts = [lambda: 1 // 0, lambda: 1 % 0, lambda: 1 / 0, lambda: 1.0 // 0, lambda: 1.0 % 0, lambda: 1.5 / 0, lambda: 0 ** -1, lambda: 2.0 ** 10000]
for attempt in attempts:
    try:
        attempt()
    except (ZeroDivisionError, OverflowError) as error:
        print(type(error).__name__, error)
print(int("12345678901234567890123") + 1, float("1.5") * 2, int("-42"), str(2 ** 70), int(" 7 "))
