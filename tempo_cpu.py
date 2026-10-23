"""Explicit, time-bounded CPU-only execution under an Orchestra booking."""
import time
import ipaddress
from contextlib import contextmanager
from urllib.parse import urlsplit


def require_loopback(url):
    parsed = urlsplit(url)
    try:
        local = parsed.hostname == 'localhost' or ipaddress.ip_address(parsed.hostname).is_loopback
    except ValueError:
        local = False
    if parsed.scheme != 'http' or not local or parsed.username or parsed.password:
        raise RuntimeError('CPU acceptance requires a loopback local runtime')


def check_window(booking, until, minimum_seconds=0):
    remaining = float(until or 0) - time.time()
    if not booking or not 0 < remaining <= 7200 or remaining < minimum_seconds:
        raise RuntimeError("a live CPU booking with enough time remaining is required")
    return remaining


@contextmanager
def cpu_window(booking, until, minimum_seconds=0):
    check_window(booking, until, minimum_seconds)
    yield
