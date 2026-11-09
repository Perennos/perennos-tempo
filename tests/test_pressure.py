"""Host pressure report: tmpfs per mount, largest tmpfs folders with age, top memory processes, swap.
Run: python -m unittest discover -s tests"""
import os
import pathlib
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import tempo_pressure as P  # noqa: E402

GIB = 1 << 30


def fake_proc(root, pid, name, rss_kb, swap_kb=0, uid=1000):
    d = pathlib.Path(root, str(pid))
    d.mkdir(parents=True)
    (d / "status").write_text(f"Name:\t{name}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nVmRSS:\t{rss_kb} kB\nVmSwap:\t{swap_kb} kB\n")


class Pressure(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.proc = self.root / "proc"
        self.proc.mkdir()
        (self.proc / "meminfo").write_text("MemTotal: 33554432 kB\nMemAvailable: 6291456 kB\nSwapTotal: 8388608 kB\nSwapFree: 0 kB\n")
        fake_proc(self.proc, 101, "llama-server", 3_000_000)
        fake_proc(self.proc, 102, "node", 1_000_000, swap_kb=500_000)
        fake_proc(self.proc, 103, "tiny", 10)
        self.ram = self.root / "ramdisk"
        (self.ram / "job-old").mkdir(parents=True)
        (self.ram / "job-old" / "cache.bin").write_bytes(b"x" * 4096)
        old = time.time() - 31 * 3600
        os.utime(self.ram / "job-old" / "cache.bin", (old, old))
        os.utime(self.ram / "job-old", (old, old))
        (self.ram / "job-new").mkdir()
        (self.ram / "job-new" / "f").write_bytes(b"y" * 100)
        self.mounts = self.root / "mounts"
        self.mounts.write_text(f"tmpfs {self.ram} tmpfs rw 0 0\nproc /proc proc rw 0 0\next4 / ext4 rw 0 0\ntmpfs /sys/fs/cgroup tmpfs ro 0 0\n")

    def tearDown(self):
        self.tmp.cleanup()

    def statvfs(self, size_gib=32, used_gib=25):
        vfs = mock.Mock(f_blocks=size_gib * GIB // 4096, f_bavail=(size_gib - used_gib) * GIB // 4096, f_frsize=4096)
        return mock.patch.object(os, "statvfs", create=True, return_value=vfs)

    def collect(self, **kw):
        with self.statvfs(**kw):
            return P.collect(proc=str(self.proc), mounts=str(self.mounts))

    def test_only_tmpfs_mounts_outside_the_skip_list_are_read(self):
        self.assertEqual(P.tmpfs_mounts(str(self.mounts)), [str(self.ram)])

    def test_tmpfs_use_is_reported_per_mount(self):
        r = self.collect()
        self.assertEqual(len(r["tmpfs"]), 1)
        self.assertEqual(r["tmpfs"][0]["mount"], str(self.ram))
        self.assertEqual(r["tmpfs"][0]["used"], 25 * GIB)
        self.assertEqual(r["tmpfs"][0]["used_pct"], 78)

    def test_largest_tmpfs_folder_comes_first_with_its_age(self):
        r = self.collect()
        top = r["tmpfs_top"]
        self.assertEqual(top[0]["path"], str(self.ram / "job-old"))
        self.assertGreaterEqual(top[0]["idle_hours"], 31)
        self.assertLess(top[-1]["idle_hours"], 1)

    def test_space_not_visible_as_files_is_reported(self):
        self.assertAlmostEqual(self.collect()["tmpfs"][0]["unseen"] / GIB, 25, places=2)

    def test_swap_and_top_processes(self):
        r = self.collect()
        self.assertEqual(r["mem"]["swap_used"], 8 * GIB)
        self.assertEqual([p["name"] for p in r["top_rss"][:2]], ["llama-server", "node"])
        self.assertEqual(r["top_swap"][0]["name"], "node")

    def test_summary_names_the_cause(self):
        line = P.summarise(self.collect())
        for word in ("tmpfs (RAM disk) holds 25.0 GiB", "job-old", "untouched for 31 h", "25.0 GiB of it is not visible as files",
                     "swap is 100% used", "llama-server"):
            self.assertIn(word, line)

    def test_a_calm_host_has_no_tmpfs_or_swap_complaint(self):
        (self.proc / "meminfo").write_text("MemTotal: 33554432 kB\nMemAvailable: 30000000 kB\nSwapTotal: 8388608 kB\nSwapFree: 8388608 kB\n")
        line = P.summarise(self.collect(used_gib=0))
        self.assertNotIn("tmpfs", line)
        self.assertNotIn("swap", line)

    def test_unreadable_proc_gives_an_empty_report_not_an_error(self):
        r = P.collect(proc=str(self.root / "missing"), mounts=str(self.root / "missing"))
        self.assertEqual((r["tmpfs"], r["top_rss"], r["mem"]["total"]), ([], [], 0))

    def test_walk_stops_at_its_time_budget(self):
        _, _, complete = P._walk(str(self.ram), time.monotonic() - 1)
        self.assertFalse(complete)


if __name__ == "__main__":
    unittest.main()
