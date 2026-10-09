async def main(tool, output):
    big = 2 ** 63
    output([big, big + 1, big * big, -big - 1, 2 ** 64 - 1, (2 ** 64) // 3, (2 ** 64) % 1000, 10 ** 30 // 7])
    output([7 // 2, -7 // 2, 7 // -2, -7 // -2, 7 % 3, -7 % 3, 7 % -3, -7 % -3])
    output([7 / 2, -7 / 2, 1 / 3, 10 / 5, (2 ** 53 + 1) / 1, 7.5 // 2, -7.5 // 2, 7.5 % 2, -7.5 % 2, 2 ** -1])
    output([divmod(-7, 2), divmod(7.5, 2), (abs(-big), int(7.9), int(-7.9), round(2.5), round(3.5))])
    output([1 == 1.0, 2 ** 53 == 2.0 ** 53, 2 ** 53 + 1 == 2.0 ** 53, 0.1 + 0.2 == 0.3, 2 ** 100 > 1e30])
    problems = []
    for attempt in [lambda: 1 // 0, lambda: 1 % 0, lambda: 1 / 0, lambda: 1.5 // 0, lambda: 1.5 % 0]:
        try:
            attempt()
        except ZeroDivisionError as error:
            problems.append(str(error))
    output(problems)
    return int("12345678901234567890123") + 1
