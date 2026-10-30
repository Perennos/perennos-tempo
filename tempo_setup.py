"""Perennos Tempo first-run setup: put it on a computer or server and it prepares itself.

  python tempo_setup.py --home D:\\tempo\\acme --tenant acme          # INVENTORY + PLAN: writes nothing, no network
  python tempo_setup.py --home ... --check-registry                   # plan, and confirm each engine exists upstream
  python tempo_setup.py --home ... --apply --pull --fetch-models      # APPLY the plan, then QUALIFY it
  python tempo_setup.py --home ... --apply --mode hybrid --allow-provider anthropic,groq
  python tempo_setup.py --home ... --first-boot --apply ...           # run once (marker file), for a boot task

Four states (Codex/Sol review, 17 Oct 2026):
 1. Inventory, read-only: OS, CPU threads, RAM, GPU total and free VRAM, free disk, Ollama, and which provider keys
    exist in the environment (presence only; values are never read or stored).
 2. Plan, printed, nothing written: tenant settings for the vertical and mode, and engines per role from
    model_catalog.json sized to the hardware. Only names confirmed upstream (--check-registry) are pullable;
    unconfirmed ones are listed as unverified, never pulled.
 3. Apply (--apply): write the tenant (an existing ledger is never touched), pull, fetch the pinned decision-head
    files, register engines, build priors, run the selftest. Every step goes to <home>/setup-receipt.json.
 4. Qualify: a real round-trip under the shared GPU lease, or an explicit booked CPU-only window. "ready" needs a
    qualified engine plus a passing selftest, decision-head files, engine sync and priors. Anything else: not ready.
Rules: --offline refuses downloads and upstream registry checks. Provider keys alone do not enable hosted routing;
the owner must explicitly allow a provider. Local-only privacy classes remain local in every mode. The public core
starts with neutral routing priors and does not include benchmark datasets or owner-specific prior maps.
"""
import argparse, ipaddress, json, os, platform, shutil, subprocess, sys, time, urllib.request
from pathlib import Path
from urllib.parse import urlsplit

CODE = Path(__file__).resolve().parent

VERTICALS = {'general': {'task_classes': ['docs', 'summary', 'lint', 'draft', 'code', 'research', 'image-draft', 'storyboard'], 'low_stakes': ['docs', 'summary', 'lint', 'draft', 'image-draft', 'storyboard'], 'never_explore_add': [], 'local_only_add': [], 'verdicts': {'writing-code': ['tests', 'lint', 'reviewer', 'owner'], 'research': ['source-check', 'owner'], 'thinking': ['reviewer', 'owner'], 'imagery': ['visual-audit', 'owner'], 'video': ['visual-audit', 'av-qc', 'owner']}}}
PROVIDER_KEYS = {"anthropic": "ANTHROPIC_API_KEY", "openai": "OPENAI_API_KEY", "google": "GEMINI_API_KEY",
                 "groq": "GROQ_API_KEY", "xai": "XAI_API_KEY", "openrouter": "OPENROUTER_API_KEY"}


def _run(cmd, timeout=20):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip() if r.returncode == 0 else None
    except Exception:
        return None


def _ram_gb():
    try:
        if os.name == "nt":
            import ctypes
            class M(ctypes.Structure):
                _fields_ = [("l", ctypes.c_ulong), ("load", ctypes.c_ulong), ("total", ctypes.c_ulonglong),
                            ("avail", ctypes.c_ulonglong), ("a", ctypes.c_ulonglong), ("b", ctypes.c_ulonglong),
                            ("c", ctypes.c_ulonglong), ("d", ctypes.c_ulonglong), ("e", ctypes.c_ulonglong)]
            m = M(); m.l = ctypes.sizeof(M); ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(m))
            return round(m.total / 2**30, 1)
        if sys.platform == "darwin":
            return round(int(_run(["sysctl", "-n", "hw.memsize"])) / 2**30, 1)
        for line in open("/proc/meminfo"):
            if line.startswith("MemTotal"):
                return round(int(line.split()[1]) / 2**20, 1)
    except Exception:
        return None


