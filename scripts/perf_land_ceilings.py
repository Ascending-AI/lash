"""Reviewed instruction ceilings and the fixed measurement contract for FIG-7682."""

# The runtime switch gate: every PY row not exempt below must also be <=3x Monty 1.4.0 in this
# same Kiln configuration, with no margin above 3x, and no PY or TS row may
# exceed its own ceiling below. This rule is binding.
# The <=2x Monty goal is report-only until after the switch: a PY row above it
# is named in the receipt and never fails the gate.
# Every ceiling is main's median of five raw paired secants times
# CEILING_FACTOR, checked on both sides. Recalibrated 16 Nov for the bytecode
# core, lead as reviewer: medians of main e189e14bf6 measured by this gate with
# the landed harness (each row on its own thread), five repeats, every row's
# repeat noise below 0.1%. No ceiling was raised; the region-era ceilings sat
# up to 1000x above today's medians.
#
# How these numbers move (FIG-7694, job D):
# - Every instruction count the gate measures (row secants, references and
#   the boundary seal) holds its repeat noise, range over median, below NOISE.
# - Every row, and the boundary seal, holds the candidate's median below the
#   base's median times 1 + BASE_MARGIN, measured in the same land.
# - A ceiling moves down only by the land's ratchet: when the candidate's
#   median times CEILING_FACTOR sits more than RATCHET_SLACK below a row's
#   ceiling, scripts/land commits exactly that lower ceiling with the land.
# - Any other change to this file's values is a reviewed replacement: its
#   REVIEWED_REPLACEMENT names the exact predecessor file, the reviewer and
#   what it changes. A row in its replaced_ceilings takes the reviewed ceiling
#   and is exempt from the base margin for that land only, printed with the
#   reviewer. There is no other way around either check.

MONTY_BINDING = True
MONTY_MAX_RATIO = 3.0
MONTY_REPORT_RATIO = 2.0
# Every PY row is held to the binding 3x and report-only 2x rules, a row
# added later included, except the rows listed here. Only a reviewed
# replacement adds an entry; the gate refuses an entry whose candidate is at
# or under 3x, so the list empties itself.
MONTY_EXEMPT_ROWS = {}

NOISE = 0.01
BASE_MARGIN = 0.01
CEILING_FACTOR = 1.03
RATCHET_SLACK = 0.03

# Active only while the base's copy of this file has previous_sha256.
REVIEWED_REPLACEMENT = {
    'previous_sha256': '72f9aaa19737ad560442ce256a1d26f417875cd7329de2e9a59297c06fb55dd5',
    'reviewer': 'lead',
    'added_rows': [],
    'replaced_ceilings': [],
    'changed_fields': ['MONTY_EXEMPT_ROWS'],
}

CONFIGURATION = 'Kiln optimized, full optimization, perf_iteration=false'

COMPILER = 'rustc 1.98.1 (48a229cea 2026-09-01)'

BUILD_FLAGS = ['-Copt-level=3', '-Cdebuginfo=0', '-Cstrip=debuginfo', '-Cembed-bitcode=no']

ENGINE_VERSION = 'Lash 0.0.0-dev; library/native versions are bound by each artifact source tree'

QUICKJS_REVISION = '04be246001599f5995fa2f2d8c91a0f198d3f34c'

QUICKJS_SHA256 = 'a82e1ad094bad69d8a904ddc8c05367d4f5b1509415d0c931d14c3abd5349a34'

# The QuickJS pin is its revision built by exactly this compiler, from a
# pristine export with the build directory mapped out of the binary, so the
# digest does not depend on where the cache lives.
QUICKJS_COMPILER = 'gcc (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0'

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

COST_ORDER = ['typescript/object_sort_200k',
 'typescript/object_sort',
 'python/object_sort_200k',
 'typescript/sort_bench',
 'python/object_sort',
 'typescript/records',
 'python/records',
 'typescript/object_build',
 'python/object_build',
 'typescript/map_set_string',
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
 'python/int_to_string',
 'typescript/array_read',
 'python/array_read',
 'typescript/prop_write',
 'python/prop_write',
 'typescript/string_build1',
 'typescript/empty_loop',
 'python/empty_loop']

