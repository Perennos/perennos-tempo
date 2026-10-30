"""Command entry point for the Perennos Tempo v1 runtime."""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

LOOPBACK_HOSTS = {"127.0.0.1"}

# `mcp-config` deliberately starts Python in isolated mode. Keep the import
# closure anchored to this installed script folder in that mode.
_SOURCE_DIR = str(Path(__file__).resolve().parent)
if _SOURCE_DIR not in sys.path:
    sys.path.insert(0, _SOURCE_DIR)


def _add_home(parser):
    parser.add_argument("--home", default=os.environ.get("TEMPO_HOME"),
                        help="this install's private data folder (also sets TEMPO_HOME)")


def _parser():
    parser = argparse.ArgumentParser(description="Perennos Tempo v1 local runtime")
    modes = parser.add_subparsers(dest="mode", required=True)

    gui = modes.add_parser("gui", help="open the Perennos Tempo desktop interface")
    _add_home(gui)
    gui.add_argument("--port", type=int, default=8765)

    headless = modes.add_parser("headless", help="run the authenticated local Perennos Tempo API")
    _add_home(headless)
    headless.add_argument("--host", choices=sorted(LOOPBACK_HOSTS), default="127.0.0.1")
    headless.add_argument("--port", type=int, default=8765)
    headless.add_argument("--device", choices=("auto", "gpu", "cpu"), default="auto")
    headless.add_argument("--lease-script")
    headless.add_argument("--lease-need-mib", type=int,
                          default=int(os.environ.get("TEMPO_GPU_LEASE_NEED_MIB", "4096")))
    headless.add_argument("--lease-ttl-min", type=int, default=20)
    headless.add_argument("--timeout-seconds", type=int, default=600)
    headless.add_argument("--cpu-booking")
    headless.add_argument("--cpu-until", type=float, default=0)
    headless.add_argument("--tool-policy")

    mcp = modes.add_parser("mcp", help="serve bounded Perennos Tempo MCP tools over stdio")
    _add_home(mcp)
    mcp.add_argument("--host", choices=sorted(LOOPBACK_HOSTS), default="127.0.0.1")
    mcp.add_argument("--port", type=int, default=8765)

    status = modes.add_parser("status", help="check local Perennos Tempo gateway process health")
    _add_home(status)
    status.add_argument("--host", choices=sorted(LOOPBACK_HOSTS), default="127.0.0.1")
    status.add_argument("--port", type=int, default=8765)

    config = modes.add_parser("mcp-config", help="print a secret-free MCP client config")
    _add_home(config)
    config.add_argument("--host", choices=sorted(LOOPBACK_HOSTS), default="127.0.0.1")
    config.add_argument("--port", type=int, default=8765)

    doctor = modes.add_parser("doctor", help="read-only, content-free setup diagnostics (offline by default)")
    _add_home(doctor)
    doctor.add_argument("--port", type=int, default=8765)
    doctor.add_argument("--connect", action="store_true", help="also verify the local gateway; never runs inference")
    return parser


def _home_path(value, *, required=True):
    value = value or os.environ.get("TEMPO_HOME")
    if not value:
        if required:
            raise ValueError("set --home or TEMPO_HOME to this Perennos Tempo install's data folder")
        return None
    home = Path(value).expanduser().resolve()
    if not home.exists() or not home.is_dir():
        raise ValueError("Perennos Tempo data folder must be an existing directory")
    os.environ["TEMPO_HOME"] = str(home)
    return home


class _SafeGatewayStdout:
    """Allow only the known startup metadata record and remove its private token path."""
    def __init__(self, destination):
        self.destination = destination
        self.pending = ""

    def write(self, text):
        self.pending += text
        while "\n" in self.pending:
            line, self.pending = self.pending.split("\n", 1)
            try:
                row = json.loads(line)
            except (ValueError, TypeError):
                continue
            if not isinstance(row, dict) or row.get("status") != "starting":
                continue
            bind = row.get("bind")
            port = row.get("port")
            lease_need_mib = row.get("lease_need_mib")
            safe = {
                "status": "starting",
                "bind": bind if isinstance(bind, str) and bind in LOOPBACK_HOSTS else "127.0.0.1",
                "port": port if isinstance(port, int) and not isinstance(port, bool) else 8765,
                "mode": "loopback",
                "lease_script_configured": bool(row.get("lease_script_configured")),
                "lease_need_mib": lease_need_mib if isinstance(lease_need_mib, int) and not isinstance(lease_need_mib, bool) else 4096,
            }
            self.destination.write(json.dumps(safe, separators=(",", ":")) + "\n")
            self.destination.flush()
        return len(text)

    def flush(self):
        self.destination.flush()


