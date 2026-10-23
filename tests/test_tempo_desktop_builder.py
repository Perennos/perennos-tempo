import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import warnings
import zipfile

import build_desktop_installer as builder


RUNTIME_FILES = {
    'python.exe': b'not executed',
    'pythonw.exe': b'not executed',
    'python312.dll': b'fixture DLL bytes',
    'LICENSE.txt': b'fixture license',
    'Lib/tkinter/__init__.py': b'fixture tkinter',
    'Lib/venv/__init__.py': b'fixture venv',
    'Lib/ensurepip/__init__.py': b'fixture ensurepip',
}


def write_runtime_zip(path, files=None, *, manifest_files=None, duplicate=None):
    files = dict(RUNTIME_FILES if files is None else files)
    hashes = manifest_files or {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}
    manifest = {
        'product': 'Perennos Tempo Python runtime',
        'python_version': builder.PYTHON_VERSION,
        'upstream_installer_sha256': builder.PYTHON_INSTALLER_SHA256,
        'files': hashes,
    }
    with zipfile.ZipFile(path, 'w', zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(builder.RUNTIME_MANIFEST, json.dumps(manifest))
        for name, data in files.items():
            archive.writestr(name, data)
        if duplicate:
            with warnings.catch_warnings():
                warnings.simplefilter('ignore', UserWarning)
                archive.writestr(duplicate, files[duplicate])


class BundledPythonArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.archive = Path(self.temp.name) / 'python-runtime.zip'

    def tearDown(self):
        self.temp.cleanup()

    def test_accepts_closed_manifest_and_hashes_every_runtime_file(self):
        write_runtime_zip(self.archive)
        result = builder.validate_python_runtime_archive(self.archive)
        self.assertEqual(result['upstream_installer_sha256'], builder.PYTHON_INSTALLER_SHA256)
        self.assertEqual(result['runtime_file_count'], len(RUNTIME_FILES))
        self.assertEqual(len(result['runtime_zip_sha256']), 64)

    def test_rejects_runtime_member_tampering(self):
        files = dict(RUNTIME_FILES)
        files['python.exe'] = b'changed after manifest generation'
        original_hashes = {name: hashlib.sha256(data).hexdigest() for name, data in RUNTIME_FILES.items()}
        write_runtime_zip(self.archive, files, manifest_files=original_hashes)
        with self.assertRaisesRegex(ValueError, 'hash mismatch'):
            builder.validate_python_runtime_archive(self.archive)

    def test_rejects_traversal_paths(self):
        files = dict(RUNTIME_FILES)
        files['../outside.txt'] = b'escape'
        with self.assertRaisesRegex(ValueError, 'unsafe path'):
            write_runtime_zip(self.archive, files)
            builder.validate_python_runtime_archive(self.archive)

    def test_rejects_casefold_collisions(self):
        files = dict(RUNTIME_FILES)
        files['Lib/Tkinter/__init__.py'] = b'collision'
        with self.assertRaisesRegex(ValueError, 'case-folding'):
            write_runtime_zip(self.archive, files)
            builder.validate_python_runtime_archive(self.archive)

    def test_rejects_duplicate_entries(self):
        write_runtime_zip(self.archive, duplicate='python.exe')
        with self.assertRaisesRegex(ValueError, 'duplicate entries'):
            builder.validate_python_runtime_archive(self.archive)


if __name__ == '__main__':
    unittest.main()
