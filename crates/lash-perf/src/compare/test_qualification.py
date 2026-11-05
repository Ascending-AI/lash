"""Reference-counter refusal law; no benchmark process is started by this test."""
from pathlib import Path
import runpy
import unittest


class CounterRefusalLaw(unittest.TestCase):
    def test_unqualified_windows_are_refusals_and_guest_errors_are_not(self):
        driver = runpy.run_path(str(Path(__file__).with_name('qualification.py')))
        refused = driver['counter_refusal']
        # Exact diagnostic forms from InstructionReading::qualify and the
        # paired reference reader. Losing any of them would allow more runs
        # after a hardware-counter refusal.
        for error in (
            'lash python prop_read: perf_event_open instructions:u: Permission denied',
            'qjs-0 typescript prop_read: unqualified instructions:u (98% running): raw.perf.tsv',
            'lash python int_arith: unqualified instructions:u: requires >=99% time running',
            'monty python prop_read: missing instructions:u counter: raw.perf.tsv',
            'monty python prop_read: unavailable instructions:u count',
            'lash python prop_read: PMU counter moved backwards',
        ):
            with self.subTest(error=error):
                self.assertTrue(refused([error]))
        self.assertFalse(refused(['lash python records: guest operation not supported']))
        self.assertFalse(refused([]))


if __name__ == '__main__':
    unittest.main()
