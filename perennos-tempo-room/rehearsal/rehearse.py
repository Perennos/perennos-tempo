"""Room rehearsal: the real tempo-room and tempo-local-agent binaries, a mock Ollama, an owner,
three guests and two teammates (an engine and an agent). Prints one line per check and writes results.json.

Usage: python3 -I rehearse.py BIN_DIR RUN_DIR
  BIN_DIR holds release builds of tempo-room and tempo-local-agent; RUN_DIR is a new, empty folder
  (its Tempo home, logs and results). Uses 127.0.0.1 ports 18970-18972. Takes about two minutes."""
import http.client, json, os, re, signal, socket, subprocess, sys, threading, time

BIN, RUN = sys.argv[1], sys.argv[2]
HERE = os.path.dirname(os.path.abspath(__file__))
MOCK, AGENT, ROOM = 18970, 18971, 18972
HOME = os.path.join(RUN, "home")
os.makedirs(os.path.join(HOME, "agent"), exist_ok=True)
os.makedirs(os.path.join(HOME, "room"), exist_ok=True)
with open(os.path.join(HOME, "room", "config.json"), "w") as f:
    # where engine teammates find (the mock) Ollama; ask limits raised so the many questions of one
    # run are not told to wait (the limits themselves are covered by the unit tests)
    json.dump({"ollama_url": f"http://127.0.0.1:{MOCK}", "limits": {"ask_per_member": 60, "ask_per_room": 200}}, f)
with open(os.path.join(HOME, "agent", "config.json"), "w") as f:
    json.dump({"models": ["mock:4b"], "ollama_url": f"http://127.0.0.1:{MOCK}"}, f)
ENV = {k: v for k, v in os.environ.items()
       if k not in ("TEMPO_AGENT_CONFIG", "TEMPO_AGENT_DATA", "TEMPO_ROOM_DATA", "TEMPO_AGENT_KEY", "TEMPO_VAULT")}
ENV["TEMPO_HOME"] = HOME
PWNED = os.path.join(RUN, "PWNED")  # the mock's tool call would write it; it must never appear
results, procs = [], {}


def check(name, ok, detail=""):
    results.append({"check": name, "ok": bool(ok), "detail": str(detail)[:300]})
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail and not ok else ""), flush=True)
    return ok


def start(name, args, log):
    p = subprocess.Popen(args, env=ENV, stdout=open(log + ".out", "w"), stderr=open(log + ".err", "w"))
    procs[name] = p
    return p


