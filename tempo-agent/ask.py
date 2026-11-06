"""Tiny test/CLI client for the Tempo chat server: python ask.py [--conv ID] [--approve] "question" """
import json, sys, time, urllib.request
B = "http://127.0.0.1:8765/agent/"
def api(p, b=None):
    r = urllib.request.Request(B + p, data=None if b is None else json.dumps(b).encode(), headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(r, timeout=600).read())
a = sys.argv[1:]
conv = a[a.index("--conv") + 1] if "--conv" in a else api("new", {})["conv"]
approve = "--approve" in a
text = a[-1]
api("send", {"conv": conv, "text": text})
since, t0 = 0, time.time()
print("conv", conv)
while time.time() - t0 < 1500:
    r = api(f"events?conv={conv}&since={since}")
    for e in r["events"]:
        since += 1
        if e["type"] == "confirm":
            print("CONFIRM", e["what"], e["detail"][:120], "->", "approve" if approve else "deny")
            api("confirm", {"conv": conv, "id": e["id"], "ok": approve and e["what"] != "run command"})
        else:
            print(f"[{e['type']}]", (e.get("text") or e.get("name", "") + " " + e.get("args", ""))[:500])
    if not r["busy"] and since and r["events"] == [] and any(True for _ in [0]):
        break
    time.sleep(1.5)