ROWS = {'python/int_arith': {'source': 'crates/lash-perf/src/compare/workloads/int_arith.py',
                      'sha256': '0ff439911aedb3817aea93dc6af64ce82e33efa9b9e137511b8171bd053eeb3f',
                      'lo': 1,
                      'ops': 1000,
                      'ceiling': 660.8321705263157},
 'python/func_call': {'source': 'crates/lash-perf/src/compare/workloads/func_call.py',
                      'sha256': '08c224d8bf1cfef23bd6442cf3a1b597b14da8eff9df776bab133a5112ccf3d7',
                      'lo': 1000,
                      'ops': 4,
                      'ceiling': 1231.1553272368421},
 'python/prop_read': {'source': 'crates/lash-perf/src/compare/workloads/prop_read.py',
                      'sha256': '8d208e04816c73ef8785cde508434d4266ed489200aad36bce34886ee18c18d4',
                      'lo': 1000,
                      'ops': 4,
                      'ceiling': 966.3934748684211},
 'python/map_set_string': {'source': 'crates/lash-perf/src/compare/workloads/map_set_string.py',
                           'sha256': '693596e060cdc7bc1836d7dcbbf23fb1e4ee2941d94718dba82db13adc7f49b7',
                           'lo': 1,
                           'ops': 1000,
                           'ceiling': 5025.334925789474},
 'python/json_transform': {'source': 'crates/lash-perf/src/compare/workloads/json_transform.py',
                           'sha256': '7881cb629936ab28c552588d7a74ac29d63acfadc811691d4688b23bf9eba5ab',
                           'lo': 1,
                           'ops': 1,
                           'ceiling': 2906064.4431578945},
 'python/sort_bench': {'source': 'crates/lash-perf/src/compare/workloads/sort_bench.py',
                       'sha256': 'dc9d0f1b7fd397c2bcba5db1bb4e43de61ecc421bf80dad0e657bc17bfec4cf2',
                       'lo': 1,
                       'ops': 1,
                       'ceiling': 7646384.274210527},
 'typescript/int_arith': {'source': 'crates/lash-perf/src/compare/workloads/int_arith.js',
                          'sha256': '9c2b9a8eebc3fc780e4127e511c64d5bbce36e099f449480768e1403e33cb3d1',
                          'lo': 1,
                          'ops': 1000,
                          'ceiling': 613.7909321052632},
 'typescript/func_call': {'source': 'crates/lash-perf/src/compare/workloads/func_call.js',
                          'sha256': '3fdcf149dca1fb86431b5101990fb2abfe2f5e503ef017f31c4274b0c186614f',
                          'lo': 1000,
                          'ops': 4,
                          'ceiling': 1148.4343331578948},
 'typescript/prop_read': {'source': 'crates/lash-perf/src/compare/workloads/prop_read.js',
                          'sha256': 'eced51007902ff8150e551a60f778195dbcfaa7a860f56b316559f717ecd2c4e',
                          'lo': 1000,
                          'ops': 4,
                          'ceiling': 921.0549484210527},
 'typescript/map_set_string': {'source': 'crates/lash-perf/src/compare/workloads/map_set_string.js',
                               'sha256': 'cc1167209575863657c87be02c813598f26a7e0b7eb37adb2947a1e458c8e515',
                               'lo': 1,
                               'ops': 1000,
                               'ceiling': 6075.842930526315},
 'typescript/json_transform': {'source': 'crates/lash-perf/src/compare/workloads/json_transform.js',
                               'sha256': 'eb67836ac3d63d22ae77797a30d8f6bd454175f00b6f1fb38622a6ebbb2a336d',
                               'lo': 1,
                               'ops': 1,
                               'ceiling': 5263590.62263158},
 'python/func_closure_call': {'source': 'crates/lash-perf/src/compare/workloads/func_closure_call.py',
                              'sha256': '37e6120f7e3f91f05d4cad4303562de365c1295741a2df39aa573a625558e6c7',
                              'lo': 1000,
                              'ops': 4,
                              'ceiling': 1790.416595657895},
 'python/float_arith': {'source': 'crates/lash-perf/src/compare/workloads/float_arith.py',
                        'sha256': '096caf1f86bc46450ea51f55bd0c53074ac9c7ea82d0b05555fe8893684a45ac',
                        'lo': 1,
                        'ops': 1000,
                        'ceiling': 761.6916678947368},
 'python/string_build1': {'source': 'crates/lash-perf/src/compare/workloads/string_build1.py',
                          'sha256': 'f7f211a6d7b28ddca506ffeb9f1fd3a62b0a2db0b8dc48292d1a1be1f821d7b9',
                          'lo': 1,
                          'ops': 1000,
                          'ceiling': 3234.189808421053},
 'python/int_to_string': {'source': 'crates/lash-perf/src/compare/workloads/int_to_string.py',
                          'sha256': '3973e856976fafb5b440acc83a65d61457ee588644b7f88864291782ff5b5cdb',
                          'lo': 1000,
                          'ops': 3,
                          'ceiling': 1147.8339154385965},
 'typescript/sort_bench': {'source': 'crates/lash-perf/src/compare/workloads/sort_bench.js',
                           'sha256': '5cdc375d6143cb6fcc630d0504da19de734105f44c4ea990bb8aa5568a7dc199',
                           'lo': 1,
                           'ops': 1,
                           'ceiling': 695253817.071579},
 'typescript/object_sort': {'source': 'crates/lash-perf/src/compare/workloads/object_sort.js',
                            'sha256': 'd92fc420a4faef7860ee70bc86e1d61d608a206ba2279bfe9d630f6668775f9b',
                            'lo': 10000,
                            'ops': 1,
                            'ceiling': 253338.9136415263},
 'python/object_sort': {'source': 'crates/lash-perf/src/compare/workloads/object_sort.py',
                        'sha256': '11b70d8466f36b7b021c62f810f6f91d5bb71af365ba7869cf035ab93d40a664',
                        'lo': 10000,
                        'ops': 1,
                        'ceiling': 19244.379578473687},
 'typescript/object_sort_200k': {'source': 'crates/lash-perf/src/compare/workloads/object_sort.js',
                                 'sha256': 'd92fc420a4faef7860ee70bc86e1d61d608a206ba2279bfe9d630f6668775f9b',
                                 'lo': 20000,
                                 'ops': 1,
                                 'ceiling': 262738.93313007895},
 'python/object_sort_200k': {'source': 'crates/lash-perf/src/compare/workloads/object_sort.py',
                             'sha256': '11b70d8466f36b7b021c62f810f6f91d5bb71af365ba7869cf035ab93d40a664',
                             'lo': 20000,
                             'ops': 1,
                             'ceiling': 19254.800286342106},
 'typescript/array_read': {'source': 'crates/lash-perf/src/compare/workloads/array_read.js',
                           'sha256': 'c9dbaaf2aa2f45e4aafe73ac2de9477e23818f0c20ade6c26540c981be75b9c9',
                           'lo': 1000,
                           'ops': 10,
                           'ceiling': 3812.545851105263},
 'python/array_read': {'source': 'crates/lash-perf/src/compare/workloads/array_read.py',
                       'sha256': 'aefc32a93c8bbee248a88ce26810667d93e6bb2650c50926bed4c8ecc74429f6',
                       'lo': 1000,
                       'ops': 10,
                       'ceiling': 1353.5367694736842},
 'typescript/prop_write': {'source': 'crates/lash-perf/src/compare/workloads/prop_write.js',
                           'sha256': 'f37dc968fcd42a5245176938513aaa524f8a6f8a4daba175dd2b27ae545bd375',
                           'lo': 1000,
                           'ops': 4,
                           'ceiling': 1315.054343157895},
 'python/prop_write': {'source': 'crates/lash-perf/src/compare/workloads/prop_write.py',
                       'sha256': '6fd5a7378e94da237608c105fe4a3a6f1b0f7c61021daeb434803d6941986921',
                       'lo': 1000,
                       'ops': 4,
                       'ceiling': 1166.277118026316},
 'typescript/records': {'source': 'crates/lash-perf/src/compare/workloads/records.js',
                        'sha256': '9ccaf39805acb4a702481d27e6a29b1e25aef407962dbb0075f8252622a6c408',
                        'lo': 1,
                        'ops': 1,
                        'ceiling': 149307489.46052632},
 'python/records': {'source': 'crates/lash-perf/src/compare/workloads/records.py',
                    'sha256': 'd774b6a74d3d70c7b27049f61eaac81013a355b53aa2a5286c0aa7d34bf44ca1',
                    'lo': 1,
                    'ops': 1,
                    'ceiling': 63145863.377894744},
 'typescript/string_build1': {'source': 'crates/lash-perf/src/compare/workloads/string_build1.js',
                              'sha256': '50bcc41da676e579ba17ff0457509dafd64e2cd75132c2ab8ac76f821c92e343',
                              'lo': 1,
                              'ops': 1000,
                              'ceiling': 3364.872522105263},
 'typescript/empty_loop': {'source': 'crates/lash-perf/src/compare/workloads/empty_loop.js',
                           'sha256': '5db9a4e816911a041294fc70bfe29a990b83571e7af5b6f851f5a455cfbbfa8e',
                           'lo': 1000,
                           'ops': 1,
                           'ceiling': 266.6530136842105},
 'python/empty_loop': {'source': 'crates/lash-perf/src/compare/workloads/empty_loop.py',
                       'sha256': 'efeb20152fc845f3cb4f80eefd00186813bba28a88f9cf174e088ecfd7b12b89',
                       'lo': 1000,
                       'ops': 1,
                       'ceiling': 243.07685578947368},
 'typescript/object_build': {'source': 'crates/lash-perf/src/compare/workloads/object_build.js',
                             'sha256': 'dc86c054dc72a895e57ffc063d6f8cb8d553664bd1241a0783a0b2e8ed8f03e5',
                             'lo': 10000,
                             'ops': 1,
                             'ceiling': 4921.764322052632},
 'python/object_build': {'source': 'crates/lash-perf/src/compare/workloads/object_build.py',
                         'sha256': '6ee0f8a356d69d0072786e778063bc42f68a600d5ffef95182d67aa4b3ce9320',
                         'lo': 10000,
                         'ops': 1,
                         'ceiling': 8694.603976736842}}

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
}
