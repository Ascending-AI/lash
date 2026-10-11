for text, base in [('1__2', 10), ('0xff', 16), ('0xff', 0), ('١٢', 10), ('1_2', 10), ('0x_ff', 0), ('-0B_101', 0), ('0o_77', 8), ('00_0', 0), ('012', 0), ('0_1', 0), ('_12', 10), ('12_', 10), ('0x__f', 16), ('0x', 16), ('0z1', 36), ('１２', 10), ('  -١_٢  ', 10), ('²', 10), ('\x1c12', 10)]:
    try:
        print(int(text, base))
    except ValueError as error:
        print(type(error).__name__)
for text in ['1__2.0', '1_2.0', '١_٢.٥', '.５', '1_.0', '1._0', '1e_2', '1_e2', '1e+_2', '_1.0', '1.0_', '+Infinity', '-NaN', '  1_2.5e-1_0  ', '\x1c1.0']:
    try:
        print(float(text))
    except ValueError as error:
        print(type(error).__name__)
