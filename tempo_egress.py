"""Hash-bound Windows and dedicated-service-UID Linux egress plans; explicit apply and rollback."""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import ipaddress
import subprocess
from pathlib import Path

POLICY_SCHEMA = "perennos-tempo.egress-preview.v1"
SUPPORTED_ACTIONS = {"preview", "status", "apply", "rollback"}


def build_plan(programs, platform=None, allowed_lan=None, dedicated_uid=None):
    platform = platform or sys.platform
    clean = sorted({str(Path(item).expanduser().resolve()) for item in programs if str(item).strip()})
    if not clean:
        raise ValueError("at least one executable path is required")
    if platform.startswith("win"):
        adapter = "windows-firewall"
        note = ("Owner approval and administrator rights are required. Only the selected executables are affected. "
                "The block-address list excludes loopback and the explicitly approved LAN ranges, avoiding conflicting allow/block rules. "
                "Do not select a shared gateway executable that must also serve approved frontier profiles.")
    elif platform == "linux":
        adapter = "nftables"
        note = ("Enforcement requires a dedicated service UID and CAP_NET_ADMIN in this network namespace. "
                "Every process with that UID is restricted; never select a shared login UID.")
    elif platform == "darwin":
        adapter = "pf"
        note = "A tested per-process macOS network extension or PF anchor is not available in this build."
    else:
        adapter = "unsupported"
        note = "This operating system has no reviewed enforcement adapter."
    payload = {"schema": POLICY_SCHEMA, "platform": platform, "adapter": adapter,
               "enforcement": "preview-only", "programs": clean,
               "rules": [
                   {"action": "allow", "scope": "loopback", "purpose": "local Perennos Tempo gateway and inference"},
                   {"action": "allow", "scope": "approved private LAN", "purpose": "configured local devices only"},
                   {"action": "block", "scope": "public IPv4 and IPv6", "purpose": "prevent direct hosted model egress"},
                   {"action": "block", "scope": "unlisted destinations", "purpose": "fail closed"}
               ],
               "note": note}
    payload["plan_sha256"] = hashlib.sha256(
        json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    if platform.startswith('win'):
        allowed = [ipaddress.ip_network('127.0.0.0/8'),ipaddress.ip_network('::1/128')]
        private = [ipaddress.ip_network(s) for s in ('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','fc00::/7')]
        for value in allowed_lan or []:
            net=ipaddress.ip_network(value,strict=False)
            if not any(net.version==p.version and net.subnet_of(p) for p in private):
                raise ValueError('approved LAN ranges must be inside RFC1918 or IPv6 ULA')
            allowed.append(net)
        blocked=[ipaddress.ip_network('0.0.0.0/0'),ipaddress.ip_network('::/0')]
        for net in allowed:
            next_ranges=[]
            for block in blocked:
                if net.version!=block.version or not net.overlaps(block): next_ranges.append(block)
                elif block.subnet_of(net): continue
                else: next_ranges.extend(block.address_exclude(net))
            blocked=next_ranges
        payload.update(enforcement='owner-approval-required',allowed_networks=[str(n) for n in allowed],
            blocked_networks=[str(n) for n in blocked],
            executable_sha256={p:hashlib.sha256(Path(p).read_bytes()).hexdigest() if Path(p).is_file() else None for p in clean})
        payload.pop('plan_sha256')
        payload['plan_sha256']=hashlib.sha256(json.dumps(payload,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    if platform == 'linux' and dedicated_uid is not None:
        if type(dedicated_uid) is not int or not 1 <= dedicated_uid <= 4294967294:
            raise ValueError('a non-root dedicated numeric service UID is required')
        allowed=[]
        private=[ipaddress.ip_network(s) for s in ('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','fc00::/7')]
        for value in allowed_lan or []:
            net=ipaddress.ip_network(value,strict=False)
            if not any(net.version==p.version and net.subnet_of(p) for p in private):
                raise ValueError('approved LAN ranges must be inside RFC1918 or IPv6 ULA')
            allowed.append(str(net))
        payload.update(enforcement='owner-approval-required',dedicated_uid=dedicated_uid,allowed_networks=sorted(set(allowed)))
        payload.pop('plan_sha256')
        payload['plan_sha256']=hashlib.sha256(json.dumps(payload,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    return payload


def apply_plan(plan, confirmation, runner=None, rollback=False):
    expected = ("REMOVE " if rollback else "APPLY ") + plan["plan_sha256"][:12]
    if confirmation != expected:
        raise PermissionError("confirmation does not match this exact egress plan")
    if plan.get("enforcement") != "owner-approval-required":
        raise RuntimeError("no firewall changes made: this platform adapter is preview-only")
    raw={k:v for k,v in plan.items() if k!='plan_sha256'}
    actual=hashlib.sha256(json.dumps(raw,sort_keys=True,separators=(',',':')).encode()).hexdigest()
    if actual!=plan['plan_sha256']: raise ValueError('egress plan was modified')
    if plan.get('platform')=='linux':
        uid=plan.get('dedicated_uid')
        if type(uid) is not int or not 1 <= uid <= 4294967294:raise ValueError('invalid dedicated UID')
        private=[ipaddress.ip_network(n) for n in ('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','fc00::/7')]
        for value in plan.get('allowed_networks',[]):
            net=ipaddress.ip_network(value,strict=True)
            if not any(net.version==n.version and net.subnet_of(n) for n in private):
                raise ValueError('approved LAN ranges must be private')
        if runner is None and sys.platform!='linux':raise RuntimeError('nftables adapter requires Linux')
        table='tempo_'+actual[:16]
        if rollback:
            script=f'delete table inet {table}\n'
        else:
            # `create` refuses a pre-existing table. A failed batch is atomic;
            # no global flush and no unrelated firewall table are ever touched.
            rules=[f'create table inet {table}',
                   f'add chain inet {table} output {{ type filter hook output priority 0; policy accept; }}',
                   f'add rule inet {table} output meta skuid {uid} ip daddr 127.0.0.0/8 accept',
                   f'add rule inet {table} output meta skuid {uid} ip6 daddr ::1 accept']
            for value in plan.get('allowed_networks',[]):
                net=ipaddress.ip_network(value)
                rules.append(f"add rule inet {table} output meta skuid {uid} {'ip' if net.version==4 else 'ip6'} daddr {net} accept")
            rules += [f'add rule inet {table} output meta skuid {uid} reject']
            script='\n'.join(rules)+'\n'
        result=(runner or subprocess.run)(['nft','-f','-'],input=script,capture_output=True,text=True,timeout=30)
        if result.returncode:raise RuntimeError('nftables adapter refused or failed: '+(result.stderr or '')[-1200:])
        return {'status':'removed' if rollback else 'applied','table':table,'dedicated_uid':uid,'plan_sha256':actual}
    if not plan.get('platform','').startswith('win'):raise RuntimeError('unsupported enforcement adapter')
    if not rollback:
        for path,digest in plan.get('executable_sha256',{}).items():
            if not digest or not Path(path).is_file() or hashlib.sha256(Path(path).read_bytes()).hexdigest()!=digest:
                raise ValueError('selected executable changed or is missing; review a fresh plan')
    if runner is None and sys.platform!='win32': raise RuntimeError('Windows adapter requires Windows')
    script = Path(__file__).with_name('tempo-egress-windows.ps1')
    command=['powershell.exe','-NoProfile','-File',str(script)]+(['-Rollback'] if rollback else [])
    result=(runner or subprocess.run)(command,
        input=json.dumps({**plan,'owner_confirmation':confirmation}),capture_output=True,text=True,timeout=60)
    if result.returncode: raise RuntimeError('firewall adapter refused or failed: '+(result.stderr or 'no local error text')[-1200:])
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description="Review egress rules. Windows changes require exact owner confirmation and administrator rights.")
    parser.add_argument("action", choices=sorted(SUPPORTED_ACTIONS))
    parser.add_argument("--program", action="append", default=[])
    parser.add_argument("--platform", help=argparse.SUPPRESS)
    parser.add_argument("--confirm", help="APPLY or REMOVE plus the exact displayed plan hash prefix")
    parser.add_argument('--allow-lan',action='append',default=[],help='explicit private CIDR allowed for the selected executables')
    parser.add_argument('--dedicated-uid',type=int,help='Linux only: an exclusive service UID, never a shared login')
    parser.add_argument('--plan-file',help='read an original saved plan, including for rollback after executable updates')
    parser.add_argument('--save-plan',help='save the displayed plan to a new local file')
    args = parser.parse_args()
    if args.action == "status":
        print(json.dumps({"status": "owner-approval-required" if sys.platform=='win32' else 'preview-only', "live_rules_checked": False}))
        return
    plan = json.loads(Path(args.plan_file).read_text(encoding='utf-8')) if args.plan_file else build_plan(args.program, args.platform, args.allow_lan,args.dedicated_uid)
    if args.save_plan:
        with Path(args.save_plan).open('x',encoding='utf-8') as saved: json.dump(plan,saved,indent=2)
    print(json.dumps(plan, indent=2))
    if args.action in {"apply","rollback"}:
        expected = ("REMOVE " if args.action=='rollback' else "APPLY ") + plan["plan_sha256"][:12]
        confirm = args.confirm
        if not confirm and sys.stdin.isatty():
            confirm = input(f"Type {expected} to approve these exact rules: ")
        result=apply_plan(plan, confirm or "", rollback=args.action=='rollback')
        print(json.dumps(result,indent=2))


if __name__ == "__main__":
    main()
