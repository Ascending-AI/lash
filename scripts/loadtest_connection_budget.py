"""Server capacity for the load topology, including witness clients."""


def peak_connections(values):
    workers, postgres = values['workers'], values['postgres']
    count = workers['count']
    store = workers['pgConnections']
    witness = workers['witnessConnections']
    generations = postgres['maxGenerations']
    other = postgres['otherWorkers']
    headroom = postgres['adminHeadroom']
    for value, minimum in [(count, 1), (store, 1), (witness, 1),
                           (generations, 2), (other, 0), (headroom, 3)]:
        if type(value) is not int or not minimum <= value <= 1000000:
            raise ValueError('invalid PostgreSQL connection budget')
    if other < 3 * witness + 6:
        raise ValueError('otherWorkers must cover provider, smoke, driver and probes')
    return count * (store + witness) * generations + other + headroom