def _existing(p):
    while not p.exists() and p.parent != p:
        p = p.parent
    return p


def _offline_loopback_url(url):
    """Allow offline inventory probes only to parsed numeric loopback or localhost."""
    try:
        parsed = urlsplit(url)
        if parsed.scheme.lower() not in {"http", "https"} or not parsed.hostname:
            return False
        _ = parsed.port
        host = parsed.hostname.lower()
        if host == "localhost":
            return True
        return ipaddress.ip_address(host).is_loopback
    except (AttributeError, TypeError, ValueError):
        return False


def inventory(home, ollama_url, offline):
    """Read-only. Never creates the home folder."""
    gpus = []
    out = _run(["nvidia-smi", "--query-gpu=name,memory.total,memory.free", "--format=csv,noheader,nounits"])
    for line in (out or "").splitlines():
        name, total, free = [x.strip() for x in line.rsplit(",", 2)]
        gpus.append({"name": name, "vram_gb": round(int(total) / 1024, 1), "vram_free_gb": round(int(free) / 1024, 1)})
    ver = None
    if not offline or _offline_loopback_url(ollama_url):
        try:
            from tempo_local_http import open_local
            ver = json.load(open_local(ollama_url.rstrip("/") + "/api/version", timeout=5)).get("version")
        except Exception:
            ver = None
    return {"os": f"{platform.system()} {platform.release()}", "machine": platform.machine(),
            "cpu_threads": os.cpu_count(), "ram_gb": _ram_gb(), "gpus": gpus,
            "apple_unified_memory": sys.platform == "darwin" and platform.machine() == "arm64",
            "disk_free_gb": round(shutil.disk_usage(_existing(home)).free / 2**30, 1),
            "ollama": {"url": ollama_url, "version": ver, "cli": bool(shutil.which("ollama"))},
            "provider_keys_present": {p: bool(os.environ.get(k)) for p, k in PROVIDER_KEYS.items()}}


def memory_budget(hw):
    """GB one engine may use, from TOTAL memory: an install plan for the machine, not a capacity guarantee.
    Free VRAM and the shared GPU lease decide at run time; qualification measures the real fit."""
    if hw["gpus"]:
        return round(max(g["vram_gb"] for g in hw["gpus"]) * 0.95, 1)
    ram = hw["ram_gb"] or 8
    return round(ram * (0.6 if hw["apple_unified_memory"] else 0.4), 1)


def registry_ok(name):
    repo, tag = (name.split(":") + ["latest"])[:2]
    repo = repo if "/" in repo else "library/" + repo
    req = urllib.request.Request(f"https://registry.ollama.ai/v2/{repo}/manifests/{tag}",
                                 headers={"Accept": "application/vnd.docker.distribution.manifest.v2+json"})
    try:
        urllib.request.urlopen(req, timeout=20)
        return True
    except Exception:
        return False


