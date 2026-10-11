print(repr('a\x00b\x1bc\xa0d'))
for text in ['\\\n\r\t\v\f', "'", '"', "both'\"", '\x7f\x85\xad', '\u200b\u2028\u2029\u3000', '\u0378\ue000\uffff', '\U0001f600\U000e0001', 'é中']:
    print(repr(text))
