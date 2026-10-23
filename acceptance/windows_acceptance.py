"""Native Windows acceptance driver for the Perennos Tempo v1 setup EXE.

Subcommands: baseline | verify-build | run | stage | api-probe | mcp-probe.
Host Python 3.12 + stdlib only. The installer UI is driven with fixed Win32 messages restricted to the exact
installer PID and its known captions; it never touches OS/UAC/SmartScreen/antivirus windows. Any such prompt,
an unknown dialog, an exited installer or missing evidence is a FAILURE recorded in the result JSON.
Tokens are read only in memory by the installed API client, never written to results.
"""
from __future__ import annotations

import argparse
import ctypes
import datetime
import hashlib
import http.client
import json
import os
import platform
import queue
import re
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import traceback
import zlib
from ctypes import wintypes as wt
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = Path("C:/TempoCI")
OUT = Path("C:/TempoCI-out")
PAYLOAD, BUILD, EXTRACT = ROOT / "payload", ROOT / "build", ROOT / "extract"
APP, DATA, MODELS = ROOT / "app", ROOT / "data", ROOT / "models"
EXE_NAME = "PerennosTempo-Setup-unsigned.exe"
MAIN_TITLE = "Perennos Tempo | Local setup"
GUI_TITLE = "Perennos Tempo"
PY_SHA = "67b5635e80ea51072b87941312d00ec8927c4db9ba18938f7ad2d27b328b95fb"  # pinned in desktop/Bootstrap.cs
INSTALL_DEADLINE_S = 30 * 60
POLL_S, HEARTBEAT_S = 5, 30
MIN_FREE_BYTES_START = 10 * 1024 ** 3
MIN_FREE_BYTES_RUNNING = 2 * 1024 ** 3
STAGE_CAP_BYTES = 100 * 1024 ** 2  # The released EXE now embeds its licensed Python/Tk runtime.
GIB = 1024 ** 3
# Exact captions/bodies read from desktop/Bootstrap.cs (2 own dialogs, in this order on a fresh app folder).
KNOWN_DIALOGS = (
    ("preflight", "Allow local setup preflight", ("Perennos Tempo setup consent", "remain enabled")),
    ("model_downloads", "Confirm local model and dependency downloads",
     ("Perennos Tempo will download", "No hosted provider is configured")),
)
INSTALLER_ERROR_TITLE = "Perennos Tempo setup"  # Main() error MessageBox == failure
FAILURE_STATUS_MARKERS = ("Setup stopped safely", "Local setup is not ready", "could not open")
OS_PROMPT_IMAGES = {"consent.exe", "smartscreen.exe", "securityhealthhost.exe", "securityhealthsystray.exe",
                    "mpcmdrun.exe", "msmpeng.exe"}
OS_PROMPT_TITLES = ("user account control", "windows protected your pc", "smartscreen", "windows security")
IGNORED_CLASSES = {"IME", "MSCTFIME UI"}
SYNTHETIC_API_PROMPT = "Reply with one short sentence confirming you can answer a synthetic acceptance-test question."
SYNTHETIC_MCP_PROMPT = "In one short sentence, what is a hash function?"
REQUIRED_CHECKS = (
    "installer_matches_build", "fresh_targets", "controlled_foreign_listener_fixture",
    "ollama_avoids_controlled_foreign_listener", "controlled_foreign_listener_still_owned",
    "ollama_log_no_foreign_dll_warning", "installer_window", "textboxes_filled", "install_clicked",
    "consent_1_preflight", "consent_2_model_downloads", "consent_count",
    "no_os_security_prompt", "install_within_30min", "receipts_valid", "installed_files_match_manifest",
    "gateway_process", "ollama_process", "gateway_health_after_install", "gui_launched_by_installer",
    "gateway_health_after_gui_close", "gateway_health_after_installer_exit", "probe_native_gui", "probe_api",
    "probe_mcp", "final_gateway_health")

BM_CLICK, WM_CLOSE, WM_GETTEXT, WM_SETTEXT, WM_LBUTTONDOWN, WM_LBUTTONUP = 0xF5, 0x10, 0x0D, 0x0C, 0x201, 0x202
SMTO_ABORTIFHUNG = 0x2

if os.name == "nt":
    u32 = ctypes.WinDLL("user32", use_last_error=True)
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    g32 = ctypes.WinDLL("gdi32", use_last_error=True)
    WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG), ("biPlanes", wt.WORD),
                    ("biBitCount", wt.WORD), ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD),
                    ("biXPelsPerMeter", wt.LONG), ("biYPelsPerMeter", wt.LONG), ("biClrUsed", wt.DWORD),
                    ("biClrImportant", wt.DWORD)]

    class MEMORYSTATUSEX(ctypes.Structure):
        _fields_ = [("dwLength", wt.DWORD), ("dwMemoryLoad", wt.DWORD), ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong), ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong), ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong), ("ullAvailExtendedVirtual", ctypes.c_ulonglong)]

    def _sig(lib, name, restype, *argtypes):
        fn = getattr(lib, name)
        fn.restype, fn.argtypes = restype, list(argtypes)

    _sig(u32, "EnumWindows", wt.BOOL, WNDENUMPROC, wt.LPARAM)
    _sig(u32, "EnumChildWindows", wt.BOOL, wt.HWND, WNDENUMPROC, wt.LPARAM)
    _sig(u32, "GetWindowThreadProcessId", wt.DWORD, wt.HWND, ctypes.POINTER(wt.DWORD))
    _sig(u32, "GetWindowTextW", ctypes.c_int, wt.HWND, wt.LPWSTR, ctypes.c_int)
    _sig(u32, "GetClassNameW", ctypes.c_int, wt.HWND, wt.LPWSTR, ctypes.c_int)
    _sig(u32, "IsWindowVisible", wt.BOOL, wt.HWND)
    _sig(u32, "IsWindowEnabled", wt.BOOL, wt.HWND)
    _sig(u32, "IsWindow", wt.BOOL, wt.HWND)
    _sig(u32, "GetWindowRect", wt.BOOL, wt.HWND, ctypes.POINTER(wt.RECT))
    _sig(u32, "GetAncestor", wt.HWND, wt.HWND, wt.UINT)
    _sig(u32, "PostMessageW", wt.BOOL, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM)
    _sig(u32, "SendMessageTimeoutW", wt.LPARAM, wt.HWND, wt.UINT, wt.WPARAM, ctypes.c_void_p, wt.UINT, wt.UINT,
         ctypes.POINTER(ctypes.c_size_t))
    _sig(u32, "PrintWindow", wt.BOOL, wt.HWND, wt.HDC, wt.UINT)
    _sig(u32, "GetWindowDC", wt.HDC, wt.HWND)
    _sig(u32, "ReleaseDC", ctypes.c_int, wt.HWND, wt.HDC)
    _sig(u32, "GetSystemMetrics", ctypes.c_int, ctypes.c_int)
    _sig(g32, "CreateCompatibleDC", wt.HDC, wt.HDC)
    _sig(g32, "CreateCompatibleBitmap", wt.HBITMAP, wt.HDC, ctypes.c_int, ctypes.c_int)
    _sig(g32, "SelectObject", wt.HGDIOBJ, wt.HDC, wt.HGDIOBJ)
    _sig(g32, "DeleteObject", wt.BOOL, wt.HGDIOBJ)
    _sig(g32, "DeleteDC", wt.BOOL, wt.HDC)
    _sig(g32, "GetDIBits", ctypes.c_int, wt.HDC, wt.HBITMAP, wt.UINT, wt.UINT, ctypes.c_void_p, ctypes.c_void_p,
         wt.UINT)
    _sig(k32, "OpenProcess", wt.HANDLE, wt.DWORD, wt.BOOL, wt.DWORD)
    _sig(k32, "CloseHandle", wt.BOOL, wt.HANDLE)
    _sig(k32, "QueryFullProcessImageNameW", wt.BOOL, wt.HANDLE, wt.DWORD, wt.LPWSTR, ctypes.POINTER(wt.DWORD))
    _sig(k32, "GetExitCodeProcess", wt.BOOL, wt.HANDLE, ctypes.POINTER(wt.DWORD))
    _sig(k32, "ProcessIdToSessionId", wt.BOOL, wt.DWORD, ctypes.POINTER(wt.DWORD))
    _sig(k32, "GlobalMemoryStatusEx", wt.BOOL, ctypes.POINTER(MEMORYSTATUSEX))


# ---------------------------------------------------------------- generic helpers
def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def log(message):
    line = "[%s] %s" % (now(), message)
    print(line, flush=True)
    try:
        OUT.mkdir(parents=True, exist_ok=True)
        with (OUT / "acceptance-log.txt").open("a", encoding="utf-8") as stream:
            stream.write(line + "\n")
    except OSError:
        pass