def recommend(hw, uncensored, check_registry, disk_reserve_gb=20):
    cat = json.loads((CODE / "model_catalog.json").read_text(encoding="utf-8-sig"))["engines"]
    budget = memory_budget(hw)
    headroom = (lambda e: e["gb"] + 1.0) if hw["gpus"] else (lambda e: e["gb"] * 1.2)   # weights plus context memory
    fits = [e for e in cat if headroom(e) <= budget]
    picks, why = {}, {}
    for role in ("thinking", "research", "inference", "writing-code", "embedding"):
        cands = sorted([e for e in fits if role in e["roles"]], key=lambda e: (-e.get("params_b", 0), e["gb"]))
        if uncensored:
            unc = [e for e in cands if e["uncensored"]]
            cands = unc + [e for e in cands if not e["uncensored"]]
        else:
            cands = [e for e in cands if not e["uncensored"]]
        if role == "inference":                              # the fast lane: smallest capable engine
            cands = sorted(cands, key=lambda e: (not e["uncensored"] if uncensored else 0, e["gb"]))
        if cands:
            picks[role] = cands[0]["name"]
            why[role] = f'{cands[0]["gb"]} GB fits a {budget} GB plan budget; licence {cands[0]["licence"]}'
    names = sorted(set(picks.values()))
    checked = {n: (registry_ok(n) if check_registry else None) for n in names}
    pullable = [n for n in names if checked[n] is True]          # fail closed: unverified is never pulled
    gb = lambda n: next(e["gb"] for e in cat if e["name"] == n)
    trimmed = []
    while pullable and sum(map(gb, pullable)) > hw["disk_free_gb"] - disk_reserve_gb:
        big = max(pullable, key=gb); pullable.remove(big); trimmed.append(big)
    return {"memory_budget_gb": budget, "by_role": picks, "why": why, "pull": pullable,
            "unverified": [n for n in names if checked[n] is not True], "download_gb": round(sum(map(gb, pullable)), 1),
            "registry_checked": checked, "dropped_for_disk": trimmed}


def tenant_plan(tenant, vertical, mode, uncensored, ollama_url, models_dir, allow, hw):
    from tempo_home import DEFAULTS
    v = VERTICALS[vertical]
    t = {"tenant": tenant, "vertical": vertical, "deployment": mode, "uncensored": uncensored,
         "allowed_providers": sorted(allow),
         "models_dir": models_dir or "models/decision", "ollama_url": ollama_url,
         "image_models_dir": None, "video_models_dir": None,
         "never_explore": sorted(set(DEFAULTS["never_explore"] + v["never_explore_add"])),
         "local_only": sorted(set(DEFAULTS["local_only"] + v["local_only_add"])),
         "scope": v.get("scope", "General work.")}
    cfg = {"mode": "shadow", "N0": 8, "min_judged_live": 20, "epsilon": 0.10, "low_stakes_classes": v["low_stakes"],
           "task_classes": v["task_classes"], "use_sev_job_risk": True, "verdict_sources": v["verdicts"]}
    att, refused = [], []
    if mode in ("hybrid", "frontier"):
        ex = json.loads((CODE / "attached.example.json").read_text(encoding="utf-8-sig"))
        for e in ex:
            kp = e.get("key_provider", e["provider"])
            if kp in allow and str(e.get("model", "")).startswith("<"):
                refused.append(f'{e["id"]}: set a real model id in attached.json first (example holds a placeholder)')
            elif kp in allow and (kp == "none" or hw["provider_keys_present"].get(kp)):
                e = dict(e); att.append(e)
            elif kp in allow:
                refused.append(f"{kp}: allowed but no key in the environment")
        if mode == "frontier":                               # hosted thinking lanes first in the attached order
            att.sort(key=lambda e: "thinking" not in e.get("roles", []))
    return t, cfg, att, refused


def _atomic_write(path, obj):
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=1), encoding="utf-8")
    tmp.replace(path)


UNCENSORED_MARK = ("abliterat", "uncensored", "heretic", "heresy", "dolphin")


