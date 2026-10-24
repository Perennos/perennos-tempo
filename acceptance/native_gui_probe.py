"""Exercise the installed native chat and its real loopback gateway on a fresh CI VM."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from windows_acceptance import Result, Fail, capture_window_png, u32, same_path, health


def main():
    p = argparse.ArgumentParser()
    for name in ('app', 'home', 'out-dir', 'result', 'expect-desktop-sha'):
        p.add_argument('--' + name, required=True)
    p.add_argument('--port', type=int, required=True)
    args = p.parse_args()
    app, out = Path(args.app).resolve(), Path(args.out_dir)
    res = Result(args.result, 'native-gui-probe')
    root = None
    try:
        sys.path.insert(0, str(app))
        import tkinter as tk
        import tempo_desktop
        res.require('installed_native_module', same_path(tempo_desktop.__file__, app / 'tempo_desktop.py')
                    and hashlib.sha256((app / 'tempo_desktop.py').read_bytes()).hexdigest() == args.expect_desktop_sha
                    and same_path(sys.prefix, app / 'runtime/venv'))
        root = tk.Tk()
        desk = tempo_desktop.TempoDesktop(root, Path(args.home), args.port)
        shots = []
        for width, height in ((1024, 740), (760, 560)):
            root.geometry(f'{width}x{height}')
            until = time.monotonic() + 1
            while time.monotonic() < until:
                root.update()
                time.sleep(.02)
            state = desk.smoke_snapshot()
            widgets = (desk.entry, desk.send_button, desk.cancel_button,
                       desk.new_chat_button, desk.copy_answer_button)
            visible = all(w.winfo_ismapped() and w.winfo_width() > 20 and w.winfo_height() > 15
                          and w.winfo_rootx() >= root.winfo_rootx()
                          and w.winfo_rooty() >= root.winfo_rooty()
                          and w.winfo_rootx() + w.winfo_width() <= root.winfo_rootx() + root.winfo_width()
                          and w.winfo_rooty() + w.winfo_height() <= root.winfo_rooty() + root.winfo_height()
                          for w in widgets)
            res.require(f'controls_visible_{width}', visible, geometry=state)
            res.require(f'buttons_fit_{width}', all(w.winfo_width() >= w.winfo_reqwidth()
                        for w in (desk.send_button, desk.cancel_button,
                                  desk.new_chat_button, desk.copy_answer_button)))
            hwnd = u32.GetAncestor(root.winfo_id(), 2)
            path = out / f'native-gui-{width}x{height}.png'
            capture_window_png(hwnd, path, expect_pid=os.getpid())
            shots.append(path.name)
        for index, prompt in enumerate((
                'Reply with one short greeting for this synthetic desktop test.',
                'Write one line of Python that prints the word hello.')):
            before = desk.transcript.index('end-1c')
            desk.entry.insert('1.0', prompt)
            desk.send_button.invoke()
            res.require(f'send_started_{index}', desk.busy)
            deadline = time.monotonic() + 180
            next_heartbeat = 0
            while desk.busy and time.monotonic() < deadline:
                root.update()
                if time.monotonic() >= next_heartbeat:
                    print(f'GUI request {index}: waiting for actual local answer', flush=True)
                    next_heartbeat = time.monotonic() + 30
                time.sleep(.05)
            text = desk.transcript.get(before, 'end-1c')
            answers = desk.transcript.tag_ranges('answer')
            receipts = desk.transcript.tag_ranges('receipt')
            res.require(f'actual_answer_{index}', not desk.busy and len(answers) >= (index + 1) * 2
                        and len(receipts) >= (index + 1) * 2 and 'Receipt metadata' in text
                        and re.search(r'\b(?:completion )?receipt id: [0-9a-f]{32}\b', text) is not None,
                        transcript_chars=len(text), answer_ranges=len(answers), receipt_ranges=len(receipts))
            path = out / f'native-gui-answer-{index}.png'
            capture_window_png(u32.GetAncestor(root.winfo_id(), 2), path, expect_pid=os.getpid())
            shots.append(path.name)
        desk.close_window()
        root = None
        res.require('headless_after_native_gui_close', health(args.port)['ok'])
        res.doc['screenshots'] = shots
    except Fail:
        pass
    except Exception as exc:
        res.doc['failure_reason'] = 'native GUI exception: ' + type(exc).__name__
    finally:
        if root is not None:
            root.destroy()
    return 0 if res.finish(('installed_native_module', 'controls_visible_1024', 'controls_visible_760',
                           'buttons_fit_1024', 'buttons_fit_760', 'send_started_0', 'send_started_1',
                           'actual_answer_0', 'actual_answer_1', 'headless_after_native_gui_close')) else 1


if __name__ == '__main__':
    sys.exit(main())
