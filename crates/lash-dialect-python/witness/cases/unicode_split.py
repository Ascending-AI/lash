print('a b c\vd\fe'.split())
spaces = [9, 10, 11, 12, 13, 28, 29, 30, 31, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288]
for point in spaces:
    text = chr(point) + 'a' + chr(point) + chr(point) + 'b' + chr(point)
    print(text.split(), text.split(None), text.split(chr(point)))
print(''.split(), '   '.split(), 'a\u200bb'.split())
