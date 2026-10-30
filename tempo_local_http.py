"""Direct, no-redirect HTTP access for explicitly configured local runtimes."""
from __future__ import annotations

from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def open_local(url_or_request, *, timeout):
    """Open one configured local-runtime URL without environment proxies or redirects.

    The caller owns the destination policy. In particular, configured private-LAN
    HTTP/HTTPS endpoints remain valid; this helper only removes ambient proxy and
    redirect behavior at the local-runtime trust boundary.
    """
    url = url_or_request.full_url if isinstance(url_or_request, Request) else url_or_request
    if not isinstance(url, str):
        raise ValueError("local runtime URL must be text")
    try:
        parts = urlsplit(url)
        if parts.scheme.lower() not in {"http", "https"} or not parts.hostname:
            raise ValueError
        _ = parts.port  # Reject malformed ports before opening a socket.
    except ValueError:
        raise ValueError("local runtime URL must be a valid HTTP or HTTPS URL") from None
    opener = build_opener(ProxyHandler({}), _NoRedirect())
    return opener.open(url_or_request, timeout=timeout)
