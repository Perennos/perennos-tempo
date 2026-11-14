import hashlib
import json
import os
import subprocess
import tempfile
from contextlib import redirect_stdout
from io import StringIO
import unittest
from pathlib import Path

from installer.control_bundle import BundleError, SUPPORTED_TARGETS, build_bundle, main


class ControlBundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.bin_dir = self.root / "bin"
        self.legal_dir = self.root / "legal"
        self.bin_dir.mkdir()
        self.legal_dir.mkdir()
        for name, data in {
            "LICENSE-PolyForm-Shield-1.0.0.txt": b"license fixture\n",
            "NOTICE.txt": b"notice fixture\n",
            "THIRD-PARTY-NOTICES.txt": b"third-party fixture\n",
        }.items():
            (self.legal_dir / name).write_bytes(data)

    def tearDown(self):
        self.temp.cleanup()

    def sources(self, target="x86_64-unknown-linux-gnu"):
        suffix = ".exe" if "windows" in target else ""
        launcher = self.bin_dir / f"tempo-launch{suffix}"
        agent = self.bin_dir / f"tempo-local-agent{suffix}"
        launcher.write_bytes(b"fake launcher\x00\xff\n")
        agent.write_bytes(b"fake local agent\x00\xfe\n")
        return launcher, agent

    def build(self, out_dir, target="x86_64-unknown-linux-gnu", launcher=None, agent=None):
        if launcher is None or agent is None:
            default_launcher, default_agent = self.sources(target)
            launcher = launcher or default_launcher
            agent = agent or default_agent
        return build_bundle(
            version="2.1.0",
            target=target,
            launcher=launcher,
            agent=agent,
            license_file=self.legal_dir / "LICENSE-PolyForm-Shield-1.0.0.txt",
            notice_file=self.legal_dir / "NOTICE.txt",
            third_party_notices=self.legal_dir / "THIRD-PARTY-NOTICES.txt",
            out_dir=out_dir,
        )

    def test_archive_extracts_with_exact_layout_and_verified_manifest(self):
        launcher, agent = self.sources()
        before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in [
            launcher,
            agent,
            self.legal_dir / "LICENSE-PolyForm-Shield-1.0.0.txt",
            self.legal_dir / "NOTICE.txt",
            self.legal_dir / "THIRD-PARTY-NOTICES.txt",
        ]}
        archive, sidecar = self.build(self.root / "out", launcher=launcher, agent=agent)
        root = "perennos-tempo-controls-2.1.0-x86_64-unknown-linux-gnu"
        expected_files = {
            "VERSION",
            "TARGET",
            "EDITION",
            "bin/tempo-launch",
            "bin/tempo-local-agent",
            "legal/LICENSE-PolyForm-Shield-1.0.0.txt",
            "legal/NOTICE.txt",
            "legal/THIRD-PARTY-NOTICES.txt",
            "CONTROLS.json",
        }
        expected_members = sorted(
            [f"{root}/", f"{root}/bin/", f"{root}/legal/"]
            + [f"{root}/{path}" for path in expected_files]
        )
        extracted = self.root / "extracted"
        extracted.mkdir()
        with tarfile_open(archive) as tar:
            members = tar.getmembers()
            self.assertEqual([member.name + "/" if member.isdir() else member.name for member in members], expected_members)
            for member in members:
                destination = extracted / member.name
                if member.isdir():
                    destination.mkdir(parents=True, exist_ok=True)
                else:
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    source = tar.extractfile(member)
                    self.assertIsNotNone(source)
                    with source, destination.open("wb") as output:
                        output.write(source.read())

        bundle_root = extracted / root
        self.assertEqual((bundle_root / "VERSION").read_bytes(), b"2.1.0\n")
        self.assertEqual((bundle_root / "TARGET").read_bytes(), b"x86_64-unknown-linux-gnu\n")
        self.assertEqual((bundle_root / "EDITION").read_bytes(), b"source-available\n")
        self.assertEqual((bundle_root / "bin/tempo-launch").read_bytes(), b"fake launcher\x00\xff\n")
        self.assertEqual((bundle_root / "bin/tempo-local-agent").read_bytes(), b"fake local agent\x00\xfe\n")

        manifest = json.loads((bundle_root / "CONTROLS.json").read_text(encoding="utf-8"))
        self.assertEqual((manifest["schema"], manifest["version"], manifest["target"]),
                         (1, "2.1.0", "x86_64-unknown-linux-gnu"))
        self.assertEqual({entry["path"] for entry in manifest["files"]}, expected_files - {"CONTROLS.json"})
        for entry in manifest["files"]:
            self.assertEqual(set(entry), {"path", "bytes", "sha256"})
            data = (bundle_root / entry["path"]).read_bytes()
            self.assertEqual(entry["bytes"], len(data))
            self.assertEqual(entry["sha256"], hashlib.sha256(data).hexdigest())

        self.assertEqual(sidecar.read_text(encoding="ascii"),
                         f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        for path, (data, mtime) in before.items():
            self.assertEqual(path.read_bytes(), data)
            self.assertEqual(path.stat().st_mtime_ns, mtime)

    def test_archive_is_byte_for_byte_deterministic(self):
        launcher, agent = self.sources()
        first, _ = self.build(self.root / "one", launcher=launcher, agent=agent)
        second, _ = self.build(self.root / "two", launcher=launcher, agent=agent)
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_cli_contract_builds_the_named_archive_and_checksum(self):
        launcher, agent = self.sources()
        out_dir = self.root / "cli-out"
        captured = StringIO()
        with redirect_stdout(captured):
            result = main([
                "--version", "2.1.0",
                "--target", "x86_64-unknown-linux-gnu",
                "--launcher", str(launcher),
                "--agent", str(agent),
                "--license", str(self.legal_dir / "LICENSE-PolyForm-Shield-1.0.0.txt"),
                "--notice", str(self.legal_dir / "NOTICE.txt"),
                "--third-party-notices", str(self.legal_dir / "THIRD-PARTY-NOTICES.txt"),
                "--out-dir", str(out_dir),
            ])
        archive = out_dir / "perennos-tempo-controls-2.1.0-x86_64-unknown-linux-gnu.tar.gz"
        self.assertEqual(result, 0)
        self.assertEqual(captured.getvalue().splitlines()[0], str(archive))
        self.assertTrue(archive.is_file())
        self.assertTrue(Path(f"{archive}.sha256").is_file())
    def test_supported_targets_use_only_the_exact_binary_filenames(self):
        expected = {
            "x86_64-unknown-linux-gnu": ("tempo-launch", "tempo-local-agent"),
            "aarch64-apple-darwin": ("tempo-launch", "tempo-local-agent"),
            "x86_64-pc-windows-msvc": ("tempo-launch.exe", "tempo-local-agent.exe"),
            "x86_64-pc-windows-gnu": ("tempo-launch.exe", "tempo-local-agent.exe"),
        }
        self.assertEqual(set(expected), set(SUPPORTED_TARGETS))
        for index, target in enumerate(sorted(SUPPORTED_TARGETS)):
            with self.subTest(target=target):
                launcher, agent = self.sources(target)
                archive, _ = self.build(self.root / f"target-{index}", target, launcher, agent)
                root = archive.name[:-len(".tar.gz")]
                with tarfile_open(archive) as tar:
                    names = {member.name for member in tar.getmembers()}
                self.assertIn(f"{root}/bin/{expected[target][0]}", names)
                self.assertIn(f"{root}/bin/{expected[target][1]}", names)
                self.assertFalse(any(name.startswith(f"{root}/bin/") and name.endswith(".exe")
                                     for name in names) if "windows" not in target else
                                 any(name in names for name in (f"{root}/bin/tempo-launch",
                                                                f"{root}/bin/tempo-local-agent")))

    def test_unsafe_metadata_and_paths_are_rejected(self):
        launcher, agent = self.sources()
        common = {
            "target": "x86_64-unknown-linux-gnu",
            "launcher": launcher,
            "agent": agent,
            "license_file": self.legal_dir / "LICENSE-PolyForm-Shield-1.0.0.txt",
            "notice_file": self.legal_dir / "NOTICE.txt",
            "third_party_notices": self.legal_dir / "THIRD-PARTY-NOTICES.txt",
            "out_dir": self.root / "out",
        }
        for version in ("../2.1.0", "2.1.0/../../else", "01.2.3", "2.1.0-beta"):
            with self.subTest(version=version), self.assertRaises(BundleError):
                build_bundle(version=version, **common)
        with self.assertRaises(BundleError):
            build_bundle(version="2.1.0", **{**common, "target": "../../etc/passwd"})
        with self.assertRaises(BundleError):
            build_bundle(version="2.1.0", **{**common, "out_dir": self.root / ".." / "outside"})

    def test_wrong_names_missing_and_nonregular_inputs_are_rejected(self):
        launcher, agent = self.sources()
        with self.assertRaises(BundleError):
            self.build(self.root / "out", launcher=self.bin_dir / "other-launch", agent=agent)
        (self.legal_dir / "NOTICE.txt").unlink()
        with self.assertRaises(BundleError):
            self.build(self.root / "out", launcher=launcher, agent=agent)
        (self.legal_dir / "NOTICE.txt").write_bytes(b"notice\n")
        agent.unlink()
        agent.mkdir()
        with self.assertRaises(BundleError):
            self.build(self.root / "out", launcher=launcher, agent=agent)

    def test_symlink_input_is_rejected_when_symlinks_are_available(self):
        launcher, agent = self.sources()
        link_dir = self.root / "linked-bin"
        if os.name == "nt":
            result = subprocess.run(
                ["cmd.exe", "/c", "mklink", "/J", str(link_dir), str(self.bin_dir)],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                self.skipTest(f"this filesystem cannot create a junction: {result.stderr or result.stdout}")
        else:
            link_dir.symlink_to(self.bin_dir, target_is_directory=True)
        with self.assertRaises(BundleError):
            self.build(self.root / "out", launcher=link_dir / launcher.name, agent=agent)


def tarfile_open(path):
    import tarfile
    return tarfile.open(path, mode="r:gz")


if __name__ == "__main__":
    unittest.main()