def redact(text):
    """Defence in depth: nothing token-shaped (long opaque runs) is ever kept in saved text."""
    return re.sub(r"[A-Za-z0-9_\-]{32,}", "<redacted>", text or "")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def jdump(path, doc):
    path = Path(path)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def same_path(a, b):
    return bool(a) and bool(b) and os.path.normcase(os.path.abspath(str(a))) == os.path.normcase(os.path.abspath(str(b)))


def under(path, folder):
    p, f = os.path.normcase(os.path.abspath(str(path))), os.path.normcase(os.path.abspath(str(folder)))
    return p == f or p.startswith(f + os.sep)


def env_clean():
    return {k: v for k, v in os.environ.items()
            if not k.upper().startswith("TEMPO_") and "TOKEN" not in k.upper() and "SECRET" not in k.upper()}


class ControlledForeignListener:
    """Harmless test-owned loopback listener; this is not an actual Ollama process."""

    port = 11434

    def __init__(self):
        self.pid = os.getpid()
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            self.sock.bind(("127.0.0.1", self.port))
            self.sock.listen(1)
        except OSError:
            self.sock.close()
            raise

    def close(self):
        self.sock.close()

    def still_owned(self):
        try:
            bound = self.sock.fileno() >= 0 and self.sock.getsockname() == ("127.0.0.1", self.port)
            listening = bound and self.sock.getsockopt(socket.SOL_SOCKET, socket.SO_ACCEPTCONN) == 1
        except OSError:
            bound = listening = False
        pids = []
        for line in subprocess.run(["netstat", "-ano", "-p", "tcp"], capture_output=True, text=True,
                                   timeout=60).stdout.splitlines():
            parts = line.split()
            if (len(parts) >= 5 and parts[3] == "LISTENING"
                    and parts[1].endswith(":%d" % self.port) and parts[4].isdigit()):
                pids.append(int(parts[4]))
        return {"bound": bound, "listening": listening, "listener_pids": pids,
                "owned_by_acceptance_process": pids == [self.pid], "pid": self.pid, "port": self.port}


def _foreign_path_fixture():
    folder = OUT / "fixtures" / "controlled-foreign-path"
    folder.mkdir(parents=True, exist_ok=True)
    dll = folder / "ggml-base.dll"
    # Intentionally inert bytes, not a PE image and never loaded by the test.
    data = b"Perennos Tempo controlled foreign PATH fixture; inert bytes only.\n"
    dll.write_bytes(data)
    return folder, dll, hashlib.sha256(data).hexdigest()


def _remove_foreign_path_fixture(dll):
    if dll is None:
        return
    try:
        folder = dll.parent
        dll.unlink(missing_ok=True)
        folder.rmdir()
        folder.parent.rmdir()
    except OSError:
        pass


def _ollama_log_check(path):
    path = Path(path)
    if not path.is_file():
        return {"passed": False, "present": False, "size_bytes": 0, "sha256": None,
                "foreign_dll_warning_count": None}
    size = path.stat().st_size
    warnings = 0
    markers = ("warn", "error", "failed", "unable", "cannot", "could not", "incompatible")
    with path.open("r", encoding="utf-8", errors="replace") as stream:
        for line in stream:
            low = line.lower()
            if "ggml-base.dll" in low and any(marker in low for marker in markers):
                warnings += 1
    return {"passed": size > 0 and warnings == 0, "present": True, "size_bytes": size,
            "sha256": sha256_file(path), "foreign_dll_warning_count": warnings}


def ps(command, timeout=60):
    cp = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", command],
                        capture_output=True, text=True, timeout=timeout)
    return cp.stdout


def free_bytes(path="C:/"):
    return shutil.disk_usage(path).free


def parse_drive_space(text):
    """Parse drive-size sentences, including the producer's drive-plus-colon c:: spelling."""
    matches = re.findall(r"([A-Za-z]):(?::|\\:)? require ([\d,]+) bytes, ([\d,]+) bytes free", text)
    return [{"drive": drive.upper(), "required": int(required.replace(",", "")),
             "free": int(free.replace(",", ""))} for drive, required, free in matches]


def dir_bytes(*paths):
    total = 0
    for p in paths:
        for dirpath, _dirs, files in os.walk(p):
            for name in files:
                try:
                    total += os.path.getsize(os.path.join(dirpath, name))
                except OSError:
                    pass
    return total


def http_json(port, method, path, headers=None, body=None, timeout=10):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        conn.request(method, path, body=body, headers=headers or {})
        response = conn.getresponse()
        raw = response.read(1 << 20)
        try:
            doc = json.loads(raw.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            doc = None
        return response.status, doc
    finally:
        conn.close()


def health(port):
    """Unauthenticated GET /health (an allowed public endpoint of the product client)."""
    try:
        status, doc = http_json(port, "GET", "/health")
        return {"http": status, "status": doc.get("status") if isinstance(doc, dict) and isinstance(doc.get("status"), str) else None,
                "ok": status == 200 and isinstance(doc, dict) and isinstance(doc.get("status"), str)}
    except (OSError, http.client.HTTPException) as exc:
        return {"http": None, "status": None, "ok": False, "error": type(exc).__name__}


class Fail(Exception):
    pass


class Result:
    def __init__(self, path, kind, **extra):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.doc = {"schema": 1, "kind": kind, "started": now(), "finished": None, "passed": False,
                    "failure_reason": None, "checks": {}, "notes": {}, **extra}
        self.save()

    def save(self):
        jdump(self.path, self.doc)

    def note(self, name, **detail):
        self.doc["notes"][name] = detail
        self.save()

    def check(self, name, ok, /, **detail):
        self.doc["checks"][name] = {"passed": bool(ok), **detail}
        if not ok and not self.doc["failure_reason"]:
            reason = detail.get("reason")
            self.doc["failure_reason"] = "check failed: " + name + (": " + str(reason) if reason else "")
        log("CHECK %s %s %s" % ("PASS" if ok else "FAIL", name, redact(json.dumps(detail, default=str))[:300]))
        self.save()
        return bool(ok)

    def require(self, name, ok, /, **detail):
        if not self.check(name, ok, **detail):
            raise Fail(self.doc["failure_reason"])

    def finish(self, required=()):
        checks = self.doc["checks"]
        missing = [n for n in required if n not in checks]
        failed = [n for n, c in checks.items() if not c["passed"]]
        passed = not missing and not failed and self.doc["failure_reason"] is None
        if not passed and self.doc["failure_reason"] is None:
            self.doc["failure_reason"] = "missing evidence: " + ", ".join(missing) if missing else "failed checks"
        self.doc.update(finished=now(), missing_checks=missing, failed_checks=failed, passed=passed)
        self.save()
        log("RESULT %s passed=%s reason=%s" % (self.doc["kind"], passed, self.doc["failure_reason"]))
        return passed


# ---------------------------------------------------------------- Win32 window helpers
def _title(hwnd):
    buf = ctypes.create_unicode_buffer(512)
    u32.GetWindowTextW(hwnd, buf, 512)
    return buf.value


def _cls(hwnd):
    buf = ctypes.create_unicode_buffer(256)
    u32.GetClassNameW(hwnd, buf, 256)
    return buf.value


def window_pid(hwnd):
    pid = wt.DWORD()
    u32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    return pid.value


def window_rect(hwnd):
    rc = wt.RECT()
    u32.GetWindowRect(hwnd, ctypes.byref(rc))
    return rc.left, rc.top, rc.right, rc.bottom


def windows(pid=None, visible=True):
    found = []

    def cb(hwnd, _lparam):
        owner = window_pid(hwnd)
        if (pid is None or owner == pid) and (not visible or u32.IsWindowVisible(hwnd)):
            found.append({"hwnd": hwnd, "pid": owner, "title": _title(hwnd), "cls": _cls(hwnd)})
        return True

    callback = WNDENUMPROC(cb)
    u32.EnumWindows(callback, 0)
    return found


def children(hwnd):
    found = []

    def cb(child, _lparam):
        left, top, right, bottom = window_rect(child)
        found.append({"hwnd": child, "cls": _cls(child), "top": top, "left": left, "w": right - left,
                      "visible": bool(u32.IsWindowVisible(child)), "enabled": bool(u32.IsWindowEnabled(child))})
        return True

    callback = WNDENUMPROC(cb)
    u32.EnumChildWindows(hwnd, callback, 0)
    return found


def wm_text(hwnd, timeout_ms=2000):
    """Control text via WM_GETTEXT with a timeout; None means the owning UI thread is busy/hung."""
    buf = ctypes.create_unicode_buffer(8192)
    result = ctypes.c_size_t()
    ok = u32.SendMessageTimeoutW(hwnd, WM_GETTEXT, 8192, buf, SMTO_ABORTIFHUNG, timeout_ms, ctypes.byref(result))
    return buf.value if ok else None


def owned_window(hwnd, pid, root, caption):
    if (not u32.IsWindow(hwnd) or window_pid(hwnd) != pid or window_pid(root) != pid
            or toplevel_hwnd(hwnd) != root or _title(root) != caption):
        raise Fail('own application window identity changed before UI action')


def post_owned(hwnd, message, pid, root, caption, wparam=0, lparam=0):
    owned_window(hwnd, pid, root, caption)
    if not u32.PostMessageW(hwnd, message, wparam, lparam):
        raise Fail('own application UI message failed')


def set_text(hwnd, text, pid, root):
    owned_window(hwnd, pid, root, MAIN_TITLE)
    buf = ctypes.create_unicode_buffer(text)
    result = ctypes.c_size_t()
    return bool(u32.SendMessageTimeoutW(hwnd, WM_SETTEXT, 0, buf, SMTO_ABORTIFHUNG, 5000, ctypes.byref(result)))


def proc_image(pid):
    handle = k32.OpenProcess(0x1000, False, int(pid))  # PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        return None
    try:
        buf, size = ctypes.create_unicode_buffer(1024), wt.DWORD(1024)
        return buf.value if k32.QueryFullProcessImageNameW(handle, 0, buf, ctypes.byref(size)) else None
    finally:
        k32.CloseHandle(handle)


def pid_alive(pid):
    handle = k32.OpenProcess(0x1000, False, int(pid))
    if not handle:
        return False
    try:
        code = wt.DWORD()
        return bool(k32.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == 259
    finally:
        k32.CloseHandle(handle)


def toplevel_hwnd(child_hwnd):
    return u32.GetAncestor(child_hwnd, 2)  # GA_ROOT


def _png(w, h, raw):
    rows = []
    for y in range(h):
        row = raw[y * w * 4:(y + 1) * w * 4]
        rgb = bytearray(w * 3)
        rgb[0::3], rgb[1::3], rgb[2::3] = row[2::4], row[1::4], row[0::4]  # BGRA -> RGB
        rows.append(b"\x00" + bytes(rgb))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"".join(rows), 6)) + chunk(b"IEND", b""))


