#!/usr/bin/env python3
"""Prove the harness can tell a good Desk from bad ones, using stub_desk.py. Run: python selftest.py"""
import json, os, subprocess, sys, tempfile
from pathlib import Path

import gen_firm

here = Path(__file__).resolve().parent
tmp = Path(tempfile.mkdtemp(prefix="desk-selftest-"))
gen_firm.build(tmp / "firm", 20261110)
py = sys.executable
adapter = {"kind": "cmd", "argv": [py, str(here / "stub_desk.py")], "timeout": 60,
           "egress": {"begin": [py, "-c", "pass"], "end": [py, str(here / "stub_desk.py"), "egress-end"]},
           "restart": {"kill": [py, "-c", "pass"], "start": [py, "-c", "pass"]}}
(tmp / "adapter.json").write_text(json.dumps(adapter))


def run(mode, suites, extra_env=None):
    env = dict(os.environ, STUB_MODE=mode, STUB_EVAL=str(tmp / "firm" / "eval"), STUB_STATE=str(tmp / "state.json"), **(extra_env or {}))
    out = tmp / f"out-{mode}-{suites.replace(',', '_')}"
    subprocess.run([py, str(here / "harness.py"), "--eval", str(tmp / "firm" / "eval"), "--adapter", str(tmp / "adapter.json"), "--out", str(out), "--suite", suites, "--workers", "8"], env=env, capture_output=True)
    return {r["suite"]: r["status"] for r in json.loads((out / "results.json").read_text())["results"]}


cases = [("good", "citation,unanswerable,wall,egress,completion,draft,restart", None, "PASS"),
         ("leaky", "wall", None, "FAIL"),
         ("liar", "citation,unanswerable,draft,restart", None, "FAIL"),
         ("good", "egress", {"STUB_BYTES": "1200"}, "FAIL")]
bad = 0
for mode, suites, env, want in cases:
    got = run(mode, suites, env)
    ok = all(v == want for v in got.values())
    bad += not ok
    print(("ok  " if ok else "BAD ") + f"{mode:<6} expected {want}: {got}")
sys.exit(1 if bad else 0)
