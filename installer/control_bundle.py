#!/usr/bin/env python3
"""Build a small, deterministic Perennos Tempo controls archive."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import stat
import sys
import tarfile
import tempfile
from pathlib import Path
from typing import BinaryIO

VERSION_RE = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
SUPPORTED_TARGETS = frozenset({
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
})
LICENSE_NAME = "LICENSE-PolyForm-Shield-1.0.0.txt"
NOTICE_NAME = "NOTICE.txt"
THIRD_PARTY_NAME = "THIRD-PARTY-NOTICES.txt"


class BundleError(ValueError):
    """The supplied release inputs cannot form a safe controls archive."""


def _is_reparse_or_link(info: os.stat_result) -> bool:
    reparse = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    return stat.S_ISLNK(info.st_mode) or bool(getattr(info, "st_file_attributes", 0) & reparse)


def _absolute_without_traversal(value: str | os.PathLike[str], label: str) -> Path:
    raw = Path(value)
    if ".." in raw.parts:
        raise BundleError(f"{label} must not contain a parent-directory path")
    return Path(os.path.abspath(os.fspath(raw)))


def _checked_file(value: str | os.PathLike[str], expected_name: str, label: str) -> Path:
    path = _absolute_without_traversal(value, label)
    if path.name != expected_name:
        raise BundleError(f"{label} must be named {expected_name}")
    parts = path.parts
    current = Path(path.anchor)
    for index, part in enumerate(parts[1:], start=1):
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError as exc:
            raise BundleError(f"{label} is missing: {path}") from exc
        if _is_reparse_or_link(info):
            raise BundleError(f"{label} must not be a symlink or reparse point: {current}")
        final = index == len(parts) - 1
        if final:
            if not stat.S_ISREG(info.st_mode):
                raise BundleError(f"{label} must be a regular file: {current}")
        elif not stat.S_ISDIR(info.st_mode):
            raise BundleError(f"{label} has a non-directory parent: {current}")
    return path


def _open_regular(path: Path, expected_name: str, label: str) -> BinaryIO:
    _checked_file(path, expected_name, label)
    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        raise BundleError(f"cannot open {label}: {path}: {exc}") from exc
    try:
        info = os.fstat(descriptor)
        if _is_reparse_or_link(info) or not stat.S_ISREG(info.st_mode):
            raise BundleError(f"{label} must be a regular file: {path}")
        return os.fdopen(descriptor, "rb")
    except Exception:
        os.close(descriptor)
        raise


def _digest_file(path: Path, expected_name: str, label: str) -> tuple[int, str]:
    size = 0
    digest = hashlib.sha256()
    with _open_regular(path, expected_name, label) as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            size += len(block)
            digest.update(block)
    return size, digest.hexdigest()


def _safe_output_dir(value: str | os.PathLike[str]) -> Path:
    path = _absolute_without_traversal(value, "output directory")
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError:
            try:
                current.mkdir()
            except FileExistsError:
                info = current.lstat()
            else:
                continue
        if _is_reparse_or_link(info) or not stat.S_ISDIR(info.st_mode):
            raise BundleError(f"output path must contain only regular directories: {current}")
    return path


def _check_replaceable_file(path: Path) -> None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return
    if _is_reparse_or_link(info) or not stat.S_ISREG(info.st_mode):
        raise BundleError(f"output file must be regular, not a symlink or special file: {path}")


class _HashingReader:
    def __init__(self, stream: BinaryIO):
        self.stream = stream
        self.digest = hashlib.sha256()
        self.size = 0

    def read(self, size: int = -1) -> bytes:
        data = self.stream.read(size)
        self.digest.update(data)
        self.size += len(data)
        return data


def _tar_info(name: str, *, is_dir: bool, mode: int, size: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE if is_dir else tarfile.REGTYPE
    info.mode = mode
    info.size = size
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.pax_headers = {}
    return info


def build_bundle(
    *,
    version: str,
    target: str,
    launcher: str | os.PathLike[str],
    agent: str | os.PathLike[str],
    license_file: str | os.PathLike[str],
    notice_file: str | os.PathLike[str],
    third_party_notices: str | os.PathLike[str],
    out_dir: str | os.PathLike[str],
) -> tuple[Path, Path]:
    if not VERSION_RE.fullmatch(version):
        raise BundleError("version must be a stable numeric version such as 2.1.0")
    if target not in SUPPORTED_TARGETS:
        raise BundleError(f"unsupported target: {target}")

    suffix = ".exe" if "windows" in target else ""
    source_files = {
        f"bin/tempo-launch{suffix}": _checked_file(launcher, f"tempo-launch{suffix}", "launcher"),
        f"bin/tempo-local-agent{suffix}": _checked_file(agent, f"tempo-local-agent{suffix}", "local agent"),
        f"legal/{LICENSE_NAME}": _checked_file(license_file, LICENSE_NAME, "license"),
        f"legal/{NOTICE_NAME}": _checked_file(notice_file, NOTICE_NAME, "notice"),
        f"legal/{THIRD_PARTY_NAME}": _checked_file(third_party_notices, THIRD_PARTY_NAME, "third-party notices"),
    }
    payload: dict[str, Path | bytes] = {
        "VERSION": f"{version}\n".encode("ascii"),
        "TARGET": f"{target}\n".encode("ascii"),
        "EDITION": b"source-available\n",
        **source_files,
    }

    manifest_files = []
    file_info: dict[str, tuple[int, str]] = {}
    for relative in sorted(payload):
        item = payload[relative]
        if isinstance(item, bytes):
            digest = hashlib.sha256(item).hexdigest()
            size = len(item)
        else:
            size, digest = _digest_file(item, item.name, relative)
            file_info[relative] = (size, digest)
        manifest_files.append({"path": relative, "bytes": size, "sha256": digest})
    manifest = json.dumps(
        {"schema": 1, "version": version, "target": target, "files": manifest_files},
        indent=2,
        sort_keys=True,
    ).encode("utf-8") + b"\n"
    payload["CONTROLS.json"] = manifest

    root = f"perennos-tempo-controls-{version}-{target}"
    output_dir = _safe_output_dir(out_dir)
    archive = output_dir / f"{root}.tar.gz"
    sidecar = output_dir / f"{archive.name}.sha256"
    _check_replaceable_file(archive)
    _check_replaceable_file(sidecar)

    directory_members = [f"{root}/", f"{root}/bin/", f"{root}/legal/"]
    member_names = sorted(directory_members + [f"{root}/{name}" for name in payload])
    binary_mode = 0o644 if "windows" in target else 0o755
    temp_archive = None
    temp_sidecar = None
    try:
        archive_fd, temp_archive_name = tempfile.mkstemp(prefix=f".{archive.name}.", suffix=".tmp", dir=output_dir)
        temp_archive = Path(temp_archive_name)
        with os.fdopen(archive_fd, "wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                    for member in member_names:
                        relative = member[len(root) + 1:].rstrip("/")
                        is_dir = member.endswith("/")
                        if is_dir:
                            mode = 0o755
                            tar.addfile(_tar_info(member, is_dir=True, mode=mode))
                            continue
                        item = payload[relative]
                        mode = binary_mode if relative.startswith("bin/") else 0o644
                        size = len(item) if isinstance(item, bytes) else file_info[relative][0]
                        info = _tar_info(member, is_dir=False, mode=mode, size=size)
                        if isinstance(item, bytes):
                            tar.addfile(info, io.BytesIO(item))
                        else:
                            with _open_regular(item, item.name, relative) as source:
                                reader = _HashingReader(source)
                                tar.addfile(info, reader)
                                if reader.size != size or reader.digest.hexdigest() != file_info[relative][1]:
                                    raise BundleError(f"input changed while packaging: {relative}")

        archive_digest = hashlib.sha256()
        with temp_archive.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                archive_digest.update(block)
        sidecar_content = f"{archive_digest.hexdigest()}  {archive.name}\n".encode("ascii")
        sidecar_fd, temp_sidecar_name = tempfile.mkstemp(prefix=f".{sidecar.name}.", suffix=".tmp", dir=output_dir)
        temp_sidecar = Path(temp_sidecar_name)
        with os.fdopen(sidecar_fd, "wb") as stream:
            stream.write(sidecar_content)
        os.replace(temp_archive, archive)
        temp_archive = None
        os.replace(temp_sidecar, sidecar)
        temp_sidecar = None
    finally:
        if temp_archive is not None:
            temp_archive.unlink(missing_ok=True)
        if temp_sidecar is not None:
            temp_sidecar.unlink(missing_ok=True)
    return archive, sidecar


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Build a controls-only Perennos Tempo release archive.")
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--launcher", required=True, help="already-built tempo-launch binary")
    parser.add_argument("--agent", required=True, help="already-built tempo-local-agent binary")
    parser.add_argument("--license", required=True, help="staged PolyForm Shield license")
    parser.add_argument("--notice", required=True, help="staged notice")
    parser.add_argument("--third-party-notices", required=True)
    parser.add_argument("--out-dir", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        archive, sidecar = build_bundle(
            version=args.version,
            target=args.target,
            launcher=args.launcher,
            agent=args.agent,
            license_file=args.license,
            notice_file=args.notice,
            third_party_notices=args.third_party_notices,
            out_dir=args.out_dir,
        )
    except (BundleError, OSError) as exc:
        print(f"control bundle: {exc}", file=sys.stderr)
        return 2
    print(archive)
    print(sidecar.read_text(encoding="ascii"), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
