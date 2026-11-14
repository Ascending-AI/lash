"""Reviewed instruction ceilings and the fixed measurement contract for FIG-7682."""

# The runtime switch gate: every PY row must also be <=3x Monty 1.4.0 in this
# same Kiln configuration, with no margin above 3x, and no PY or TS row may
# exceed its own ceiling below. This rule is binding.
# The <=2x Monty goal is report-only until after the switch: a PY row above it
# is named in the receipt and never fails the gate.
# Current ceilings use the median of five raw paired secants times 1.03.
# The four rows the reviewed replacement adds follow the same convention,
# lead as reviewer: main d709cfaf7f's medians times 1.03, from the L7 land run
# of 45f2a0ccf6 (14 Nov), whose candidate binary carries the diagnostic
# statement counters. Ceilings are checked on both sides.

MONTY_BINDING = True
MONTY_MAX_RATIO = 3.0
MONTY_REPORT_RATIO = 2.0

# Contract r57, L7: only this exact predecessor may add these four rows.
REVIEWED_REPLACEMENT = {
    'previous_sha256': '6c12001454776ad428013cc59821d6d4babd437299a82fcef6c53e863dddca32',
    'added_rows': ['python/func_closure_call', 'python/float_arith',
                   'python/string_build1', 'python/int_to_string'],
}

CONFIGURATION = 'Kiln optimized, full optimization, perf_iteration=false'

COMPILER = 'rustc 1.98.1 (48a229cea 2026-09-01)'

BUILD_FLAGS = ['-Copt-level=3', '-Cdebuginfo=0', '-Cstrip=debuginfo', '-Cembed-bitcode=no']

ENGINE_VERSION = 'Lash 0.0.0-dev; library/native versions are bound by each artifact source tree'

QUICKJS_REVISION = '04be246001599f5995fa2f2d8c91a0f198d3f34c'

QUICKJS_SHA256 = '6b74c81e8b8929e1d2d89e2091ac1731e55106087d7e7a7c7d8d014501a3e11e'

PYTHON_SHA256 = 'e50d468e8b0adfb05733f5b87b3cff34829c4a8c1aea50c865aa8bdfe4bb150f'

PYTHON_VERSION = 'Python 3.12.3'

# Part of the reference definition: every compare process, and so every
# CPython and Monty process it starts, runs with address-space randomisation
# off and exactly this environment (fixed hash seed, no bytecode writes, C
# locale). Base and candidate run identically.
REFERENCE_ENVIRONMENT = {
    'personality': ['setarch', '-R'],
    'env': {'PATH': '/usr/bin:/bin', 'LANG': 'C', 'LC_ALL': 'C',
            'PYTHONHASHSEED': '0', 'PYTHONDONTWRITEBYTECODE': '1'},
}

MONTY_VERSION = '1.4.0'

MONTY_SHA256 = '4479e7306fdf0c6a2a567aa139150ac47c75ab3c0138ec2c4efa010c382a87e0'

WORKERS = 16

CPU_MODEL = 'AMD Ryzen 9 5950X 16-Core Processor'

SCOPE = ('instructions:u around synchronous fresh in-memory cell construction/run/drop/result validation; '
 'lower/link/driver/diagnostics excluded; exact charge and memory on, observation off, 512 MiB guest, 8 '
 'GiB process cap; five independent processes per row, each one paired secant after one low/high '
 'warmup, factor 20; range/median noise <1%')

SEED_COMMIT = '17f77ea352071617acda6ba854930a4c4bc291e0'

COST_ORDER = ['typescript/map_set_string',
 'typescript/json_transform',
 'typescript/prop_read',
 'python/sort_bench',
 'python/prop_read',
 'python/json_transform',
 'typescript/int_arith',
 'typescript/func_call',
 'python/map_set_string',
 'python/func_call',
 'python/int_arith',
 'python/func_closure_call',
 'python/float_arith',
 'python/string_build1',
 'python/int_to_string']

