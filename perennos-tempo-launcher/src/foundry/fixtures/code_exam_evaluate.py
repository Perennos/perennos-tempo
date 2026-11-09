#!/usr/bin/env python3
"""Compile and execute budget.rs tests inside a fixed bubblewrap boundary."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

RESOURCE_LIMITS = {
    "address_space_bytes": 2 * 1024 * 1024 * 1024,
    "cpu_seconds": 90,
    # RLIMIT_NPROC is per real UID, so a busy system can refuse this launch.
    "processes": 512,
    "output_file_bytes": 64 * 1024 * 1024,
    "wall_timeout_seconds": 90,
}
PRLIMIT = Path("/usr/bin/prlimit")

def sha(data):
    return hashlib.sha256(data).hexdigest()
def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
def sandbox(out, toolchain, writable):
    args = ["/usr/bin/bwrap", "--unshare-all", "--unshare-net", "--die-with-parent",
            "--new-session", "--clearenv"]
    for path in ("/usr", "/lib", "/lib64", "/bin"):
        if Path(path).exists():
            args += ["--ro-bind", path, path]
    args += ["--dir", "/etc"]
    if Path("/etc/ld.so.cache").is_file():
        args += ["--ro-bind", "/etc/ld.so.cache", "/etc/ld.so.cache"]
    if Path("/etc/alternatives").is_dir():
        args += ["--ro-bind", "/etc/alternatives", "/etc/alternatives"]
    args += ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
             "--tmpfs", "/opt", "--dir", "/opt/rust-toolchain",
             "--ro-bind", str(toolchain), "/opt/rust-toolchain",
             "--tmpfs", "/home", "--dir", "/home/checker",
             "--tmpfs", "/root", "--setenv", "HOME", "/home/checker",
             "--setenv", "TMPDIR", "/tmp", "--setenv", "PATH", "/usr/bin:/bin",
             "--setenv", "RUST_TEST_THREADS", "1"]
    args += [("--bind" if writable else "--ro-bind"), str(out), "/work",
             "--chdir", "/work"]
    return args
def run(name, argv, out, timeout):
    started = time.monotonic()
    timeout = min(timeout, RESOURCE_LIMITS["wall_timeout_seconds"])
    stdout_path = out / f"{name}.stdout.log"
    stderr_path = out / f"{name}.stderr.log"
    limited_argv = [
        str(PRLIMIT),
        f"--as={RESOURCE_LIMITS['address_space_bytes']}",
        f"--cpu={RESOURCE_LIMITS['cpu_seconds']}",
        f"--nproc={RESOURCE_LIMITS['processes']}",
        f"--fsize={RESOURCE_LIMITS['output_file_bytes']}",
        "--",
        *argv,
    ]
    limiter_available = PRLIMIT.is_file() and os.access(PRLIMIT, os.X_OK)
    launch_attempted = False
    try:
        with stdout_path.open("wb") as stdout_log, stderr_path.open("wb") as stderr_log:
            if not limiter_available:
                stderr_log.write(b"required resource limiter unavailable: /usr/bin/prlimit; command refused\n")
                code, timed_out = None, False
            else:
                launch_attempted = True
                result = subprocess.run(limited_argv, stdout=stdout_log, stderr=stderr_log,
                                        timeout=timeout, check=False)
                code, timed_out = result.returncode, False
    except subprocess.TimeoutExpired as exc:
        code, timed_out = None, True
        del exc
    except OSError as exc:
        code, timed_out = None, False
        stderr_path.write_bytes(str(exc).encode("utf-8", "replace"))
    return {"argv": limited_argv, "resource_limits": {**RESOURCE_LIMITS, "launcher": str(PRLIMIT),
            "launcher_available": limiter_available, "launch_attempted": launch_attempted},
            "returncode": code, "timed_out": timed_out, "timeout_seconds": timeout,
            "elapsed_seconds": round(time.monotonic() - started, 3), "stdout": stdout_path.name, "stderr": stderr_path.name}
def region(data):
    start_mark = b"    pub fn charge_tokens(&self, n: u64) {"
    end_mark = b"    /// The stop switch:"
    if data.count(start_mark) != 1 or data.count(end_mark) != 1:
        raise ValueError("budget.rs charge-method anchors are missing or duplicated")
    start, end = data.index(start_mark), data.index(end_mark)
    if end <= start:
        raise ValueError("budget.rs charge-method anchors are out of order")
    return data[:start], data[start:end], data[end:]
def approved_qualified_paths(body):
    whitespace = b" \t\n\r\v\f"
    def identifier_byte(byte):
        return byte >= 0x80 or byte == ord("_") or ord("0") <= byte <= ord("9") or ord("A") <= byte <= ord("Z") or ord("a") <= byte <= ord("z")
    for index in range(len(body) - 1):
        if body[index:index + 2] != b"::":
            continue
        left_end = index
        while left_end and body[left_end - 1] in whitespace:
            left_end -= 1
        left_start = left_end
        while left_start and identifier_byte(body[left_start - 1]):
            left_start -= 1
        right_start = index + 2
        while right_start < len(body) and body[right_start] in whitespace:
            right_start += 1
        right_end = right_start
        while right_end < len(body) and identifier_byte(body[right_end]):
            right_end += 1
        if (body[left_start:left_end], body[right_start:right_end]) not in ((b"Ordering", b"SeqCst"), (b"u64", b"MAX")):
            return False
    return True
def validate_candidate(original, candidate):
    op, ob, os = region(original)
    cp, cb, cs = region(candidate)
    if cp != op or cs != os:
        raise ValueError("candidate changes content outside the three charge methods")
    if re.search(rb"//|/\*|['\"!#]|\bunsafe\b", cb):
        raise ValueError("charge-method replacement contains a comment, literal, macro, attribute, or unsafe")
    if re.search(rb"\b(?:use|extern)\b", cb):
        raise ValueError("charge-method replacement contains an import")
    if not approved_qualified_paths(cb):
        raise ValueError("charge-method replacement uses an unapproved qualified path")
    heads = [b"pub fn charge_tokens(&self, n: u64) {", b"pub fn charge_tool(&self) {", b"pub fn charge_cents(&self, n: u64) {"]
    pos = 0
    for head in heads:
        while pos < len(cb) and cb[pos:pos + 1].isspace():
            pos += 1
        if not cb.startswith(head, pos):
            raise ValueError("candidate must keep the three charge-method signatures and order")
        depth, cursor = 1, pos + len(head)
        while cursor < len(cb) and depth:
            depth += (cb[cursor] == ord("{")) - (cb[cursor] == ord("}"))
            cursor += 1
        if depth:
            raise ValueError("unbalanced charge-method body")
        pos = cursor
    if cb[pos:].strip():
        raise ValueError("candidate adds code outside the three charge methods")
def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--source", required=True, type=Path, help="original budget.rs")
    ap.add_argument("--candidate", type=Path, help="optional complete modified budget.rs")
    ap.add_argument("--out", required=True, type=Path, help="new output directory (must not exist)")
    ns = ap.parse_args()
    out = ns.out.expanduser().resolve()
    out.mkdir(parents=True, exist_ok=False)
    result = {"status": "Untested", "reason": None, "resource_limits": {**RESOURCE_LIMITS,
              "launcher": str(PRLIMIT), "required": True}, "commands": {}, "tests": {"original_budget_tests": {"count": 0, "outcomes": []}, "fixed_overflow_exam": {"count": 0, "outcomes": []}, "other": []}}
    write_json(out / "result.json", result)
    try:
        src_path = ns.source.resolve(strict=True); src = src_path.read_bytes()
        chosen_path = ns.candidate.resolve(strict=True) if ns.candidate else src_path; chosen = chosen_path.read_bytes()
        harness_path = Path(__file__).with_name("overflow_tests.rs").resolve(strict=True); harness = harness_path.read_bytes()
    except (OSError, FileNotFoundError) as exc:
        result["reason"] = f"input refused: {exc}"
        write_json(out / "result.json", result)
        return 2
    base_copy = out / ("candidate_input.rs" if ns.candidate else "source_original.rs")
    base_copy.write_bytes(chosen)
    if ns.candidate:
        (out / "source_original.rs").write_bytes(src)
        try:
            validate_candidate(src, chosen)
        except ValueError as exc:
            result.update({"reason": f"candidate refused: {exc}", "source_sha256": sha(src),
                           "candidate_sha256": sha(chosen)})
            write_json(out / "result.json", result)
            return 2
    (out / "overflow_tests.rs").write_bytes(harness)
    composed = chosen + (b"" if chosen.endswith((b"\n", b"\r")) else b"\n") + b"\n" + harness
    source_file = out / "isolated_budget.rs"
    source_file.write_bytes(composed)
    rustup = Path.home() / ".cargo" / "bin" / "rustup"
    bwrap = Path("/usr/bin/bwrap")
    if not bwrap.is_file():
        result["reason"] = "sandbox refused: /usr/bin/bwrap unavailable"
        write_json(out / "result.json", result)
        return 3
    if not rustup.is_file():
        result["reason"] = "sandbox refused: configured rustup unavailable"
        write_json(out / "result.json", result)
        return 3
    discovery = run("rustup_which", [str(rustup), "which", "rustc"], out, 5)
    result["commands"]["rustup_which"] = discovery
    if discovery["returncode"] != 0:
        result["reason"] = "sandbox refused: rustup which rustc failed"
        write_json(out / "result.json", result)
        return 3
    try: compiler = Path((out / discovery["stdout"]).read_text(encoding="utf-8").strip()).resolve(strict=True)
    except OSError as exc: result["reason"] = f"sandbox refused: invalid rustc path {exc}"; write_json(out / "result.json", result); return 3
    toolchain = compiler.parents[1]
    rustup_home = Path(os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))).expanduser().resolve()
    try:
        toolchain.relative_to(rustup_home / "toolchains")
    except ValueError:
        result["reason"] = "sandbox refused: rustup compiler is outside the configured toolchains"
        write_json(out / "result.json", result)
        return 3
    if not compiler.is_file() or compiler.name != "rustc" or compiler.parent != toolchain / "bin":
        result["reason"] = "sandbox refused: rustup returned an unexpected compiler path"
        write_json(out / "result.json", result)
        return 3
    result.update({"source": str(src_path), "source_sha256": sha(src),
                   "candidate": str(chosen_path) if ns.candidate else None,
                   "candidate_sha256": sha(chosen) if ns.candidate else None, "harness": str(harness_path),
                   "harness_sha256": sha(harness), "compiled_source": str(source_file),
                   "compiled_source_sha256": sha(composed), "compiler": "bin/rustc",
                   "compiler_sha256": sha(compiler.read_bytes()), "toolchain": toolchain.name,
                   "sandbox": "bwrap --unshare-all --unshare-net; readonly system/toolchain; private /tmp and home"})
    source_names = re.findall(rb"(?m)^[ \t]*#\[test\][ \t]*\r?\n[ \t]*fn[ \t]+(\w+)", src); harness_names = re.findall(rb"(?m)^[ \t]*#\[test\][ \t]*\r?\n[ \t]*fn[ \t]+(\w+)", harness)
    expected_original = ["tests::" + x.decode("ascii") for x in source_names]; expected_exam = ["code_exam_tests::" + x.decode("ascii") for x in harness_names]
    result["expected_tests"] = {"original_budget_tests": expected_original,
                                "fixed_overflow_exam": expected_exam}
    write_json(out / "result.json", result)
    timeout = 90
    # Keep rustc codegen and LLD link threads single-threaded under the finite resource caps.
    compile_argv = sandbox(out, toolchain, True) + ["/opt/rust-toolchain/bin/rustc", "--edition=2021", "-C", "codegen-units=1", "-C", "link-arg=-Wl,--threads=1", "--test", "/work/isolated_budget.rs", "-o", "/work/budget_tests"]
    result["commands"]["rustc"] = run("rustc", compile_argv, out, timeout)
    c = result["commands"]["rustc"]
    if c["returncode"] != 0:
        err = (out / c["stderr"]).read_text(encoding="utf-8", errors="replace")
        refused = c["timed_out"] or any(s in err.lower() for s in ("bwrap", "namespace", "operation not permitted"))
        result["status"] = "Untested" if refused else "In progress"
        result["reason"] = "sandbox refused or timed out" if refused else "rustc failed; see raw logs"
        write_json(out / "result.json", result)
        return 1
    binary = out / "budget_tests"
    result["compiled_binary"] = str(binary)
    result["compiled_binary_sha256"] = sha(binary.read_bytes())
    test_argv = sandbox(out, toolchain, False) + ["/work/budget_tests", "--test-threads=1", "--nocapture"]
    result["commands"]["tests"] = run("tests", test_argv, out, timeout)
    output = (out / "tests.stdout.log").read_text(encoding="utf-8", errors="replace")
    outcomes = re.findall(r"^test ([^\s]+) \.\.\. (ok|FAILED)$", output, re.MULTILINE)
    for name, outcome in outcomes:
        row = {"name": name, "outcome": outcome}
        if name.startswith("tests::"):
            result["tests"]["original_budget_tests"]["outcomes"].append(row)
        elif name.startswith("code_exam_tests::"):
            result["tests"]["fixed_overflow_exam"]["outcomes"].append(row)
        else:
            result["tests"]["other"].append(row)
    for key in ("original_budget_tests", "fixed_overflow_exam"):
        result["tests"][key]["count"] = len(result["tests"][key]["outcomes"])
    t = result["commands"]["tests"]
    actual_original = [x["name"] for x in result["tests"]["original_budget_tests"]["outcomes"]]
    actual_exam = [x["name"] for x in result["tests"]["fixed_overflow_exam"]["outcomes"]]
    names_match = sorted(actual_original) == sorted(expected_original) and sorted(actual_exam) == sorted(expected_exam)
    all_pass = all(x["outcome"] == "ok" for group in ("original_budget_tests", "fixed_overflow_exam")
                   for x in result["tests"][group]["outcomes"])
    if t["returncode"] == 0 and names_match and all_pass and len(expected_exam) == 5:
        result["status"] = "Verified complete"
        result["reason"] = "compile passed; all original tests and all five fixed exam tests executed and passed"
    elif t["timed_out"] or any(s in (out / t["stderr"]).read_text(encoding="utf-8", errors="replace").lower() for s in ("bwrap", "namespace", "operation not permitted")):
        result["status"], result["reason"] = "Untested", "test sandbox refused or timed out"
    else:
        result["status"], result["reason"] = "In progress", "test failure, missing expected test, or sandbox error; see raw logs"
    write_json(out / "result.json", result)
    return 0 if result["status"] == "Verified complete" else 1
if __name__ == "__main__":
    raise SystemExit(main())
