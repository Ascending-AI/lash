# The Python dialect has no stdlib imports. This fixture's restricted JSON
# codec is the same source on CPython and lash: arrays of flat tool records,
# ASCII strings without escapes, integer IDs/values and boolean active flags.
# It parses the JSON text, transforms every record, and serializes it again.
def encode_record(record):
    active = "false"
    if record["active"]:
        active = "true"
    return '{"id":'+str(record["id"])+',"name":"'+record["name"]+'","value":'+str(record["value"])+',"active":'+active+'}'

def encode(records):
    parts = []
    for record in records:
        parts.append(encode_record(record))
    return '{"tool":"search","records":['+','.join(parts)+']}'

def decode(source):
    body = source[len('{"tool":"search","records":['):-2]
    records = []
    for raw in body.split('},{'):
        fields = raw.replace('{','').replace('}','').split(',')
        records.append({"id":int(fields[0].split(':')[1]),"name":fields[1].split(':')[1][1:-1],"value":int(fields[2].split(':')[1]),"active":fields[3].split(':')[1]=="true"})
    return records

def benchmark(n):
    records = []
    for i in range(128):
        records.append({"id":i,"name":"tool-record-"+str(i),"value":i,"active":i%2==0})
    source = encode(records)
    result = 0
    for trip in range(n):
        data = decode(source)
        for record in data:
            record["value"] += 1
        result += len(encode(data))
    return result