def qualify(home, ollama_url, wanted, lease_script, env, uncensored=False, lease_need_mib=4096,
            device="gpu", cpu_booking=None, cpu_until=0, only_engine=None, *, resident_policy=None, witness_provider=None):
    """Real round-trip under a GPU lease or an explicit CPU-only booking; no silent device fallback."""
    eng = json.loads((home / "engines.json").read_text(encoding="utf-8")).get("engines", [])
    from tempo_standalone import is_standalone,StandaloneGuard
    try:standalone=is_standalone(home,lease_script,cpu_booking,cpu_until)
    except RuntimeError as exc:return {'qualified':None,'tried':[],'reason':str(exc)}
    if resident_policy is not None or witness_provider is not None:
        if standalone or device!='cpu' or type(resident_policy) is not dict or not callable(witness_provider):
            return {'qualified':None,'tried':[],'reason':'owner resident backend requires fleet CPU and a trusted witness provider'}
    if device=='auto' and not standalone:device='cpu' if cpu_booking else 'gpu' 
    if device == "cpu":
        from tempo_cpu import require_loopback
        require_loopback(ollama_url)
    local = [e for e in eng if e.get("provider") == "ollama" and "embedding" not in e.get("roles", [])
             and (uncensored or not any(m in e["model"].lower() for m in UNCENSORED_MARK))]
    def size_b(e):
        try:
            return float(str(e.get("size") or "99B").rstrip("BM"))
        except ValueError:
            return 99.0
    first = [e for e in local if e["model"].removesuffix(":latest") in wanted or e["id"] in wanted]
    order = first + sorted([e for e in local if e not in first], key=size_b)
    if only_engine:
        order = [e for e in order if e["id"] == only_engine]
    tried = []
    if device == "cpu" and not only_engine and not standalone:
        return {"qualified": None, "tried": [], "reason": "CPU qualification requires one explicit engine"}
    if not standalone and device == "gpu" and (not lease_script or not Path(lease_script).is_file()):
        return {"qualified": None, "tried": [], "reason": "shared GPU lease script is required"}
    from tempo_leases import gpu_lease
    for e in order[:3]:
        try:
            from tempo_cpu import cpu_window
            guard = (StandaloneGuard(e['model'],ollama_url,device,180,e.get('revision')) if standalone else
                     cpu_window(cpu_booking, cpu_until, 180) if device == "cpu" else
                     gpu_lease(lease_script, "tempo-setup-qualify", int(lease_need_mib), ttl_min=15, wait_min=0))
            with guard as admitted:
                effective=admitted.device if standalone else device
                from tempo_local_http import open_local
                tags = json.load(open_local(ollama_url.rstrip('/') + '/api/tags', timeout=10))
                live = next((m for m in tags.get('models', []) if m.get('name') == e['model']), None)
                if not live or live.get('digest') != e.get('revision'):
                    raise ValueError('installed engine digest differs from catalogue')
                from tempo_runtime import request,policy,visible_answer
                extra={}
                if resident_policy is not None:
                    from tempo_runtime import begin_resident_request
                    witness=begin_resident_request(resident_policy,witness_provider,e['model'],e.get('family'),
                        e.get('revision'),cpu_booking,cpu_until,180)
                    extra=dict(resident_policy=resident_policy,backend_witness=witness)
                endpoint,payload=request(e['model'],[{'role':'user','content':'Reply with the single word OK.'}],.2,512,
                                         cpu=effective=='cpu',family=e.get('family'),stream=False,
                                         release_after=standalone,**extra)
                if standalone:
                    payload['options']['num_ctx']=admitted.context_tokens
                    admitted.dispatched()
                body=json.dumps(payload).encode()
                t0 = time.perf_counter()
                deadline=time.monotonic()+180 if resident_policy is not None else None
                from tempo_local_http import open_local
                stream = open_local(
                    urllib.request.Request(ollama_url.rstrip("/") + endpoint, body,
                                           {"Content-Type": "application/json"}), timeout=180 if effective == "cpu" or standalone else 600)
                try:
                    if resident_policy is not None:
                        from tempo_runtime import read_resident_json
                        response=read_resident_json(stream,deadline)
                    else:response=json.load(stream)
                finally:stream.close()
                if resident_policy is not None:
                    from tempo_runtime import confirm_resident_backend
                    if response.get('done') is not True:raise RuntimeError('incomplete resident qualification response')
                    confirm_resident_backend(resident_policy,witness,witness_provider(),e['model'],e.get('family'))
                    if time.monotonic()>=deadline:raise TimeoutError('resident qualification witness exceeded request deadline')
                if standalone:admitted.check()
                secs = time.perf_counter() - t0
            gen_s = (response.get("eval_duration") or 0) / 1e9
            answer=visible_answer(response.get('response') or response.get('message',{}).get('content') or '',e['model'],e.get('family'),require_envelope=policy(e['model'],e.get('family'))['wire']=='qwen3-empty-think')
            res = {"engine": e["id"], "revision": e.get("revision"),
                   **({'processing_host_id':resident_policy['host_id'],'processing_guest_id':resident_policy['guest_id'],
                       'owner_resident_backend':True} if resident_policy is not None else {}),
                   "execution_device": effective, "cpu_booking": cpu_booking if effective == "cpu" else None,
                   "standalone_guard":admitted.receipt if standalone else None,
                   "ok": answer.rstrip('.').upper()=='OK' and response.get('done_reason')!='length',
                   "answer": answer[:40],
                   "runtime_policy":policy(e['model'],e.get('family')),
                   "thought": bool(response.get("thinking")), "wall_s": round(secs, 2),
                   "load_s": round((response.get("load_duration") or 0) / 1e9, 2),
                   "prompt_tokens": response.get("prompt_eval_count"),
                   "gen_tokens": response.get("eval_count"),
                   "gen_tok_per_s": round(response["eval_count"] / gen_s, 1)
                   if gen_s and response.get("eval_count") else None}
        except Exception as ex:
            res = {"engine": e["id"], "ok": False, "reason": type(ex).__name__}
        tried.append(res)
        if res["ok"]:
            break
    return {"qualified": next((t["engine"] for t in tried if t["ok"]), None), "tried": tried}