def capture_window_png(hwnd, path, expect_pid=None):
    """PrintWindow into a memory DC (never a screen/desktop grab) -> GetDIBits -> PNG of ONE window."""
    if expect_pid is not None and window_pid(hwnd) != expect_pid:
        raise RuntimeError("refusing to capture a window owned by another process")
    left, top, right, bottom = window_rect(hwnd)
    w, h = right - left, bottom - top
    if not (50 <= w <= 4096 and 50 <= h <= 4096):
        raise RuntimeError("unexpected window size %dx%d" % (w, h))
    wdc = u32.GetWindowDC(hwnd)
    mdc = g32.CreateCompatibleDC(wdc)
    bmp = g32.CreateCompatibleBitmap(wdc, w, h)
    old = g32.SelectObject(mdc, bmp)
    try:
        if not u32.PrintWindow(hwnd, mdc, 2):  # PW_RENDERFULLCONTENT
            raise RuntimeError("PrintWindow failed")
        info = BITMAPINFOHEADER()
        info.biSize, info.biWidth, info.biHeight = ctypes.sizeof(BITMAPINFOHEADER), w, -h  # top-down
        info.biPlanes, info.biBitCount, info.biCompression = 1, 32, 0
        buf = (ctypes.c_ubyte * (w * h * 4))()
        g32.SelectObject(mdc, old)  # GetDIBits requires the bitmap not be selected into a DC.
        if g32.GetDIBits(wdc, bmp, 0, h, buf, ctypes.byref(info), 0) != h:
            raise RuntimeError("GetDIBits failed")
        raw = bytes(buf)
    finally:
        g32.SelectObject(mdc, old)
        g32.DeleteObject(bmp)
        g32.DeleteDC(mdc)
        u32.ReleaseDC(hwnd, wdc)
    distinct = len({raw[i:i + 4] for i in range(0, len(raw), 28)})
    data = _png(w, h, raw)
    Path(path).write_bytes(data)
    if distinct < 8:
        raise RuntimeError("captured window looks blank (%d distinct colours)" % distinct)
    return {"file": Path(path).name, "width": w, "height": h, "bytes": len(data), "distinct_colors_sampled": distinct,
            "sha256": hashlib.sha256(data).hexdigest(), "window_title": _title(hwnd)}


def snap(shots, name, hwnd, pid):
    try:
        shots.append(capture_window_png(hwnd, OUT / (name + ".png"), expect_pid=pid))
        return shots[-1]
    except Exception as exc:  # evidence is best-effort; the gating checks are elsewhere
        log("screenshot %s failed: %s" % (name, type(exc).__name__))
        return None


def os_prompts():
    hits = []
    for w in windows(visible=True):
        if w["pid"] == os.getpid():
            continue
        image = os.path.basename(proc_image(w["pid"]) or "").lower()
        title = w["title"].lower()
        if image in OS_PROMPT_IMAGES or any(t in title for t in OS_PROMPT_TITLES):
            hits.append({"title": w["title"], "image": image, "cls": w["cls"]})
    return hits


# ---------------------------------------------------------------- environment / baseline
def environment():
    mem = MEMORYSTATUSEX()
    mem.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
    k32.GlobalMemoryStatusEx(ctypes.byref(mem))
    session = wt.DWORD()
    k32.ProcessIdToSessionId(os.getpid(), ctypes.byref(session))
    osinfo = ps("$o=Get-CimInstance Win32_OperatingSystem;'{0}|{1}|{2}|{3}' -f $o.Caption,$o.Version,$o.BuildNumber,$o.OSArchitecture").strip()
    uac = {}
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System") as key:
            for name in ("EnableLUA", "ConsentPromptBehaviorAdmin"):
                try:
                    uac[name] = winreg.QueryValueEx(key, name)[0]
                except OSError:
                    uac[name] = None
    except OSError:
        pass
    defender = ps("try{$m=Get-MpComputerStatus;'AntivirusEnabled={0};RealTimeProtectionEnabled={1}' -f $m.AntivirusEnabled,$m.RealTimeProtectionEnabled}catch{'unavailable'}").strip()
    volumes = {}
    for letter in "CDEFGH":
        if Path(letter + ":/").exists():
            usage = shutil.disk_usage(letter + ":/")
            volumes[letter + ":"] = {"total_gib": round(usage.total / GIB, 1), "free_gib": round(usage.free / GIB, 1)}
    return {"statement": "GitHub-hosted standard windows-2025 runner VM with preinstalled developer tools; NOT a consumer "
                         "Windows 11 machine and NOT a blank/virgin OS.",
            "os": osinfo, "python_platform": platform.platform(), "cpu_count": os.cpu_count(),
            "ram_gib": round(mem.ullTotalPhys / GIB, 1), "session_id": session.value,
            "screen": [u32.GetSystemMetrics(0), u32.GetSystemMetrics(1)], "uac_registry": uac,
            "defender_read_only": defender, "volumes": volumes,
            "runner_env": {k: os.environ.get(k) for k in ("RUNNER_OS", "RUNNER_ARCH", "RUNNER_ENVIRONMENT", "ImageOS",
                                                           "ImageVersion", "GITHUB_SHA", "GITHUB_REF", "GITHUB_RUN_ID")},
            "host_python": sys.version.split()[0]}


def cmd_baseline(_args):
    OUT.mkdir(parents=True, exist_ok=True)
    res = Result(OUT / "baseline.json", "baseline")
    res.doc["environment"] = environment()
    local = Path(os.environ.get("LOCALAPPDATA", "C:/nonexistent")) / "PerennosTempo"
    res.check("no_existing_perennos_paths", not ROOT.exists() and not local.exists(), root_exists=ROOT.exists(),
              localappdata_perennos_exists=local.exists())
    procs = []
    for line in ps("Get-CimInstance Win32_Process | ForEach-Object { '{0}|{1}|{2}' -f $_.ProcessId,$_.Name,$_.ExecutablePath }").splitlines():
        low = line.lower()
        if "perennos" in low or "tempoci" in low:
            procs.append(line.split("|")[1] if "|" in line else "?")
    res.check("no_existing_perennos_processes", not procs, found=procs)
    services = subprocess.run(["sc.exe", "query", "state=", "all"], capture_output=True, text=True, timeout=60).stdout.lower()
    res.check("no_existing_perennos_services", "perennos" not in services and "tempo" not in services)
    ports = []
    for line in subprocess.run(["netstat", "-ano", "-p", "tcp"], capture_output=True, text=True, timeout=60).stdout.splitlines():
        parts = line.split()
        if len(parts) >= 5 and parts[3] == "LISTENING":
            port = parts[1].rsplit(":", 1)[-1]
            if port.isdigit() and (8765 <= int(port) <= 8800 or 11434 <= int(port) <= 11520):
                ports.append(int(port))
    res.note("occupied_product_port_ranges", ports=ports)
    res.check("c_drive_free_space", free_bytes() >= MIN_FREE_BYTES_START, free_gib=round(free_bytes() / GIB, 1))
    return 0 if res.finish(("no_existing_perennos_paths", "no_existing_perennos_processes",
                            "no_existing_perennos_services", "c_drive_free_space")) else 1