def _run_headless(args, home):
    if not 1024 <= args.port <= 65535 or args.lease_need_mib < 1:
        raise ValueError("invalid Perennos Tempo gateway port or lease memory request")
    argv = [str(Path(__file__).with_name("tempo_gateway.py")), "--host", args.host,
            "--port", str(args.port), "--device", args.device,
            "--lease-need-mib", str(args.lease_need_mib), "--lease-ttl-min", str(args.lease_ttl_min),
            "--timeout-seconds", str(args.timeout_seconds), "--cpu-until", str(args.cpu_until)]
    for flag, value in (("--lease-script", args.lease_script), ("--cpu-booking", args.cpu_booking),
                        ("--tool-policy", args.tool_policy)):
        if value:
            argv.extend((flag, value))
    old_argv = sys.argv
    had_token_env = "TEMPO_GATEWAY_TOKEN" in os.environ
    token_env = os.environ.pop("TEMPO_GATEWAY_TOKEN", None)
    os.environ["TEMPO_HOME"] = str(home)
    sys.argv = argv
    old_stdout = sys.stdout
    sys.stdout = _SafeGatewayStdout(old_stdout)
    try:
        from tempo_gateway import main as gateway_main
        gateway_main()
    finally:
        sys.stdout.flush()
        sys.stdout = old_stdout
        sys.argv = old_argv
        if had_token_env:
            os.environ["TEMPO_GATEWAY_TOKEN"] = token_env


def _run_status(args, home):
    if not 1024 <= args.port <= 65535:
        raise ValueError("invalid Perennos Tempo gateway port")
    from tempo_client import TempoAPIError, gateway_status, gateway_token
    try:
        token = gateway_token(home)
        payload, _ = gateway_status(args.host, args.port, token)
        result = {
            "gateway_reachable": True,
            "process_status": payload.get("status") if isinstance(payload.get("status"), str) else "unknown",
            "mode": payload.get("mode") if isinstance(payload.get("mode"), str) else "unknown",
        }
        print(json.dumps(result, separators=(",", ":")))
        return 0
    except (TempoAPIError, OSError, ValueError):
        print(json.dumps({"gateway_reachable": False, "process_status": "unavailable"}, separators=(",", ":")))
        return 1


def _run_mcp(args, home):
    if not 1024 <= args.port <= 65535:
        raise ValueError("invalid Perennos Tempo gateway port")
    from tempo_client import gateway_token
    from tempo_mcp import serve
    serve(args.host, args.port, gateway_token(home))
    return 0


def _print_mcp_config(args, home):
    config = {
        "mcpServers": {
            "perennos-tempo": {
                "command": sys.executable,
                "args": ["-I", "-B", str(Path(__file__).resolve()), "mcp", "--home", str(home),
                         "--host", args.host, "--port", str(args.port)],
            }
        }
    }
    print(json.dumps(config, indent=2))
    return 0


def main(argv=None):
    parser = _parser()
    args = parser.parse_args(argv)
    try:
        if args.mode == "doctor":
            from tempo_doctor import diagnose, exit_code
            selected = args.home or (Path.home() / "PerennosTempo")
            report = diagnose(Path(selected), port=args.port, connect=args.connect)
            print(json.dumps(report, indent=2))
            return exit_code(report)
        home = _home_path(args.home, required=args.mode != "gui")
        if args.mode == "gui":
            if not 1024 <= args.port <= 65535:
                raise ValueError("invalid Perennos Tempo gateway port")
            from tempo_desktop import main as desktop_main
            return desktop_main(home=str(home) if home else None, port=args.port, argv=[])
        if args.mode == "headless":
            _run_headless(args, home)
            return 0
        if args.mode == "mcp":
            return _run_mcp(args, home)
        if args.mode == "status":
            return _run_status(args, home)
        if args.mode == "mcp-config":
            if not 1024 <= args.port <= 65535:
                raise ValueError("invalid Perennos Tempo gateway port")
            return _print_mcp_config(args, home)
    except (OSError, ValueError, RuntimeError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    raise SystemExit(main())
