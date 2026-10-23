"""Build an unsigned Windows bootstrap from a closed app package and Python runtime ZIP."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import zipfile

PYTHON_VERSION = '3.12.10'
PYTHON_INSTALLER_SHA256 = '67b5635e80ea51072b87941312d00ec8927c4db9ba18938f7ad2d27b328b95fb'
MAX_RUNTIME_ZIP = 200 * 1024 * 1024
MAX_RUNTIME_FILES = 512 * 1024 * 1024
RUNTIME_MANIFEST = 'PYTHON-RUNTIME-MANIFEST.json'
REQUIRED_RUNTIME_FILES = {
    'python.exe', 'pythonw.exe', 'python312.dll', 'LICENSE.txt',
    'Lib/tkinter/__init__.py', 'Lib/venv/__init__.py', 'Lib/ensurepip/__init__.py',
}
_HASH = re.compile(r'[0-9a-f]{64}\Z')


def _reject_duplicate_json_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('runtime manifest contains duplicate JSON keys')
        result[key] = value
    return result


def _assert_plain_source_zip(path):
    source = Path(path)
    if not source.is_absolute():
        source = Path.cwd() / source
    for candidate in reversed((source, *source.parents)):
        try:
            info = candidate.lstat()
        except OSError as exc:
            raise ValueError('Python runtime ZIP path is unavailable') from exc
        attributes = getattr(info, 'st_file_attributes', 0)
        if stat.S_ISLNK(info.st_mode) or attributes & getattr(stat, 'FILE_ATTRIBUTE_REPARSE_POINT', 0x400):
            raise ValueError('Python runtime ZIP path contains a symlink or reparse point')
    info = source.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError('Python runtime input must be a regular ZIP file')
    if info.st_size > MAX_RUNTIME_ZIP:
        raise ValueError('Python runtime ZIP exceeds the 200 MiB compressed limit')
    return source.resolve(strict=True), info.st_size


def _safe_runtime_name(name):
    if (not name or '\x00' in name or '\\' in name or ':' in name or name.startswith('/')
            or name.endswith('/') or any(ord(ch) < 32 for ch in name)):
        raise ValueError('Python runtime ZIP contains an unsafe path')
    path = PurePosixPath(name)
    if path.is_absolute() or path.as_posix() != name or any(part in ('', '.', '..') for part in name.split('/')):
        raise ValueError('Python runtime ZIP contains an unsafe path')
    for part in path.parts:
        if part.endswith(('.', ' ')) or any(ch in part for ch in '<>"|?*'):
            raise ValueError('Python runtime ZIP contains a Windows-unsafe filename')
        if part.split('.', 1)[0].upper() in {
            'CON', 'PRN', 'AUX', 'NUL', *(f'COM{i}' for i in range(1, 10)), *(f'LPT{i}' for i in range(1, 10))
        }:
            raise ValueError('Python runtime ZIP contains a reserved Windows filename')
    return path.parts


def _runtime_entry_hash(archive, entry, remaining_limit):
    digest = hashlib.sha256()
    total = 0
    with archive.open(entry, 'r') as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > remaining_limit or total > entry.file_size:
                raise ValueError('Python runtime ZIP expands beyond its declared size limit')
            digest.update(chunk)
    if total != entry.file_size:
        raise ValueError('Python runtime ZIP member size does not match its directory record')
    return digest.hexdigest(), total


def validate_python_runtime_archive(python_runtime):
    """Verify the supplied runtime ZIP's closed manifest and every file byte."""
    archive_path, archive_size = _assert_plain_source_zip(python_runtime)
    try:
        archive = zipfile.ZipFile(archive_path, 'r')
    except (OSError, zipfile.BadZipFile) as exc:
        raise ValueError('Python runtime input is not a valid ZIP') from exc
    with archive:
        entries = archive.infolist()
        names = [entry.filename for entry in entries]
        if len(names) != len(set(names)):
            raise ValueError('Python runtime ZIP contains duplicate entries')
        if not entries or RUNTIME_MANIFEST not in names:
            raise ValueError('Python runtime ZIP is missing its manifest')
        if any(entry.is_dir() for entry in entries):
            raise ValueError('Python runtime ZIP must contain files only')
        if sum(entry.file_size for entry in entries) > MAX_RUNTIME_FILES:
            raise ValueError('Python runtime ZIP exceeds the 512 MiB unpacked limit')

        folded_parts = {}
        for entry in entries:
            parts = _safe_runtime_name(entry.filename)
            mode = (entry.external_attr >> 16) & 0xFFFF
            dos_attributes = entry.external_attr & 0xFFFF
            if (entry.flag_bits & 0x1 or dos_attributes & 0x400
                    or stat.S_ISLNK(mode) or (stat.S_IFMT(mode) not in (0, stat.S_IFREG))):
                raise ValueError('Python runtime ZIP contains a link, reparse point, or special file')
            if entry.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
                raise ValueError('Python runtime ZIP uses an unsupported compression method')
            for index, part in enumerate(parts):
                key = '/'.join(parts[:index + 1]).casefold()
                prior = folded_parts.setdefault(key, '/'.join(parts[:index + 1]))
                if prior != '/'.join(parts[:index + 1]):
                    raise ValueError('Python runtime ZIP contains case-folding path collisions')

        manifest_info = archive.getinfo(RUNTIME_MANIFEST)
        if manifest_info.file_size > 2 * 1024 * 1024:
            raise ValueError('Python runtime manifest is too large')
        try:
            manifest = json.loads(
                archive.read(manifest_info).decode('utf-8'), object_pairs_hook=_reject_duplicate_json_keys
            )
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ValueError('Python runtime manifest is invalid UTF-8 JSON') from exc
        if (not isinstance(manifest, dict)
                or set(manifest) != {'product', 'python_version', 'upstream_installer_sha256', 'files'}
                or manifest.get('product') != 'Perennos Tempo Python runtime'
                or manifest.get('python_version') != PYTHON_VERSION
                or manifest.get('upstream_installer_sha256') != PYTHON_INSTALLER_SHA256
                or not isinstance(manifest.get('files'), dict)):
            raise ValueError('Python runtime manifest metadata is invalid')

        listed = manifest['files']
        if RUNTIME_MANIFEST in listed or not REQUIRED_RUNTIME_FILES.issubset(listed):
            raise ValueError('Python runtime manifest is missing required files or lists itself')
        for name, digest in listed.items():
            _safe_runtime_name(name)
            if not isinstance(digest, str) or not _HASH.fullmatch(digest):
                raise ValueError('Python runtime manifest contains an invalid SHA-256')
        if set(names) != set(listed) | {RUNTIME_MANIFEST}:
            raise ValueError('Python runtime ZIP has missing or unlisted files')
        name_set = set(names)
        for name in names:
            parts = name.split('/')
            if any('/'.join(parts[:i]) in name_set for i in range(1, len(parts))):
                raise ValueError('Python runtime ZIP has a file where a directory is required')

        unpacked = manifest_info.file_size
        for name, expected in listed.items():
            digest, size = _runtime_entry_hash(archive, archive.getinfo(name), MAX_RUNTIME_FILES - unpacked)
            unpacked += size
            if unpacked > MAX_RUNTIME_FILES:
                raise ValueError('Python runtime ZIP exceeds the 512 MiB unpacked limit')
            if digest != expected:
                raise ValueError('Python runtime manifest hash mismatch: ' + name)

    digest = hashlib.sha256()
    with archive_path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(chunk)
    return {
        'upstream_installer_sha256': PYTHON_INSTALLER_SHA256,
        'runtime_zip_sha256': digest.hexdigest(),
        'runtime_file_count': len(listed),
        'runtime_zip_bytes': archive_size,
    }


