"""Pinned, app-local Windows bootstrap for Perennos Tempo.

The WinForms shell extracts its bundled, pinned Python runtime, then calls
this module. Before any dependency, engine, or decision-model download, this
module reports the resolved byte totals and waits for a second explicit Yes.
It owns only children it starts and never installs a Windows service or edits
PATH, firewall, or machine-wide configuration.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
import errno
import hashlib
import http.client
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile


SOURCE = Path(__file__).resolve().parent
if str(SOURCE) not in sys.path:
    sys.path.insert(0, str(SOURCE))
BOOTSTRAP_PYTHON_INSTALLER_SHA256 = "67b5635e80ea51072b87941312d00ec8927c4db9ba18938f7ad2d27b328b95fb"
OLLAMA_URL = "https://github.com/ollama/ollama/releases/download/v0.40.6/ollama-windows-amd64.zip"
OLLAMA_SHA256 = "e359637575302f288d76c0007c408a4be9c7376d59c4f28684d63b232a44b427"
ENGINE = "qwen3:4b"
OLLAMA_MANIFEST_URL = "https://registry.ollama.ai/v2/library/qwen3/manifests/4b"
STATE_NAME = "desktop-runtime-state.json"
RECEIPT_NAME = "desktop-install-receipt.json"
OWNER_MARKER = ".perennos-tempo-owned.json"
MAX_ZIP_BYTES = 3 * 1024**3
MAX_HTTP_BYTES = 16 * 1024**3
MAX_RESOLVER_PREFLIGHT_BYTES = 2 * 1024**3
RESOLVER_PREFLIGHT_TIMEOUT = 15 * 60
MAX_DOWNLOAD_SECONDS = 4 * 60 * 60


class BootstrapError(RuntimeError):
    pass


class _TransientTransportError(BootstrapError):
    """A bounded local-engine transport failure that readiness may retry."""


def emit(kind: str, **details):
    row = {"type": kind, **details}
    print(json.dumps(row, separators=(",", ":")), flush=True)


def _private_error(exc: BaseException) -> str:
    if isinstance(exc, BootstrapError):
        return str(exc)[:400]
    from tempo_client import TempoAPIError
    if isinstance(exc, TempoAPIError):
        safe_codes = {"GuardError", "RuntimeError", "ValueError", "TimeoutError", "ConnectionError",
                      "OSError", "ImportError", "PermissionError", "server_error", "authentication_error"}
        code = exc.code if isinstance(exc.code, str) and exc.code in safe_codes else "gateway_error"
        status = str(exc.status) if type(exc.status) is int and 100 <= exc.status <= 599 else "unknown"
        message = f"Local gateway request failed (HTTP {status}, {code})."
        if exc.request_may_continue:
            message += " The request outcome is unknown; inspect receipts before retrying."
        return message
    return type(exc).__name__


def _check_windows():
    if os.name != "nt" or sys.version_info < (3, 12):
        raise BootstrapError("The isolated bootstrap requires Windows and Python 3.12 or newer.")


def _https_opener():
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


class _DeadlineHTTPSConnection(http.client.HTTPSConnection):
    def __init__(self, host, *, deadline, **kwargs):
        self._deadline = deadline
        self._deadline_timer = None
        self._deadline_expired = threading.Event()
        super().__init__(host, **kwargs)

    def _abort_at_deadline(self):
        self._deadline_expired.set()
        if self.sock is not None:
            try:
                self.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                self.sock.close()
            except OSError:
                pass

    def connect(self):
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Pinned download exceeded its four-hour overall deadline.")
        self.timeout = min(self.timeout or remaining, remaining)
        self._deadline_timer = threading.Timer(remaining, self._abort_at_deadline)
        self._deadline_timer.daemon = True
        self._deadline_timer.start()
        try:
            last_error = None
            for family, kind, protocol, _canonname, address in socket.getaddrinfo(
                    self.host, self.port, 0, socket.SOCK_STREAM):
                remaining = self._deadline - time.monotonic()
                if remaining <= 0 or self._deadline_expired.is_set():
                    raise TimeoutError("Pinned download exceeded its four-hour overall deadline.")
                sock = socket.socket(family, kind, protocol)
                self.sock = sock
                try:
                    sock.settimeout(remaining)
                    if self.source_address:
                        sock.bind(self.source_address)
                    sock.connect(address)
                    break
                except OSError as error:
                    last_error = error
                    sock.close()
                    self.sock = None
                    if time.monotonic() >= self._deadline or self._deadline_expired.is_set():
                        raise TimeoutError("Pinned download exceeded its four-hour overall deadline.") from None
            else:
                if last_error:
                    raise last_error
                raise OSError("The pinned download host returned no usable address.")
            if self._tunnel_host:
                self._tunnel()
            if self._deadline_expired.is_set():
                raise TimeoutError("Pinned download exceeded its four-hour overall deadline.")
            server_hostname = self._tunnel_host if self._tunnel_host else self.host
            self.sock = self._context.wrap_socket(self.sock, server_hostname=server_hostname,
                                                  do_handshake_on_connect=False)
            remaining = self._deadline - time.monotonic()
            if remaining <= 0 or self._deadline_expired.is_set():
                raise TimeoutError("Pinned download exceeded its four-hour overall deadline.")
            self.sock.settimeout(remaining)
            self.sock.do_handshake()
            if self._deadline_expired.is_set():
                raise TimeoutError("Pinned download exceeded its four-hour overall deadline.")
        except Exception:
            self.close()
            raise

    def close(self):
        if self._deadline_timer is not None:
            self._deadline_timer.cancel()
        super().close()


class _DeadlineHTTPSHandler(urllib.request.HTTPSHandler):
    def __init__(self, deadline):
        super().__init__()
        self._deadline = deadline

    def https_open(self, request):
        def connection(host, **kwargs):
            return _DeadlineHTTPSConnection(host, deadline=self._deadline, **kwargs)
        return self.do_open(connection, request, context=self._context)


class _PinnedRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        old, new = urllib.parse.urlsplit(req.full_url), urllib.parse.urlsplit(newurl)
        if new.scheme != "https" or new.username or new.password or new.port not in (None, 443):
            raise BootstrapError("A download redirected outside verified HTTPS.")
        if old.hostname not in {"huggingface.co", "registry.ollama.ai", "auth.ollama.ai",
                                "github.com", "files.pythonhosted.org", "download.pytorch.org", "download-r2.pytorch.org", "pypi.org"}:
            # Hugging Face and GitHub use signed object hosts; content is SHA-256
            # checked before use. Never permit a downgrade, credential, or port.
            raise BootstrapError("A download used an unapproved source host.")
        next_host = (new.hostname or "").lower()
        host_is_signed_asset = (next_host.endswith(".hf.co") or next_host.endswith(".huggingface.co")
                                or next_host.endswith(".githubusercontent.com")
                                or next_host.endswith(".pythonhosted.org")
                                or next_host.endswith(".pytorch.org"))
        if next_host not in {"huggingface.co", "registry.ollama.ai", "auth.ollama.ai", "github.com",
                             "objects.githubusercontent.com", "release-assets.githubusercontent.com",
                             "files.pythonhosted.org", "download.pytorch.org", "download-r2.pytorch.org", "pypi.org"} and not host_is_signed_asset:
            raise BootstrapError("A download redirected to an unapproved host.")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _opener(deadline=None):
    handlers = [urllib.request.ProxyHandler({})]
    if deadline is not None:
        handlers.append(_DeadlineHTTPSHandler(deadline))
    handlers.append(_PinnedRedirect())
    return urllib.request.build_opener(*handlers)


def _open_https(url: str, *, method="GET", headers=None, timeout=30, deadline=None):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.username or parsed.password or parsed.port not in (None, 443):
        raise BootstrapError("Download URL must use verified HTTPS.")
    allowed = {"huggingface.co", "registry.ollama.ai", "auth.ollama.ai", "github.com",
               "files.pythonhosted.org", "download.pytorch.org", "download-r2.pytorch.org", "pypi.org"}
    if parsed.hostname not in allowed:
        raise BootstrapError("Download URL uses an unapproved source host.")
    req = urllib.request.Request(url, headers=headers or {}, method=method)
    if deadline is not None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise BootstrapError("Pinned download exceeded its four-hour overall deadline.")
        timeout = min(timeout, remaining)
    return _opener(deadline).open(req, timeout=timeout)


def _response_socket(response):
    pending = [getattr(response, "fp", None)]
    seen = set()
    while pending:
        item = pending.pop()
        if item is None or id(item) in seen:
            continue
        seen.add(id(item))
        if callable(getattr(item, "settimeout", None)):
            return item
        pending.extend(getattr(item, name, None) for name in ("fp", "raw", "_sock"))
    return None


def _read_download_block(response, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise BootstrapError("Pinned download exceeded its four-hour overall deadline.")
    sock = _response_socket(response)
    if sock is not None:
        sock.settimeout(remaining)
    read1 = getattr(response, "read1", None)
    if not callable(read1):
        read1 = getattr(getattr(response, "fp", None), "read1", None)
    if callable(read1):
        return read1(1024 * 1024)
    if sock is None:
        raise BootstrapError("The pinned download stream cannot enforce its overall deadline.")
    return response.read(1)


def content_length(url: str) -> int:
    with _open_https(url, method="HEAD", timeout=45) as response:
        size = response.headers.get("Content-Length")
        if not size or not size.isdecimal():
            raise BootstrapError("A pinned download did not disclose a measurable size; setup stopped before download.")
        result = int(size)
        if result < 0 or result > MAX_HTTP_BYTES:
            raise BootstrapError("A pinned download size is outside the supported limit.")
        return result


def _download(url: str, dest: Path, expected_sha256: str | None, *, expected_bytes: int,
              progress=None):
    if type(expected_bytes) is not int or not 1 <= expected_bytes <= MAX_HTTP_BYTES:
        raise BootstrapError("Pinned download size is outside the supported limit.")
    dest.parent.mkdir(parents=True, exist_ok=True)
    temp = dest.with_name(dest.name + ".part")
    digest = hashlib.sha256()
    total = 0
    deadline = time.monotonic() + MAX_DOWNLOAD_SECONDS
    try:
        with _open_https(url, timeout=60, deadline=deadline) as response, temp.open("wb") as out:
            length = response.headers.get("Content-Length")
            expected_len = int(length) if length and length.isdecimal() else None
            if expected_len != expected_bytes:
                raise BootstrapError("Pinned download size changed since confirmation; setup stopped.")
            while True:
                if time.monotonic() >= deadline:
                    raise BootstrapError("Pinned download exceeded its four-hour overall deadline.")
                block = _read_download_block(response, deadline)
                if time.monotonic() >= deadline:
                    raise BootstrapError("Pinned download exceeded its four-hour overall deadline.")
                if not block:
                    break
                total += len(block)
                if total > expected_bytes:
                    raise BootstrapError("Download exceeded the size confirmed for this install.")
                digest.update(block)
                out.write(block)
                if progress:
                    progress(total, expected_len)
            out.flush()
            os.fsync(out.fileno())
        if total != expected_bytes:
            raise BootstrapError("Download ended before the confirmed size.")
        actual = digest.hexdigest()
        if expected_sha256 and actual.lower() != expected_sha256.lower():
            raise BootstrapError("Pinned download SHA-256 did not match; the file was not installed.")
        os.replace(temp, dest)
        return {"bytes": total, "sha256": actual}
    except Exception:
        try:
            temp.unlink()
        except OSError:
            pass
        if time.monotonic() >= deadline:
            raise BootstrapError("Pinned download exceeded its four-hour overall deadline.") from None
        raise


def safe_extract_zip(archive: Path, target: Path, *, max_unpacked=MAX_ZIP_BYTES):
    """Extract a verified archive without traversal, links, duplicate paths, or overwrite."""
    if target.exists() and any(target.iterdir()):
        raise BootstrapError("The isolated runtime folder is not empty; existing files were left untouched.")
    target.mkdir(parents=True, exist_ok=True)
    prefix = target.resolve()
    seen = set()
    total = 0
    with zipfile.ZipFile(archive) as zf:
        for item in zf.infolist():
            raw = item.filename
            path = PurePosixPath(raw)
            mode = (item.external_attr >> 16) & 0xF000
            if (not raw or "\\" in raw or path.is_absolute() or ":" in raw
                    or any(part in ("", ".", "..") for part in path.parts)
                    or raw.casefold() in seen or mode == 0xA000):
                raise BootstrapError("The runtime archive contains an unsafe path; nothing was extracted.")
            seen.add(raw.casefold())
            total += item.file_size
            if total > max_unpacked:
                raise BootstrapError("The runtime archive exceeds its extraction limit.")
            output = (prefix / Path(*path.parts)).resolve()
            if prefix not in output.parents and output != prefix:
                raise BootstrapError("The runtime archive escaped its install folder.")
            if item.is_dir():
                output.mkdir(parents=True, exist_ok=True)
                continue
            output.parent.mkdir(parents=True, exist_ok=True)
            with zf.open(item) as src, output.open("xb") as dst:
                shutil.copyfileobj(src, dst, 1024 * 1024)
    return total


def _ensure_owned_model_root(path: Path, *, purpose="owned-model-root"):
    path.mkdir(parents=True, exist_ok=True)
    marker = path / OWNER_MARKER
    if marker.exists():
        try:
            data = json.loads(marker.read_text(encoding="utf-8"))
            if data.get("product") == "Perennos Tempo" and data.get("purpose") == purpose:
                return
        except (OSError, ValueError):
            pass
        raise BootstrapError("The selected local model folder has an invalid ownership marker; no files were changed.")
    if any(path.iterdir()):
        raise BootstrapError("The selected local model folder is not empty and is not marked as Perennos Tempo-owned.")
    _atomic_json(marker, {"product": "Perennos Tempo", "purpose": purpose, "created": time.time()})


def _ensure_owned_data_root(home: Path, app_root: Path, models_root: Path):
    home.mkdir(parents=True, exist_ok=True)
    marker = home / OWNER_MARKER
    expected = {"product": "Perennos Tempo", "purpose": "owned-private-data",
                "app_root": str(app_root.resolve()), "models_root": str(models_root.resolve())}
    if marker.exists():
        try:
            current = json.loads(marker.read_text(encoding="utf-8"))
            if all(current.get(key) == value for key, value in expected.items()):
                return
        except (OSError, ValueError):
            pass
        raise BootstrapError("The selected private data folder belongs to a different or invalid install; existing files were left untouched.")
    if any(home.iterdir()):
        raise BootstrapError("The selected private data folder is not empty and is not owned by this install; existing files were left untouched.")
    _atomic_json(marker, {**expected, "created": time.time()})


def _disjoint_paths(paths):
    resolved = [Path(path).resolve() for path in paths]
    for index, left in enumerate(resolved):
        for right in resolved[index + 1:]:
            if left == right or left in right.parents or right in left.parents:
                raise BootstrapError("App files, private data, and model weights must use separate folders.")


def _assert_local_config(home: Path, *, expected_models_dir=None, expected_ollama_url=None,
                         require_owned_endpoint=False):
    tenant_path = home / "tenant.json"
    attached_path = home / "attached.json"
    try:
        tenant = json.loads(tenant_path.read_text(encoding="utf-8-sig")) if tenant_path.is_file() else {}
        attached = json.loads(attached_path.read_text(encoding="utf-8-sig")) if attached_path.is_file() else []
    except (OSError, ValueError):
        raise BootstrapError("Existing private setup files are unreadable; they were left untouched.") from None
    allowed = tenant.get("allowed_providers", []) if isinstance(tenant, dict) else None
    deployment = tenant.get("deployment", "local") if isinstance(tenant, dict) else None
    if deployment != "local" or not isinstance(allowed, list) or allowed:
        raise BootstrapError("The selected data folder has hosted or unknown provider settings; choose a separate local folder.")
    if tenant_path.is_file():
        raw_models = tenant.get("models_dir", "models/decision") if isinstance(tenant, dict) else None
        if not isinstance(raw_models, str):
            raise BootstrapError("The selected data folder has an invalid model path; existing files were left untouched.")
        configured_models = Path(os.path.expandvars(raw_models))
        if not configured_models.is_absolute():
            configured_models = home / configured_models
        if expected_models_dir is not None and configured_models.resolve() != Path(expected_models_dir).resolve():
            raise BootstrapError("The existing private setup points to a different model folder; existing files were left untouched.")
        configured_url = tenant.get("ollama_url", "http://127.0.0.1:11434") if isinstance(tenant, dict) else None
        parsed_url = urllib.parse.urlsplit(configured_url) if isinstance(configured_url, str) else None
        if (parsed_url is None or parsed_url.scheme != "http" or parsed_url.hostname != "127.0.0.1"
                or parsed_url.username or parsed_url.password or parsed_url.port is None
                or parsed_url.path not in ("", "/") or parsed_url.query or parsed_url.fragment):
            raise BootstrapError("The existing private setup has a non-loopback engine URL; existing services were left untouched.")
        if require_owned_endpoint and (expected_ollama_url is None
                or configured_url.rstrip("/") != expected_ollama_url.rstrip("/")):
            raise BootstrapError("The existing engine URL does not match this install's recorded local runtime.")
    if not isinstance(attached, list):
        raise BootstrapError("Existing provider attachments are invalid; they were left untouched.")
    for item in attached:
        if not isinstance(item, dict) or item.get("locality") not in {None, "local"}:
            raise BootstrapError("The selected data folder has a non-local attachment; choose a separate local folder.")
        if item.get("provider") not in {None, "ollama"}:
            raise BootstrapError("The selected data folder has a hosted attachment; choose a separate local folder.")


def _load_state(app_root: Path):
    state_path = app_root / STATE_NAME
    if not state_path.exists():
        return {}
    try:
        state = json.loads(state_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raise BootstrapError("Existing app runtime state is invalid; existing services were not changed.") from None
    if not isinstance(state, dict):
        raise BootstrapError("Existing app runtime state is invalid; existing services were not changed.")
    return state


def _verify_home_binding(home: Path, app_root: Path, models_root: Path, state: dict):
    _ensure_owned_data_root(home, app_root, models_root)
    tenant_path = home / "tenant.json"
    expected_url = None
    if tenant_path.is_file():
        ollama = state.get("ollama")
        if (not isinstance(ollama, dict) or not isinstance(ollama.get("port"), int)
                or ollama.get("models_root") != str((models_root / "ollama").resolve())):
            raise BootstrapError("Existing private setup has no matching app-owned engine record; existing services were left untouched.")
        expected_url = f"http://127.0.0.1:{ollama['port']}"
    _assert_local_config(home, expected_models_dir=models_root / "decision",
                         expected_ollama_url=expected_url, require_owned_endpoint=tenant_path.is_file())


def _update_owned_tenant_url(home: Path, models_root: Path, ollama_url: str, previous_url=None):
    tenant_path = home / "tenant.json"
    if not tenant_path.is_file():
        return
    _assert_local_config(home, expected_models_dir=models_root / "decision",
                         expected_ollama_url=previous_url, require_owned_endpoint=previous_url is not None)
    tenant = json.loads(tenant_path.read_text(encoding="utf-8-sig"))
    tenant["ollama_url"] = ollama_url
    _atomic_json(tenant_path, tenant)


def _atomic_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    with tmp.open("w", encoding="utf-8", newline="\n") as f:
        json.dump(value, f, indent=2)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def _directory_bytes(root: Path) -> int:
    total = 0
    if not root.exists():
        return total
    for path in root.rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except OSError:
            continue
    return total


def _report_wheel_sizes(python: Path, code: Path, app_root: Path, *, consent_resolve=False):
    if consent_resolve is not True:
        raise BootstrapError("Explicit user consent is required before dependency resolver network access.")
    venv = app_root / "runtime" / "venv"
    vpy = venv / "Scripts" / "python.exe"
    if not vpy.exists():
        subprocess.run([str(python), "-I", "-B", "-m", "venv", str(venv)], check=True, timeout=120,
                       env=_child_env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    report = app_root / "runtime" / "pip-plan.json"
    env = _child_env()
    env["PIP_CONFIG_FILE"] = os.devnull
    env["PIP_INDEX_URL"] = "https://pypi.org/simple"
    env["PIP_EXTRA_INDEX_URL"] = "https://download.pytorch.org/whl/cpu"
    resolver_temp = app_root / "runtime" / "resolver-temp"
    resolver_temp.mkdir(parents=True, exist_ok=True)
    env["TMP"] = env["TEMP"] = env["TMPDIR"] = str(resolver_temp)
    args = [str(vpy), "-I", "-B", "-m", "pip", "install", "--disable-pip-version-check", "--no-cache-dir",
            "--only-binary=:all:", "--dry-run", "--report", str(report), "-r", str(code / "requirements.txt")]
    log_path = app_root / "runtime" / "resolver-preflight.log"
    with log_path.open("wb") as log:
        process = subprocess.Popen(args, cwd=str(code), env=env, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + RESOLVER_PREFLIGHT_TIMEOUT
        while process.poll() is None:
            try:
                report_bytes = report.stat().st_size if report.exists() else 0
                log_bytes = log_path.stat().st_size
            except OSError:
                report_bytes = log_bytes = MAX_RESOLVER_PREFLIGHT_BYTES
            preflight_bytes = _directory_bytes(resolver_temp) + report_bytes + log_bytes
            if time.monotonic() >= deadline or preflight_bytes > MAX_RESOLVER_PREFLIGHT_BYTES:
                process.kill()
                process.wait(timeout=10)
                raise BootstrapError("Dependency resolution exceeded its 15-minute or 2 GiB app-local preflight limit.")
            time.sleep(0.2)
        return_code = process.returncode
    if return_code or not report.is_file():
        raise BootstrapError("Pinned dependency planning failed; no dependency download was started.")
    try:
        document = json.loads(report.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raise BootstrapError("Dependency planner returned an invalid report; setup stopped before download.") from None
    files = []
    for install in document.get("install", []):
        info = install.get("download_info", {})
        url = info.get("url")
        if not isinstance(url, str):
            continue
        size = content_length(url)
        hashes = info.get("archive_info", {}).get("hashes", {})
        sha = hashes.get("sha256") if isinstance(hashes, dict) else None
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", sha):
            raise BootstrapError("A dependency wheel has no SHA-256 in the official package report.")
        package = install.get("metadata", {}).get("name", "package")
        filename = urllib.parse.unquote(Path(urllib.parse.urlsplit(url).path).name)
        if not filename.lower().endswith(".whl") or not re.fullmatch(r"[A-Za-z0-9_.+-]+\.whl", filename, re.IGNORECASE):
            raise BootstrapError("Dependency planner selected a non-wheel archive; setup stopped before download.")
        files.append({"name": package, "filename": filename, "bytes": size,
                      "sha256": sha.lower(), "url": url})
    return venv, vpy, files, str(report)


def _registry_manifest():
    headers = {"Accept": "application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json"}
    try:
        response = _open_https(OLLAMA_MANIFEST_URL, headers=headers, timeout=45)
    except urllib.error.HTTPError as exc:
        if exc.code != 401:
            raise BootstrapError("The selected local engine could not be verified with its official registry.") from None
        challenge = exc.headers.get("WWW-Authenticate", "")
        match = re.fullmatch(r'Bearer realm="(https://[^" ]+)",service="([^"]+)"(?:,scope="([^"]+)")?', challenge)
        if not match:
            raise BootstrapError("The official model registry returned an unsupported authentication challenge.") from None
        realm, service, scope = match.groups()
        if urllib.parse.urlsplit(realm).hostname != "auth.ollama.ai" or service != "registry.ollama.ai":
            raise BootstrapError("The official model registry requested an unapproved authentication host.") from None
        query = urllib.parse.urlencode({"service": service, "scope": scope or "repository:library/qwen3:pull"})
        with _open_https(realm + "?" + query, timeout=45) as token_response:
            token_payload = json.load(token_response)
        token = token_payload.get("token") or token_payload.get("access_token")
        if not isinstance(token, str) or len(token) > 8192:
            raise BootstrapError("The model registry did not provide a valid pull token.")
        response = _open_https(OLLAMA_MANIFEST_URL, headers={**headers, "Authorization": "Bearer " + token}, timeout=45)
    with response:
        if int(response.headers.get("Content-Length", "0")) > 2 * 1024 * 1024:
            raise BootstrapError("The selected engine manifest exceeds its size limit.")
        raw = response.read((2 * 1024 * 1024) + 1)
        if len(raw) > 2 * 1024 * 1024:
            raise BootstrapError("The selected engine manifest exceeds its size limit.")
        manifest_digest = response.headers.get("Docker-Content-Digest") or ("sha256:" + hashlib.sha256(raw).hexdigest())
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", manifest_digest):
            raise BootstrapError("The selected engine manifest has an invalid registry digest.")
        doc = json.loads(raw.decode("utf-8"))
    layers = doc.get("layers", [])
    if not isinstance(layers, list) or not layers:
        raise BootstrapError("The selected engine registry manifest is incomplete.")
    blobs = []
    for layer in [doc.get("config", {})] + layers:
        digest, size = layer.get("digest"), layer.get("size")
        if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise BootstrapError("The selected engine manifest has an invalid blob digest.")
        if isinstance(size, bool) or not isinstance(size, int) or not 0 <= size <= MAX_HTTP_BYTES:
            raise BootstrapError("The selected engine manifest has an invalid blob size.")
        blobs.append({"digest": digest, "bytes": size})
    return {"digest": manifest_digest, "blobs": blobs, "total_bytes": sum(x["bytes"] for x in blobs)}


def _valid_pinned_file(path: Path, digest: str) -> bool:
    if not path.is_file():
        return False
    h = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            h.update(block)
    return h.hexdigest() == digest


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            h.update(block)
    return h.hexdigest()


def _verified_bootstrap_python(python: Path, app_root: Path):
    python = python.resolve()
    expected = (app_root / "runtime" / "python" / "python.exe").resolve()
    try:
        receipt = json.loads((expected.parent / "python-runtime.json").read_text(encoding="utf-8-sig"))
        digest = _sha256_file(python)
    except (OSError, ValueError):
        raise BootstrapError("The verified app-owned bootstrap Python receipt is unavailable.") from None
    if (not isinstance(receipt, dict) or python != expected
            or receipt.get("product") != "Perennos Tempo"
            or receipt.get("python_version") != "3.12.10"
            or receipt.get("installer_sha256") != BOOTSTRAP_PYTHON_INSTALLER_SHA256
            or receipt.get("python_sha256") != digest):
        raise BootstrapError("The bootstrap Python argument does not match its verified app-owned receipt.")
    return python, digest


def _verified_ollama_runtime(path: Path) -> bool:
    exe, marker = path / "ollama.exe", path / OWNER_MARKER
    try:
        data = json.loads(marker.read_text(encoding="utf-8"))
        return (data.get("product") == "Perennos Tempo" and data.get("purpose") == "owned-ollama-runtime"
                and data.get("version") == "0.40.6" and data.get("archive_sha256") == OLLAMA_SHA256
                and exe.is_file() and data.get("ollama_exe_sha256") == _sha256_file(exe))
    except (OSError, ValueError):
        return False


def _decision_plan(home: Path, models_dir: Path):
    os.environ["TEMPO_HOME"] = str(home)
    from fetch_models import PINS
    items = []
    for rel, url, digest in PINS:
        target = models_dir / rel
        if target.exists() and not _valid_pinned_file(target, digest):
            raise BootstrapError("A selected decision-model file exists with a different hash; existing files were left untouched.")
        if not _valid_pinned_file(target, digest):
            items.append({"path": str(target), "url": url, "sha256": digest, "bytes": content_length(url)})
    return items


def _engine_catalog_digest(code: Path) -> str:
    try:
        catalog = json.loads((code / "model_catalog.json").read_text(encoding="utf-8-sig"))
        matches = [item for item in catalog["engines"] if item.get("name") == ENGINE]
        digest = matches[0].get("installed_digest") if len(matches) == 1 else None
    except (OSError, ValueError, KeyError, TypeError):
        digest = None
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise BootstrapError("The selected engine has no valid pinned local revision in the app catalog.")
    return digest


def _manifest_blob_plan(ollama_models: Path):
    manifest = _registry_manifest()
    missing = {}
    store = ollama_models / "models" / "blobs"
    for blob in manifest["blobs"]:
        name = "sha256-" + blob["digest"].split(":", 1)[1]
        dest = store / name
        if not dest.is_file() or dest.stat().st_size != blob["bytes"]:
            missing[blob["digest"]] = blob
    return list(missing.values()), manifest


def _disk_plan(paths_and_needs):
    by_drive = {}
    for path, required in paths_and_needs:
        drive = path.drive.lower() or path.anchor.lower()
        slot = by_drive.setdefault(drive, {"required_bytes": 0, "paths": []})
        slot["required_bytes"] += required
        slot["paths"].append(path)
    result = []
    for drive, slot in sorted(by_drive.items()):
        path = next((p for p in slot["paths"] if p.exists()), slot["paths"][0])
        try:
            free = shutil.disk_usage(path if path.exists() else path.parent).free
        except OSError:
            raise BootstrapError("Could not verify free space on the selected install or model drive.") from None
        if free < slot["required_bytes"]:
            raise BootstrapError(f"Not enough free space on drive {drive}: need {slot['required_bytes']} bytes; {free} bytes are free.")
        result.append({"drive": drive, "required_bytes": slot["required_bytes"], "free_bytes": free})
    return result


def plan(python: Path, app_root: Path, home: Path, models_root: Path, code: Path, *, consent_resolve=False):
    if consent_resolve is not True:
        raise BootstrapError("Explicit user consent is required before dependency resolver network access.")
    _check_windows()
    for value in (python, app_root, home, models_root, code):
        if not value.is_absolute():
            raise BootstrapError("All selected install and data folders must be absolute paths.")
    if app_root == Path(app_root.anchor) or len(str(app_root)) > 120:
        raise BootstrapError("Choose a short, non-root app folder for the Windows runtime.")
    _disjoint_paths((app_root, home, models_root))
    if not (code / "tempo.py").is_file() or not (code / "tempo_setup.py").is_file():
        raise BootstrapError("The extracted Perennos Tempo app files are incomplete.")
    if not python.is_file():
        raise BootstrapError("The per-user Python runtime is missing; return to the setup window.")
    _ensure_owned_model_root(models_root, purpose="owned-model-root")
    _verify_home_binding(home, app_root, models_root, _load_state(app_root))
    ollama_models = models_root / "ollama"
    _ensure_owned_model_root(ollama_models, purpose="owned-ollama-models")
    venv, vpy, wheels, pip_report = _report_wheel_sizes(python, code, app_root,
                                                         consent_resolve=consent_resolve)
    decision = _decision_plan(home, models_root / "decision")
    missing_blobs, engine_manifest = _manifest_blob_plan(ollama_models)
    ollama_dir = app_root / "runtime" / "ollama"
    ollama_zip = app_root / "runtime" / "ollama-windows-amd64-v0.40.6.zip"
    ollama_needed = not _verified_ollama_runtime(ollama_dir)
    if ollama_needed and ollama_dir.exists() and any(ollama_dir.iterdir()):
        raise BootstrapError("The app Ollama runtime folder contains unverified files; existing files were left untouched.")
    if ollama_needed and ollama_zip.exists() and not _valid_pinned_file(ollama_zip, OLLAMA_SHA256):
        raise BootstrapError("An existing Ollama archive failed its pinned hash; existing files were left untouched.")
    ollama_zip_bytes = content_length(OLLAMA_URL) if ollama_needed and not ollama_zip.exists() else 0
    wheel_bytes = sum(item["bytes"] for item in wheels)
    engine_bytes = sum(item["bytes"] for item in missing_blobs)
    decision_bytes = sum(item["bytes"] for item in decision)
    total_download = wheel_bytes + engine_bytes + decision_bytes + ollama_zip_bytes
    # Reserve unpacked wheels, the decision head, Ollama's extracted files, and
    # a 20% safety margin. Downloads are partitioned by their chosen locations.
    app_required = (ollama_zip_bytes * 2 + wheel_bytes * 3 + MAX_RESOLVER_PREFLIGHT_BYTES
                    + 512 * 1024**2) if ollama_needed or wheels else 0
    model_required = engine_bytes * 2 + decision_bytes * 2 + 256 * 1024**2
    free_by_drive = _disk_plan(((app_root, app_required), (models_root, model_required),
                                (home, 256 * 1024**2)))
    drive_text = "; ".join(f"{entry['drive']}: require {entry['required_bytes']:,} bytes, "
                            f"{entry['free_bytes']:,} bytes free" for entry in free_by_drive)
    return {
        "python": str(python), "app_root": str(app_root), "home": str(home),
        "models_root": str(models_root), "ollama_models": str(ollama_models),
        "venv": str(venv), "venv_python": str(vpy), "pip_report": pip_report, "engine": ENGINE,
        "engine_digest": _engine_catalog_digest(code),
        "wheel_count": len(wheels), "wheel_bytes": wheel_bytes,
        "engine_blob_count": len(missing_blobs), "engine_bytes": engine_bytes,
        "engine_manifest_digest": engine_manifest["digest"],
        "engine_manifest_total_bytes": engine_manifest["total_bytes"],
        "decision_file_count": len(decision), "decision_bytes": decision_bytes,
        "ollama_zip_bytes": ollama_zip_bytes, "download_bytes": total_download,
        "required_app_bytes": app_required, "required_models_bytes": model_required,
        "drive_space": free_by_drive, "drive_space_text": drive_text,
        "downloads": {"wheels": wheels, "engine_blobs": missing_blobs, "decision_files": decision,
                      "ollama_zip": OLLAMA_URL if ollama_needed and not ollama_zip.exists() else None},
        "ollama_zip": str(ollama_zip), "ollama_dir": str(ollama_dir),
        "decision_dir": str(models_root / "decision"),
    }


def _child_env(*, home=None, ollama_host=None, ollama_models=None, ollama_runtime=None):
    env = dict(os.environ)
    for key in list(env):
        if key.upper().startswith(("OLLAMA_", "TEMPO_", "PIP_", "PYTHON")):
            env.pop(key, None)
    env.pop("HTTP_PROXY", None); env.pop("HTTPS_PROXY", None); env.pop("ALL_PROXY", None)
    env.pop("http_proxy", None); env.pop("https_proxy", None); env.pop("all_proxy", None)
    if home:
        env["TEMPO_HOME"] = str(home)
    if ollama_host:
        env["OLLAMA_HOST"] = ollama_host
    if ollama_models:
        env["OLLAMA_MODELS"] = str(ollama_models)
    if ollama_runtime is not None:
        system_root = env.get("SystemRoot") or env.get("SYSTEMROOT") or env.get("WINDIR")
        if not system_root:
            raise BootstrapError("The Windows system path is unavailable for the bundled local engine.")
        runtime_path = str(Path(ollama_runtime).resolve())
        windows_paths = (str(Path(system_root) / "System32"), system_root)
        env["PATH"] = os.pathsep.join(dict.fromkeys((runtime_path, *windows_paths)))
    env["PYTHONUTF8"] = "1"
    return env


def _process_parents():
    if os.name != "nt":
        return None
    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [("dwSize", ctypes.c_ulong), ("cntUsage", ctypes.c_ulong),
                    ("th32ProcessID", ctypes.c_ulong), ("th32DefaultHeapID", ctypes.c_size_t),
                    ("th32ModuleID", ctypes.c_ulong), ("cntThreads", ctypes.c_ulong),
                    ("th32ParentProcessID", ctypes.c_ulong), ("pcPriClassBase", ctypes.c_long),
                    ("dwFlags", ctypes.c_ulong), ("szExeFile", ctypes.c_wchar * 260)]
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateToolhelp32Snapshot.argtypes = [ctypes.c_ulong, ctypes.c_ulong]
    kernel.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
    kernel.Process32FirstW.argtypes = [ctypes.c_void_p, ctypes.POINTER(PROCESSENTRY32W)]
    kernel.Process32FirstW.restype = ctypes.c_int
    kernel.Process32NextW.argtypes = [ctypes.c_void_p, ctypes.POINTER(PROCESSENTRY32W)]
    kernel.Process32NextW.restype = ctypes.c_int
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel.CloseHandle.restype = ctypes.c_int
    snapshot = kernel.CreateToolhelp32Snapshot(0x00000002, 0)
    if not snapshot or snapshot == ctypes.c_void_p(-1).value:
        return None
    try:
        parents = {}
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(entry)
        ctypes.set_last_error(0)
        ok = kernel.Process32FirstW(snapshot, ctypes.byref(entry))
        if not ok:
            return None
        while ok:
            parents[int(entry.th32ProcessID)] = int(entry.th32ParentProcessID)
            ctypes.set_last_error(0)
            ok = kernel.Process32NextW(snapshot, ctypes.byref(entry))
            if not ok and ctypes.get_last_error() != 18:  # ERROR_NO_MORE_FILES
                return None
        return parents
    finally:
        kernel.CloseHandle(snapshot)


def _parent_pid(pid: int):
    if pid < 1:
        return None
    parents = _process_parents()
    return parents.get(pid) if parents is not None else None


def _direct_child_pids(parent_pid: int):
    parents = _process_parents()
    if parents is None:
        raise BootstrapError("Could not enumerate direct child process identities.")
    return [pid for pid, owner in parents.items() if owner == parent_pid]


def _parse_ipv4_tcp_table(raw: bytes):
    if len(raw) < 4:
        raise BootstrapError("Windows returned an incomplete TCP ownership table.")
    count = struct.unpack_from("<I", raw, 0)[0]
    row_size = 24  # MIB_TCPROW_OWNER_PID: six DWORD fields.
    if count > 65535 or len(raw) < 4 + count * row_size:
        raise BootstrapError("Windows returned a malformed TCP ownership table.")
    entries = []
    for index in range(count):
        state, local_addr, local_port, remote_addr, remote_port, pid = struct.unpack_from(
            "<IIIIII", raw, 4 + index * row_size)
        entries.append({"state": int(state),
                        "local_address": socket.inet_ntoa(struct.pack("<I", local_addr)),
                        "local_port": socket.ntohs(local_port & 0xFFFF),
                        "remote_address": socket.inet_ntoa(struct.pack("<I", remote_addr)),
                        "remote_port": socket.ntohs(remote_port & 0xFFFF),
                        "pid": int(pid)})
    return entries


def _parse_ipv4_listener_table(raw: bytes, port: int):
    return [{"pid": entry["pid"], "address": entry["local_address"], "port": entry["local_port"]}
            for entry in _parse_ipv4_tcp_table(raw)
            if entry["state"] == 2 and entry["local_port"] == port]


def _server_peer_is_owned(raw: bytes, port: int, client_port: int, owner_pid: int):
    matches = [entry for entry in _parse_ipv4_tcp_table(raw)
               if entry["state"] == 5 and entry["local_address"] == "127.0.0.1"
               and entry["local_port"] == port and entry["remote_address"] == "127.0.0.1"
               and entry["remote_port"] == client_port]
    return len(matches) == 1 and matches[0]["pid"] == owner_pid


def _extended_tcp_table(table_class: int):
    iphlpapi = ctypes.WinDLL("iphlpapi", use_last_error=True)
    call = iphlpapi.GetExtendedTcpTable
    call.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong), ctypes.c_int,
                     ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    call.restype = ctypes.c_ulong
    size = ctypes.c_ulong(0)
    result = call(None, ctypes.byref(size), 0, 2, table_class, 0)
    if result not in (0, 122) or size.value > 16 * 1024 * 1024:
        raise BootstrapError("Could not read Windows TCP connection ownership.")
    for _ in range(3):
        buffer = ctypes.create_string_buffer(size.value)
        result = call(buffer, ctypes.byref(size), 0, 2, table_class, 0)
        if result == 0:
            return buffer.raw[:size.value]
        if result != 122 or size.value > 16 * 1024 * 1024:
            break
    raise BootstrapError("Windows TCP connection ownership changed or could not be verified.")


def _listener_owners(port: int):
    if os.name != "nt" or not 1 <= port <= 65535:
        raise BootstrapError("Could not verify the app-owned local engine listener.")
    # AF_INET=2, TCP_TABLE_OWNER_PID_LISTENER=3.
    return _parse_ipv4_listener_table(_extended_tcp_table(3), port)


def _process_identity(pid: int):
    if os.name != "nt" or pid < 1:
        return None
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.QueryFullProcessImageNameW.argtypes = [ctypes.c_void_p, ctypes.c_ulong,
                                                   ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel.QueryFullProcessImageNameW.restype = ctypes.c_int
    kernel.GetProcessTimes.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
                                       ctypes.c_void_p, ctypes.c_void_p]
    kernel.GetProcessTimes.restype = ctypes.c_int
    kernel.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel.GetExitCodeProcess.restype = ctypes.c_int
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel.CloseHandle.restype = ctypes.c_int
    handle = kernel.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        # Invalid parameter means the PID is no longer present. Access denied
        # or any other failure is unknown, so callers must fail closed.
        return None if ctypes.get_last_error() == 87 else {"unavailable": True}
    try:
        exit_code = ctypes.c_ulong()
        if not kernel.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
            return {"unavailable": True}
        if exit_code.value != 259:  # A retained handle can outlive its process.
            return None
        size = ctypes.c_ulong(32768)
        buffer = ctypes.create_unicode_buffer(size.value)
        if not kernel.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
            return {"unavailable": True}
        class FILETIME(ctypes.Structure):
            _fields_ = [("low", ctypes.c_ulong), ("high", ctypes.c_ulong)]
        creation, exit_time, kernel_time, user_time = FILETIME(), FILETIME(), FILETIME(), FILETIME()
        if not kernel.GetProcessTimes(handle, ctypes.byref(creation), ctypes.byref(exit_time),
                                      ctypes.byref(kernel_time), ctypes.byref(user_time)):
            return {"unavailable": True}
        stamp = (creation.high << 32) | creation.low
        parent_pid = _parent_pid(pid)
        if parent_pid is None:
            return {"unavailable": True}
        return {"path": str(Path(buffer.value).resolve()), "created": stamp,
                "parent_pid": parent_pid}
    finally:
        kernel.CloseHandle(handle)


def _same_executable(actual: str, expected: Path, digest: str | None = None):
    try:
        path = Path(actual)
        return path.samefile(expected) and (digest is None or _sha256_file(path) == digest)
    except (OSError, TypeError, ValueError):
        return False


def _find_runtime_child(launcher_pid: int, python: Path, python_sha256: str):
    matches = []
    for pid in _direct_child_pids(launcher_pid):
        identity = _process_identity(pid)
        if identity is None:
            continue
        if identity.get("unavailable") or identity.get("parent_pid") != launcher_pid:
            raise BootstrapError("A direct child identity could not be verified.")
        if not _same_executable(identity.get("path"), python, python_sha256):
            raise BootstrapError("A direct child did not match the verified bootstrap Python image.")
        matches.append((pid, identity))
    if len(matches) > 1:
        raise BootstrapError("Multiple direct children matched the verified bootstrap Python image.")
    return matches[0] if matches else None


def _owned_process(record, expected_exe: Path, python: Path | None = None,
                   python_sha256: str | None = None):
    if (not isinstance(record, dict) or type(record.get("pid")) is not int
            or record["pid"] < 1):
        return False
    identity = _process_identity(record.get("pid", 0))
    if not identity or identity.get("unavailable"):
        return False
    try:
        launcher_pid = record.get("launcher_pid")
        launcher_created = record.get("launcher_created")
        if (type(launcher_pid) is not int or launcher_pid < 1
                or type(launcher_created) is not int or launcher_created < 1
                or identity.get("parent_pid") != record.get("runtime_parent_pid")
                or record.get("runtime_parent_pid") != launcher_pid
                or record.get("runtime_parent_created") != launcher_created
                or identity.get("created") != record.get("created")
                or not _same_executable(identity.get("path"), Path(record.get("path", "")),
                                        record.get("sha256"))):
            return False
        launcher = _process_identity(launcher_pid)
        launcher_path = Path(record.get("launcher_path", ""))
        launcher_sha256 = record.get("launcher_sha256")
        if record.get("runtime_mode") == "python-child":
            if (not _same_executable(str(launcher_path), expected_exe, launcher_sha256)
                    or python is None or not isinstance(python_sha256, str)
                    or record.get("python_path") != str(python.resolve())
                    or record.get("python_sha256") != python_sha256
                    or not _same_executable(identity.get("path"), python, python_sha256)):
                return False
        elif (record.get("runtime_mode") != "direct"
              or not _same_executable(str(launcher_path), launcher_path, launcher_sha256)
              or not _same_executable(identity.get("path"), expected_exe, record.get("sha256"))):
            return False
        # ERROR_INVALID_PARAMETER means the launcher PID is confirmed absent.
        # Access/query failure is unknown and cannot authorize reuse.
        if launcher is None:
            return record.get("runtime_mode") in {"direct", "python-child"}
        if (launcher.get("unavailable") or launcher.get("created") != launcher_created
                or not _same_executable(launcher.get("path"), launcher_path, launcher_sha256)):
            return False
        return True
    except (OSError, TypeError, ValueError, KeyError):
        return False


def _choose_port(start: int, stop: int):
    for port in range(start, stop + 1):
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 0)
            try:
                sock.bind(("127.0.0.1", port))
            except OSError:
                continue
            return port
    raise BootstrapError("No unused loopback port is available for this app-owned service.")


def _launch_owned(executable: Path, args, *, cwd: Path, env, log_path: Path, role: str, port: int,
                  python: Path | None = None, python_sha256: str | None = None):
    executable = executable.resolve()
    executable_sha256 = _sha256_file(executable)
    if python is not None:
        python = python.resolve()
        if (not isinstance(python_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", python_sha256)
                or not _same_executable(str(python), python, python_sha256)):
            raise BootstrapError("The verified bootstrap Python path or hash changed before service launch.")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log = log_path.open("ab", buffering=0)
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0) | getattr(subprocess, "DETACHED_PROCESS", 0)
    proc = subprocess.Popen([str(executable), *args], cwd=str(cwd), env=env, stdin=subprocess.DEVNULL,
                            stdout=log, stderr=log, close_fds=True, creationflags=flags)
    log.close()
    deadline = time.monotonic() + 15
    launcher = None
    while time.monotonic() < deadline and proc.poll() is None:
        launcher = _process_identity(proc.pid)
        if launcher and not launcher.get("unavailable"):
            break
        time.sleep(0.1)
    if proc.poll() is not None or not launcher or launcher.get("unavailable"):
        raise BootstrapError(f"Could not start the app-owned {role} child process.")
    if (launcher.get("parent_pid") != os.getpid()
            or not _same_executable(launcher.get("path"), executable, executable_sha256)):
        raise BootstrapError(f"Could not prove the app-owned {role} child process identity.")
    owner = _process_identity(os.getpid())
    if not owner or owner.get("unavailable"):
        raise BootstrapError(f"Could not prove the app-owned {role} launcher identity.")
    runtime_mode = "direct"
    runtime_pid, identity = proc.pid, launcher
    runtime_parent_pid, runtime_parent_created = os.getpid(), owner["created"]
    launcher_pid, launcher_created = os.getpid(), owner["created"]
    launcher_path, launcher_sha256 = owner["path"], _sha256_file(Path(owner["path"]))
    if python is not None:
        runtime_mode = "python-child"
        launcher_pid, launcher_created = proc.pid, launcher["created"]
        launcher_path, launcher_sha256 = launcher["path"], executable_sha256
        runtime_parent_pid, runtime_parent_created = proc.pid, launcher["created"]
        child = None
        while time.monotonic() < deadline:
            child = _find_runtime_child(proc.pid, python, python_sha256)
            if child:
                break
            time.sleep(0.1)
        if not child:
            raise BootstrapError(f"Could not find exactly one verified Python child for the {role} launcher.")
        runtime_pid, identity = child
    return {"pid": runtime_pid, "created": identity["created"], "path": identity["path"],
            "sha256": _sha256_file(Path(identity["path"])), "runtime_mode": runtime_mode,
            "runtime_parent_pid": runtime_parent_pid, "runtime_parent_created": runtime_parent_created,
            "port": port, "launcher_pid": launcher_pid, "launcher_created": launcher_created,
            "launcher_path": launcher_path, "launcher_sha256": launcher_sha256,
            **({"python_path": str(python), "python_sha256": python_sha256} if python else {})}


def _assert_owned_listener(record, executable: Path, port: int, *, allow_absent=False,
                           python: Path | None = None, python_sha256: str | None = None):
    if (not _owned_process(record, executable, python, python_sha256)
            or record.get("port") != port):
        raise BootstrapError("The local engine process does not match this install's recorded child identity.")
    owners = _listener_owners(port)
    if not owners and allow_absent:
        return False
    if len(owners) != 1 or owners[0]["pid"] != record.get("pid") or owners[0]["address"] != "127.0.0.1":
        raise BootstrapError("The local engine listener is not exclusively bound to the recorded app-owned child.")
    return True


def _assert_connected_peer(sock, record, executable: Path, port: int, *,
                           python: Path | None = None, python_sha256: str | None = None):
    local_address, client_port = sock.getsockname()[:2]
    peer_address, peer_port = sock.getpeername()[:2]
    if (local_address != "127.0.0.1" or peer_address != "127.0.0.1" or peer_port != port
            or not isinstance(client_port, int)):
        raise BootstrapError("The connected local engine peer did not match the fixed loopback endpoint.")
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline:
        if not _owned_process(record, executable, python, python_sha256):
            raise BootstrapError("The connected local engine no longer matches its recorded child identity.")
        # TCP_TABLE_OWNER_PID_ALL=5. The accepted server-side row is keyed by
        # this socket's ephemeral client port, pinning the peer before sendall.
        if _server_peer_is_owned(_extended_tcp_table(5), port, client_port, record["pid"]):
            return
        time.sleep(0.025)
    raise BootstrapError("Could not prove the connected TCP peer belongs to the recorded local engine child.")


def _connection_owner_guard(record, executable: Path, port: int, *,
                            python: Path | None = None, python_sha256: str | None = None):
    def verify_before_request(active_request):
        try:
            _assert_owned_listener(record, executable, port, python=python,
                                   python_sha256=python_sha256)
            connection = getattr(active_request, "connection", None)
            sock = getattr(connection, "sock", None)
            if sock is None:
                raise BootstrapError("The connected local service socket is unavailable for ownership verification.")
            _assert_connected_peer(sock, record, executable, port, python=python,
                                   python_sha256=python_sha256)
        except Exception:
            close = getattr(active_request, "close", None)
            if callable(close):
                close()
            raise
    return verify_before_request


def _guarded_gateway_request(record, executable: Path, token: str, method: str, path: str,
                             body=None, *, timeout=5, python: Path | None = None,
                             python_sha256: str | None = None):
    if not isinstance(record, dict) or type(record.get("port")) is not int:
        raise BootstrapError("The local gateway has no verified app-owned listener record.")
    _assert_owned_listener(record, executable, record["port"], python=python,
                           python_sha256=python_sha256)
    from tempo_client import request_json
    return request_json("127.0.0.1", record["port"], token, method, path, body,
                        timeout=timeout,
                        on_connection=_connection_owner_guard(record, executable, record["port"],
                                                              python=python, python_sha256=python_sha256))


def _probe_error_details(exc: BaseException):
    details = [type(exc).__name__[:80]]
    for key in ("errno", "winerror"):
        value = getattr(exc, key, None)
        if type(value) is int and -(1 << 31) <= value < (1 << 31):
            details.append(f"{key}={value}")
    return ", ".join(details)


def _record_probe_error(path: Path | None, stage: str, exc: BaseException):
    if path is None:
        return
    record = {
        "service": "ollama",
        "stage": stage,
        "error_type": type(exc).__name__[:80],
        "recorded_at": int(time.time()),
    }
    for key in ("errno", "winerror"):
        value = getattr(exc, key, None)
        if type(value) is int and -(1 << 31) <= value < (1 << 31):
            record[key] = value
    try:
        _atomic_json(path, record)
    except OSError:
        pass


def _is_transient_transport_error(exc: BaseException):
    if isinstance(exc, (TimeoutError, ConnectionError, http.client.IncompleteRead)):
        return True
    return (isinstance(exc, OSError)
            and (getattr(exc, "errno", None) in {
                errno.ECONNABORTED, errno.ECONNREFUSED, errno.ECONNRESET,
                errno.EHOSTUNREACH, errno.ENETUNREACH, errno.ETIMEDOUT,
            } or getattr(exc, "winerror", None) in {10053, 10054, 10060, 10061, 10065}))


def _raise_probe_error(stage: str, exc: BaseException, diagnostic_path: Path | None = None, *, transient=False):
    _record_probe_error(diagnostic_path, stage, exc)
    error_type = _TransientTransportError if transient else BootstrapError
    raise error_type(f"Local engine probe failed at {stage}: {_probe_error_details(exc)}.") from exc


def _local_json(url: str, *, timeout=5, method="GET", body=None, owner_record=None, owner_exe=None,
                diagnostic_path: Path | None = None):
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or parsed.username or parsed.password
            or parsed.query or parsed.fragment or parsed.path not in {"/api/version", "/api/tags"}
            or method != "GET" or body is not None):
        raise BootstrapError("Local service request must use the app's loopback address.")
    if not isinstance(parsed.port, int) or owner_record is None or owner_exe is None:
        raise BootstrapError("Local engine requests require a verified app-owned listener record.")
    try:
        _assert_owned_listener(owner_record, owner_exe, parsed.port)
    except BootstrapError as exc:
        _raise_probe_error("ownership", exc, diagnostic_path)
    try:
        connection = socket.create_connection(("127.0.0.1", parsed.port), timeout=timeout)
    except OSError as exc:
        _raise_probe_error("connect", exc, diagnostic_path, transient=_is_transient_transport_error(exc))
    try:
        with connection:
            try:
                connection.settimeout(timeout)
            except OSError as exc:
                _raise_probe_error("connect", exc, diagnostic_path,
                                   transient=_is_transient_transport_error(exc))
            try:
                _assert_connected_peer(connection, owner_record, owner_exe, parsed.port)
            except BootstrapError as exc:
                _raise_probe_error("ownership", exc, diagnostic_path)
            except OSError as exc:
                _raise_probe_error("ownership", exc, diagnostic_path)
            request_path = parsed.path
            request = (f"GET {request_path} HTTP/1.1\r\nHost: 127.0.0.1:{parsed.port}\r\n"
                       "Accept: application/json\r\nConnection: close\r\n\r\n").encode("ascii")
            connection.sendall(request)
            response = http.client.HTTPResponse(connection)
            response.begin()
            if response.status < 200 or response.status >= 300:
                exc = RuntimeError("The recorded local engine returned an unsuccessful response.")
                _raise_probe_error("HTTP", exc, diagnostic_path)
            raw = response.read((1 << 20) + 1)
            response.close()
    except BootstrapError:
        raise
    except (OSError, http.client.HTTPException) as exc:
        _raise_probe_error("HTTP", exc, diagnostic_path, transient=_is_transient_transport_error(exc))
    if len(raw) > 1 << 20:
        _raise_probe_error("HTTP", RuntimeError("The local runtime returned an oversized response."), diagnostic_path)
    try:
        result = json.loads(raw.decode("utf-8"))
    except (UnicodeError, ValueError) as exc:
        _raise_probe_error("JSON", exc, diagnostic_path)
    if not isinstance(result, dict):
        _raise_probe_error("JSON", TypeError("The local runtime response must be an object."), diagnostic_path)
    return result


def _wait_ollama(url: str, deadline: float, record=None, executable: Path | None = None,
                 diagnostic_path: Path | None = None):
    port = urllib.parse.urlsplit(url).port
    if record is None or executable is None or not isinstance(port, int):
        raise BootstrapError("The local engine cannot be probed without its recorded child identity.")
    last_transport_error = None
    while time.time() < deadline:
        try:
            try:
                listening = _assert_owned_listener(record, executable, port, allow_absent=True)
            except BootstrapError as exc:
                _raise_probe_error("ownership", exc, diagnostic_path)
            if not listening:
                time.sleep(min(0.5, max(0, deadline - time.time())))
                continue
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            result = _local_json(url + "/api/version", timeout=min(3, remaining),
                                 owner_record=record, owner_exe=executable,
                                 diagnostic_path=diagnostic_path)
            if isinstance(result.get("version"), str):
                return
            _raise_probe_error("JSON", TypeError("The version response has no string version."), diagnostic_path)
        except _TransientTransportError as exc:
            last_transport_error = exc
        if time.time() < deadline:
            time.sleep(min(1, max(0, deadline - time.time())))
    message = "The app-owned local engine did not become ready; no other Ollama process was stopped."
    if last_transport_error is not None:
        raise BootstrapError(f"{message} Last probe: {last_transport_error}") from last_transport_error
    raise BootstrapError(message)


def _model_tags(url: str, record, executable: Path):
    payload = _local_json(url + "/api/tags", timeout=15, owner_record=record, owner_exe=executable)
    models = payload.get("models") if isinstance(payload, dict) else None
    return models if isinstance(models, list) else []


def _engine_model(url: str, record, executable: Path):
    return next((item for item in _model_tags(url, record, executable)
                 if isinstance(item, dict) and item.get("name") in {ENGINE, ENGINE + ":latest"}), None)


def _run_checked(args, *, env, cwd, timeout, label):
    emit("progress", phase=label)
    result = subprocess.run(args, cwd=str(cwd), env=env, capture_output=True, text=True, timeout=timeout)
    if result.returncode != 0:
        raise BootstrapError(label + " failed; the setup receipt will remain incomplete.")
    return result.stdout


def _setup_receipt_ready(output, receipt) -> bool:
    gates = receipt.get("gates") if isinstance(receipt, dict) else None
    return (isinstance(output, dict) and output.get("ready") is True
            and isinstance(receipt, dict) and receipt.get("ready") is True
            and isinstance(gates, dict) and bool(gates)
            and all(value is True for value in gates.values()))


def _completed_answer(payload, headers):
    choices = payload.get("choices") if isinstance(payload, dict) else None
    message = choices[0].get("message") if isinstance(choices, list) and choices else None
    content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(content, str) or not content.strip():
        raise BootstrapError("The private first-response check did not return an answer.")
    if not isinstance(headers, dict):
        raise BootstrapError("The private first-response check did not return receipt metadata.")
    receipt = headers.get("x-tempo-completion-receipt-id") or headers.get("x-tempo-receipt-id")
    if not isinstance(receipt, str) or not re.fullmatch(r"[0-9a-f]{32}", receipt):
        raise BootstrapError("The private first-response check did not return a valid receipt.")
    review = headers.get("x-tempo-review-required", "true")
    if review not in {"true", "false"}:
        review = "true"
    return receipt, review


def _start_or_reuse_ollama(state: dict, app_root: Path, models_root: Path, code: Path):
    ollama_exe = app_root / "runtime" / "ollama" / "ollama.exe"
    models = models_root / "ollama"
    if not _verified_ollama_runtime(ollama_exe.parent):
        raise BootstrapError("The app-owned Ollama runtime is missing or changed.")
    old = state.get("ollama")
    if old is not None:
        if not isinstance(old, dict) or type(old.get("pid")) is not int or old["pid"] < 1:
            raise BootstrapError("The saved local engine identity is incomplete; no replacement was started.")
        if _process_identity(old["pid"]) is not None:
            if (not _owned_process(old, ollama_exe) or type(old.get("port")) is not int
                    or old.get("models_root") != str(models.resolve())):
                raise BootstrapError("The saved local engine identity is unknown or mismatched; no replacement was started.")
            url = f"http://127.0.0.1:{old['port']}"
            _wait_ollama(url, time.time() + 180, old, ollama_exe,
                         diagnostic_path=app_root / "logs" / "ollama-probe-diagnostic.json")
            return old, url
    port = _choose_port(11434, 11520)
    host = f"127.0.0.1:{port}"
    rec = _launch_owned(ollama_exe, ["serve"], cwd=ollama_exe.parent,
                        env=_child_env(ollama_host=host, ollama_models=models,
                                       ollama_runtime=ollama_exe.parent),
                        log_path=app_root / "logs" / "ollama.log", role="local engine", port=port)
    rec["models_root"] = str(models.resolve())
    state["ollama"] = rec
    _atomic_json(app_root / STATE_NAME, state)
    _wait_ollama(f"http://{host}", time.time() + 180, rec, ollama_exe,
                 diagnostic_path=app_root / "logs" / "ollama-probe-diagnostic.json")
    return rec, f"http://{host}"


def _start_or_reuse_gateway(state: dict, vpy: Path, code: Path, home: Path, app_root: Path,
                            python: Path, python_sha256: str):
    gateway_exe = vpy
    old = state.get("gateway")
    ollama = state.get("ollama") if isinstance(state.get("ollama"), dict) else {}
    if old is not None:
        if not isinstance(old, dict) or type(old.get("pid")) is not int or old["pid"] < 1:
            raise BootstrapError("The saved local gateway identity is incomplete; no replacement was started.")
        if _owned_process(old, gateway_exe, python, python_sha256):
            if (not isinstance(old.get("port"), int) or old.get("home") != str(home.resolve())
                    or old.get("ollama_port") != ollama.get("port")):
                raise BootstrapError("The saved local gateway belongs to a different runtime selection.")
            try:
                from tempo_client import gateway_token
                token = gateway_token(home)
                _guarded_gateway_request(old, gateway_exe, token, "GET", "/health", timeout=3,
                                         python=python, python_sha256=python_sha256)
                return old
            except Exception:
                raise BootstrapError("The recorded local gateway failed its live ownership or health check; it was left running.") from None
        if _process_identity(old["pid"]) is not None:
            raise BootstrapError("The saved local gateway identity is unknown or mismatched; no replacement was started.")
    port = _choose_port(8765, 8800)
    rec = _launch_owned(gateway_exe,
                        ["-I", "-B", str(code / "tempo.py"), "headless", "--home", str(home),
                         "--host", "127.0.0.1", "--port", str(port)],
                        cwd=code, env=_child_env(home=home),
                        log_path=app_root / "logs" / "gateway.log", role="local gateway", port=port,
                        python=python, python_sha256=python_sha256)
    rec["home"] = str(home.resolve())
    rec["ollama_port"] = ollama.get("port")
    from tempo_client import gateway_token
    deadline = time.time() + 60
    while time.time() < deadline:
        if not _owned_process(rec, gateway_exe, python, python_sha256):
            raise BootstrapError("The app-owned local gateway exited during startup.")
        try:
            if not _listener_owners(port):
                time.sleep(0.5)
                continue
            token = gateway_token(home)
            _guarded_gateway_request(rec, gateway_exe, token, "GET", "/health", timeout=4,
                                     python=python, python_sha256=python_sha256)
            return rec
        except BootstrapError:
            raise
        except Exception:
            time.sleep(0.5)
    raise BootstrapError("The app-owned local gateway did not become reachable.")


def install(python: Path, app_root: Path, home: Path, models_root: Path, code: Path, *, consent_resolve=False):
    if consent_resolve is not True:
        raise BootstrapError("Explicit user consent is required before dependency resolver network access.")
    python, python_sha256 = _verified_bootstrap_python(python, app_root)
    plan_doc = plan(python, app_root, home, models_root, code, consent_resolve=consent_resolve)
    answer_gib = plan_doc["download_bytes"] / (1024 ** 3)
    emit("consent_required", plan=plan_doc, message=(
        f"Perennos Tempo will download {plan_doc['download_bytes']:,} bytes ({answer_gib:.2f} GiB): "
        f"{plan_doc['wheel_count']} dependency wheels, one private {ENGINE} model, "
        f"{plan_doc['decision_file_count']} pinned decision files, and the pinned Ollama runtime if missing. "
        "Everything is stored under the selected app/model folders. No hosted provider is configured."))
    if sys.stdin.readline().strip().lower() not in {"yes", "y"}:
        emit("cancelled", message="No model or dependency download was started.")
        return 2

    current_manifest = _registry_manifest()
    if (current_manifest["digest"] != plan_doc["engine_manifest_digest"]
            or current_manifest["total_bytes"] != plan_doc["engine_manifest_total_bytes"]):
        raise BootstrapError("The selected engine changed after the size confirmation. Reopen setup to review a new plan.")

    app_root.mkdir(parents=True, exist_ok=True)
    runtime = app_root / "runtime"
    ollama_dir = runtime / "ollama"
    ollama_zip = Path(plan_doc["ollama_zip"])
    if not _verified_ollama_runtime(ollama_dir):
        if not ollama_zip.is_file():
            emit("progress", phase="Downloading the verified local engine runtime")
            _download(OLLAMA_URL, ollama_zip, OLLAMA_SHA256,
                      expected_bytes=plan_doc["ollama_zip_bytes"])
        safe_extract_zip(ollama_zip, ollama_dir)
        if not (ollama_dir / "ollama.exe").is_file():
            raise BootstrapError("The pinned runtime archive did not contain its expected app binary.")
        _atomic_json(ollama_dir / OWNER_MARKER, {"product": "Perennos Tempo", "purpose": "owned-ollama-runtime",
                                                  "version": "0.40.6", "archive_sha256": OLLAMA_SHA256,
                                                  "ollama_exe_sha256": _sha256_file(ollama_dir / "ollama.exe")})

    _ensure_owned_model_root(models_root / "ollama", purpose="owned-ollama-models")
    state_path = app_root / STATE_NAME
    state = _load_state(app_root)
    _verify_home_binding(home, app_root, models_root, state)
    old_ollama = state.get("ollama") if isinstance(state.get("ollama"), dict) else {}
    previous_ollama_url = (f"http://127.0.0.1:{old_ollama['port']}"
                           if isinstance(old_ollama.get("port"), int) else None)
    ollama_rec, ollama_url = _start_or_reuse_ollama(state, app_root, models_root, code)
    state["ollama"] = ollama_rec
    _atomic_json(state_path, state)
    _update_owned_tenant_url(home, models_root, ollama_url, previous_ollama_url)

    emit("progress", phase=f"Verifying the selected local {ENGINE} engine")
    from tempo_pull import PullError, pull_owned_model

    cancel_pull = threading.Event()
    with ThreadPoolExecutor(max_workers=1) as pull_executor:
        try:
            pull_future = pull_executor.submit(
                pull_owned_model, ENGINE, ollama_rec["port"], ollama_rec, ollama_dir / "ollama.exe",
                assert_owned_listener=_assert_owned_listener,
                assert_connected_peer=_assert_connected_peer,
                cancel_event=cancel_pull,
            )
            venv, vpy = Path(plan_doc["venv"]), Path(plan_doc["venv_python"])
            pip_report = Path(plan_doc["pip_report"])
            wheelhouse = app_root / "runtime" / "wheelhouse"
            wheelhouse.mkdir(parents=True, exist_ok=True)
            wheel_files = []
            for item in plan_doc["downloads"]["wheels"]:
                dest = wheelhouse / item["sha256"][:16] / item["filename"]
                if not _valid_pinned_file(dest, item["sha256"]):
                    emit("progress", phase="Downloading a hash-checked Python dependency wheel")
                    downloaded = _download(item["url"], dest, item["sha256"], expected_bytes=item["bytes"])
                    if downloaded["bytes"] != item["bytes"]:
                        raise BootstrapError("A dependency wheel size changed after confirmation; setup stopped.")
                wheel_files.append(str(dest))
            install_env = _child_env()
            install_env["PIP_CONFIG_FILE"] = os.devnull
            install_env["PIP_INDEX_URL"] = "https://pypi.org/simple"
            install_env["PIP_EXTRA_INDEX_URL"] = "https://download.pytorch.org/whl/cpu"
            if wheel_files:
                _run_checked([str(vpy), "-I", "-B", "-m", "pip", "install", "--disable-pip-version-check", "--no-cache-dir",
                              "--no-index", "--no-deps", "--only-binary=:all:", *wheel_files],
                             env=install_env, cwd=code, timeout=7200,
                             label="Installing the resolved local dependencies")
            try:
                pip_report.unlink()
            except OSError:
                pass

            for item in plan_doc["downloads"]["decision_files"]:
                dest = Path(item["path"])
                if not _valid_pinned_file(dest, item["sha256"]):
                    emit("progress", phase="Downloading a hash-checked decision-model file")
                    downloaded = _download(item["url"], dest, item["sha256"], expected_bytes=item["bytes"])
                    if downloaded["bytes"] != item["bytes"]:
                        raise BootstrapError("A pinned decision-model file size changed after confirmation; setup stopped.")
            try:
                pull_future.result()
            except PullError as exc:
                raise BootstrapError(str(exc)) from exc
        finally:
            cancel_pull.set()

    engine_model = _engine_model(ollama_url, ollama_rec, ollama_dir / "ollama.exe")
    if not isinstance(engine_model, dict) or engine_model.get("digest") != plan_doc["engine_digest"]:
        raise BootstrapError("The downloaded local engine revision differs from the pinned app catalog.")

    setup_env = _child_env(home=home)
    setup_env["OLLAMA_HOST"] = ollama_url.removeprefix("http://")
    setup_env["OLLAMA_MODELS"] = str(models_root / "ollama")
    setup_args = [str(vpy), "-I", "-B", str(code / "tempo_setup.py"), "--home", str(home),
                  "--models-dir", str(models_root / "decision"), "--ollama-url", ollama_url,
                  "--device", "auto", "--qualify-engine", ENGINE, "--check-registry", "--apply", "--fetch-models"]
    _assert_owned_listener(ollama_rec, ollama_dir / "ollama.exe", ollama_rec["port"])
    raw = _run_checked(setup_args, env=setup_env, cwd=code, timeout=4 * 60 * 60,
                       label="Configuring and qualifying Perennos Tempo")
    try:
        setup_output = json.loads(raw)
        setup_receipt = json.loads((home / "setup-receipt.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raise BootstrapError("Perennos Tempo setup did not produce a valid setup receipt.") from None
    if not _setup_receipt_ready(setup_output, setup_receipt):
        raise BootstrapError("Perennos Tempo setup or local-engine qualification did not pass; the app is not ready.")
    qualification = setup_receipt.get("steps", {}).get("qualification", {})
    tried = qualification.get("tried", []) if isinstance(qualification, dict) else []
    qualified = next((item for item in tried if item.get("engine") == ENGINE and item.get("ok") is True), None)
    if (qualification.get("qualified") != ENGINE or not isinstance(qualified, dict)
            or qualified.get("revision") != plan_doc["engine_digest"]):
        raise BootstrapError("The selected pinned local engine did not pass its exact-revision qualification.")

    gateway_rec = _start_or_reuse_gateway(state, vpy, code, home, app_root, python, python_sha256)
    state["gateway"] = gateway_rec
    _atomic_json(state_path, state)
    from tempo_client import chat, gateway_token
    token = gateway_token(home)
    emit("progress", phase="Checking the real private gateway response")
    _assert_owned_listener(ollama_rec, ollama_dir / "ollama.exe", ollama_rec["port"])
    answer, headers = chat("127.0.0.1", gateway_rec["port"], token,
                          "Reply with exactly READY.", timeout=610,
                          on_connection=_connection_owner_guard(gateway_rec, vpy, gateway_rec["port"],
                                                                python=python, python_sha256=python_sha256))
    receipt, review_required = _completed_answer(answer, headers)
    ready = {
        "product": "Perennos Tempo", "state": "Verified complete",
        "verified_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "data_home": str(home), "models_root": str(models_root),
        "setup_receipt": str(home / "setup-receipt.json"), "engine": ENGINE,
        "engine_digest": plan_doc["engine_digest"],
        "ollama_pid": ollama_rec["pid"], "ollama_port": ollama_rec["port"],
        "gateway_pid": gateway_rec["pid"], "gateway_port": gateway_rec["port"],
        "gateway_process": gateway_rec,
        "bootstrap_python_path": str(python), "bootstrap_python_sha256": python_sha256,
        "venv_python_sha256": _sha256_file(vpy),
        "venv_pythonw_sha256": _sha256_file(vpy.with_name("pythonw.exe")),
        "first_private_answer": True,
        "first_completion_receipt_id": re.sub(r"[^A-Za-z0-9._:-]", "", receipt)[:96],
        "review_required": review_required if review_required in {"true", "false"} else "true",
        "hosted_provider_configured": False,
    }
    _atomic_json(home / RECEIPT_NAME, ready)
    _atomic_json(app_root / RECEIPT_NAME, ready)
    emit("ready", receipt=str(app_root / RECEIPT_NAME), gateway_port=gateway_rec["port"])
    return 0


def open_existing(python: Path, app_root: Path, home: Path, models_root: Path, code: Path):
    """Reopen a complete install; start only recorded app-owned child services."""
    _check_windows()
    python, python_sha256 = _verified_bootstrap_python(python, app_root)
    _disjoint_paths((app_root, home, models_root))
    receipt_path = app_root / RECEIPT_NAME
    setup_path = home / "setup-receipt.json"
    try:
        install_receipt = json.loads(receipt_path.read_text(encoding="utf-8-sig"))
        setup_receipt = json.loads(setup_path.read_text(encoding="utf-8"))
        state = _load_state(app_root)
    except (OSError, ValueError):
        raise BootstrapError("A complete Perennos Tempo install receipt is unavailable; local data was left untouched.") from None
    if (install_receipt.get("product") != "Perennos Tempo" or install_receipt.get("state") != "Verified complete"
            or install_receipt.get("first_private_answer") is not True
            or not install_receipt.get("first_completion_receipt_id")
            or setup_receipt.get("ready") is not True):
        raise BootstrapError("The existing local setup is incomplete; no service was stopped or changed.")
    if (install_receipt.get("bootstrap_python_path") != str(python)
            or install_receipt.get("bootstrap_python_sha256") != python_sha256):
        raise BootstrapError("The verified bootstrap Python path or hash changed; no service was reused.")
    if not _setup_receipt_ready({"ready": True}, setup_receipt):
        raise BootstrapError("The saved local setup gates are incomplete; no service was stopped or changed.")
    if str(install_receipt.get("data_home", "")) != str(home) or str(install_receipt.get("models_root", "")) != str(models_root):
        raise BootstrapError("The selected folders differ from the verified install receipt.")
    _ensure_owned_data_root(home, app_root, models_root)
    _verify_home_binding(home, app_root, models_root, state)
    ollama_exe = app_root / "runtime" / "ollama" / "ollama.exe"
    vpy = app_root / "runtime" / "venv" / "Scripts" / "python.exe"
    pythonw = vpy.with_name("pythonw.exe")
    if (not _verified_ollama_runtime(ollama_exe.parent) or not vpy.is_file() or not pythonw.is_file()
            or install_receipt.get("venv_python_sha256") != _sha256_file(vpy)
            or install_receipt.get("venv_pythonw_sha256") != _sha256_file(pythonw)):
        raise BootstrapError("The app-owned local runtimes are missing or changed.")
    _ensure_owned_model_root(models_root, purpose="owned-model-root")
    _ensure_owned_model_root(models_root / "ollama", purpose="owned-ollama-models")
    if not home.is_dir():
        raise BootstrapError("The selected private data folder is missing; existing data was left untouched.")
    expected_digest = _engine_catalog_digest(code)
    if install_receipt.get("engine_digest") != expected_digest:
        raise BootstrapError("The selected engine revision differs from the verified app catalog.")
    old_ollama = state.get("ollama") if isinstance(state.get("ollama"), dict) else {}
    previous_ollama_url = (f"http://127.0.0.1:{old_ollama['port']}"
                           if isinstance(old_ollama.get("port"), int) else None)
    ollama_rec, ollama_url = _start_or_reuse_ollama(state, app_root, models_root, code)
    engine_model = _engine_model(ollama_url, ollama_rec, ollama_exe)
    if not isinstance(engine_model, dict) or engine_model.get("digest") != expected_digest:
        raise BootstrapError("The selected local engine revision is missing or changed. Re-run setup to review a new download plan.")
    state["ollama"] = ollama_rec
    _update_owned_tenant_url(home, models_root, ollama_url, previous_ollama_url)
    gateway_rec = _start_or_reuse_gateway(state, vpy, code, home, app_root, python, python_sha256)
    state["gateway"] = gateway_rec
    _atomic_json(app_root / STATE_NAME, state)
    install_receipt["ollama_pid"] = ollama_rec["pid"]
    install_receipt["ollama_port"] = ollama_rec["port"]
    install_receipt["gateway_pid"] = gateway_rec["pid"]
    install_receipt["gateway_port"] = gateway_rec["port"]
    install_receipt["gateway_process"] = gateway_rec
    _atomic_json(receipt_path, install_receipt)
    _atomic_json(home / RECEIPT_NAME, install_receipt)
    emit("opened", receipt=str(receipt_path), gateway_port=gateway_rec["port"])
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description="Perennos Tempo local bootstrap")
    sub = parser.add_subparsers(dest="command", required=True)
    install_parser = sub.add_parser("install", help="prepare the selected private local runtime")
    install_parser.add_argument("--consent-resolve", action="store_true",
                                help="user consented to bounded dependency resolution before exact package totals")
    open_parser = sub.add_parser("open", help="reopen a verified install and its own services")
    for command_parser in (install_parser, open_parser):
        for name in ("python", "app-root", "home", "models-root", "code"):
            command_parser.add_argument("--" + name, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "install":
            return install(Path(args.python).resolve(), Path(args.app_root).resolve(), Path(args.home).resolve(),
                           Path(args.models_root).resolve(), Path(args.code).resolve(),
                           consent_resolve=args.consent_resolve)
        if args.command == "open":
            return open_existing(Path(args.python).resolve(), Path(args.app_root).resolve(), Path(args.home).resolve(),
                                 Path(args.models_root).resolve(), Path(args.code).resolve())
    except Exception as exc:
        emit("error", message=_private_error(exc))
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
