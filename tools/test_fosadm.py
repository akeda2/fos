#!/usr/bin/env python3
"""Host-side checks of fosadm's pure parts: name grouping, `ls` parsing, per-CPU source layout."""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import fosadm  # noqa: E402


class Fosadm(unittest.TestCase):
    def test_names(self):
        self.assertEqual(fosadm.machine_name("skylake3"), "skylake")
        self.assertEqual(fosadm.machine_name("42"), "42")

    def test_ls(self):
        text = "hello               1234 bytes\nfos> \nbad name 5 bytes\n"
        self.assertEqual(fosadm.parse_ls(text), {"hello": 1234})

    def test_arch(self):
        self.assertEqual(fosadm.arch_of("Fosfor OS (aarch64 port): shell"), "aarch64")
        self.assertEqual(fosadm.arch_of("Fosfor OS (fos) is a small"), "x86_64")

    def test_release_layout(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "x86_64/tapes").mkdir(parents=True)
            (root / "aarch64/tapes").mkdir(parents=True)
            (root / "x86_64/kernel").write_bytes(b"x" * 1025)
            (root / "x86_64/tapes/a").write_bytes(b"12")
            (root / "aarch64/tapes/b").write_bytes(b"123")
            kernel, _, tapes = fosadm.source_files(root, "x86_64")
            self.assertEqual((fosadm.kernel_sectors(kernel), tapes), (3, {"a": 2}))
            kernel, _, tapes = fosadm.source_files(root, "aarch64")
            self.assertEqual((kernel, tapes), (None, {"b": 3}))

    def test_flat_folder_is_x86_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "tapes").mkdir()
            (root / "tapes/a").write_bytes(b"1")
            self.assertEqual(fosadm.source_files(root, "x86_64")[2], {"a": 1})
            self.assertEqual(fosadm.source_files(root, "aarch64")[2], {})


if __name__ == "__main__":
    unittest.main()