def build(package, out, python_runtime):
    package, out = Path(package).resolve(), Path(out).resolve()
    runtime = validate_python_runtime_archive(python_runtime)
    manifest=json.loads((package/'MANIFEST.json').read_text(encoding='utf-8'))
    names=set(manifest['files']) | {'MANIFEST.json'}
    required={'tempo.py','tempo_client.py','tempo_desktop.py','tempo_bootstrap.py','tempo_setup.py','tempo_gateway.py',
              'tempo_mcp.py','fetch_models.py','model_catalog.json','tempo_local_http.py'}
    if not required.issubset(names):
        raise ValueError('package is missing required native desktop or local runtime files: '+', '.join(sorted(required-names)))
    actual={p.relative_to(package).as_posix() for p in package.rglob('*') if p.is_file()}
    if actual!=names: raise ValueError('package has missing or extra files')
    for name,digest in manifest['files'].items():
        p=(package/name).resolve()
        if package not in p.parents or p.is_symlink() or hashlib.sha256(p.read_bytes()).hexdigest()!=digest:
            raise ValueError('package path or manifest hash is invalid')
    if out.exists(): raise ValueError('output folder exists; choose a fresh folder')
    out.mkdir(parents=True)
    archive=out/'payload.zip'
    with zipfile.ZipFile(archive,'w',zipfile.ZIP_DEFLATED) as z:
        for name in sorted(names): z.write(package/name,name)
    digest=hashlib.sha256(archive.read_bytes()).hexdigest()
    bootstrap=Path(__file__).parent/'desktop/Bootstrap.cs'
    cs=bootstrap.read_text(encoding='utf-8')
    if ('__PYTHON_RUNTIME_HASH__' not in cs
            or PYTHON_INSTALLER_SHA256 not in cs):
        raise ValueError('native bootstrap is missing the bundled-runtime hash placeholder or reviewed Python pin')
    desktop=package/'tempo_bootstrap.py'
    helper=desktop.read_text(encoding='utf-8')
    if 'ollama-windows-amd64.zip' not in helper or 'e359637575302f288d76c0007c408a4be9c7376d59c4f28684d63b232a44b427' not in helper:
        raise ValueError('package bootstrap is missing the reviewed pinned isolated Ollama runtime')
    code=cs.replace('__PAYLOAD_HASH__',digest).replace('__PYTHON_RUNTIME_HASH__',runtime['runtime_zip_sha256'])
    source=out/'Bootstrap.cs';source.write_text(code,encoding='utf-8')
    framework=Path(os.environ.get('WINDIR','C:/Windows'))/'Microsoft.NET/Framework64/v4.0.30319'
    compiler=framework/'csc.exe'; exe=out/'PerennosTempo-Setup-unsigned.exe'
    if not compiler.is_file(): raise RuntimeError('installed Windows .NET compiler is required')
    args=[str(compiler),'/nologo','/target:winexe','/platform:anycpu','/optimize+','/warnaserror+',
          '/out:'+str(exe),'/resource:'+str(archive)+',payload.zip',
          '/resource:'+str(Path(python_runtime).resolve())+',python-runtime.zip',
          '/reference:System.Windows.Forms.dll','/reference:System.Drawing.dll','/reference:System.Web.Extensions.dll',
          '/reference:'+str(framework/'System.IO.Compression.dll'),
          '/reference:'+str(framework/'System.IO.Compression.FileSystem.dll'),str(source)]
    result=subprocess.run(args,capture_output=True,text=True,timeout=90)
    (out/'BUILD.log').write_text(result.stdout+result.stderr,encoding='utf-8')
    if result.returncode: raise RuntimeError('compiler failed; see BUILD.log')
    receipt={'product':'Perennos Tempo','unsigned':True,'files':len(names),'payload_sha256':digest,
             'exe_sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'compiler':str(compiler),
             'runtime_bundled':True,'runtime_bootstrap':'bundled-app-local-Python-and-isolated-local-Ollama',
             **runtime,
             'native_desktop':'WinForms-bootstrap-and-Tk-chat'}
    (out/'BUILD-RECEIPT.json').write_text(json.dumps(receipt,indent=2),encoding='utf-8')
    return {'installer':str(exe),**receipt}


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--package',required=True);p.add_argument('--out',required=True)
    p.add_argument('--python-runtime',required=True);a=p.parse_args()
    print(json.dumps(build(a.package,a.out,a.python_runtime),indent=2))
