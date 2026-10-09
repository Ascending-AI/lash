#!/usr/bin/env python3
"""BINARY-SIZE-OWNERSHIP: ambiguous bytes never acquire a crate owner."""
import unittest

from scripts.binary_size import crate_sizes, parse_sections, parse_symbols


class CrateAttribution(unittest.TestCase):
    def test_demangled_prefix_owns_only_unambiguous_allocated_bytes(self):
        sections = parse_sections(
            "  [ 1] .text PROGBITS 0000000000001000 001000 000070 00 AX 0 0 16\n"
            "  [ 2] .debug_info PROGBITS 0000000000000000 002000 000080 00 0 0 1\n")
        symbols = parse_symbols(
            "00001000 00000010 T lash_perf::run::h0123456789abcdef\n"
            "00001010 00000010 T <lash_core::Runner as core::fmt::Debug>::fmt\n"
            "00001020 00000010 T memcpy\n"
            "00001030 00000010 T lash_perf::merged.llvm.123\n"
            "00001040 00000010 T lash_core::shared\n"
            "00001040 00000010 T lash_perf::alias\n"
            "00001060 00000010 T lash_perf::last\n"
            "00002000 T unsized\n")
        sizes, reasons = crate_sizes(sections, symbols)
        self.assertEqual(symbols[0]["key"], "lash_perf::run")
        self.assertEqual(sizes, {"lash_perf": 32, "lash_core": 16, "unattributed": 64})
        self.assertEqual(reasons, {"unknown": 32, "shared": 16, "lto_merged": 16})
        self.assertEqual(sum(sizes.values()), 112)


if __name__ == "__main__":
    unittest.main()
