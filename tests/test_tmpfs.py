"""tmpfs leftovers: register, move to a disk trash on release (never delete), flag old folders with no open handle.
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
import tempo_tmpfs as T  # noqa: E402


def can_symlink():
    with tempfile.TemporaryDirectory() as d:
        try:
            os.symlink(d, os.path.join(d, "l"))
            return True
        except (OSError, NotImplementedError):
            return False


class Tmpfs(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.ram = self.root / "ram"
        self.ram.mkdir()
        self.mounts = self.root / "mounts"
        self.mounts.write_text(f"tmpfs {self.ram} tmpfs rw 0 0\n")
        self.registry, self.trash = self.root / "disk" / "registry.json", self.root / "disk" / "trash"
        self.job_dir = self.ram / "job-a-cache"
        self.job_dir.mkdir()
        (self.job_dir / "blob").write_bytes(b"z" * 5000)

    def tearDown(self):
        self.tmp.cleanup()

    def reg(self, job="job-a", path=None):
        return T.register(job, path or self.job_dir, self.registry, str(self.mounts))

    def test_only_tmpfs_paths_are_registered(self):
        self.assertTrue(self.reg()["ok"])
        outside = self.root / "plain"
        outside.mkdir()
        self.assertFalse(self.reg(path=outside)["ok"])
        self.assertEqual(len(T._load(self.registry)), 1)

    def test_registering_twice_keeps_one_row(self):
        self.reg(); self.reg()
        self.assertEqual(len(T._load(self.registry)), 1)

    def test_release_moves_to_trash_and_deletes_nothing(self):
        self.reg()
        out = T.release("job-a", self.registry, self.trash, keep_free=0)
        self.assertFalse(self.job_dir.exists())
        dest = pathlib.Path(out["moved"][0]["trashed_to"])
        self.assertEqual((dest / "blob").read_bytes(), b"z" * 5000)
        self.assertIn("trash", dest.parts)
        self.assertEqual(T._load(self.registry)[0]["state"], "trashed")

    def test_release_refuses_a_move_that_would_fill_the_disk(self):
        self.reg()
        out = T.release("job-a", self.registry, self.trash, keep_free=1 << 60)
        self.assertTrue(self.job_dir.exists())
        self.assertEqual(out["moved"], [])
        self.assertIn("free on the trash disk", out["kept"][0]["reason"])

    def test_release_ignores_other_jobs_and_unregistered_folders(self):
        other = self.ram / "someone-else"
        other.mkdir()
        self.reg()
        T.release("job-b", self.registry, self.trash, keep_free=0)
        self.assertTrue(self.job_dir.exists() and other.exists())

    def test_a_folder_already_gone_is_marked_gone(self):
        self.reg()
        self.job_dir.rename(self.ram / "renamed")
        T.release("job-a", self.registry, self.trash, keep_free=0)
        self.assertEqual(T._load(self.registry)[0]["state"], "gone")

    def sweep(self, hours=24, used_gib=1):
        vfs = mock.Mock(f_blocks=100 << 20, f_bavail=0, f_frsize=4096)
        with mock.patch.object(os, "statvfs", create=True, return_value=vfs):
            return T.sweep(self.registry, hours, proc=str(self.root / "noproc"), mounts=str(self.mounts))

    def test_sweep_flags_old_folders_only_and_never_moves_them(self):
        old = time.time() - 30 * 3600
        for p in (self.job_dir / "blob", self.job_dir):
            os.utime(p, (old, old))
        fresh = self.ram / "fresh"
        fresh.mkdir()
        (fresh / "f").write_bytes(b"1")
        flagged = self.sweep()["flagged"]
        self.assertEqual([f["path"] for f in flagged], [str(self.job_dir)])
        self.assertTrue(self.job_dir.exists())

    def test_sweep_says_whether_a_flagged_folder_was_registered(self):
        self.reg()
        old = time.time() - 30 * 3600
        for p in (self.job_dir / "blob", self.job_dir):
            os.utime(p, (old, old))
        row = self.sweep()["flagged"][0]
        self.assertTrue(row["registered"])
        self.assertEqual(row["job"], "job-a")

    @unittest.skipUnless(can_symlink(), "needs symlinks to fake /proc/<pid>/cwd")
    def test_a_folder_a_process_still_holds_is_not_flagged(self):
        proc = self.root / "proc"
        (proc / "42" / "fd").mkdir(parents=True)
        os.symlink(self.job_dir / "blob", proc / "42" / "fd" / "3")
        os.symlink(self.root, proc / "42" / "cwd")
        os.symlink("/", proc / "42" / "root")
        self.assertEqual(T.open_handles(str(self.job_dir), str(proc)), (1, 0))
        (proc / "43").mkdir()   # no fd folder: could not be read
        self.assertEqual(T.open_handles(str(self.job_dir), str(proc)), (1, 1))
        self.assertEqual(T.open_handles(str(self.ram / "other"), str(proc)), (0, 1))


if __name__ == "__main__":
    unittest.main()