# ---------------------------------------------------------------- build verification (--verify / --extract-only)
def cmd_verify_build(_args):
    res = Result(OUT / "verify-build.json", "verify-build")
    try:
        exe = BUILD / EXE_NAME
        receipt = load_json(BUILD / "BUILD-RECEIPT.json")
        manifest = load_json(PAYLOAD / "MANIFEST.json")
        files = manifest["files"]
        exe_sha = sha256_file(exe)
        res.require("exe_matches_build_receipt", exe_sha == receipt.get("exe_sha256"), exe_sha256=exe_sha,
                    size_bytes=exe.stat().st_size)
        res.require("build_receipt_fields", receipt.get("unsigned") is True and receipt.get("files") == len(files) + 1
                    and receipt.get("runtime_bundled") is True, receipt={k: receipt.get(k) for k in
                    ("unsigned", "files", "runtime_bundled", "payload_sha256", "compiler")})
        res.require("payload_zip_hash", sha256_file(BUILD / "payload.zip") == receipt["payload_sha256"])
        repo_cs = (HERE.parent / "desktop" / "Bootstrap.cs").read_text(encoding="utf-8")
        built_cs = (BUILD / "Bootstrap.cs").read_text(encoding="utf-8")
        res.require("compiled_source_is_reviewed_source",
                    sha256_file(HERE.parent / "desktop" / "Bootstrap.cs") == files["desktop/Bootstrap.cs"]
                    and repo_cs.replace("__PAYLOAD_HASH__", receipt["payload_sha256"]).replace(
                        '__PYTHON_RUNTIME_HASH__', receipt['runtime_zip_sha256']) == built_cs)
        res.require('bundled_runtime_hash', sha256_file(ROOT / 'python-runtime.zip') == receipt['runtime_zip_sha256'])
        res.doc["hashes"] = {"manifest_sha256": sha256_file(PAYLOAD / "MANIFEST.json"), "exe_sha256": exe_sha,
                             "payload_sha256": receipt["payload_sha256"],
                             "bootstrap_cs_sha256": files["desktop/Bootstrap.cs"]}
        cp = subprocess.run([str(exe), "--verify"], capture_output=True, text=True, timeout=180, env=env_clean())
        try:
            doc = json.loads(cp.stdout)
        except ValueError:
            doc = {}
        res.require("verify_flag", cp.returncode == 0 and doc.get("payload_verified") is True
                    and doc.get('python_runtime_verified') is True and doc.get("files") == len(files),
                    returncode=cp.returncode, output=doc, stderr=redact(cp.stderr)[:200])
        if EXTRACT.exists():
            raise Fail("extract folder already exists; a fresh folder is required")
        cp = subprocess.run([str(exe), "--extract-only", str(EXTRACT)], capture_output=True, text=True, timeout=180,
                            env=env_clean())
        try:
            doc = json.loads(cp.stdout)
        except ValueError:
            doc = {}
        res.require("extract_only_flag", cp.returncode == 0 and doc.get("files") == len(files)
                    and doc.get("runtime_verified") is False, returncode=cp.returncode, output=doc,
                    stderr=redact(cp.stderr)[:200])
        actual = {p.relative_to(EXTRACT).as_posix() for p in EXTRACT.rglob("*") if p.is_file()}
        expected = set(files) | {"MANIFEST.json", "INSTALL-RECEIPT.json"}
        mismatched = [n for n, h in files.items() if not (EXTRACT / n).is_file() or sha256_file(EXTRACT / n) != h]
        extracted_receipt = load_json(EXTRACT / "INSTALL-RECEIPT.json")
        res.require("extracted_tree_matches_manifest", actual == expected and not mismatched
                    and extracted_receipt.get("runtime_verified") is False
                    and extracted_receipt.get("payload_sha256") == receipt["payload_sha256"],
                    extracted_files=len(actual), hash_mismatches=mismatched[:10])
        res.require("exe_unchanged_by_verification", sha256_file(exe) == exe_sha)
    except Fail:
        pass
    except Exception as exc:
        res.doc["failure_reason"] = "unexpected %s: %s" % (type(exc).__name__, redact(str(exc))[:300])
    return 0 if res.finish(("exe_matches_build_receipt", "build_receipt_fields", "payload_zip_hash",
                            "compiled_source_is_reviewed_source", "bundled_runtime_hash", "verify_flag", "extract_only_flag",
                            "extracted_tree_matches_manifest", "exe_unchanged_by_verification")) else 1