def main(*, resident_policy=None, witness_provider=None):
    ap = argparse.ArgumentParser(description="Perennos Tempo first-run setup")
    ap.add_argument("--home", required=True, help="data folder for this owner or client")
    ap.add_argument("--tenant", default="default")
    ap.add_argument("--vertical", choices=sorted(VERTICALS), default="general")
    ap.add_argument("--mode", choices=["local", "hybrid", "frontier"], default="local")
    ap.add_argument("--allow-provider", default="", help="comma list of hosted providers this tenant may use")
    ap.add_argument("--uncensored", action="store_true", help="prefer abliterated engines where one fits")
    ap.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    ap.add_argument("--models-dir", help="where the decision-head files live (default: <home>/models/decision)")
    ap.add_argument("--check-registry", action="store_true", help="confirm each recommended engine exists upstream")
    ap.add_argument("--apply", action="store_true", help="carry out the plan (otherwise nothing is written)")
    ap.add_argument("--pull", action="store_true", help="with --apply: ollama pull the verified engines")
    ap.add_argument("--fetch-models", action="store_true", help="with --apply: download the pinned decision-head files")
    ap.add_argument("--lease-script", default=os.environ.get("TEMPO_GPU_LEASE_SCRIPT"),
                    help="shared GPU lease helper required around every inference")
    ap.add_argument("--lease-need-mib", type=int, default=int(os.environ.get("TEMPO_GPU_LEASE_NEED_MIB", "4096")), help="minimum free VRAM requested for each local inference")
    ap.add_argument("--offline", action="store_true", help="no network at all")
    ap.add_argument("--device", choices=["auto", "gpu", "cpu"], default="auto")
    ap.add_argument("--cpu-booking")
    ap.add_argument("--cpu-until", type=float, default=0)
    ap.add_argument("--qualify-engine", help="qualify exactly this registered engine")
    ap.add_argument("--reconfigure", action="store_true")
    ap.add_argument("--first-boot", action="store_true", help="do nothing if setup already completed here")
    a = ap.parse_args()
    if a.offline and (a.pull or a.fetch_models or a.check_registry):
        ap.error("--offline cannot be combined with --pull, --fetch-models or --check-registry")
    if (a.pull or a.fetch_models) and not a.apply:
        ap.error("--pull and --fetch-models need --apply")
    if a.lease_need_mib < 1:
        ap.error("--lease-need-mib must be positive")
    if a.pull:
        a.check_registry = True                               # only verified names are ever pulled

    home = Path(a.home).resolve()
    marker = home / ".tempo-setup-complete"
    if a.first_boot and marker.exists():
        print(json.dumps({"status": "already configured", "home": str(home)})); return
    os.environ["TEMPO_HOME"] = str(home)
    sys.path.insert(0, str(CODE))
    allow = {x.strip() for x in a.allow_provider.split(",") if x.strip()}

    hw = inventory(home, a.ollama_url, a.offline)
    rec_engines = recommend(hw, a.uncensored, a.check_registry)
    t, cfg, att, refused = tenant_plan(a.tenant, a.vertical, a.mode, a.uncensored, a.ollama_url, a.models_dir, allow, hw)
    plan = {"kind": "tempo.setup.plan.v2", "home": str(home), "hardware": hw, "engines": rec_engines,
            "tenant": t, "config": cfg, "hosted_lanes": [e["id"] for e in att], "hosted_refused": refused,
            "will_write": [str(home / n) for n in ("tenant.json", "config.json", "attached.json", "profiles.json", "ledger/")],
            "will_pull": rec_engines["pull"] if a.pull else [], "will_fetch_decision_head": a.fetch_models,
            "priors": "neutral (no benchmark priors included)"}
    if not a.apply:
        print(json.dumps({"state": "plan", "writes": "none", **plan}, indent=1)); return

    home.mkdir(parents=True, exist_ok=True)
    rec = {"kind": "tempo.setup.v2", "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
           "args": vars(a), "plan": plan, "steps": {}}
    tf = home / "tenant.json"
    if tf.exists() and not a.reconfigure:
        rec["steps"]["tenant"] = "kept existing tenant.json (--reconfigure rewrites it; the ledger is never touched)"
    else:
        _atomic_write(tf, t); _atomic_write(home / "config.json", cfg); _atomic_write(home / "attached.json", att)
        rec["steps"]["tenant"] = "written"
    (home / "ledger").mkdir(exist_ok=True)
    if not (home / 'profiles.json').exists():
        from tempo_profiles import defaults
        _atomic_write(home / 'profiles.json', defaults())
    if not (home / "prior_map.json").exists():
        _atomic_write(home / "prior_map.json", {})             # neutral priors: no owner aliases, no auto UGI match
    if a.pull and hw["ollama"]["cli"]:
        rec["steps"]["pulled"] = {n: subprocess.run(["ollama", "pull", n], timeout=7200).returncode == 0
                                  for n in rec_engines["pull"]}

    from tempo_standalone import is_standalone
    standalone=is_standalone(home,a.lease_script,a.cpu_booking,a.cpu_until)
    env = dict(os.environ, TEMPO_HOME=str(home))
    if a.device == "cpu" or standalone:
        env.update(TEMPO_HEAD_DEVICE="cpu", TEMPO_HEAD_THREADS="2", CUDA_VISIBLE_DEVICES="", OMP_NUM_THREADS="2", MKL_NUM_THREADS="2")
    def step(script, *args, timeout=600):
        r = subprocess.run([sys.executable, "-I", str(CODE / script), *args], capture_output=True, text=True,
                           env=env, timeout=timeout)
        try:
            out = json.loads(r.stdout)
        except Exception:
            out = {"stdout": r.stdout[-800:], "stderr": r.stderr[-800:]}
        out = out if isinstance(out, dict) else {"result": out}
        out["returncode"] = r.returncode
        return out
    rec["steps"]["decision_head_files"] = step("fetch_models.py", timeout=7200) if a.fetch_models \
        else step("fetch_models.py", "--verify")
    rec["steps"]["engines"] = step("tempo_engines.py", "sync")
    rec["steps"]["priors"] = {"returncode": 0, "priors": "neutral"}
    _atomic_write(home / "priors.json", {"as_of": rec["started"], "priors": {}, "note": "neutral public-core default"})
    # the decision head must actually load and score, not only have its files (Codex hands-on test, 17 Oct:
    # a failed torch install still reported ready). selftest loads Sev on CPU and scores three probes.
    try:
        from tempo_leases import gpu_lease
        from tempo_cpu import cpu_window
        from tempo_standalone import decision_head_guard
        guard = (decision_head_guard(600) if standalone else
                 cpu_window(a.cpu_booking, a.cpu_until, 900) if a.device == "cpu" else
                 gpu_lease(a.lease_script, "tempo-setup-decision-head", a.lease_need_mib, ttl_min=15, wait_min=0))
        with guard:
            dh = step("tempo_decision_head.py", "selftest", timeout=900)
    except Exception as exc:
        dh = {"returncode": 1, "load_s": None, "stderr": type(exc).__name__}
    rec["steps"]["decision_head_loads"] = {"returncode": dh.get("returncode"), "load_s": dh.get("load_s"),
                                           "error": (dh.get("stderr") or "")[-300:] or None}
    st = step("tempo_ratings.py", "selftest")
    rec["steps"]["ratings_selftest"] = {"passed": st.get("passed"), "failed": st.get("failed")}
    q = (qualify(home, a.ollama_url, set(rec_engines["by_role"].values()), a.lease_script, env,
                 a.uncensored, a.lease_need_mib, a.device, a.cpu_booking, a.cpu_until, a.qualify_engine,
                 **({'resident_policy':resident_policy,'witness_provider':witness_provider}
                    if resident_policy is not None or witness_provider is not None else {}))
         if rec["steps"]["engines"].get("returncode") == 0 else
         {"qualified": None, "tried": [], "reason": "engine sync failed"})
    rec["steps"]["qualification"] = q
    gates = {"selftest": st.get("failed") == 0 and (st.get("passed") or 0) > 0,
             "decision_head_files": rec["steps"]["decision_head_files"].get("returncode") == 0,
             "decision_head_loads": dh.get("returncode") == 0 and dh.get("load_s") is not None,
             "engines_synced": rec["steps"]["engines"].get("returncode") == 0 and (rec["steps"]["engines"].get("engines") or 0) > 0,
             "priors": rec["steps"]["priors"].get("returncode") == 0,
             "qualified_local_engine": bool(q["qualified"])}
    rec["gates"] = gates
    rec["ready"] = all(gates.values())
    RECOVERY = {"decision_head_loads": "The risk model could not load (usually a broken PyTorch install). Delete the "
                                       "Python environment folder and run start-tempo again; on Windows keep it on a short path.",
                "decision_head_files": "Model files are missing or changed. Run setup again with --fetch-models.",
                "qualified_local_engine": "No local engine answered. Start Ollama and pull an engine (setup --pull).",
                "engines_synced": "Ollama was not reachable. Start Ollama and run setup again.",
                "selftest": "The router's own checks failed. Reinstall the package.", "priors": "Prior build failed. Run setup again."}
    rec["recovery"] = [RECOVERY[g] for g, ok in gates.items() if not ok]
    rec["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _atomic_write(home / "setup-receipt.json", rec)
    if rec["ready"]:
        marker.write_text(rec["finished"], encoding="utf-8")
    print(json.dumps({"state": "applied", "home": str(home), "ready": rec["ready"], "gates": gates, "recovery": rec["recovery"],
                      "recommended": rec_engines["by_role"], "unverified": rec_engines["unverified"],
                      "qualified": q["qualified"], "measure": next((x for x in q["tried"] if x.get("ok")), None),
                      "hosted_lanes": plan["hosted_lanes"], "receipt": str(home / "setup-receipt.json")}, indent=1))


if __name__ == "__main__":
    main()
