"""Perennos Tempo's private, local-first native desktop chat.

The module is imported lazily by ``tempo.py gui``. Tkinter is imported only
when a window is requested so headless and MCP users do not need Tk at import
time. Chat text is held in the window only; the gateway stores its own
content-free receipts.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import queue
import re
import subprocess
import sys
import threading
import time

from tempo_conversation import Conversation


CODE = Path(__file__).resolve().parent
PALETTE = {
    "paper": "#F3F0E8",
    "surface": "#FFFEFA",
    "ink": "#172522",
    "muted": "#626D68",
    "rule": "#D6D9CE",
    "control_rule": "#7D8B83",
    "lime": "#C6F35B",
    "lime_ink": "#1D2A20",
    "lime_pressed": "#B5E94B",
    "error": "#8B3429",
}
IDLE_STATUS = "Conversation kept in this window only"


def _safe_metadata(headers: object) -> dict[str, str]:
    if not isinstance(headers, dict):
        return {}
    names = (
        "x-tempo-receipt-id",
        "x-tempo-completion-receipt-id",
        "x-tempo-job-id",
        "x-tempo-profile",
        "x-tempo-review-required",
    )
    patterns = {
        "x-tempo-receipt-id": re.compile(r"[0-9a-f]{32}"),
        "x-tempo-completion-receipt-id": re.compile(r"[0-9a-f]{32}"),
        "x-tempo-job-id": re.compile(r"chatcmpl-[0-9a-f]{32}"),
        "x-tempo-profile": re.compile(r"[a-z][a-z0-9_-]{0,31}"),
        "x-tempo-review-required": re.compile(r"(?:true|false)"),
    }
    result = {}
    for name in names:
        value = headers.get(name)
        if isinstance(value, str) and patterns[name].fullmatch(value):
            result[name.removeprefix("x-tempo-").replace("-", " ")] = value
    return result


def _error_notice(error_name: str, request_may_continue: bool) -> str:
    if request_may_continue:
        return ("Request outcome is unknown. The backend may continue; check receipts before retrying. "
                "No automatic replay was sent.\n")
    return f"Local request failed ({error_name}). Check the gateway status before sending again.\n"


class TempoDesktop:
    def __init__(self, root, home: Path, port: int, *, check_local_status: bool = True):
        import tkinter as tk
        from tkinter import ttk

        self.tk, self.ttk = tk, ttk
        self.root = root
        self.home = home.resolve()
        self.port = int(port)
        self.events: queue.Queue = queue.Queue()
        self.cancel_event: threading.Event | None = None
        self.connection_lock = threading.Lock()
        self.active_connection = None
        self.busy = False
        self.busy_started = None
        self.conversation = Conversation()
        self.pending_prompt = None
        self.last_answer = ""
        self.status_checking = False
        self.client = None
        self.token = None

        try:
            import tempo_client
            self.client = tempo_client
            self.token = tempo_client.gateway_token(self.home)
        except Exception:
            self.client = None
            self.token = None

        self.root.title("Perennos Tempo")
        self.root.configure(background=PALETTE["paper"])
        self.root.geometry("1024x740")
        self.root.minsize(760, 560)
        self.root.protocol("WM_DELETE_WINDOW", self.close_window)
        self._style(ttk)
        self.status = tk.StringVar(value=IDLE_STATUS)
        self._layout(tk, ttk)
        self.root.bind("<Control-Return>", self._send_shortcut)
        self.root.bind("<Control-KP_Enter>", self._send_shortcut)
        self.root.bind("<Escape>", self._cancel_shortcut)
        self.root.after(100, self._poll)
        if check_local_status:
            self.check_status()

    def _style(self, ttk):
        style = ttk.Style(self.root)
        try:
            style.theme_use("clam")
        except Exception:
            pass
        style.configure("Tempo.TFrame", background=PALETTE["paper"])
        style.configure("Surface.TFrame", background=PALETTE["surface"])
        style.configure("Brand.TLabel", background=PALETTE["paper"], foreground=PALETTE["ink"],
                         font=("Segoe UI Variable Display", 14, "bold"))
        style.configure("Title.TLabel", background=PALETTE["paper"], foreground=PALETTE["ink"],
                         font=("Segoe UI Variable Display", 23, "bold"))
        style.configure("Body.TLabel", background=PALETTE["paper"], foreground=PALETTE["ink"],
                         font=("Segoe UI", 10))
        style.configure("Muted.TLabel", background=PALETTE["paper"], foreground=PALETTE["muted"],
                         font=("Segoe UI", 9))
        style.configure("SurfaceBody.TLabel", background=PALETTE["surface"], foreground=PALETTE["ink"],
                         font=("Segoe UI", 10))
        style.configure("Accent.TButton", background=PALETTE["lime"], foreground=PALETTE["lime_ink"],
                         padding=(16, 10), font=("Segoe UI", 10, "bold"))
        style.map("Accent.TButton", background=[("disabled", PALETTE["rule"]),
                                                  ("pressed", PALETTE["lime_pressed"])])
        style.configure("Plain.TButton", background=PALETTE["surface"], foreground=PALETTE["ink"],
                         padding=(11, 8), font=("Segoe UI", 9, "bold"))
        style.map("Plain.TButton", background=[("disabled", PALETTE["paper"]),
                                                 ("pressed", PALETTE["rule"])])

    def _layout(self, tk, ttk):
        outer = ttk.Frame(self.root, style="Tempo.TFrame", padding=(28, 22, 28, 20))
        outer.pack(fill="both", expand=True)

        top = ttk.Frame(outer, style="Tempo.TFrame")
        top.pack(fill="x")
        ttk.Label(top, text="Perennos Tempo", style="Brand.TLabel").pack(side="left", anchor="w")
        private = tk.Label(top, text="LOCAL  /  PRIVATE BY DEFAULT", bg=PALETTE["ink"], fg=PALETTE["paper"],
                           padx=10, pady=6, font=("Segoe UI", 8, "bold"))
        private.pack(side="left", padx=(16, 0))
        # Own row below the brand/badge so they never touch at the 760px minimum width.
        controls = ttk.Frame(outer, style="Tempo.TFrame")
        controls.pack(anchor="e", pady=(10, 0))
        self.new_chat_button = ttk.Button(controls, text="New chat", style="Plain.TButton",
                                          command=self.new_conversation)
        self.new_chat_button.pack(side="left", padx=(0, 8))
        ttk.Button(controls, text="New window", style="Plain.TButton", command=self.open_new_window).pack(
            side="left", padx=(0, 8))
        self.status_button = ttk.Button(controls, text="Check local status", style="Plain.TButton",
                                        command=self.check_status)
        self.status_button.pack(side="left", padx=(0, 8))
        self.copy_answer_button = ttk.Button(controls, text="Copy answer", style="Plain.TButton",
                                             command=self.copy_answer, state="disabled")
        self.copy_answer_button.pack(side="left", padx=(0, 8))
        self.cli_button = ttk.Button(controls, text="Copy CLI", style="Plain.TButton",
                                     command=self.copy_headless_command)
        self.cli_button.pack(side="left")

        title_row = ttk.Frame(outer, style="Tempo.TFrame")
        title_row.pack(fill="x", pady=(12, 14))
        ttk.Label(title_row, text="A private conversation.", style="Title.TLabel").pack(anchor="w")
        subtitle = ttk.Label(title_row, text="Recent turns stay in memory. Close or start a new chat to clear them.",
                             style="Muted.TLabel", justify="left")
        subtitle.pack(fill="x", anchor="w", pady=(5, 0))
        subtitle.bind("<Configure>", lambda event: subtitle.configure(wraplength=max(event.width, 100)))

        transcript_frame = ttk.Frame(outer, style="Surface.TFrame")
        self.transcript = tk.Text(transcript_frame, wrap="word", bg=PALETTE["surface"], fg=PALETTE["ink"],
                                  relief="flat", borderwidth=0, padx=22, pady=18,
                                  font=("Segoe UI", 11), state="disabled", takefocus=True,
                                  highlightthickness=1, highlightbackground=PALETTE["control_rule"],
                                  highlightcolor=PALETTE["ink"])
        self.transcript_scrollbar = ttk.Scrollbar(transcript_frame, orient="vertical", command=self.transcript.yview)
        self.transcript.configure(yscrollcommand=self.transcript_scrollbar.set)
        self.transcript.pack(side="left", fill="both", expand=True)
        self.transcript_scrollbar.pack(side="right", fill="y")
        self.transcript.tag_configure("intro", foreground=PALETTE["muted"], spacing1=4, spacing3=12)
        self.transcript.tag_configure("you", font=("Segoe UI", 9, "bold"), foreground=PALETTE["muted"],
                                      spacing1=12, spacing3=4)
        self.transcript.tag_configure("prompt", font=("Segoe UI", 11), foreground=PALETTE["ink"], spacing3=12)
        self.transcript.tag_configure("tempo", font=("Segoe UI", 9, "bold"), foreground=PALETTE["ink"],
                                      spacing1=10, spacing3=4)
        self.transcript.tag_configure("answer", font=("Segoe UI", 11), foreground=PALETTE["ink"],
                                      spacing3=8, lmargin1=12, lmargin2=12)
        self.transcript.tag_configure("receipt", font=("Consolas", 8), foreground=PALETTE["muted"],
                                      lmargin1=12, lmargin2=12, spacing3=10)
        self.transcript.tag_configure("error", foreground=PALETTE["error"], spacing1=8, spacing3=8)
        self._append_intro()

        message_label = ttk.Label(outer, text="MESSAGE", style="Muted.TLabel")
        compose = ttk.Frame(outer, style="Surface.TFrame", padding=(12, 10, 12, 10))
        self.entry = tk.Text(compose, height=4, wrap="word", bg=PALETTE["surface"], fg=PALETTE["ink"],
                             relief="flat", borderwidth=0, font=("Segoe UI", 11), undo=True,
                             highlightthickness=2, highlightbackground=PALETTE["control_rule"],
                             highlightcolor=PALETTE["ink"], padx=3, pady=3)
        actions = ttk.Frame(compose, style="Surface.TFrame")
        actions.pack(side="right", anchor="s")
        self.send_button = ttk.Button(actions, text="Send", style="Accent.TButton", command=self.send)
        self.send_button.pack(fill="x")
        self.cancel_button = ttk.Button(actions, text="Stop waiting", style="Plain.TButton", command=self.cancel,
                                        state="disabled")
        self.cancel_button.pack(fill="x", pady=(7, 0))
        self.entry.pack(side="left", fill="both", expand=True, padx=(0, 12))
        self.entry.bind("<Shift-Return>", self._newline_shortcut)
        self.entry.bind("<Control-Return>", self._send_shortcut)
        self.entry.bind("<Control-KP_Enter>", self._send_shortcut)
        self.entry.focus_set()

        bottom = ttk.Frame(outer, style="Tempo.TFrame")
        # Status gets its own wrapping row so long busy/cancel warnings are never truncated.
        status_label = ttk.Label(bottom, textvariable=self.status, style="Muted.TLabel", justify="left")
        status_label.pack(fill="x", anchor="w")
        status_label.bind("<Configure>", lambda event: status_label.configure(wraplength=max(event.width, 100)))
        ttk.Label(bottom, text="Close window to leave the local gateway running.", style="Muted.TLabel").pack(
            anchor="w", pady=(3, 0))
        # Reserve the bottom controls first. The transcript is packed last with
        # expand/fill, so it can take only the space left between title and composer.
        bottom.pack(side="bottom", fill="x", pady=(9, 0))
        compose.pack(side="bottom", fill="x")
        message_label.pack(side="bottom", anchor="w", pady=(15, 5))
        transcript_frame.pack(fill="both", expand=True)

    def _append(self, text: str, tag: str):
        self.transcript.configure(state="normal")
        self.transcript.insert("end", text, tag)
        self.transcript.configure(state="disabled")
        self.transcript.see("end")

    def _append_intro(self):
        self._append("Ask a question, then follow up. Recent completed turns are included automatically "
                     "(up to 12 turns and 12,000 UTF-8 bytes); older context drops first.\n\n"
                     "Default setup runs on this machine. An owner-approved private host may process requests. "
                     "The gateway keeps content-free receipts; review important answers.\n"
                     "Ctrl+Enter to send · Shift+Enter for a new line · Esc to stop waiting\n", "intro")

    def new_conversation(self):
        if self.busy:
            return
        self.conversation.clear()
        self.pending_prompt = None
        self.last_answer = ""
        self.copy_answer_button.configure(state="disabled")
        self.transcript.configure(state="normal")
        self.transcript.delete("1.0", "end")
        self.transcript.configure(state="disabled")
        self.entry.delete("1.0", "end")
        self.entry.edit_reset()
        self._append_intro()
        self.status.set("New chat · conversation cleared from this window")
        self.entry.focus_set()

    def copy_answer(self):
        if not self.last_answer:
            return
        try:
            self.root.clipboard_clear()
            self.root.clipboard_append(self.last_answer)
            self.status.set("Answer copied to your system clipboard.")
        except Exception:
            self.status.set("Clipboard unavailable. Select and copy the answer manually.")

    def _send_shortcut(self, _event=None):
        self.send()
        return "break"

    def _newline_shortcut(self, _event=None):
        self.entry.insert("insert", "\n")
        return "break"

    def _cancel_shortcut(self, _event=None):
        if self.busy:
            self.cancel()
            return "break"
        return None

    def send(self):
        if self.busy:
            return
        prompt = self.entry.get("1.0", "end-1c").strip()
        if not prompt:
            self.status.set("Write a message first.")
            self.entry.focus_set()
            return
        try:
            messages = self.conversation.messages(prompt)
        except ValueError as exc:
            self.status.set(str(exc))
            self.entry.focus_set()
            return
        if self.client is None or not self.token:
            self._append("The local gateway is not configured. Run the private setup again.\n", "error")
            self.status.set("Local setup needs attention.")
            return
        self.entry.delete("1.0", "end")
        self.pending_prompt = prompt
        self._append("YOU\n", "you")
        self._append(prompt + "\n", "prompt")
        self._set_busy(True, "Sending to the local gateway…")
        self.cancel_event = threading.Event()
        worker = threading.Thread(target=self._send_worker, args=(messages, self.cancel_event), daemon=True)
        worker.start()

    def _send_worker(self, messages: list[dict[str, str]], cancel_event: threading.Event):
        try:
            payload, headers = self.client.chat_messages(
                "127.0.0.1", self.port, self.token, messages,
                timeout=610, cancel_event=cancel_event, on_connection=self._set_connection)
            choices = payload.get("choices") if isinstance(payload, dict) else None
            message = choices[0].get("message") if isinstance(choices, list) and choices else None
            content = message.get("content") if isinstance(message, dict) else None
            if not isinstance(content, str) or not content.strip():
                raise ValueError("empty local response")
            content.encode("utf-8")  # reject malformed JSON Unicode before the UI-thread commit
            self.events.put(("success", content, _safe_metadata(headers)))
        except Exception as exc:
            cancelled = cancel_event.is_set() or type(exc).__name__ == "TempoCancelled"
            if cancelled:
                request_may_continue = bool(getattr(exc, "request_may_continue", False))
                self.events.put(("cancelled", request_may_continue, {}))
            else:
                request_may_continue = bool(getattr(exc, "request_may_continue", False))
                self.events.put(("error", type(exc).__name__,
                                 {"request_may_continue": request_may_continue}))
        finally:
            self._set_connection(None)

    def _set_connection(self, connection):
        with self.connection_lock:
            self.active_connection = connection
            if connection is None and self.cancel_event is not None and self.cancel_event.is_set():
                return
            if connection is not None and self.cancel_event is not None and self.cancel_event.is_set():
                try:
                    connection.close()
                except Exception:
                    pass

    def _set_busy(self, busy: bool, message: str):
        self.busy = busy
        self.busy_started = time.monotonic() if busy else None
        self.send_button.configure(state="disabled" if busy else "normal")
        self.cancel_button.configure(state="normal" if busy else "disabled")
        self.new_chat_button.configure(state="disabled" if busy else "normal")
        self.copy_answer_button.configure(state="normal" if self.last_answer and not busy else "disabled")
        self.status.set(message)

    def cancel(self):
        if not self.busy:
            return
        if self.cancel_event is not None:
            self.cancel_event.set()
        with self.connection_lock:
            connection = self.active_connection
        if connection is not None:
            try:
                connection.close()
            except Exception:
                pass
        self.cancel_button.configure(state="disabled")
        self.status.set("Stopped waiting. The backend may finish; check receipts before retrying.")

    def _poll(self):
        try:
            while True:
                kind, value, metadata = self.events.get_nowait()
                if kind == "status":
                    if not self.busy:
                        self.status.set(value)
                    continue
                if kind == "status_check_done":
                    self.status_checking = False
                    continue
                self._set_busy(False, IDLE_STATUS)
                if kind == "success":
                    if self.pending_prompt is not None:
                        self.conversation.commit(self.pending_prompt, value)
                    self.last_answer = value
                    self.copy_answer_button.configure(state="normal")
                    self._append("PERENNOS TEMPO\n", "tempo")
                    self._append(value + "\n", "answer")
                    if metadata:
                        rendered = "  ·  ".join(f"{key}: {item}" for key, item in metadata.items())
                        self._append("Receipt metadata · " + rendered + "\n", "receipt")
                elif kind == "cancelled":
                    if value:
                        self._append("Stopped waiting. The backend may finish; check the receipt before retrying. "
                                     "No automatic retry was sent.\n", "error")
                    else:
                        self._append("Request cancelled before send. No message was replayed.\n", "error")
                else:
                    request_may_continue = (isinstance(metadata, dict)
                                            and metadata.get("request_may_continue") is True)
                    self._append(_error_notice(value, request_may_continue), "error")
                self.pending_prompt = None
                self.cancel_event = None
        except queue.Empty:
            pass
        if (self.busy and self.busy_started is not None
                and not (self.cancel_event is not None and self.cancel_event.is_set())):
            elapsed = int(time.monotonic() - self.busy_started)
            self.status.set(f"Waiting for the local gateway · {elapsed}s · Esc stops waiting")
        self.root.after(100, self._poll)

    def _headless_command(self) -> str:
        python = Path(sys.executable)
        if python.name.lower() == "pythonw.exe":
            python = python.with_name("python.exe")
        command = [str(python), "-I", "-B", str(CODE / "tempo.py"), "status", "--home",
                   str(self.home), "--host", "127.0.0.1", "--port", str(self.port)]
        return subprocess.list2cmdline(command)

    def copy_headless_command(self):
        try:
            self.root.clipboard_clear()
            self.root.clipboard_append(self._headless_command())
            self.status.set("Headless status command copied. Closing this window leaves services running.")
        except Exception:
            self.status.set("Clipboard unavailable; use the app's local headless command from its install folder.")

    def check_status(self):
        if self.status_checking or self.busy:
            return
        if self.client is None or not self.token:
            self.status.set("Local gateway unavailable · setup needs attention")
            return
        self.status_checking = True
        self.status.set("Checking the local gateway…")
        threading.Thread(target=self._status_worker, daemon=True).start()

    def _status_worker(self):
        try:
            payload, _headers = self.client.gateway_status("127.0.0.1", self.port, self.token, timeout=5)
            service_status = payload.get("status") if isinstance(payload, dict) else None
            mode = payload.get("mode") if isinstance(payload, dict) else None
            safe_status = (service_status if isinstance(service_status, str)
                           and re.fullmatch(r"[a-zA-Z0-9 _.-]{1,32}", service_status) else "reachable")
            safe_mode = mode if isinstance(mode, str) and re.fullmatch(r"[a-zA-Z0-9 _.-]{1,32}", mode) else "unknown"
            self.events.put(("status", f"Gateway reachable · {safe_status} · {safe_mode}", {}))
        except Exception as exc:
            self.events.put(("status", f"Gateway unavailable ({type(exc).__name__})", {}))
        finally:
            self.events.put(("status_check_done", "", {}))

    def open_new_window(self):
        command = [sys.executable, "-I", "-B", str(CODE / "tempo.py"), "gui", "--home",
                   str(self.home), "--port", str(self.port)]
        try:
            flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
            subprocess.Popen(command, cwd=str(CODE), close_fds=True, creationflags=flags)
            self.status.set("Opened another desktop window. Local services were left running.")
        except OSError as exc:
            self.status.set(f"Could not open a new window ({type(exc).__name__}).")

    def close_window(self):
        # The desktop owns no gateway/server lifetime. Closing the window only
        # cancels an in-flight request and drops its in-memory conversation.
        if self.busy:
            from tkinter import messagebox
            if not messagebox.askyesno("Perennos Tempo", "Stop waiting and close this chat window?\n\n"
                                       "A request already sent may finish in the local backend. Check its receipt before retrying.",
                                       parent=self.root):
                return
            self.cancel()
        self.root.destroy()

    def smoke_snapshot(self) -> dict:
        """Update this window once and report its current, unmodified geometry."""
        self.root.update_idletasks()
        self.root.update()
        widgets = {
            "transcript": (self.transcript.winfo_width(), self.transcript.winfo_height()),
            "message": (self.entry.winfo_width(), self.entry.winfo_height()),
            "send": (self.send_button.winfo_width(), self.send_button.winfo_height()),
            "cancel": (self.cancel_button.winfo_width(), self.cancel_button.winfo_height()),
            "new_chat": (self.new_chat_button.winfo_width(), self.new_chat_button.winfo_height()),
            "copy_answer": (self.copy_answer_button.winfo_width(), self.copy_answer_button.winfo_height()),
        }
        required = {
            "send": self.send_button.winfo_reqwidth(),
            "cancel": self.cancel_button.winfo_reqwidth(),
            "new_chat": self.new_chat_button.winfo_reqwidth(),
            "copy_answer": self.copy_answer_button.winfo_reqwidth(),
        }
        if (widgets["transcript"][1] <= 20 or widgets["message"][0] <= 100
                or widgets["message"][1] <= 30
                or any(widgets[name][0] < required[name] or widgets[name][1] <= 15
                       for name in required)):
            raise RuntimeError("Perennos Tempo composer controls are clipped at the current window size.")
        return {
            "title": self.root.title(),
            "window": self.root.winfo_geometry(),
            "widgets": {
                "transcript": self.transcript.winfo_geometry(),
                "transcript_scrollbar": self.transcript_scrollbar.winfo_geometry(),
                "message": self.entry.winfo_geometry(),
                "send": self.send_button.winfo_geometry(),
                "cancel": self.cancel_button.winfo_geometry(),
                "status": self.status_button.winfo_geometry(),
                "copy_cli": self.cli_button.winfo_geometry(),
                "new_chat": self.new_chat_button.winfo_geometry(),
                "copy_answer": self.copy_answer_button.winfo_geometry(),
            },
            "required_button_widths": required,
            "send_enabled": str(self.send_button.cget("state")),
            "cancel_enabled": str(self.cancel_button.cget("state")),
        }


def main(home=None, port: int = 8765, argv=None):
    parser = argparse.ArgumentParser(description="Perennos Tempo private desktop chat")
    parser.add_argument("--home", default=str(home or (Path.home() / "PerennosTempo")))
    parser.add_argument("--port", type=int, default=int(port))
    parser.add_argument("--smoke", action="store_true", help="lay out this Tk window and print its geometry")
    # tempo.py has already consumed its own `gui --home --port` arguments.
    # Direct module invocation still parses its command line for `--smoke`.
    if argv is None and Path(sys.argv[0]).name.lower() == "tempo.py":
        argv = []
    args = parser.parse_args(argv)
    if not 1024 <= args.port <= 65535:
        parser.error("port must be between 1024 and 65535")
    import tkinter as tk
    root = tk.Tk()
    desk = TempoDesktop(root, Path(args.home), args.port, check_local_status=not args.smoke)
    if args.smoke:
        print(json.dumps(desk.smoke_snapshot(), sort_keys=True))
        desk.close_window()
        return 0
    root.mainloop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
