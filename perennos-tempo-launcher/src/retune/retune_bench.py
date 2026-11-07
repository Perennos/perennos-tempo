# Tempo Retune bench: the part of `tempo-launch retune bench` that touches weights.
# Written by tempo-launch; read it before you let anything use it.
#
# It does a fixed set of things: read tensors, run forward passes, and make edits to a copy.
# There is no tool here that runs code, opens a file the owner did not name, or reaches the
# network, and tempo-launch only ever passes it tool names and numbers. A cloud model must never
# get a shell on the owner's machine, so none of this is a place to add one.
#
# The original weights are opened read-only. An edit never changes them: each edit is written to
# the session's edit list (edits.json, with its numbers as .npy files beside it) and applied, in
# order, on top of the original whenever a tensor is read. Undo is dropping the last entry.
# `apply` is the only thing that writes a model, and it writes a new folder.
#
# Python standard library and numpy. torch and transformers are imported only for forward passes.
import hashlib
import json
import os
import shutil
import struct
import sys

import numpy as np

READ_CAP = 20000  # numbers in one answer
MAX_PROMPTS = 512
MAX_TOKENS = 2048
MAX_COLS = 4096
LOOP_WINDOW = 8


class Refused(Exception):
    """A request the bench will not do. The message is for the model that asked."""


# ---- safetensors: 8 bytes of header length, a JSON header, then raw bytes. Read-only.

class Weights:
    def __init__(self, folder):
        self.folder = folder
        self.index = {}  # name -> (file, dtype, shape, start, end, data offset)
        files = sorted(f for f in os.listdir(folder) if f.endswith(".safetensors"))
        if not files:
            raise Refused("no .safetensors files in the model folder")
        for f in files:
            path = os.path.join(folder, f)
            with open(path, "rb") as h:
                n = struct.unpack("<Q", h.read(8))[0]
                head = json.loads(h.read(n))
            for name, m in head.items():
                if name != "__metadata__":
                    s, e = m["data_offsets"]
                    self.index[name] = (f, m["dtype"], tuple(m["shape"]), s, e, 8 + n)

    def raw(self, name):
        f, _, _, s, e, off = self.index[name]
        return np.memmap(os.path.join(self.folder, f), dtype=np.uint8, mode="r", offset=off + s, shape=(e - s,))

    def dtype(self, name):
        return self.index[name][1]

    def shape(self, name):
        return self.index[name][2]

    def get(self, name):
        """The tensor as float32 (a copy; the file is untouched)."""
        if name not in self.index:
            raise Refused(f"no tensor called {name}")
        dt = self.dtype(name)
        b = self.raw(name).tobytes()
        if dt == "BF16":
            a = (np.frombuffer(b, np.uint16).astype(np.uint32) << 16).view(np.float32)
        elif dt == "F16":
            a = np.frombuffer(b, np.float16).astype(np.float32)
        elif dt == "F32":
            a = np.frombuffer(b, np.float32).copy()
        elif dt == "F64":
            a = np.frombuffer(b, np.float64).astype(np.float32)
        else:
            raise Refused(f"{name} is {dt}, not a floating point tensor")
        return a.reshape(self.shape(name))


def to_storage(a, dt):
    """float32 back to the file's own number format (BF16 rounds to nearest even)."""
    a = np.ascontiguousarray(a, dtype=np.float32)
    if dt == "BF16":
        u = a.view(np.uint32)
        return (((u + ((u >> 16) & 1) + 0x7FFF) >> 16).astype(np.uint16)).tobytes()
    return a.astype({"F16": np.float16, "F32": np.float32, "F64": np.float64}[dt]).tobytes()


def snap(a, dt):
    """What a float32 value becomes once it is stored: edits are replayed on these."""
    if dt == "F32":
        return a
    b = to_storage(a, dt)
    if dt == "BF16":
        return (np.frombuffer(b, np.uint16).astype(np.uint32) << 16).view(np.float32).reshape(a.shape)
    return np.frombuffer(b, np.float16 if dt == "F16" else np.float64).astype(np.float32).reshape(a.shape)


