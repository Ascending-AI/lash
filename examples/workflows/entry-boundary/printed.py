#: Give back the number it is given
def arrow(value):
    return value
#: Answer from the top level
def kernel_name_1():
    return "top"
def main():
    local = k.absent
    arrow = declared.arrow
    kernel_name_1 = declared.kernel_name_1
    if True:
        def local():
            return "block"
    else:
        pass
