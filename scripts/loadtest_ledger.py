"""The generated vocabulary of the Rust-owned load witness unions."""
import json
from pathlib import Path

CONTRACT = json.loads(Path(__file__).with_name('loadtest-ledger.json').read_text())
WITNESS_CLASSES = tuple(CONTRACT['classes'])
FAULT_CLASSES = tuple(CONTRACT['fault_classes'])
UPGRADE_CLASSES = tuple(CONTRACT['upgrade_classes'])
UPGRADE_STEPS = tuple(CONTRACT['upgrade_steps'])


def require_pair(family, kind, phase):
    if [kind, phase] not in CONTRACT[family]:
        raise ValueError(f'unknown or illegal {family} pair {kind}:{phase}')