def write_safetensors(path, entries, metadata):
    """entries: [(name, dtype, shape, bytes)] in file order."""
    head, off = {}, 0
    if metadata:
        head["__metadata__"] = metadata
    for name, dt, shape, b in entries:
        head[name] = {"dtype": dt, "shape": list(shape), "data_offsets": [off, off + len(b)]}
        off += len(b)
    h = json.dumps(head, separators=(",", ":")).encode()
    h += b" " * (-len(h) % 8)
    with open(path, "xb") as f:  # "x": never replaces a file
        f.write(struct.pack("<Q", len(h)) + h)
        for _, _, _, b in entries:
            f.write(b)


# ---- the edit list

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""):
            h.update(c)
    return h.hexdigest()


def parse_index(spec, shape):
    """"10:20, 0:64" -> a tuple of slices (a bare number picks one row or column)."""
    parts = [p.strip() for p in str(spec).split(",")]
    if len(parts) != len(shape):
        raise Refused(f"index needs {len(shape)} parts for shape {list(shape)}, like \"0:4, 0:8\"")
    out = []
    for p, n in zip(parts, shape):
        try:
            if ":" in p:
                a, b = p.split(":", 1)
                a, b = (int(a) if a.strip() else 0), (int(b) if b.strip() else n)
            else:
                a = int(p)
                b = a + 1
        except ValueError:
            raise Refused(f"cannot read \"{p}\" in the index")
        if not (0 <= a < b <= n):
            raise Refused(f"\"{p}\" is outside 0..{n}")
        out.append(slice(a, b))
    return tuple(out)


def finite(a, what):
    a = np.asarray(a, dtype=np.float64)
    if not np.all(np.isfinite(a)):
        raise Refused(f"{what} has a number that is not finite")
    return a.astype(np.float32)


def apply_edit(w, e, arrays):
    """One edit on a float32 tensor; returns the new tensor. Pure: the same inputs give the same answer."""
    op, p = e["op"], e["params"]
    w = w.copy()
    if op == "ablate_direction":
        v = arrays["vector"].astype(np.float64)
        v = v / np.linalg.norm(v)
        cols = p["columns"]
        block = w[:, cols].astype(np.float64)
        w[:, cols] = (block - np.outer(v, v @ block)).astype(np.float32)
    elif op == "scale":
        if "rows" in p:
            w[p["rows"], :] *= np.float32(p["factor"])
        else:
            w[:, p["cols"]] *= np.float32(p["factor"])
    elif op == "add_rank1":
        w += np.float32(p["alpha"]) * np.outer(arrays["u"], arrays["v"]).astype(np.float32)
    elif op == "set_slice":
        sl = parse_index(p["index"], w.shape)
        w[sl] = arrays["values"].reshape(w[sl].shape)
    else:
        raise Refused(f"unknown edit {op}")
    return w


