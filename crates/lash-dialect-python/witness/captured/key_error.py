async def main(tool, output):
    prices = {"apple": 3}
    def price(name):
        try:
            return prices[name]
        except KeyError as error:
            output(["missing", str(error), repr(error), error.args])
            return 0
    output([price("apple"), price("pear")])
    try:
        prices["plum"]
    except LookupError as error:
        output([type(error).__name__, str(error)])
    try:
        [1, 2][5]
    except (KeyError, IndexError) as error:
        output([type(error).__name__, str(error)])
    try:
        try:
            {}[(1, "k")]
        except IndexError:
            output("not this one")
        finally:
            output("inner finally")
    except Exception as error:
        output(repr(error))
    return {}["uncaught"]
