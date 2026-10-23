"""Build-time only: seal a clean official Python runtime on a disposable hosted Windows VM.

The vendor installer runs here, never on the end user's machine. Its maintenance mode can
affect an existing Python installation; do not run this on a workstation or persistent runner.
"""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time
import urllib.request
import zipfile

BASE = Path('C:/TempoCI')
TARGET = BASE / 'build-python'
OUT = BASE / 'python-runtime.zip'
URL = 'https://www.python.org/ftp/python/3.12.10/python-3.12.10-amd64.exe'
PIN = '67b5635e80ea51072b87941312d00ec8927c4db9ba18938f7ad2d27b328b95fb'
SIZE = 26964224
MANIFEST = 'PYTHON-RUNTIME-MANIFEST.json'


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def main():
    if (sys.platform != 'win32' or os.environ.get('GITHUB_ACTIONS') != 'true'
            or os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted'):
        raise RuntimeError('Build only on a disposable GitHub-hosted Windows VM, never a workstation.')
    if TARGET.exists() or OUT.exists():
        raise RuntimeError('Fresh build targets required; existing files were left untouched.')
    downloads = BASE / 'build-downloads'
    downloads.mkdir(exist_ok=False)
    installer = downloads / 'python-3.12.10-amd64.exe'
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 180
    received = 0
    with opener.open(URL, timeout=30) as response, installer.open('xb') as target:
        if response.geturl() != URL or int(response.headers.get('Content-Length', '0')) != SIZE:
            raise RuntimeError('Pinned official installer URL/size changed.')
        while True:
            if time.monotonic() > deadline:
                raise TimeoutError('Build-time Python download deadline exceeded.')
            chunk = response.read(min(1 << 20, SIZE - received + 1))
            if not chunk:
                break
            received += len(chunk)
            if received > SIZE:
                raise RuntimeError('Pinned installer exceeded its reviewed size.')
            target.write(chunk)
    if received != SIZE or digest(installer) != PIN:
        raise RuntimeError('Pinned official Python installer checksum mismatch.')
    print('Pinned build-time Python installer verified; preparing disposable runtime.', flush=True)
    env = {k: v for k, v in os.environ.items()
           if not k.upper().startswith(('PYTHON', 'TEMPO_', 'PIP_'))}
    args = [str(installer), '/quiet', 'InstallAllUsers=0', 'TargetDir=' + str(TARGET),
            'Include_exe=1', 'Include_lib=1', 'Include_pip=1', 'Include_tcltk=1', 'Include_launcher=0',
            'InstallLauncherAllUsers=0', 'PrependPath=0', 'AppendPath=0', 'AssociateFiles=0',
            'Shortcuts=0', 'Include_test=0', 'Include_doc=0', 'CompileAll=0',
            '/log', str(downloads / 'vendor-install.log')]
    installed = subprocess.run(args, env=env, capture_output=True, timeout=600)
    if installed.returncode != 0 or not (TARGET / 'python.exe').is_file():
        raise RuntimeError('Disposable Python installation failed: exit ' + str(installed.returncode))
    probe = subprocess.run([str(TARGET / 'python.exe'), '-I', '-B', '-c',
                            "import sys,tkinter,venv,ensurepip; assert sys.version_info[:3]==(3,12,10); "
                            "print(tkinter.Tcl().eval('info patchlevel'))"],
                           env=env, capture_output=True, text=True, timeout=30)
    if probe.returncode != 0:
        raise RuntimeError('Clean Python/Tcl/Tk build probe failed.')
    files = {}
    total = 0
    for path in sorted(TARGET.rglob('*')):
        info = path.lstat()
        if path.is_symlink() or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise RuntimeError('Reparse/link in upstream installed runtime.')
        if not stat.S_ISREG(info.st_mode) or '__pycache__' in path.parts or path.suffix == '.pyc':
            continue
        name = path.relative_to(TARGET).as_posix()
        if name == MANIFEST:
            raise RuntimeError('Unexpected upstream runtime manifest.')
        files[name] = digest(path)
        total += info.st_size
        if total > 512 * 1024 ** 2:
            raise RuntimeError('Upstream runtime exceeds the unpacked build cap.')
    required = {'python.exe', 'pythonw.exe', 'python312.dll', 'LICENSE.txt', 'Lib/tkinter/__init__.py',
                'Lib/venv/__init__.py', 'Lib/ensurepip/__init__.py'}
    if not required <= files.keys():
        raise RuntimeError('Upstream runtime is incomplete.')
    manifest = {'product': 'Perennos Tempo Python runtime', 'python_version': '3.12.10',
                'upstream_installer_sha256': PIN, 'files': files}
    with zipfile.ZipFile(OUT, 'x', zipfile.ZIP_DEFLATED) as archive:
        for name in sorted(files):
            entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(entry, (TARGET / name).read_bytes())
        archive.writestr(MANIFEST, json.dumps(manifest, sort_keys=True, indent=2).encode('utf-8'))
    if OUT.stat().st_size > 200 * 1024 ** 2:
        raise RuntimeError('Sealed Python runtime exceeds its compressed build cap.')
    receipt = {'product': 'Perennos Tempo Python runtime', 'python_version': '3.12.10',
               'upstream_installer_sha256': PIN, 'upstream_installer_bytes': SIZE,
               'runtime_archive_sha256': digest(OUT), 'runtime_archive_bytes': OUT.stat().st_size,
               'files': len(files), 'unpacked_bytes': total, 'tcl_version': probe.stdout.strip(),
               'builder_only': True, 'end_user_vendor_installer': False}
    (Path('C:/TempoCI-out') / 'PYTHON-RUNTIME-BUILD.json').write_text(json.dumps(receipt, indent=2), encoding='utf-8')
    print(json.dumps(receipt, indent=2), flush=True)


if __name__ == '__main__':
    main()