class Session:
    """A model folder, a session folder, and the runner that does forward passes (None: reading and editing only)."""

    def __init__(self, model_dir, session_dir, runner=None):
        self.w = Weights(model_dir)
        self.dir = session_dir
        self.runner = runner
        self.edits_file = os.path.join(session_dir, "edits.json")
        os.makedirs(os.path.join(session_dir, "deltas"), exist_ok=True)
        self._cache = {}
        self._stamp = None

    # reading the list

    def edits(self):
        if not os.path.exists(self.edits_file):
            return []
        with open(self.edits_file) as f:
            return json.load(f)["edits"]

    def _arrays(self, e):
        out = {}
        for k, m in e.get("arrays", {}).items():
            path = os.path.join(self.dir, m["file"])
            if sha256_file(path) != m["sha256"]:
                raise Refused(f"{m['file']} is not what edit {e['n']} recorded")
            out[k] = np.load(path, allow_pickle=False)
        return out

    def tensor(self, name):
        """The tensor with every recorded edit on it."""
        stamp = b""
        if os.path.exists(self.edits_file):
            with open(self.edits_file, "rb") as f:
                stamp = f.read()
        if stamp != self._stamp:
            self._cache, self._stamp = {}, stamp  # an undo (or an edit) happened: start from the original
        if name not in self._cache:
            a, dt = self.w.get(name), self.w.dtype(name)
            for e in self.edits():
                if e["tensor"] == name:
                    a = snap(apply_edit(a, e, self._arrays(e)), dt)
            self._cache[name] = a
        return self._cache[name]

    def edited_names(self):
        return sorted({e["tensor"] for e in self.edits()})

    # writing the list

    def _save(self, edits):
        tmp = self.edits_file + ".tmp"
        with open(tmp, "w") as f:
            json.dump({"edits": edits}, f, indent=1, sort_keys=True)
            f.write("\n")
        os.replace(tmp, self.edits_file)

    def record(self, op, tensor, params, arrays):
        edits = self.edits()
        # a number is never reused: undone edits keep their numbers (undone.jsonl) and their files
        used = [e["n"] for e in edits] + [int(f[:4]) for f in os.listdir(os.path.join(self.dir, "deltas"))]
        gone = os.path.join(self.dir, "undone.jsonl")
        if os.path.exists(gone):
            with open(gone) as f:
                used += [json.loads(l)["n"] for l in f if l.strip()]
        n = max(used + [0]) + 1
        meta = {}
        for k, a in arrays.items():
            rel = os.path.join("deltas", f"{n:04d}_{k}.npy")
            path = os.path.join(self.dir, rel)
            np.save(path, np.asarray(a, dtype=np.float32))
            meta[k] = {"file": rel, "sha256": sha256_file(path)}
        e = {"n": n, "op": op, "tensor": tensor, "params": params, "arrays": meta}
        before = self.tensor(tensor)
        after = snap(apply_edit(before, e, {k: np.asarray(a, dtype=np.float32) for k, a in arrays.items()}), self.w.dtype(tensor))
        e["touched"] = int(np.count_nonzero(after != before))
        self._save(edits + [e])
        self._cache.pop(tensor, None)
        return {"edit": n, "tensor": tensor, "weights_changed": e["touched"],
                "norm_before": float(np.linalg.norm(before)), "norm_after": float(np.linalg.norm(after))}

    # tools

    def need(self, args, key):
        if key not in args:
            raise Refused(f"{key} is missing")
        return args[key]

    def need2d(self, name):
        if len(self.w.shape(name)) != 2:
            raise Refused(f"{name} has shape {list(self.w.shape(name))}; this edit needs a matrix")

    def check_name(self, name):
        if name not in self.w.index:
            raise Refused(f"no tensor called {name}; model_info lists them")
        if self.w.dtype(name) not in ("F32", "F16", "BF16", "F64"):
            raise Refused(f"{name} is {self.w.dtype(name)}, not floating point")

    def call(self, tool, args):
        fn = getattr(self, "t_" + tool, None) if tool in TOOLS else None
        if fn is None:
            raise Refused(f"there is no tool called {tool}")
        return fn(args)

    def t_model_info(self, args):
        cfg = {}
        p = os.path.join(self.w.folder, "config.json")
        if os.path.exists(p):
            with open(p) as f:
                cfg = json.load(f)
        names = sorted(self.w.index)
        total = int(sum(np.prod(self.w.shape(n), dtype=np.int64) for n in names))
        return {
            "architectures": cfg.get("architectures"), "model_type": cfg.get("model_type"),
            "layers": cfg.get("num_hidden_layers") or (cfg.get("text_config") or {}).get("num_hidden_layers"),
            "hidden_size": cfg.get("hidden_size") or (cfg.get("text_config") or {}).get("hidden_size"),
            "parameters": total, "edits_so_far": len(self.edits()),
            "tensors": [[n, list(self.w.shape(n)), self.w.dtype(n)] for n in names[:600]],
            "tensors_not_listed": max(0, len(names) - 600),
        }

    def t_tensor_stats(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        a = self.tensor(name)
        out = {"tensor": name, "shape": list(a.shape), "dtype": self.w.dtype(name),
               "norm": float(np.linalg.norm(a)), "mean": float(a.mean()), "std": float(a.std()),
               "abs_max": float(np.abs(a).max())}
        if a.ndim == 2:
            out["top_singular_values"] = [float(s) for s in singular(a, 6)[0]]
            if "vector" in args:  # which columns write this direction the most
                v = finite(args["vector"], "vector")
                if v.shape != (a.shape[0],):
                    raise Refused(f"vector needs {a.shape[0]} numbers, one per row")
                v = v / np.linalg.norm(v)
                dots = v @ a
                top = np.argsort(-np.abs(dots))[:16]
                out["columns_most_aligned"] = [[int(j), float(dots[j])] for j in top]
        return out

    def t_read_slice(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        a = self.tensor(name)
        piece = a[parse_index(self.need(args, "index"), a.shape)]
        if piece.size > READ_CAP:
            raise Refused(f"that slice is {piece.size} numbers; the most one call gives is {READ_CAP}")
        return {"tensor": name, "shape": list(piece.shape), "values": piece.reshape(-1).tolist()}

    def t_top_directions(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        self.need2d(name)
        a = self.tensor(name)
        k = int(args.get("k", 3))
        side = args.get("side", "output")
        if side not in ("output", "input") or not 1 <= k <= 16:
            raise Refused("side is output or input, and k is 1 to 16")
        s, u, v = singular(a, k)
        d = u if side == "output" else v
        if d.size > READ_CAP:
            raise Refused(f"{k} directions of {d.shape[0]} numbers is more than {READ_CAP}; ask for fewer")
        return {"tensor": name, "side": side, "singular_values": [float(x) for x in s],
                "directions": [d[:, i].tolist() for i in range(d.shape[1])]}

    def t_forward(self, args):
        prompts, mt = args["prompts"], int(args.get("max_tokens", 256))
        if not self.runner:
            raise Refused("forward passes are not set up in this session")
        if not 1 <= len(prompts) <= MAX_PROMPTS or not 1 <= mt <= MAX_TOKENS:
            raise Refused(f"1 to {MAX_PROMPTS} prompts, and max_tokens 1 to {MAX_TOKENS}")
        self.runner.sync(self)
        outs = self.runner.generate(prompts, mt)
        return {"answers": [{"text": t, "hit_limit": bool(c), "looped": looped(t, c)} for t, c in outs],
                "stats": loop_stats(outs)}

    def t_activation_diff(self, args):
        a, b, layer = args["prompts_a"], args["prompts_b"], int(args["layer"])
        if not self.runner:
            raise Refused("forward passes are not set up in this session")
        if not (1 <= len(a) <= MAX_PROMPTS and 1 <= len(b) <= MAX_PROMPTS):
            raise Refused(f"1 to {MAX_PROMPTS} prompts in each set")
        self.runner.sync(self)
        ra, rb = self.runner.residuals(a, layer), self.runner.residuals(b, layer)
        diff = ra.mean(0) - rb.mean(0)
        norm = float(np.linalg.norm(diff))
        both = np.vstack([ra, rb])
        both = both - both.mean(0)
        _, sv, vt = np.linalg.svd(both, full_matrices=False)
        k = min(3, len(sv))
        if diff.size > READ_CAP // 4:
            raise Refused("this model's residual stream is too wide to return in one call")
        return {"layer": layer, "mean_diff_norm": norm,
                "mean_diff_unit": (diff / norm).tolist() if norm else None,
                "principal_directions": [vt[i].tolist() for i in range(k)],
                "principal_strengths": [float(x) for x in sv[:k]]}

    def t_ablate_direction(self, args):
        name, cols = self.need(args, "tensor"), args.get("columns")
        self.check_name(name)
        self.need2d(name)
        rows, ncol = self.w.shape(name)
        v = finite(self.need(args, "vector"), "vector")
        if v.shape != (rows,) or not np.linalg.norm(v):
            raise Refused(f"vector needs {rows} numbers (one per row), not all zero")
        if not isinstance(cols, list) or not cols or len(cols) > MAX_COLS or len(set(cols)) != len(cols) \
                or not all(isinstance(c, int) and 0 <= c < ncol for c in cols):
            raise Refused(f"columns is a list of up to {MAX_COLS} different column numbers below {ncol}")
        return self.record("ablate_direction", name, {"columns": cols}, {"vector": v})

    def t_scale(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        self.need2d(name)
        f = args.get("factor")
        if not isinstance(f, (int, float)) or isinstance(f, bool) or not abs(f) <= 100 or f != f:
            raise Refused("factor is a number between -100 and 100")
        which = [k for k in ("rows", "cols") if k in args]
        if len(which) != 1:
            raise Refused("give rows or cols, one of them")
        idx = args[which[0]]
        n = self.w.shape(name)[0 if which[0] == "rows" else 1]
        if not isinstance(idx, list) or not idx or len(idx) > MAX_COLS or len(set(idx)) != len(idx) \
                or not all(isinstance(i, int) and 0 <= i < n for i in idx):
            raise Refused(f"{which[0]} is a list of up to {MAX_COLS} different numbers below {n}")
        return self.record("scale", name, {which[0]: idx, "factor": float(f)}, {})

    def t_add_rank1(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        self.need2d(name)
        r, c = self.w.shape(name)
        u, v = finite(self.need(args, "u"), "u"), finite(self.need(args, "v"), "v")
        alpha = args.get("alpha")
        if u.shape != (r,) or v.shape != (c,):
            raise Refused(f"u needs {r} numbers and v needs {c}")
        if not isinstance(alpha, (int, float)) or isinstance(alpha, bool) or not abs(alpha) < 1e6:
            raise Refused("alpha is a number")
        return self.record("add_rank1", name, {"alpha": float(alpha)}, {"u": u, "v": v})

    def t_set_slice(self, args):
        name = self.need(args, "tensor")
        self.check_name(name)
        sl = parse_index(self.need(args, "index"), self.w.shape(name))
        shape = tuple(s.stop - s.start for s in sl)
        n = int(np.prod(shape))
        if n > READ_CAP:
            raise Refused(f"that slice is {n} numbers; the most one edit sets is {READ_CAP}")
        vals = finite(self.need(args, "values"), "values")
        if vals.size != n:
            raise Refused(f"the slice holds {n} numbers and values has {vals.size}")
        return self.record("set_slice", name, {"index": str(args["index"])}, {"values": vals})


TOOLS = {"model_info", "tensor_stats", "read_slice", "top_directions", "forward", "activation_diff",
         "ablate_direction", "scale", "add_rank1", "set_slice"}


def singular(a, k):
    """Top-k singular values and vectors, from the smaller side's Gram matrix (no full SVD of a big matrix)."""
    a64 = a.astype(np.float64)
    k = min(k, min(a.shape))
    if a.shape[0] <= a.shape[1]:
        ev, u = np.linalg.eigh(a64 @ a64.T)
        order = np.argsort(-ev)[:k]
        s = np.sqrt(np.clip(ev[order], 0, None))
        u = u[:, order]
        v = (a64.T @ u) / np.where(s > 0, s, 1)
    else:
        ev, v = np.linalg.eigh(a64.T @ a64)
        order = np.argsort(-ev)[:k]
        s = np.sqrt(np.clip(ev[order], 0, None))
        v = v[:, order]
        u = (a64 @ v) / np.where(s > 0, s, 1)
    return s, u, v


# ---- loops: the same counting tempo-launch does in Rehearsal

def looped(text, hit_limit):
    if hit_limit:
        return True
    w = "".join(c.lower() if c.isalnum() else " " for c in text).split()
    if len(w) < LOOP_WINDOW * 3:
        return False
    seen = {}
    for i in range(len(w) - LOOP_WINDOW + 1):
        k = tuple(w[i:i + LOOP_WINDOW])
        seen[k] = seen.get(k, 0) + 1
        if seen[k] >= 3:
            return True
    return False


def loop_stats(outs):
    n = max(len(outs), 1)
    return {"answers": len(outs),
            "hit_limit_pct": round(100 * sum(1 for _, c in outs if c) / n, 1),
            "loop_pct": round(100 * sum(1 for t, c in outs if looped(t, c)) / n, 1),
            "mean_words": round(sum(len(t.split()) for t, _ in outs) / n, 1)}


# ---- forward passes: torch and transformers, loaded only when asked

class TorchRunner:
    def __init__(self, model_dir):
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer
        self.torch = torch
        self.tok = AutoTokenizer.from_pretrained(model_dir)
        self.tok.padding_side = "left"
        dev = "cuda" if torch.cuda.is_available() else "cpu"
        self.dev = dev
        self.model = AutoModelForCausalLM.from_pretrained(
            model_dir, torch_dtype=torch.bfloat16 if dev == "cuda" else torch.float32).to(dev)
        self.model.eval()
        self.on = set()  # names currently overlaid with edited values
        self.params = dict(self.model.named_parameters())

    def sync(self, s):
        """Put the session's edited tensors into the model, and the originals back where an edit was undone."""
        want = set(s.edited_names())
        for name in self.on - want:
            self._put(name, s.w.get(name))
        for name in want:
            self._put(name, s.tensor(name))
        self.on = want

    def _put(self, name, a):
        p = self.params.get(name)
        if p is None:
            raise Refused(f"{name} is not a parameter of the loaded model")
        with self.torch.no_grad():
            p.copy_(self.torch.from_numpy(np.ascontiguousarray(a)).to(p.dtype).to(p.device))

    def _enc(self, prompts):
        text = [self.tok.apply_chat_template([{"role": "user", "content": p}], add_generation_prompt=True, tokenize=False)
                for p in prompts]
        return self.tok(text, return_tensors="pt", padding=True).to(self.dev)

    def generate(self, prompts, max_tokens, chunk=16):
        outs = []
        for i in range(0, len(prompts), chunk):
            enc = self._enc(prompts[i:i + chunk])
            with self.torch.no_grad():
                g = self.model.generate(**enc, max_new_tokens=max_tokens, do_sample=False)
            for row in g[:, enc["input_ids"].shape[1]:]:
                done = (row == self.tok.eos_token_id).any().item()
                outs.append((self.tok.decode(row, skip_special_tokens=True), not done))
        return outs

    def residuals(self, prompts, layer, chunk=16):
        rows = []
        for i in range(0, len(prompts), chunk):
            enc = self._enc(prompts[i:i + chunk])
            with self.torch.no_grad():
                hs = self.model(**enc, output_hidden_states=True).hidden_states
            rows.append(hs[layer + 1][:, -1, :].float().cpu().numpy())
        return np.vstack(rows)


# ---- apply: a new model folder with the edits in it

def apply(model_dir, session_dir, out_dir):
    if os.path.exists(out_dir) and os.listdir(out_dir):
        raise Refused(f"{out_dir} already has files in it; apply never writes into a folder that is not empty")
    s = Session(model_dir, session_dir)
    names = set(s.edited_names())
    os.makedirs(out_dir, exist_ok=True)
    changed, per, total = 0, {}, 0
    for f in sorted(os.listdir(model_dir)):
        src = os.path.join(model_dir, f)
        if not f.endswith(".safetensors"):
            if os.path.isfile(src):
                shutil.copy2(src, os.path.join(out_dir, f))
            continue
        with open(src, "rb") as h:
            n = struct.unpack("<Q", h.read(8))[0]
            head = json.loads(h.read(n))
        meta = head.pop("__metadata__", None)
        entries = []
        for name, m in sorted(head.items(), key=lambda kv: kv[1]["data_offsets"][0]):
            total += int(np.prod(m["shape"], dtype=np.int64))
            if name in names:
                a = s.tensor(name)
                c = int(np.count_nonzero(a != s.w.get(name)))
                if c:
                    per[name], changed = c, changed + c
                entries.append((name, m["dtype"], m["shape"], to_storage(a, m["dtype"])))
            else:
                entries.append((name, m["dtype"], m["shape"], s.w.raw(name).tobytes()))
        write_safetensors(os.path.join(out_dir, f), entries, meta)
    return {"changed": changed, "total": total, "tensors": per}


# ---- the doors: serve (one JSON line in, one out), apply, and the two that fetch

def serve(model_dir, session_dir, runner):
    s = Session(model_dir, session_dir, runner)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            req = json.loads(line)
            out = {"ok": True, "result": s.call(req["tool"], req.get("args") or {})}
        except Refused as e:
            out = {"ok": False, "error": str(e)}
        except Exception as e:  # keep the bench alive; say what happened
            out = {"ok": False, "error": f"{type(e).__name__}: {e}"}
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "serve" and len(argv) == 4:
        serve(argv[2], argv[3], TorchRunner(argv[2]))
    elif cmd == "serve-read-only" and len(argv) == 4:
        serve(argv[2], argv[3], None)
    elif cmd == "apply" and len(argv) == 5:
        print(json.dumps(apply(argv[2], argv[3], argv[4])))
    elif cmd == "hfsize" and len(argv) == 3:
        from huggingface_hub import HfApi
        info = HfApi().model_info(argv[2], files_metadata=True)
        print(json.dumps({"bytes": sum(f.size or 0 for f in info.siblings if f.rfilename.endswith(".safetensors"))}))
    elif cmd == "fetch" and len(argv) == 4:
        from huggingface_hub import snapshot_download
        snapshot_download(argv[2], local_dir=argv[3], allow_patterns=["*.safetensors", "*.json", "tokenizer*", "*.txt", "*.model"])
        print(json.dumps({"folder": argv[3]}))
    else:
        sys.stderr.write("usage: retune_bench.py serve|serve-read-only MODEL SESSION | apply MODEL SESSION OUT | hfsize REPO | fetch REPO FOLDER\n")
        return 2
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except Refused as e:
        print(json.dumps({"error": str(e)}))
        sys.exit(1)
