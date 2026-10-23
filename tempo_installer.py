# Hallmark pre-emit critique: P5 H4 E4 S5 R4 V4.
"""Local-only Perennos Tempo setup console. No dependency or model downloads are initiated here."""
from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
from pathlib import Path

CODE = Path(__file__).resolve().parent
TOKENS = {
    "paper": "#F3F0E8",
    "panel": "#FFFEFA",
    "ink": "#172522",
    "muted": "#68736E",
    "line": "#D6D9CE",
    "accent": "#C6F35B",
    "accent_ink": "#1D2A20",
    "accent_active": "#B5E94B",
    "warning": "#9A4F2E",
    "output": "#F6F6F0",
}
FONTS = {
    "display": ("Segoe UI Variable Display", 24, "bold"),
    "body": ("Segoe UI", 10),
    "small": ("Segoe UI", 9),
    "strong": ("Segoe UI", 12, "bold"),
    "button": ("Segoe UI", 10, "bold"),
    "button_small": ("Segoe UI", 9, "bold"),
    "mono": ("Consolas", 9),
}
VERTICALS = ("general",)


def build_command(action, home, tenant="default", vertical="general", lease_script=None, lease_need_mib=4096, *, fetch_models=False):
    home = str(Path(home).expanduser().resolve())
    if vertical not in VERTICALS:
        raise ValueError("unsupported vertical")
    if action == "inspect":
        return [sys.executable, "-I", "-B", str(CODE / "tempo_setup.py"),
                "--home", home, "--tenant", tenant, "--vertical", vertical, "--offline"]
    if action == "apply":
        return [sys.executable, "-I", "-B", str(CODE / "tempo_setup.py"),
                "--home", home, "--tenant", tenant, "--vertical", vertical,
                *( ["--fetch-models"] if fetch_models else ["--offline"] ), "--apply",
                *(["--lease-script", str(Path(lease_script).expanduser().resolve()),
                   "--lease-need-mib", str(int(lease_need_mib))] if lease_script else
                  ["--device", "auto", "--qualify-engine", "qwen3:4b"])]
    if action == "audit":
        return [sys.executable, "-I", "-B", str(CODE / "tempo_audit.py"), "pack"]
    raise ValueError("unsupported setup action")


def run_json(command, timeout=900, home=None):
    data_home = home or (command[command.index("--home") + 1] if "--home" in command else None)
    env = dict(os.environ)
    if data_home:
        env["TEMPO_HOME"] = str(Path(data_home).expanduser().resolve())
    result = subprocess.run(command, cwd=CODE, capture_output=True, text=True, timeout=timeout, env=env)
    try:
        payload = json.loads(result.stdout)
    except (ValueError, TypeError):
        payload = {"message": "The setup tool returned non-JSON output.",
                   "stdout_tail": (result.stdout or "")[-1200:],
                   "stderr_tail": (result.stderr or "")[-1200:]}
    return {"returncode": result.returncode, "payload": payload}