def stop(name):
    p = procs.pop(name, None)
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)  # by PID only
        try:
            p.wait(5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait(5)


def wait_http(port, path, secs=15):
    t0 = time.time()
    while time.time() - t0 < secs:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            c.request("GET", path, headers={"Host": f"127.0.0.1:{port}"})
            r = c.getresponse()
            body = r.read()
            if r.status == 200:
                return json.loads(body or b"{}")
        except OSError:
            pass
        time.sleep(0.2)
    return None


def req(method, path, cookie=None, body=None, headers=None, port=ROOM):
    h = {"Host": f"127.0.0.1:{port}"}
    if cookie:
        h["Cookie"] = cookie
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json"
    h.update(headers or {})
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    c.request(method, path, body=data, headers=h)
    r = c.getresponse()
    raw = r.read()
    try:
        js = json.loads(raw or b"{}")
    except ValueError:
        js = {"raw": raw[:200].decode("utf-8", "replace")}
    return r.status, r.msg.get_all("Set-Cookie") or [], js


def cookie_of(set_cookies, name):
    for s in set_cookies:
        if s.startswith(name + "="):
            return s.split(";", 1)[0]
    return None


def messages(cookie, room="family"):
    st, _, js = req("GET", f"/room/messages?room={room}", cookie)
    return js.get("messages", []) if st == 200 else []


def ask(cookie, text, room="family", secs=30):
    st, _, js = req("POST", "/room/post", cookie, {"room": room, "text": text})
    if st != 200:
        return st, None, 0
    qid = js["msg"]["id"]
    t0 = time.time()
    while time.time() - t0 < secs:
        for m in messages(cookie, room):
            if m.get("kind") == "tempo" and m.get("reply_to") == qid:
                return st, m, time.time() - t0
        time.sleep(0.2)
    return st, None, time.time() - t0


def ask_team(cookie, text, handle, room="family", secs=40):
    """Post `text`; wait for the teammate's reply to it. Returns (status, reply or None, post id)."""
    st, _, js = req("POST", "/room/post", cookie, {"room": room, "text": text})
    if st != 200:
        return st, None, None
    qid = js["msg"]["id"]
    t0 = time.time()
    while time.time() - t0 < secs:
        for m in messages(cookie, room):
            if m.get("kind") == "teammate" and m.get("reply_to") == qid and m.get("from") == "team:" + handle:
                return st, m, qid
        time.sleep(0.2)
    return st, None, qid


def room_cli(*args):
    """The real tempo-room command line against the running server (its control key)."""
    p = subprocess.run([os.path.join(BIN, "tempo-room"), *args, "--data", os.path.join(HOME, "room")],
                       env=ENV, capture_output=True, text=True, timeout=60)
    return p.returncode, p.stdout + p.stderr


def engine_chats():
    """The chats an engine teammate (Sage) made with the mock: its system message says so."""
    out = []
    for b in mock_chats():
        sysmsg = " ".join(x.get("content", "") for x in b.get("messages", []) if x.get("role") == "system")
        if sysmsg.startswith("You are Sage."):
            out.append(b)
    return out


class Stream(threading.Thread):
    """A live /room/events stream; keeps (time, event, data) and when the server closed it."""

    def __init__(self, cookie):
        super().__init__(daemon=True)
        self.cookie, self.events, self.closed_at, self.status = cookie, [], None, None

    def run(self):
        s = socket.create_connection(("127.0.0.1", ROOM), timeout=60)
        s.sendall(f"GET /room/events HTTP/1.1\r\nHost: 127.0.0.1:{ROOM}\r\nCookie: {self.cookie}\r\n\r\n".encode())
        buf = b""
        try:
            while True:
                b = s.recv(65536)
                if not b:
                    break
                buf += b
                if self.status is None and b"\r\n" in buf:
                    self.status = int(buf.split(b" ", 2)[1])
                while b"\n\n" in buf:
                    block, buf = buf.split(b"\n\n", 1)
                    ev = re.search(rb"^event: (.*)$", block, re.M)
                    da = re.search(rb"^data: (.*)$", block, re.M)
                    if ev:
                        self.events.append((time.time(), ev.group(1).decode(), (da.group(1) if da else b"").decode()))
        except OSError:
            pass
        self.closed_at = time.time()

    def saw(self, needle, kind=None):
        return any(needle in d and (kind is None or k == kind) for _, k, d in self.events)


def mock_chats():
    out = []
    try:
        for line in open(os.path.join(RUN, "mock.jsonl")):
            r = json.loads(line)
            if r["path"].startswith("/api/chat"):
                out.append(r["body"])
    except FileNotFoundError:
        pass
    return out


def room_args():
    return [os.path.join(BIN, "tempo-room"), "serve", "--port", str(ROOM), "--agent", f"http://127.0.0.1:{AGENT}"]


try:
    if os.path.exists(PWNED):
        os.rename(PWNED, PWNED + f".old-{int(time.time())}")
    start("mock", [sys.executable, "-I", os.path.join(HERE, "mock_chat.py"), str(MOCK), os.path.join(RUN, "mock.jsonl"), PWNED], os.path.join(RUN, "mock"))
    check("mock Ollama answers", wait_http(MOCK, "/api/version") is not None)
    start("agent", [os.path.join(BIN, "tempo-local-agent"), "serve", "--port", str(AGENT)], os.path.join(RUN, "agent"))
    ah = wait_http(AGENT, "/agent/health")
    check("agent is up with a key and guest mode", ah and ah.get("auth") == "key" and ah.get("guest") is True, ah)
    start("room", room_args(), os.path.join(RUN, "room"))
    rh = wait_http(ROOM, "/room/health")
    check("room is up and sees the agent", rh and rh.get("app") == "tempo-room" and rh.get("agent") == "up", rh)

    # --- the owner signs in with the one-time link printed at the first start
    time.sleep(0.3)
    err = open(os.path.join(RUN, "room.err")).read()
    m = re.search(r"#owner=([A-Za-z0-9_-]+)", err)
    check("serve printed a one-time owner link", m is not None)
    st, sc, _ = req("POST", "/room/owner", None, {"token": m.group(1)})
    owner = cookie_of(sc, "tr_owner")
    check("the owner link signs the owner in", st == 200 and owner)
    st, _, _ = req("POST", "/room/owner", None, {"token": m.group(1)})
    check("the owner link works only once", st != 200, st)
    check("the owner is the owner", req("GET", "/room/me", owner)[2].get("role") == "owner")
    st, _, js = req("POST", "/room/create", owner, {"name": "Family"})
    check("the owner makes a room", st == 200 and js.get("room", {}).get("id") == "family", js)
    st, _, js = req("POST", "/room/create", owner, {"name": "Book Club"})
    check("the owner makes a second room", st == 200 and js.get("room", {}).get("id") == "book-club", js)

    # --- three guests join #family; Lee also joins #book-club
    def invite(room):
        st, _, js = req("POST", "/room/invite", owner, {"room": room})
        if st != 200:
            print("     invite failed:", st, js)
        return js.get("code") if st == 200 else None

    people = {}
    for name in ["Mia", "Sam", "Lee"]:
        st, sc, js = req("POST", "/room/join", None, {"code": invite("family"), "name": name})
        people[name] = cookie_of(sc, "tr_member")
        check(f"{name} joins with a one-time code", st == 200 and people[name], js)
    st, sc, js = req("POST", "/room/join", people["Lee"], {"code": invite("book-club"), "name": "Lee"})
    lee_rooms = [r["id"] for r in req("GET", "/room/rooms", people["Lee"])[2].get("rooms", [])]
    check("Lee keeps one name across two rooms", st == 200 and sorted(lee_rooms) == ["book-club", "family"], (st, js, lee_rooms))
    mia_rooms = [r["id"] for r in req("GET", "/room/rooms", people["Mia"])[2].get("rooms", [])]
    check("Mia sees only her room", mia_rooms == ["family"], mia_rooms)
    check("a member cannot read another room", req("GET", "/room/messages?room=book-club", people["Mia"])[0] == 403)

    # --- Mia's second device: a one-time sign-in link from the owner, then a real sign-out
    mia_id = req("GET", "/room/me", people["Mia"])[2].get("id")
    st, _, js = req("POST", "/room/invite", people["Sam"], {"member": mia_id})
    check("a member cannot make a sign-in link", st == 403, st)
    st, _, js = req("POST", "/room/invite", owner, {"member": mia_id})
    link_ok = st == 200 and "#signin=" in js.get("link", "") and re.fullmatch(r"[0-9A-Z]{5}-[0-9A-Z]{5}", js.get("code", ""))
    check("the owner makes a sign-in link and code for Mia", link_ok, (st, {k: v for k, v in js.items() if k not in ("code", "link")}))
    dev_code = js.get("code")
    st, sc, js = req("POST", "/room/join", None, {"code": dev_code})
    mia2 = cookie_of(sc, "tr_member")
    check("Mia's second device signs in through the link (no name asked)", st == 200 and mia2 and mia2 != people["Mia"] and js.get("device") is True, (st, js))
    me2 = req("GET", "/room/me", mia2)[2]
    rooms2 = [r["id"] for r in req("GET", "/room/rooms", mia2)[2].get("rooms", [])]
    check("device 2 is Mia and sees the same rooms", me2.get("role") == "member" and me2.get("id") == mia_id and me2.get("name") == "Mia" and rooms2 == mia_rooms, (me2.get("role"), rooms2))
    check("the sign-in link works only once", req("POST", "/room/join", None, {"code": dev_code})[0] == 410)
    check("device 1 still works beside device 2", req("GET", "/room/me", people["Mia"])[2].get("role") == "member")
    st, sc, _ = req("POST", "/room/logout", people["Mia"], {})
    check("Mia signs out on device 1", st == 200 and "Max-Age=0" in " ".join(s for s in sc if s.startswith("tr_member=")), (st, sc))
    check("device 1 is now a guest, even with its old cookie", req("GET", "/room/me", people["Mia"])[2].get("role") == "guest" and req("GET", "/room/rooms", people["Mia"])[0] == 403)
    me2 = req("GET", "/room/me", mia2)[2]
    check("device 2 still works and Mia's record stays", me2.get("role") == "member" and me2.get("id") == mia_id and any(x.get("id") == mia_id for x in req("GET", "/room/members?room=family", owner)[2].get("members", [])))
    people["Mia"] = mia2  # Mia carries on from her second device

    # --- live streams and plain talk
    sam_live = Stream(people["Sam"]); sam_live.start()
    time.sleep(0.5)
    st, _, _ = req("POST", "/room/post", people["Mia"], {"room": "family", "text": "hi all, dinner at 7?"})
    t0 = time.time()
    while not sam_live.saw("dinner at 7") and time.time() - t0 < 5:
        time.sleep(0.1)
    check("a post reaches the others live", st == 200 and sam_live.saw("dinner at 7"), sam_live.events[-3:])

    # --- @tempo through the real agent, in guest mode
    st, a, secs = ask(people["Mia"], "@tempo what is the capital of France?")
    check("@tempo answers in the thread", a and a["text"].startswith("Mock answer to:"), (st, a))
    if a:
        print(f"     (answer in {secs:.1f} s)")
    chats = mock_chats()
    last = chats[-1] if chats else {}
    users = [x for x in last.get("messages", []) if x.get("role") == "user"]
    check("the agent offered the model no tools (guest)", chats and not last.get("tools"), list(last.keys()))
    check("the request starts with the room line", users and users[-1]["content"].startswith("[Perennos Tempo room #family] Mia: @tempo what is the capital of France?"),
          users[-1]["content"][:120] if users else None)
    sys_prompt = " ".join(x.get("content", "") for x in last.get("messages", []) if x.get("role") == "system")
    check("the guest prompt carries the other-people rule", "not the owner" in sys_prompt.lower() or "other people" in sys_prompt.lower() or "other people" in (users[-1]["content"].lower() if users else ""),
          sys_prompt[:200])

    st, a, _ = ask(people["Sam"], "@tempo THINKTEST")
    check("hidden reasoning never reaches the room", a and a["text"].strip() == "Visible answer." , a and a["text"])
    st, a, _ = ask(people["Lee"], "@tempo LONGTEST")
    check("long answers are capped", a and len(a["text"]) <= 1300, a and len(a["text"]))
    st, a, _ = ask(people["Mia"], "@tempo KEYTEST")
    check("a key in an answer is redacted", a and "sk-AAAA" not in a["text"] and "REDACTED" in a["text"], a and a["text"])
    st, a, secs = ask(people["Sam"], "@tempo TOOLTEST", secs=120)
    check("a tool call from a guest question runs nothing", not os.path.exists(PWNED), "PWNED exists")
    check("and the room still gets a reply", a is not None, (st, secs))
    if a:
        print(f"     (tool-call question answered in {secs:.1f} s: {a['text'][:90]!r})")

    # --- two rooms at once, and "Tempo is thinking"
    out = {}

    def bg(who, room):
        out[who] = ask(people[who], "@tempo SLOWTEST", room=room, secs=60)

    t1 = threading.Thread(target=bg, args=("Mia", "family")); t2 = threading.Thread(target=bg, args=("Lee", "book-club"))
    t1.start(); t2.start()
    time.sleep(1.5)
    thinking = any(r.get("thinking") for r in req("GET", "/room/rooms", owner)[2].get("rooms", []))
    t1.join(); t2.join()
    check("two rooms are answered at the same time", out["Mia"][1] and out["Lee"][1], (out["Mia"][2], out["Lee"][2]))
    check("rooms show Tempo is thinking", thinking or sam_live.saw("", kind="thinking"))

    st, a, _ = ask(people["Mia"], "@tempo tldr")
    check("@tempo tldr answers", a is not None)
    st, a, _ = ask(people["Mia"], "@tempo remember the spare key is with Sam")
    rem = os.path.join(HOME, "room", "remembered", "family.md")
    check("@tempo remember keeps it (no vault: the room's own file)", os.path.exists(rem) and "spare key" in open(rem).read(), a and a["text"])


    # --- teammates: named AI members the owner sets up
    st, _, js = req("POST", "/room/team/add", people["Mia"], {"handle": "sage", "name": "Sage", "kind": "engine", "model": "mock:4b"})
    check("a member cannot add a teammate", st == 403, st)
    check("a member cannot see the installed models", req("GET", "/room/team/models", people["Mia"])[0] == 403)
    check("a member cannot list the team", req("GET", "/room/team", people["Mia"])[0] == 403)
    st, _, js = req("POST", "/room/team/add", owner, {"handle": "sage", "name": "Sage", "kind": "engine", "model": "mock:4b",
                                                      "role": "You are a calm cooking coach. Keep answers to one sentence.", "rooms": ["family"]})
    check("the owner adds the engine teammate sage (model mock:4b)", st == 200 and js.get("teammate", {}).get("handle") == "sage" and not js.get("note"), (st, js))
    st, _, js = req("GET", "/room/team/models", owner)
    check("the installed models come from Ollama, for the owner", st == 200 and js.get("ollama") == "up" and any(m.get("name") == "mock:4b" for m in js.get("models", [])), js)
    st, _, js = req("POST", "/room/team/add", owner, {"handle": "ghost", "name": "Ghost", "kind": "engine", "model": "absent:1b"})
    check("an engine model Ollama does not have is refused", st == 400 and "absent:1b" in js.get("error", ""), (st, js))
    st, _, js = req("POST", "/room/team/add", owner, {"handle": "tempo", "name": "Helper", "kind": "agent"})
    check("a teammate cannot take Tempo's name", st == 400, (st, js))
    rc, txt = room_cli("team", "add", "coder", "--kind", "agent", "--name", "Coder", "--role", "Review code. Be blunt.", "--room", "family")
    check("the command line adds the agent teammate coder through the running server", rc == 0 and "Added @coder" in txt, (rc, txt))
    rc, txt = room_cli("team", "list", "--json")
    listed = {t["handle"]: t for t in json.loads(txt).get("team", [])} if rc == 0 else {}
    check("team list shows both, with their rooms", sorted(listed) == ["coder", "sage"] and listed["sage"]["rooms"] == ["family"] and listed["coder"]["kind"] == "agent", (rc, txt[:200]))
    mem = req("GET", "/room/members?room=family", people["Mia"])[2]
    check("members see who is in the room, never a role", [t["handle"] for t in mem.get("team", [])] == ["sage", "coder"] and "Review code" not in json.dumps(mem) and "cooking coach" not in json.dumps(mem), mem.get("team"))
    st, _, js = req("POST", "/room/join", None, {"code": invite("family"), "name": "Sage"})
    check("nobody can join the room as a teammate", st == 400, (st, js))

    st, a, qid = ask_team(people["Mia"], "@sage which spice goes with rice?", "sage")
    check("Mia asks @sage and gets the engine's answer, marked as Sage", a is not None and a["name"] == "Sage" and a["text"].startswith("Sage here. Mock answer to:"), a)
    check("a teammate's reply is never Tempo's or the owner's", a is not None and a["kind"] == "teammate" and a["from"] == "team:sage" and a["from"] not in ("tempo", "owner"))
    chats = engine_chats()
    last = chats[-1] if chats else {}
    msgs = last.get("messages", [])
    check("the engine call is a direct chat: stream false, no tools", chats and last.get("stream") is False and "tools" not in last and "functions" not in last, list(last.keys()))
    check("its system message is the role plus the room rule", msgs and msgs[0]["role"] == "system" and msgs[0]["content"].startswith("You are Sage. You are a calm cooking coach.") and "Do not run commands" in msgs[0]["content"], msgs and msgs[0]["content"][:160])
    check("and the request is what Tempo gets: the room line", len(msgs) == 2 and msgs[1]["content"].startswith("[Perennos Tempo room #family] Mia: @sage which spice goes with rice?"), msgs and msgs[-1]["content"][:120])
    st, a, _ = ask_team(people["Sam"], "@Sage TOOLTEST", "sage", secs=60)
    check("a guest's @sage request that makes the model try a tool still runs nothing", a is not None and not os.path.exists(PWNED), a and a["text"])
    check("every engine call a guest's question made carried no tools", len(engine_chats()) >= 2 and all("tools" not in b and "functions" not in b for b in engine_chats()), [list(b.keys()) for b in engine_chats()])
    st, a, _ = ask_team(people["Lee"], "@sage KEYTEST", "sage")
    check("a key in a teammate's answer is redacted", a is not None and "sk-AAAA" not in a["text"] and "REDACTED" in a["text"], a and a["text"])
    st, a, _ = ask_team(people["Mia"], "@coder what does a mutex do?", "coder", secs=90)
    check("Mia asks @coder and the real agent answers, marked as Coder", a is not None and a["name"] == "Coder" and a["text"].startswith("Mock answer"), a)
    after = [x for x in messages(owner) if x.get("kind") == "tempo" and "what does a mutex" in (x.get("reply_text") or "")]
    check("Tempo did not answer the teammate's question too", not after, after)
    ah2 = wait_http(AGENT, "/agent/health") or {}
    print(f"     (the agent {'takes' if ah2.get('roles') is True else 'does not take'} a teammate's role and model; both paths are tested in the unit tests)")
    st, a, _ = ask_team(owner, "@coder and @sage, a word please?", "sage", secs=60)
    st2, c2, _ = ask_team(owner, "@coder one more thing?", "coder", secs=60)
    check("one message naming two teammates asks each", a is not None and any(x.get("from") == "team:coder" and x.get("reply_to") == a["reply_to"] for x in messages(owner)), a)

    # a one-on-one room: always mode, exactly one teammate; that teammate answers every message
    st, _, js = req("POST", "/room/create", owner, {"name": "Coder DM"})
    check("the owner makes a room for a one-on-one chat", st == 200 and js.get("room", {}).get("id") == "coder-dm", js)
    req("POST", "/room/update", owner, {"room": "coder-dm", "mode": "always"})
    st, _, js = req("POST", "/room/team/room", owner, {"room": "coder-dm", "handle": "coder", "on": True})
    check("the owner puts coder in that room", st == 200, (st, js))
    st, sc, js = req("POST", "/room/join", people["Mia"], {"code": invite("coder-dm"), "name": "Mia"})
    check("Mia joins the room", st == 200, (st, js))
    got = []
    for text in ["first thing on my mind", "and a second thing"]:
        st, a, _ = ask_team(people["Mia"], text, "coder", room="coder-dm", secs=90)
        got.append(a)
    check("a DM room with @coder answers every message, no @ needed", all(g is not None and g["name"] == "Coder" for g in got), got)
    dm = messages(owner, "coder-dm")
    check("and Tempo stays out of it", not any(x.get("kind") == "tempo" for x in dm), [x.get("kind") for x in dm])
    check("the room says who it is with", any(r.get("id") == "coder-dm" and r.get("team") == ["coder"] for r in req("GET", "/room/rooms", people["Mia"])[2].get("rooms", [])))

    # removing a teammate keeps its record in trash/
    room_cli("team", "add", "scratch", "--kind", "agent", "--room", "family")
    rc, txt = room_cli("team", "remove", "scratch")
    tr = os.path.join(HOME, "room", "trash")
    kept = [t for t in os.listdir(tr) if "teammate-scratch" in t] if os.path.isdir(tr) else []
    check("removing a teammate moves its record to trash", rc == 0 and kept and "scratch" in open(os.path.join(tr, kept[0])).read(), (rc, txt, kept))
    check("and it is gone from the team", "scratch" not in [t["handle"] for t in req("GET", "/room/team", owner)[2].get("team", [])])

    # --- limits and redaction of what people paste
    codes = [req("POST", "/room/post", people["Sam"], {"room": "family", "text": f"spam {i}"})[0] for i in range(30)]
    check("posting too fast is limited", 429 in codes and codes[0] == 200, codes)
    token = "ghp_" + "b" * 36
    req("POST", "/room/post", people["Lee"], {"room": "family", "text": f"my token is {token}"})
    stored = open(os.path.join(HOME, "room", "messages", "family.jsonl")).read() if os.path.exists(os.path.join(HOME, "room", "messages", "family.jsonl")) else json.dumps(messages(owner))
    check("a pasted token is redacted before it is stored", token not in stored and token not in json.dumps(messages(owner)))

    # --- owner powers
    st, _, _ = req("POST", "/room/create", owner, {"name": "Proxied"}, headers={"X-Forwarded-For": "203.0.113.9"})
    check("a request through a proxy is never the owner", st == 403, st)
    st, _, _ = req("POST", "/room/create", people["Sam"], {"name": "Mine"})
    check("a member cannot make rooms", st == 403, st)
    members = req("GET", "/room/members?room=family", owner)[2].get("members", [])
    sam_id = next((x["id"] for x in members if x.get("name") == "Sam"), None)
    lee_id = next((x["id"] for x in members if x.get("name") == "Lee"), None)
    req("POST", "/room/mute", owner, {"room": "family", "member": sam_id, "muted": True})
    check("a muted member cannot post", req("POST", "/room/post", people["Sam"], {"room": "family", "text": "hello?"})[0] == 403)
    req("POST", "/room/mute", owner, {"room": "family", "member": sam_id, "muted": False})
    time.sleep(61)  # Sam's per-minute post limit from the spam above runs out
    check("unmuted, the member can post again", req("POST", "/room/post", people["Sam"], {"room": "family", "text": "back"})[0] == 200)

    lee_live = Stream(people["Lee"]); lee_live.start()
    time.sleep(1.0)
    t_remove = time.time()
    st, _, _ = req("POST", "/room/remove", owner, {"member": lee_id})
    check("the owner removes a member", st == 200, st)
    check("the removed member's cookie stops at once", req("GET", "/room/me", people["Lee"])[2].get("role") == "guest")
    lee_live.join(30)
    gone = (lee_live.closed_at - t_remove) if lee_live.closed_at else None
    check("the removed member's live stream ends within 5 s", gone is not None and gone <= 6, f"{gone and round(gone, 1)} s")
    trash = os.listdir(os.path.join(HOME, "room", "trash")) if os.path.isdir(os.path.join(HOME, "room", "trash")) else []
    check("the removed record goes to trash", any(lee_id in open(os.path.join(HOME, "room", "trash", t)).read() for t in trash if os.path.isfile(os.path.join(HOME, "room", "trash", t))), trash)

    # --- restart: everything is still there
    stop("room")
    start("room", room_args(), os.path.join(RUN, "room2"))
    check("the room starts again", wait_http(ROOM, "/room/health") is not None)
    check("messages survive a restart", any("dinner at 7" in x.get("text", "") for x in messages(people["Mia"])))
    check("sessions survive a restart (member and owner)", req("GET", "/room/me", people["Mia"])[2].get("role") == "member" and req("GET", "/room/me", owner)[2].get("role") == "owner")
    check("teammates survive a restart", sorted(t["handle"] for t in req("GET", "/room/team", owner)[2].get("team", [])) == ["coder", "sage"])
    st, a, _ = ask_team(people["Mia"], "@sage still here after the restart?", "sage")
    check("@sage answers after a room restart", a is not None and a["text"].startswith("Sage here."), a)
    n_before = len(mock_chats())
    st, a, _ = ask(people["Mia"], "@tempo still there after the restart?")
    check("@tempo works after a room restart", a is not None and a["text"].startswith("Mock answer"), a and a["text"])

    # --- the agent goes away and comes back with a new key
    stop("agent")
    st, a, _ = ask(people["Mia"], "@tempo are you there?", secs=60)
    check("agent down: a ping says Tempo is here but its agent is not", a is not None and "not answering" in a["text"], a and a["text"])
    st, a, _ = ask(people["Mia"], "@tempo what should we cook tonight?", secs=60)
    check("agent down: a question gets a polite note, the room keeps working", a is not None and "can't reach the Perennos Tempo agent" in a["text"], a and a["text"])
    start("agent", [os.path.join(BIN, "tempo-local-agent"), "serve", "--port", str(AGENT)], os.path.join(RUN, "agent2"))
    wait_http(AGENT, "/agent/health")
    st, a, _ = ask(people["Mia"], "@tempo back again?", secs=60)
    check("agent back with a new key: the room re-reads it and answers", a is not None and a["text"].startswith("Mock answer"), a and a["text"])
finally:
    for n in list(procs):
        stop(n)
    with open(os.path.join(RUN, "results.json"), "w") as f:
        json.dump(results, f, indent=1)
    bad = [r for r in results if not r["ok"]]
    print(f"\n{len(results) - len(bad)} passed, {len(bad)} failed")
sys.exit(1 if bad or not results else 0)