# ---------------------------------------------------------------- native installer UI drive
def drive_install(res, build, *, foreign_path=None):
    """Launch the unmodified EXE (no args), drive ONLY its own window/dialogs, return (proc, main hwnd, shots)."""
    exe = BUILD / EXE_NAME
    exe_sha = sha256_file(exe)
    res.require("installer_matches_build", exe_sha == build["exe_sha256"], exe_sha256=exe_sha)
    clean = not any(p.exists() for p in (APP, DATA, MODELS)) and free_bytes() >= MIN_FREE_BYTES_START
    res.require("fresh_targets", clean, free_gib=round(free_bytes() / GIB, 1))
    shots, started = [], time.monotonic()
    install_env = env_clean()
    if foreign_path is not None:
        path_key = next((key for key in install_env if key.upper() == "PATH"), "PATH")
        install_env[path_key] = os.pathsep.join((str(Path(foreign_path).resolve()), install_env.get(path_key, "")))
    proc = subprocess.Popen([str(exe)], cwd=str(OUT), env=install_env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=open(OUT / "installer-stderr.txt", "wb"))
    res.doc["installer_pid"] = proc.pid
    main = None
    while time.monotonic() - started < 60:
        if proc.poll() is not None:
            res.require("installer_window", False, reason="installer exited before showing a window",
                        exit_code=proc.returncode)
        mine = windows(pid=proc.pid)
        if any(w["title"] == INSTALLER_ERROR_TITLE for w in mine):
            res.require("installer_window", False, reason="installer showed its error dialog")
        main = next((w for w in mine if w["title"] == MAIN_TITLE), None)
        if main:
            break
        time.sleep(1)
    res.require("installer_window", main is not None, reason="no installer window within 60s",
                title=MAIN_TITLE, pid=proc.pid)
    hwnd = main["hwnd"]
    time.sleep(2)
    snap(shots, "installer-1-initial", hwnd, proc.pid)

    kids = children(hwnd)
    edits = sorted((k for k in kids if ".EDIT." in k["cls"].upper() or k["cls"].upper() == "EDIT"), key=lambda k: k["top"])
    buttons = {(wm_text(k["hwnd"]) or ""): k for k in kids if "BUTTON" in k["cls"].upper()}
    statics = sorted((k for k in kids if "STATIC" in k["cls"].upper()), key=lambda k: k["top"])
    install = buttons.get("Install and open")
    defaults = [wm_text(e["hwnd"]) or "" for e in edits]
    identity_ok = (len(edits) == 3 and "\\App\\" in defaults[0] and "\\Data\\" in defaults[1] and "\\Models\\" in defaults[2])
    wanted = [str(APP), str(DATA), str(MODELS)]
    filled = identity_ok and all(set_text(e["hwnd"], t, proc.pid, hwnd) for e, t in zip(edits, wanted))
    time.sleep(1)
    readback = [wm_text(e["hwnd"]) for e in edits]
    res.require("textboxes_filled", filled and readback == wanted and install is not None and install["enabled"]
                and install["visible"] and len(statics) >= 2,
                reason="expected 3 app-owned textboxes (app/data/models), the 'Install and open' button and status label",
                edit_count=len(edits), identity_ok=identity_ok, install_button_found=install is not None)
    status_hwnd = statics[-2]["hwnd"]  # status label sits above the footer label (fixed layout in Bootstrap.cs)
    snap(shots, "installer-2-paths-filled", hwnd, proc.pid)

    clicked_at = time.monotonic()
    post_owned(install["hwnd"], BM_CLICK, proc.pid, hwnd, MAIN_TITLE)
    click_method, handled, seen, retried = "BM_CLICK", [], set(), False
    res.doc["install"] = {"click_method": click_method, "dialogs": handled, "os_prompt_detections": 0}
    last_heartbeat, busy_polls, last_status = 0.0, 0, None
    log("install clicked; polling every %ds, deadline %d min" % (POLL_S, INSTALL_DEADLINE_S // 60))

    while True:
        elapsed = time.monotonic() - clicked_at
        time.sleep(1 if (not handled and elapsed < 40) else POLL_S)
        elapsed = time.monotonic() - clicked_at
        if proc.poll() is not None:
            res.require("install_within_30min", False, reason="installer process exited during install",
                        exit_code=proc.returncode, seconds=round(elapsed))
        prompts = os_prompts()
        if prompts:
            res.doc["install"]["os_prompt_detections"] = len(prompts)
            res.require("no_os_security_prompt", False, reason="OS/security prompt present (never clicked)", prompts=prompts)
        if free_bytes() < MIN_FREE_BYTES_RUNNING:
            res.require("install_within_30min", False, reason="free disk space on C: dropped below 2 GiB",
                        free_gib=round(free_bytes() / GIB, 2))
        for w in windows(pid=proc.pid):
            if w["hwnd"] == hwnd or w["hwnd"] in seen or w["cls"] in IGNORED_CLASSES:
                continue
            left, top, right, bottom = window_rect(w["hwnd"])
            if right - left < 20 or bottom - top < 20:
                continue
            seen.add(w["hwnd"])
            index = len(handled)
            known = KNOWN_DIALOGS[index] if index < len(KNOWN_DIALOGS) else None
            if w["cls"] != "#32770" or known is None or w["title"] != known[1]:
                snap(shots, "unexpected-window", w["hwnd"], proc.pid)
                res.require("install_within_30min", False, reason="unexpected or out-of-order installer window",
                            caption=w["title"], cls=w["cls"], expected=known[1] if known else None)
            texts = [wm_text(k["hwnd"]) or "" for k in children(w["hwnd"]) if k["cls"] == "Static"]
            body = max(texts, key=len) if texts else ""
            markers = all(m in body for m in known[2])
            record = {"dialog_kind": known[0], "caption": w["title"], "t_plus_s": round(elapsed), "markers_ok": markers}
            if known[0] == "model_downloads":
                record["dialog_body"] = body  # Fixed synthetic CI paths and download sizes only; never model messages or tokens.
                m = re.search(r"will download ([\d,]+) bytes", body)
                record["download_bytes"] = int(m.group(1).replace(",", "")) if m else None
                space = parse_drive_space(body)
                record["drive_space"] = space
                record["space_sufficient"] = bool(space) and all(x["free"] >= x["required"] for x in record["drive_space"])
            record["screenshot"] = (snap(shots, "installer-dialog-%d-%s" % (index + 1, known[0]), w["hwnd"], proc.pid) or {}).get("file")
            yes = next((k for k in children(w["hwnd"]) if k["cls"] == "Button"
                        and (wm_text(k["hwnd"]) or "").replace("&", "") == "Yes" and k["enabled"]), None)
            check_name = "consent_%d_%s" % (index + 1, known[0])
            ok = markers and yes is not None and record.get("space_sufficient", True) and record.get("download_bytes", 1) is not None
            handled.append(record)
            res.require(check_name, ok, reason="dialog body markers/Yes button/sizes not as in Bootstrap.cs", **record)
            post_owned(yes["hwnd"], BM_CLICK, proc.pid, w['hwnd'], known[1])
            record["clicked"] = True
            for _ in range(30):
                if not u32.IsWindow(w["hwnd"]):
                    break
                time.sleep(0.5)
            else:
                res.require(check_name, False, reason="consent dialog did not close after Yes", **record)
        if not handled and elapsed > 15 and not retried and u32.IsWindowEnabled(install["hwnd"]):
            # button still enabled and no dialog => first click was not received; try a real mouse-message click once
            retried = True
            click_method = res.doc["install"]["click_method"] = "BM_CLICK then WM_LBUTTON"
            post_owned(install["hwnd"], WM_LBUTTONDOWN, proc.pid, hwnd, MAIN_TITLE, 1, (8 << 16) | 8)
            post_owned(install["hwnd"], WM_LBUTTONUP, proc.pid, hwnd, MAIN_TITLE, 0, (8 << 16) | 8)
        status = wm_text(status_hwnd)
        busy_polls += status is None
        if status is not None and status != last_status:
            last_status = status
            if any(marker in status for marker in FAILURE_STATUS_MARKERS):
                snap(shots, "installer-failure-status", hwnd, proc.pid)
                res.require("install_within_30min", False, reason="installer reported failure", status=redact(status)[:300])
            snap(shots, "installer-status-%02d" % min(len(shots), 99), hwnd, proc.pid) if len(shots) < 14 else None
        if status and status.startswith("Verified complete") and (APP / "desktop-install-receipt.json").is_file():
            break
        if elapsed - last_heartbeat >= HEARTBEAT_S:
            last_heartbeat = elapsed
            log("heartbeat t+%ds consents=%d ui=%s app+models=%.2f GiB free_c=%.1f GiB" % (
                elapsed, len(handled), "busy(sync native step)" if status is None else redact(last_status or "")[:90],
                dir_bytes(APP, MODELS) / GIB, free_bytes() / GIB))
        if elapsed > INSTALL_DEADLINE_S:
            snap(shots, "installer-timeout", hwnd, proc.pid)
            res.require("install_within_30min", False, reason="install exceeded the 30 minute budget",
                        status=redact(last_status or "")[:200])
    elapsed = time.monotonic() - clicked_at
    res.require("install_clicked", True, click_method=click_method)
    res.require("consent_count", len(handled) == 2 and all(d.get("clicked") for d in handled), consent_dialogs=len(handled),
                reason="expected exactly 2 own consent dialogs")
    res.check("no_os_security_prompt", res.doc["install"]["os_prompt_detections"] == 0, detections=0)
    res.check("install_within_30min", elapsed <= INSTALL_DEADLINE_S, seconds=round(elapsed), ui_busy_polls=busy_polls)
    snap(shots, "installer-3-ready", hwnd, proc.pid)
    res.doc["install"]["final_status"] = redact(last_status or "")[:200]
    res.doc["screenshots"] = shots
    return proc, hwnd, shots


# ---------------------------------------------------------------- receipts / services / GUI launch
def validate_install(res, build, manifest):
    ir, dr = load_json(APP / "INSTALL-RECEIPT.json"), load_json(APP / "desktop-install-receipt.json")
    hr, sr = load_json(DATA / "desktop-install-receipt.json"), load_json(DATA / "setup-receipt.json")
    pyrt = load_json(APP / "runtime" / "python" / "python-runtime.json")
    vpy = APP / "runtime" / "venv" / "Scripts" / "python.exe"
    vpyw, bpy = vpy.with_name("pythonw.exe"), APP / "runtime" / "python" / "python.exe"
    port = dr.get("gateway_port")
    gates = sr.get("gates")
    ck = {
        "install_receipt_runtime_verified": ir.get("product") == "Perennos Tempo" and ir.get("runtime_verified") is True
        and ir.get("payload_sha256") == build["payload_sha256"] and ir.get("setup_state") == "Verified complete",
        "desktop_receipt_state": dr.get("product") == "Perennos Tempo" and dr.get("state") == "Verified complete",
        "paths_bound": all(same_path(r.get(k), v) for r in (ir, dr) for k, v in (("data_home", DATA), ("models_root", MODELS))),
        "home_receipt_matches": hr.get("first_completion_receipt_id") == dr.get("first_completion_receipt_id")
        and hr.get("gateway_port") == port and ir.get("gateway_port") == port,
        "first_private_answer_and_receipt": dr.get("first_private_answer") is True
        and re.fullmatch(r"[0-9a-f]{32}", str(dr.get("first_completion_receipt_id", ""))) is not None,
        "no_hosted_provider": dr.get("hosted_provider_configured") is False,
        "setup_gates_all_true": sr.get("ready") is True and isinstance(gates, dict) and bool(gates)
        and all(v is True for v in gates.values()),
        "port_valid": isinstance(port, int) and 1024 <= port <= 65535,
        "venv_python_bound": sha256_file(vpy) == dr.get("venv_python_sha256"),
        "venv_pythonw_bound": sha256_file(vpyw) == dr.get("venv_pythonw_sha256"),
        "bootstrap_python_bound": same_path(dr.get("bootstrap_python_path"), bpy)
        and sha256_file(bpy) == dr.get("bootstrap_python_sha256"),
        "pinned_python_receipt": pyrt.get("python_version") == "3.12.10" and pyrt.get("installer_sha256") == PY_SHA
        and pyrt.get('runtime_archive_sha256') == build.get('runtime_zip_sha256')
        and pyrt.get("python_sha256") == sha256_file(bpy),
    }
    res.require("receipts_valid", all(ck.values()), reason="receipt gate(s) failed: " + ", ".join(k for k, v in ck.items() if not v),
                gates=ck, setup_gate_count=len(gates) if isinstance(gates, dict) else None,
                review_required=dr.get("review_required"), engine=dr.get("engine") if isinstance(dr.get("engine"), str) else None,
                first_completion_receipt_id=dr.get("first_completion_receipt_id"), gateway_port=port,
                runtime_verified=ir.get("runtime_verified"), payload_sha256=ir.get("payload_sha256"))
    bad = [n for n, h in manifest["files"].items() if not (APP / n).is_file() or sha256_file(APP / n) != h]
    res.require("installed_files_match_manifest", not bad, mismatched=bad[:10], files_checked=len(manifest["files"]))
    return dr, port


def verify_services(res, dr, port, label):
    gateway_pid, ollama_pid = dr.get("gateway_pid"), dr.get("ollama_pid")
    alive = isinstance(gateway_pid, int) and pid_alive(gateway_pid)
    image = proc_image(gateway_pid) if alive else None
    cmd = ps("(Get-CimInstance Win32_Process -Filter 'ProcessId=%d').CommandLine" % int(gateway_pid)).strip() if alive else ""
    listener = None
    listeners = []
    for line in subprocess.run(["netstat", "-ano", "-p", "tcp"], capture_output=True, text=True, timeout=60).stdout.splitlines():
        parts = line.split()
        if len(parts) >= 5 and parts[3] == "LISTENING" and parts[1].rsplit(':', 1)[-1] == str(port):
            listeners.append((parts[1], int(parts[4])))
            if parts[1] == "127.0.0.1:%d" % port:
                listener = int(parts[4])
    sys.path.insert(0, str(APP))
    import tempo_bootstrap
    binding_ok = tempo_bootstrap._assert_owned_listener(
        dr['gateway_process'], APP / 'runtime/venv/Scripts/python.exe', port,
        python=Path(dr['bootstrap_python_path']), python_sha256=dr['bootstrap_python_sha256'])
    info = {"gateway_pid": gateway_pid, "alive": alive, "image_under_app": bool(image) and under(image, APP),
            "image_name": os.path.basename(image or ""), "commandline_is_tempo_headless": "tempo.py" in cmd and "headless" in cmd
            and "-I" in cmd and ("--port %d" % port) in cmd and "127.0.0.1" in cmd,
            "listening_on_loopback_only": listeners == [("127.0.0.1:%d" % port, gateway_pid)], "listener_pid": listener,
            "listener_pid_equals_gateway_pid": listener == gateway_pid, "process_binding_verified": binding_ok,
            "gateway_process_record_keys": sorted(k for k in (dr.get("gateway_process") or {}) if k != "token")}
    res.check("gateway_process", info["alive"] and info["image_under_app"] and info["commandline_is_tempo_headless"]
              and info["listening_on_loopback_only"] and listener == gateway_pid and binding_ok, **info)
    res.check("ollama_process", isinstance(ollama_pid, int) and pid_alive(ollama_pid)
              and under(proc_image(ollama_pid) or "", APP), ollama_pid=ollama_pid)
    h = health(port)
    res.check(label, h["ok"], **h)
    return h["ok"]


def wait_for_gui(res, proc, shots):
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        for w in windows():
            if w["title"] == GUI_TITLE and w["pid"] != proc.pid:
                image = proc_image(w["pid"]) or ""
                if under(image, APP) and os.path.basename(image).lower() in ("python.exe", "pythonw.exe"):
                    time.sleep(3)
                    shot = snap(shots, "installer-launched-gui", w["hwnd"], w["pid"])
                    return w, image, shot
        time.sleep(2)
    return None, None, None


def close_window(hwnd, pid, caption, timeout=30):
    post_owned(hwnd, WM_CLOSE, pid, hwnd, caption)
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if not u32.IsWindow(hwnd):
            return True
        time.sleep(1)
    return False


def run_probe(res, name, argv, result_file, timeout):
    started = time.monotonic()
    try:
        cp = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, env=env_clean(), cwd=str(APP))
        out, err, code = cp.stdout, cp.stderr, cp.returncode
    except subprocess.TimeoutExpired as exc:
        out, err, code = (exc.stdout or ""), (exc.stderr or ""), "timeout"
        out = out.decode("utf-8", "replace") if isinstance(out, bytes) else out
        err = err.decode("utf-8", "replace") if isinstance(err, bytes) else err
    (OUT / ("%s-probe-output.txt" % name)).write_text("exit=%s\n--- stdout\n%s\n--- stderr\n%s\n" % (
        code, redact(out)[-20000:], redact(err)[-20000:]), encoding="utf-8")
    try:
        doc = load_json(result_file)
    except (OSError, ValueError):
        doc = {}
    res.check("probe_" + name, code == 0 and doc.get("passed") is True, returncode=code, seconds=round(time.monotonic() - started),
              result_file=Path(result_file).name, failed_checks=doc.get("failed_checks"),
              missing_checks=doc.get("missing_checks"), probe_failure_reason=doc.get("failure_reason"),
              reason="probe result missing or not passed" if doc.get("passed") is not True else None)


def cmd_run(_args):
    OUT.mkdir(parents=True, exist_ok=True)
    res = Result(OUT / "acceptance-result.json", "perennos-tempo-windows-acceptance")
    proc = None
    foreign_listener = None
    foreign_dll = None
    try:
        build, manifest = load_json(BUILD / "BUILD-RECEIPT.json"), load_json(PAYLOAD / "MANIFEST.json")
        res.doc["environment"] = environment()
        res.doc["hashes"] = {"exe_sha256": build["exe_sha256"], "payload_sha256": build["payload_sha256"],
                             "manifest_sha256": sha256_file(PAYLOAD / "MANIFEST.json"),
                             "bootstrap_cs_sha256": manifest["files"]["desktop/Bootstrap.cs"],
                             "acceptance_scripts": {p.name: sha256_file(p) for p in HERE.glob("*.py")}}
        res.save()
        foreign_path, foreign_dll, foreign_dll_sha256 = _foreign_path_fixture()
        try:
            foreign_listener = ControlledForeignListener()
        except OSError as exc:
            res.require("controlled_foreign_listener_fixture", False,
                        reason="the test-owned loopback fixture could not reserve 127.0.0.1:11434",
                        error_type=type(exc).__name__, errno=getattr(exc, "errno", None),
                        winerror=getattr(exc, "winerror", None))
        fixture_status = foreign_listener.still_owned()
        res.require("controlled_foreign_listener_fixture",
                    fixture_status["bound"] and fixture_status["listening"]
                    and fixture_status["owned_by_acceptance_process"],
                    fixture="controlled foreign listener; not an actual Ollama",
                    foreign_dll="inert bytes; not a PE image", foreign_dll_sha256=foreign_dll_sha256,
                    **fixture_status)
        res.note("controlled_foreign_listener_fixture",
                 scope="controlled foreign listener; not an actual Ollama",
                 two_ollama_scope="reserved for separate Beast acceptance",
                 port=foreign_listener.port, owner_pid=foreign_listener.pid,
                 foreign_dll="inert bytes; not a PE image", foreign_dll_sha256=foreign_dll_sha256)
        proc, hwnd, shots = drive_install(res, build, foreign_path=foreign_path)
        dr, port = validate_install(res, build, manifest)
        ollama_port = dr.get("ollama_port")
        port_avoided = isinstance(ollama_port, int) and ollama_port != foreign_listener.port
        res.check("ollama_avoids_controlled_foreign_listener", port_avoided,
                  controlled_listener_port=foreign_listener.port, installed_ollama_port=ollama_port,
                  fixture="controlled foreign listener; not an actual Ollama")
        listener_status = foreign_listener.still_owned()
        listener_owned = (listener_status["bound"] and listener_status["listening"]
                          and listener_status["owned_by_acceptance_process"])
        res.check("controlled_foreign_listener_still_owned", listener_owned,
                  fixture="controlled foreign listener; not an actual Ollama", **listener_status)
        log_status = _ollama_log_check(APP / "logs" / "ollama.log")
        res.check("ollama_log_no_foreign_dll_warning", log_status["passed"], **log_status)
        if not (port_avoided and listener_owned and log_status["passed"]):
            raise Fail("controlled Windows installer fixtures did not pass")
        verify_services(res, dr, port, "gateway_health_after_install")
        res.doc["gateway_receipt_facts"] = {"gateway_pid": dr.get("gateway_pid"), "ollama_pid": dr.get("ollama_pid"),
                                            "gateway_port": port, "ollama_port": dr.get("ollama_port")}

        window, image, shot = wait_for_gui(res, proc, shots)
        res.require("gui_launched_by_installer", window is not None, reason="no 'Perennos Tempo' Tk window from the app venv within 90s",
                    gui_pid=window["pid"] if window else None, image=os.path.basename(image or ""), screenshot=(shot or {}).get("file"))
        closed = close_window(window["hwnd"], window['pid'], GUI_TITLE)
        time.sleep(3)
        h = health(port)
        res.require("gateway_health_after_gui_close", closed and h["ok"], reason="GUI window did not close or gateway stopped",
                    gui_closed=closed, **h)

        snap(shots, "installer-4-after-gui", hwnd, proc.pid)
        installer_closed = close_window(hwnd, proc.pid, MAIN_TITLE)
        exit_code = None
        try:
            exit_code = proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.terminate()  # Popen-owned installer only
            exit_code = "terminated-by-test"
        time.sleep(3)
        h = health(port)
        res.check("gateway_health_after_installer_exit", installer_closed and exit_code == 0
                  and h["ok"] and pid_alive(dr["gateway_pid"]), installer_closed=installer_closed,
                  installer_exit=exit_code, **h)
        res.doc["screenshots"] = shots
        res.save()

        py = str(APP / "runtime" / "venv" / "Scripts" / "python.exe")
        me = str(Path(__file__).resolve())
        common = ["--app", str(APP), "--home", str(DATA), "--port", str(port)]
        run_probe(res, "native_gui", [py, "-I", "-B", str(HERE / "native_gui_probe.py"), *common, "--out-dir", str(OUT),
                                      "--result", str(OUT / "native-gui-result.json"),
                                      "--expect-desktop-sha", manifest["files"]["tempo_desktop.py"]],
                  OUT / "native-gui-result.json", 1500)
        run_probe(res, "api", [py, "-I", "-B", me, "api-probe", *common, "--result", str(OUT / "api-result.json"),
                               "--expect-client-sha", manifest["files"]["tempo_client.py"]],
                  OUT / "api-result.json", 600)
        run_probe(res, "mcp", [sys.executable, "-I", "-B", me, "mcp-probe", *common, "--result", str(OUT / "mcp-result.json")],
                  OUT / "mcp-result.json", 900)
        res.check("final_gateway_health", health(port)["ok"] and pid_alive(dr["gateway_pid"]), same_gateway_pid=dr["gateway_pid"])
    except Fail:
        pass
    except Exception as exc:
        res.doc["failure_reason"] = "unexpected %s: %s" % (type(exc).__name__, redact(str(exc))[:300])
        log(redact(traceback.format_exc()))
    finally:
        try:
            if proc is not None and proc.poll() is None:
                proc.terminate()  # own child only; installer-owned services are deliberately left running
                res.doc["installer_terminated_by_test_cleanup"] = True
        finally:
            try:
                if foreign_listener is not None:
                    foreign_listener.close()
            finally:
                _remove_foreign_path_fixture(foreign_dll)
                passed = res.finish(REQUIRED_CHECKS)
    return 0 if passed else 1


# ---------------------------------------------------------------- API probe (runs under the INSTALLED venv python)
def cmd_api_probe(args):
    res = Result(args.result, "api-probe")
    try:
        app = Path(args.app).resolve()
        sys.path.insert(0, str(app))
        import tempo_client
        res.require("installed_client_module", same_path(tempo_client.__file__, app / "tempo_client.py")
                    and sha256_file(app / "tempo_client.py") == args.expect_client_sha
                    and same_path(sys.prefix, app / "runtime" / "venv"), reason="not the installed, manifest-pinned client/venv")
        token = tempo_client.gateway_token(Path(args.home))  # in memory only, never printed or stored
        host, port = "127.0.0.1", args.port
        payload, _ = tempo_client.gateway_status(host, port, token)
        res.require("health", isinstance(payload.get("status"), str), status=payload.get("status"),
                    mode=payload.get("mode") if isinstance(payload.get("mode"), str) else None)
        payload, _ = tempo_client.list_models(host, port, token)
        data = payload.get("data")
        ids = [m.get("id") for m in data if isinstance(m, dict)] if isinstance(data, list) else []
        res.require("authenticated_model_list", bool(ids) and all(isinstance(i, str) for i in ids), model_count=len(ids),
                    model_ids=ids[:20], tempo_auto_listed="tempo/auto" in ids)
        wrong = {"Authorization": "Bearer " + "x" * 40}
        s_none, _ = http_json(port, "GET", "/v1/models")
        s_bad, _ = http_json(port, "GET", "/v1/models", headers=wrong)
        body = json.dumps({"model": "tempo/auto", "messages": [{"role": "user", "content": "ping"}],
                           "metadata": {"privacy_class": "private"}, "stream": False, "max_tokens": 1}).encode()
        s_chat, _ = http_json(port, "POST", "/v1/chat/completions", headers={"Content-Type": "application/json"}, body=body)
        res.require("missing_or_wrong_auth_rejected", s_none in (401, 403) and s_bad in (401, 403) and s_chat in (401, 403),
                    models_no_auth=s_none, models_wrong_token=s_bad, chat_no_auth=s_chat)
        try:  # exactly one real private request; no retry whatever the outcome
            payload, headers = tempo_client.chat(host, port, token, SYNTHETIC_API_PROMPT, timeout=300)
        except tempo_client.TempoAPIError as exc:
            res.require("private_chat", False, reason="chat failed: %s" % type(exc).__name__, http_status=exc.status,
                        code=exc.code, request_may_continue=exc.request_may_continue)
        choices = payload.get("choices") if isinstance(payload, dict) else None
        message = choices[0].get("message") if isinstance(choices, list) and choices and isinstance(choices[0], dict) else None
        answer = message.get("content") if isinstance(message, dict) else None
        receipt = headers.get("x-tempo-completion-receipt-id") or headers.get("x-tempo-receipt-id") or ""
        profile = headers.get("x-tempo-profile")
        res.require("private_chat", isinstance(answer, str) and bool(answer.strip())
                    and re.fullmatch(r"[0-9a-f]{32}", receipt) is not None and profile == 'private',
                    reason="empty answer, missing/invalid receipt id or missing profile header",
                    answer_chars=len(answer) if isinstance(answer, str) else None,
                    answer_preview=redact(answer)[:120] if isinstance(answer, str) else None, receipt_id=receipt,
                    job_id=headers.get("x-tempo-job-id"), profile=profile, profile_is_literally_private=profile == "private",
                    review_required=headers.get("x-tempo-review-required"), request_metadata_privacy_class="private")
    except Fail:
        pass
    except Exception as exc:
        res.doc["failure_reason"] = "unexpected %s: %s" % (type(exc).__name__, redact(str(exc))[:200])
    return 0 if res.finish(("installed_client_module", "health", "authenticated_model_list",
                            "missing_or_wrong_auth_rejected", "private_chat")) else 1


# ---------------------------------------------------------------- MCP stdio client transport probe
EXPECTED_TOOLS = ("status", "listmodels", "ask")


def cmd_mcp_probe(args):
    res = Result(args.result, "mcp-probe")
    app, proc = Path(args.app), None
    lines, contamination, crlf = [], [], 0
    try:
        py = app / "runtime" / "venv" / "Scripts" / "python.exe"
        cmd = [str(py), "-I", "-B", str(app / "tempo.py"), "mcp", "--home", args.home, "--port", str(args.port)]
        proc = subprocess.Popen(cmd, cwd=str(app), env=env_clean(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=open(OUT / "mcp-stderr.txt", "wb"))
        inbox = queue.Queue()

        def reader():
            for raw in iter(proc.stdout.readline, b""):
                inbox.put(raw)
            inbox.put(None)

        threading.Thread(target=reader, daemon=True).start()

        def send(message):
            proc.stdin.write((json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8"))
            proc.stdin.flush()

        def receive(want_id, timeout):
            nonlocal crlf
            end = time.monotonic() + timeout
            while True:
                left = end - time.monotonic()
                if left <= 0:
                    raise Fail("timed out waiting for JSON-RPC response id=%s" % want_id)
                try:
                    raw = inbox.get(timeout=left)
                except queue.Empty:
                    raise Fail("timed out waiting for JSON-RPC response id=%s" % want_id) from None
                if raw is None:
                    raise Fail("MCP server closed stdout before response id=%s" % want_id)
                crlf += raw.endswith(b"\r\n")
                text = raw.decode("utf-8", "replace").strip()
                lines.append(text)
                try:
                    message = json.loads(text)
                except ValueError:
                    contamination.append(redact(text)[:120])
                    continue
                if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
                    contamination.append(redact(text)[:120])
                elif message.get("id") == want_id and ("result" in message) != ("error" in message):
                    return message
                else:
                    contamination.append("unsolicited/invalid JSON-RPC message: " + redact(text)[:100])

        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "perennos-ci-acceptance", "version": "1"}}})
        init = receive(1, 60).get("result")
        res.require("initialize", isinstance(init, dict) and isinstance(init.get("protocolVersion"), str),
                    protocol_version=init.get("protocolVersion") if isinstance(init, dict) else None,
                    server_info={k: v for k, v in (init.get("serverInfo") or {}).items() if isinstance(v, str)}
                    if isinstance(init, dict) and isinstance(init.get("serverInfo"), dict) else None,
                    capabilities=sorted(init.get("capabilities", {})) if isinstance(init, dict) and isinstance(init.get("capabilities"), dict) else None)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools = (receive(2, 60).get("result") or {}).get("tools")
        tools = tools if isinstance(tools, list) else []
        names = [t.get("name") for t in tools if isinstance(t, dict)]
        schemas_ok = all(isinstance(t.get("inputSchema"), dict) and t["inputSchema"].get("type") == "object"
                         and isinstance(t.get("description", ""), str) for t in tools if isinstance(t, dict))
        res.require("tools_list", sorted(names) == sorted(EXPECTED_TOOLS) and len(tools) == 3 and schemas_ok,
                    reason="expected exactly the 3 tools %s with object input schemas" % list(EXPECTED_TOOLS),
                    tool_names=names, tool_count=len(tools), schemas_ok=schemas_ok)
        by_name = {t["name"]: t for t in tools}
        ask_schema = by_name["ask"]["inputSchema"]
        props = ask_schema.get("properties") if isinstance(ask_schema.get("properties"), dict) else {}
        string_props = [k for k, v in props.items() if isinstance(v, dict) and v.get("type") == "string"]
        required = [k for k in ask_schema.get("required", []) if isinstance(k, str)]
        ask_key = next((k for k in required if k in string_props), string_props[0] if string_props else None)
        res.require("ask_schema_has_prompt_field", ask_key is not None and set(required) <= {ask_key},
                    reason="cannot derive the ask prompt argument from its schema", properties=sorted(props), required=required)

        def call(call_id, name, arguments, timeout):
            send({"jsonrpc": "2.0", "id": call_id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
            message = receive(call_id, timeout)
            result = message.get("result") if isinstance(message.get("result"), dict) else {}
            parts = result.get("content") if isinstance(result.get("content"), list) else []
            text = "\n".join(p.get("text", "") for p in parts if isinstance(p, dict) and isinstance(p.get("text"), str))
            return message, result, text

        for call_id, name, timeout in ((3, "status", 60), (4, "listmodels", 90)):
            message, result, text = call(call_id, name, {}, timeout)
            value = json.loads(text)
            valid = (value.get('gateway_reachable') is True if name == 'status' else
                     isinstance(value.get('models'), list) and bool(value['models']))
            res.require("tool_" + name, "error" not in message and result.get("isError") is not True and valid,
                        reason="error result or empty content", text_chars=len(text), preview=redact(text)[:200])
        message, result, text = call(5, "ask", {ask_key: SYNTHETIC_MCP_PROMPT}, 300)
        parsed = json.loads(text)
        receipt = re.fullmatch(r'[0-9a-f]{32}', str(parsed.get('receipt_id', '')))
        answer = parsed.get('answer', '')
        res.require("tool_ask", "error" not in message and result.get("isError") is not True and bool(answer.strip())
                    and receipt is not None, reason="error result, empty answer, or no 32-hex receipt id in the result",
                    answer_chars=len(answer), answer_preview=redact(answer)[:120],
                    receipt_id=receipt.group(0) if receipt else None)
    except Fail as exc:
        res.doc["failure_reason"] = res.doc["failure_reason"] or str(exc)
    except Exception as exc:
        res.doc["failure_reason"] = "unexpected %s: %s" % (type(exc).__name__, redact(str(exc))[:200])
    finally:
        exit_code = None
        if proc is not None:
            try:
                proc.stdin.close()
            except OSError:
                pass
            try:
                exit_code = proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.terminate()  # own Popen child only
                try:
                    exit_code = proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=10)
                    exit_code = "killed"
            time.sleep(1)  # drain anything the child wrote while shutting down
            while True:
                try:
                    raw = inbox.get_nowait()
                except queue.Empty:
                    break
                if raw:
                    contamination.append("output after stdin close: " + redact(raw.decode("utf-8", "replace").strip())[:100])
        res.check("stdout_is_pure_jsonrpc", not contamination and bool(lines), json_lines=len(lines) - len(contamination),
                  total_lines=len(lines), contamination_count=len(contamination), contamination_samples=contamination[:5],
                  crlf_lines=crlf)
        res.check("child_cleanup", exit_code == 0, child_exit=exit_code)
        res.doc["transport"] = "stdio newline-delimited JSON-RPC to installed tempo.py mcp (this is NOT an external Codex/Claude client test)"
    return 0 if res.finish(("initialize", "tools_list", "ask_schema_has_prompt_field", "tool_status", "tool_listmodels",
                            "tool_ask", "stdout_is_pure_jsonrpc", "child_cleanup")) else 1


# ---------------------------------------------------------------- evidence staging (allow-list, size cap)
def cmd_stage(_args):
    up = OUT / "upload"
    shutil.rmtree(up, ignore_errors=True)
    up.mkdir(parents=True)
    problems, staged = [], []
    wanted = [BUILD / EXE_NAME, BUILD / "BUILD-RECEIPT.json", BUILD / "BUILD.log"]
    wanted += sorted(p for p in OUT.iterdir() if p.is_file() and p.suffix.lower() in {".json", ".png", ".txt", ".log"})
    for source in wanted:
        if not source.is_file():
            problems.append("missing: " + source.name)
            continue
        if source.suffix.lower() in {".json", ".txt", ".log"} and re.search(
                r"(?i)bearer\s+\S{16,}|authorization\s*:", source.read_text(encoding="utf-8", errors="replace")):
            problems.append("credential-looking text, not staged: " + source.name)
            continue
        shutil.copyfile(source, up / source.name)
        staged.append(source.name)
    for name in ("acceptance-result.json", EXE_NAME, "BUILD-RECEIPT.json"):
        if name not in staged:
            problems.append("required evidence absent: " + name)
    total = sum(p.stat().st_size for p in up.iterdir())
    if total > STAGE_CAP_BYTES:
        problems.append("evidence %d bytes exceeds the %d byte cap; only json/txt kept" % (total, STAGE_CAP_BYTES))
        for p in list(up.iterdir()):
            if p.suffix.lower() not in {".json", ".txt"}:
                p.unlink()
    sums = "".join("%s  %s\n" % (sha256_file(p), p.name) for p in sorted(up.iterdir()) if p.is_file())
    (up / "SHA256SUMS.txt").write_text(sums, encoding="utf-8")
    summary = {"staged": sorted(p.name for p in up.iterdir()), "total_bytes": sum(p.stat().st_size for p in up.iterdir()),
               "problems": problems, "passed": not problems}
    jdump(up / "stage-result.json", summary)
    print(json.dumps(summary, indent=2))
    return 0 if not problems else 1


def main():
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("baseline", "verify-build", "run", "stage"):
        sub.add_parser(name)
    for name in ("api-probe", "mcp-probe"):
        p = sub.add_parser(name)
        p.add_argument("--app", required=True)
        p.add_argument("--home", required=True)
        p.add_argument("--port", type=int, required=True)
        p.add_argument("--result", required=True)
        if name == "api-probe":
            p.add_argument("--expect-client-sha", required=True)
    args = parser.parse_args()
    handler = {"baseline": cmd_baseline, "verify-build": cmd_verify_build, "run": cmd_run, "stage": cmd_stage,
               "api-probe": cmd_api_probe, "mcp-probe": cmd_mcp_probe}[args.command]
    return handler(args)


if __name__ == "__main__":
    sys.exit(main())