ROWS = {'python/int_arith': {'source': 'crates/lash-perf/src/compare/workloads/int_arith.py',
                      'sha256': '0ff439911aedb3817aea93dc6af64ce82e33efa9b9e137511b8171bd053eeb3f',
                      'lo': 1,
                      'ops': 1000,
                      'ceiling': 79407.95641526315},
 'python/func_call': {'source': 'crates/lash-perf/src/compare/workloads/func_call.py',
                      'sha256': '08c224d8bf1cfef23bd6442cf3a1b597b14da8eff9df776bab133a5112ccf3d7',
                      'lo': 1000,
                      'ops': 4,
                      'ceiling': 124624.25296592106},
 'python/prop_read': {'source': 'crates/lash-perf/src/compare/workloads/prop_read.py',
                      'sha256': '8d208e04816c73ef8785cde508434d4266ed489200aad36bce34886ee18c18d4',
                      'lo': 1000,
                      'ops': 4,
                      'ceiling': 1712642.6642975002},
 'python/map_set_string': {'source': 'crates/lash-perf/src/compare/workloads/map_set_string.py',
                           'sha256': '693596e060cdc7bc1836d7dcbbf23fb1e4ee2941d94718dba82db13adc7f49b7',
                           'lo': 1,
                           'ops': 1000,
                           'ceiling': 1412393.818128421},
 'python/json_transform': {'source': 'crates/lash-perf/src/compare/workloads/json_transform.py',
                           'sha256': '7881cb629936ab28c552588d7a74ac29d63acfadc811691d4688b23bf9eba5ab',
                           'lo': 1,
                           'ops': 1,
                           'ceiling': 3272729399.2489476},
 'python/sort_bench': {'source': 'crates/lash-perf/src/compare/workloads/sort_bench.py',
                       'sha256': 'dc9d0f1b7fd397c2bcba5db1bb4e43de61ecc421bf80dad0e657bc17bfec4cf2',
                       'lo': 1,
                       'ops': 1,
                       'ceiling': 7530501267.586843},
 'typescript/int_arith': {'source': 'crates/lash-perf/src/compare/workloads/int_arith.js',
                          'sha256': '9c2b9a8eebc3fc780e4127e511c64d5bbce36e099f449480768e1403e33cb3d1',
                          'lo': 1,
                          'ops': 1000,
                          'ceiling': 1434843.2556621053},
 'typescript/func_call': {'source': 'crates/lash-perf/src/compare/workloads/func_call.js',
                          'sha256': '3fdcf149dca1fb86431b5101990fb2abfe2f5e503ef017f31c4274b0c186614f',
                          'lo': 1000,
                          'ops': 4,
                          'ceiling': 472169.0596325},
 'typescript/prop_read': {'source': 'crates/lash-perf/src/compare/workloads/prop_read.js',
                          'sha256': 'eced51007902ff8150e551a60f778195dbcfaa7a860f56b316559f717ecd2c4e',
                          'lo': 1000,
                          'ops': 4,
                          'ceiling': 2910306.9838028946},
 'typescript/map_set_string': {'source': 'crates/lash-perf/src/compare/workloads/map_set_string.js',
                               'sha256': 'cc1167209575863657c87be02c813598f26a7e0b7eb37adb2947a1e458c8e515',
                               'lo': 1,
                               'ops': 1000,
                               'ceiling': 29076898.77104737},
 'typescript/json_transform': {'source': 'crates/lash-perf/src/compare/workloads/json_transform.js',
                               'sha256': 'eb67836ac3d63d22ae77797a30d8f6bd454175f00b6f1fb38622a6ebbb2a336d',
                               'lo': 1,
                               'ops': 1,
                               'ceiling': 13405047694.847895},
 'python/func_closure_call': {'source': 'crates/lash-perf/src/compare/workloads/func_closure_call.py',
                              'sha256': '37e6120f7e3f91f05d4cad4303562de365c1295741a2df39aa573a625558e6c7',
                              'lo': 1000,
                              'ops': 4,
                              'ceiling': 166471.98706013156},
 'python/float_arith': {'source': 'crates/lash-perf/src/compare/workloads/float_arith.py',
                        'sha256': '096caf1f86bc46450ea51f55bd0c53074ac9c7ea82d0b05555fe8893684a45ac',
                        'lo': 1,
                        'ops': 1000,
                        'ceiling': 96336.92663894738},
 'python/string_build1': {'source': 'crates/lash-perf/src/compare/workloads/string_build1.py',
                          'sha256': 'f7f211a6d7b28ddca506ffeb9f1fd3a62b0a2db0b8dc48292d1a1be1f821d7b9',
                          'lo': 1,
                          'ops': 1000,
                          'ceiling': 569673.8072510527},
 'python/int_to_string': {'source': 'crates/lash-perf/src/compare/workloads/int_to_string.py',
                          'sha256': '3973e856976fafb5b440acc83a65d61457ee588644b7f88864291782ff5b5cdb',
                          'lo': 1000,
                          'ops': 3,
                          'ceiling': 524099.4512266667}}

# Information only, never checked: candidate (45f2a0ccf6) medians of the
# rows added above, from the same land run.
ADDED_ROW_CANDIDATE_MEDIANS = {'python/func_closure_call': 1738.2, 'python/float_arith': 739.6,
                               'python/string_build1': 3099.2, 'python/int_to_string': 1113.5}

# The old engine with this exact boundary instrument is pinned after its first land.
CHECKPOINT_BASE = '3abbc2bd519228a4f43f7be7ba289f470324068a'
BOUNDARY = {
    'source': 'crates/lash-perf/src/boundary/kernel_cold_workload.rs',
    'sha256': '9177216c2c801757575fe139410ccf484818cfdfb90a8c8914095ba1ccd07e91',
    'elements': 200_000,
    'samples': 5,
    # Contract section 6, cold access (ruling A, 2026-11-09): reads are
    # d + 2 for a fully cold container (d the directory depth including the
    # leaf); heap writes C + I + E + D are held to the fixture ceiling, and
    # all writes per seal to that plus s run segments, RUN_ROOT and
    # CHECKPOINT_ROOT. Both fixtures hold 200,000 integers, which are
    # immediates: one container row, so d = 1, and an overwrite writes one
    # chunk, one chunk-index page and one directory leaf.
    'fixtures': {
        'list': {'directory_depth': 1, 'heap_writes': 3, 'run_segments': 1},
        'map': {'directory_depth': 1, 'heap_writes': 3, 'run_segments': 1},
    },
    'max_checkpoint_ratio': 1.0,
}
