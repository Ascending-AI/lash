async def main(tool, output):
    pi = 3.14159265
    count = 1234567
    output(f"{pi:.2f} {pi:.0f} {pi:10.3f}| {pi:<10.3f}| {pi:^10.1f}| {pi:+.1f} {-pi:.3f}")
    output(f"{count:d} {count:,} {count:_} {count:>12,} {count:012d} {count:x} {count:X} {count:#x} {count:b} {count:o} {-count:+,}")
    output(f"{0.5:.1%} {1.0:.0%} {12345.678:e} {12345.678:.2e} {0.000123:.1E} {1e10:,.1f} {2.5:.0f} {3.5:.0f} {0.125:.2f}")
    output(f"{'left':<8}| {'right':>8}| {'mid':^8}| {'dots':.^10} {'cut':.2} {42!r} {'q'!r} {None} {True} {[1, 'a']} {(1,)}")
    output(f"{1.0} {1.5e300} {1e-7} {10 ** 20} {2 ** 0.5} {float('inf')} {-0.0} {1e16} {123456789.123456789}")
    try:
        output(f"{pi:d}")
    except ValueError as error:
        output(str(error))
    output(f"{42:>05d} {1.0:#.0f} {0x123456:_x} {True:}")
    output(f"{42:*<05d}|{42: >05d}|{42:^05d}|{-42:>06d}|{1.0:#.0e}|{1.0:#.0%}|{0x123456:_X}|{0b101010101:_b}")
    for attempt in [lambda: f"{'x':+}", lambda: f"{'x':-}", lambda: f"{'x':#}", lambda: f"{'x':=5}", lambda: f"{'x':,}", lambda: f"{None:5}", lambda: f"{None:0}"]:
        try:
            output(attempt())
        except (ValueError, TypeError) as error:
            output(type(error).__name__)
    return f"{1234.5:+010.2f}|{-1234.5:010.2f}|{7:5}|{7:<5}|{'x':5}|"