class TempoInstaller:
    def __init__(self, root):
        import tkinter as tk
        from tkinter import ttk

        self.tk, self.ttk = tk, ttk
        self.root = root
        self.events = queue.Queue()
        self.busy = False
        self.root.title("Perennos Tempo | Local setup")
        self.root.configure(bg=TOKENS["paper"])
        self.root.geometry("980x700")
        self.root.minsize(760, 560)
        style = ttk.Style(root)
        style.theme_use("clam")
        style.configure("Tempo.TFrame", background=TOKENS["paper"])
        style.configure("Card.TFrame", background=TOKENS["panel"])
        style.configure("Eyebrow.TLabel", background=TOKENS["paper"], foreground=TOKENS["muted"],
                         font=(*FONTS["small"], "bold"))
        style.configure("Title.TLabel", background=TOKENS["paper"], foreground=TOKENS["ink"],
                         font=FONTS["display"])
        style.configure("Body.TLabel", background=TOKENS["paper"], foreground=TOKENS["ink"],
                         font=FONTS["body"])
        style.configure("Muted.TLabel", background=TOKENS["paper"], foreground=TOKENS["muted"],
                         font=FONTS["small"])
        style.configure("CardTitle.TLabel", background=TOKENS["panel"], foreground=TOKENS["ink"],
                         font=FONTS["strong"])
        style.configure("CardEyebrow.TLabel", background=TOKENS["panel"], foreground=TOKENS["muted"],
                         font=(*FONTS["small"], "bold"))
        style.configure("CardBody.TLabel", background=TOKENS["panel"], foreground=TOKENS["ink"],
                         font=FONTS["body"])
        style.configure("CardMuted.TLabel", background=TOKENS["panel"], foreground=TOKENS["muted"],
                         font=FONTS["small"])
        style.configure("Accent.TButton", background=TOKENS["accent"], foreground=TOKENS["accent_ink"],
                         padding=(16, 11), font=FONTS["button"])
        style.map("Accent.TButton", background=[("disabled", TOKENS["line"]),
                                                  ("active", TOKENS["accent_active"])])
        style.configure("Plain.TButton", background=TOKENS["panel"], foreground=TOKENS["ink"],
                         padding=(13, 10), font=FONTS["button_small"])
        style.configure("TEntry", padding=8)
        style.configure("TCombobox", padding=8)

        self.home = tk.StringVar(value=str(Path.home() / "PerennosTempo"))
        self.tenant = tk.StringVar(value="default")
        self.vertical = tk.StringVar(value="general")
        self.lease = tk.StringVar(value=os.environ.get("TEMPO_GPU_LEASE_SCRIPT", ""))
        self.need = tk.StringVar(value=os.environ.get("TEMPO_GPU_LEASE_NEED_MIB", "4096"))
        self.status = tk.StringVar(value="Local setup. Nothing has been changed.")
        self._layout(ttk)
        self.root.after(150, self._poll)

    def _layout(self, ttk):
        tk = self.tk
        outer = ttk.Frame(self.root, style="Tempo.TFrame", padding=(28, 22, 28, 20))
        outer.pack(fill="both", expand=True)
        ttk.Label(outer, text="Perennos Tempo     /     Local setup", style="Eyebrow.TLabel").pack(anchor="w")
        ttk.Label(outer, text="Make Perennos Tempo yours.", style="Title.TLabel").pack(anchor="w", pady=(9, 2))
        ttk.Label(outer, text="Whatever your tempo. Your data stays on this machine.",
                  style="Muted.TLabel").pack(anchor="w", pady=(0, 18))

        body = ttk.Frame(outer, style="Tempo.TFrame")
        body.pack(fill="both", expand=True)
        rail = ttk.Frame(body, style="Card.TFrame", padding=18)
        rail.pack(side="left", fill="y", padx=(0, 16))
        for number, title, detail in (
            ("01", "Place", "Choose the local data folder"),
            ("02", "Inspect", "Read the machine and make a plan"),
            ("03", "Prepare", "Use only installed local engines"),
            ("04", "Verify", "Keep an audit record")
        ):
            block = ttk.Frame(rail, style="Card.TFrame")
            block.pack(fill="x", pady=(2, 15))
            ttk.Label(block, text=number, style="CardEyebrow.TLabel").pack(anchor="w")
            ttk.Label(block, text=title, style="CardTitle.TLabel").pack(anchor="w", pady=(2, 1))
            ttk.Label(block, text=detail, style="CardMuted.TLabel", wraplength=155).pack(anchor="w")
        ttk.Label(rail, text="LOCAL MODE   /   NO CLOUD ROUTE", style="CardEyebrow.TLabel",
                  wraplength=170).pack(side="bottom", anchor="w", pady=(20, 0))

        card = ttk.Frame(body, style="Card.TFrame", padding=20)
        card.pack(side="left", fill="both", expand=True)
        ttk.Label(card, text="Install settings", style="CardTitle.TLabel").grid(row=0, column=0, sticky="w", columnspan=3)
        ttk.Label(card, text="Data folder", style="CardBody.TLabel").grid(row=1, column=0, sticky="w", pady=(18, 4))
        self._entry(card, self.home, 2)
        ttk.Button(card, text="Choose folder", style="Plain.TButton", command=self._choose_home).grid(
            row=2, column=2, sticky="e", padx=(8, 0))
        ttk.Label(card, text="Workspace name", style="CardBody.TLabel").grid(row=3, column=0, sticky="w", pady=(14, 4))
        ttk.Entry(card, textvariable=self.tenant).grid(row=4, column=0, sticky="ew", columnspan=3)
        ttk.Label(card, text="First workflow area", style="CardBody.TLabel").grid(row=5, column=0, sticky="w", pady=(14, 4))
        ttk.Combobox(card, textvariable=self.vertical, values=VERTICALS, state="readonly").grid(
            row=6, column=0, sticky="ew", columnspan=3)
        ttk.Label(card, text="Shared GPU lease helper", style="CardBody.TLabel").grid(row=7, column=0, sticky="w", pady=(14, 4))
        self._entry(card, self.lease, 8)
        ttk.Label(card, text="Requested free VRAM (MiB)", style="CardBody.TLabel").grid(
            row=9, column=0, sticky="w", pady=(14, 4))
        ttk.Entry(card, textvariable=self.need, width=12).grid(row=10, column=0, sticky="w")
        ttk.Label(card, text="This setup never downloads models or containers. It only checks local files and installed engines.",
                  style="CardMuted.TLabel", wraplength=560).grid(row=11, column=0, columnspan=3, sticky="w", pady=(15, 4))
        self.output = tk.Text(card, height=9, wrap="word", bg=TOKENS["output"], fg=TOKENS["ink"],
                              relief="flat", padx=12, pady=10, font=FONTS["mono"], state="disabled")
        self.output.grid(row=12, column=0, columnspan=3, sticky="nsew", pady=(12, 0))
        card.columnconfigure(0, weight=1)
        card.rowconfigure(12, weight=1)

        bar = ttk.Frame(outer, style="Tempo.TFrame")
        bar.pack(fill="x", pady=(14, 0))
        self.inspect_button = ttk.Button(bar, text="Inspect this machine", style="Plain.TButton",
                                         command=lambda: self._run("inspect"))
        self.inspect_button.pack(side="left")
        self.apply_button = ttk.Button(bar, text="Prepare local setup", style="Accent.TButton",
                                       command=lambda: self._run("apply"))
        self.apply_button.pack(side="left", padx=(8, 0))
        self.audit_button = ttk.Button(bar, text="Create audit pack", style="Plain.TButton",
                                       command=lambda: self._run("audit"))
        self.audit_button.pack(side="left", padx=(8, 0))
        ttk.Label(bar, textvariable=self.status, style="Muted.TLabel").pack(side="right")

    def _entry(self, parent, variable, row):
        self.ttk.Entry(parent, textvariable=variable).grid(row=row, column=0, sticky="ew", columnspan=2)

    def _choose_home(self):
        from tkinter import filedialog
        path = filedialog.askdirectory(title="Choose Perennos Tempo data folder", mustexist=False)
        if path:
            self.home.set(path)

    def _show(self, value):
        text = json.dumps(value, indent=2, ensure_ascii=False)
        self.output.configure(state="normal")
        self.output.delete("1.0", "end")
        self.output.insert("1.0", text)
        self.output.configure(state="disabled")

    def _run(self, action):
        from tkinter import messagebox
        if self.busy:
            return
        home = self.home.get()
        try:
            if action == "audit":
                command = build_command("audit", home)
            else:
                command = build_command(action, home, self.tenant.get().strip(),
                                        self.vertical.get(), self.lease.get().strip(), int(self.need.get()))
        except (ValueError, OSError) as exc:
            messagebox.showerror("Check the settings", str(exc))
            return
        if action == "apply" and not messagebox.askyesno(
                "Prepare local setup?", "This writes the workspace configuration and runs local checks. "
                "It does not pull models or containers. Continue?"):
            return
        self.busy = True
        self.status.set("Working locally...")
        for button in (self.inspect_button, self.apply_button, self.audit_button):
            button.configure(state="disabled")
        worker = threading.Thread(target=self._worker, args=(action, command, home), daemon=True)
        worker.start()

    def _worker(self, action, command, home):
        try:
            result = run_json(command, timeout=1800 if action == "apply" else 900, home=home)
            self.events.put((action, result))
        except Exception as exc:
            self.events.put((action, {"returncode": 1, "payload": {"error": type(exc).__name__}}))

    def _poll(self):
        try:
            action, result = self.events.get_nowait()
        except queue.Empty:
            self.root.after(150, self._poll)
            return
        self.busy = False
        for button in (self.inspect_button, self.apply_button, self.audit_button):
            button.configure(state="normal")
        payload = result["payload"]
        if action == "audit":
            self.status.set("Audit pack created." if result["returncode"] == 0 else "Audit pack failed.")
        elif action == "apply":
            setup = payload.get("setup") if isinstance(payload, dict) else None
            self.status.set("Ready." if result["returncode"] == 0 and (payload.get("ready") or
                              (setup and setup.get("ready"))) else "Setup needs attention.")
        else:
            self.status.set("Plan ready." if result["returncode"] == 0 else "Inspection needs attention.")
        self._show({"returncode": result["returncode"], "result": payload})
        self.root.after(150, self._poll)


def main():
    parser = argparse.ArgumentParser(description="Perennos Tempo local setup console")
    parser.add_argument("--smoke", action="store_true", help="check standard-library GUI availability without opening a window")
    args = parser.parse_args()
    if args.smoke:
        try:
            import tkinter
            print(json.dumps({"status": "available", "tk": tkinter.TkVersion}))
            return 0
        except Exception as exc:
            print(json.dumps({"status": "unavailable", "error": type(exc).__name__}))
            return 2
    import tkinter as tk
    root = tk.Tk()
    TempoInstaller(root)
    root.mainloop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
