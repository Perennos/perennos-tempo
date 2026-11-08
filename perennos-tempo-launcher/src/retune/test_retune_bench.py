# Tests for retune_bench.py on a tiny fake model: a few small numpy tensors written as
# safetensors when the test runs, and a fake forward function. No torch, no downloads.
# `python3 test_retune_bench.py` runs them; `serve-fake MODEL SESSION` is the stand-in the
# Rust tests talk to.
import json
import os
import sys
import tempfile
import unittest

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import retune_bench as rb  # noqa: E402


class FakeRunner:
    """Forward passes without a model: the answer loops while a tensor still holds a big number."""

    def sync(self, s):
        self.loud = float(np.abs(s.tensor("mlp.down")).max()) > 1.5

    def generate(self, prompts, max_tokens):
        return [(("again and again so " * 20) if self.loud else "the answer is " + p, self.loud) for p in prompts]

    def residuals(self, prompts, layer):
        return np.array([[float(len(p)), 1.0, 0.0, 0.0] for p in prompts])


def make_model(folder, dtype="F32"):
    rng = np.random.default_rng(7)
    down = (rng.normal(size=(4, 6)) * 0.3).clip(-1, 1).astype(np.float32)
    down[0, 0] = 2.0  # the loud number
    t = {"mlp.down": down, "mlp.up": rng.normal(size=(6, 4)).astype(np.float32),
         "norm": rng.normal(size=(4,)).astype(np.float32)}
    entries = []
    for n, a in t.items():
        entries.append((n, dtype, a.shape, rb.to_storage(a, dtype)))
    rb.write_safetensors(os.path.join(folder, "model.safetensors"), entries, {"format": "pt"})
    with open(os.path.join(folder, "config.json"), "w") as f:
        json.dump({"architectures": ["Fake"], "num_hidden_layers": 2, "hidden_size": 4}, f)
    return t


def digest(path):
    return rb.sha256_file(path)


class Bench(unittest.TestCase):
    dtype = "F32"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.model = os.path.join(self.tmp.name, "model")
        self.sess = os.path.join(self.tmp.name, "session")
        os.makedirs(self.model)
        self.t = make_model(self.model, self.dtype)
        self.file = os.path.join(self.model, "model.safetensors")
        self.s = rb.Session(self.model, self.sess, FakeRunner())

    def tearDown(self):
        self.tmp.cleanup()

    def original(self, name):
        return rb.snap(self.t[name], self.dtype)

    def test_ablate_direction_matches_the_math(self):
        v = np.array([1.0, 2.0, -1.0, 0.5])
        cols = [0, 3, 5]
        r = self.s.call("ablate_direction", {"tensor": "mlp.down", "vector": v.tolist(), "columns": cols})
        self.assertEqual(r["edit"], 1)
        u = v / np.linalg.norm(v)
        w0 = self.original("mlp.down").astype(np.float64)
        want = w0.copy()
        for j in cols:
            want[:, j] = w0[:, j] - (u @ w0[:, j]) * u
        got = self.s.tensor("mlp.down").astype(np.float64)
        tol = 1e-5 if self.dtype == "F32" else 3e-2
        self.assertTrue(np.allclose(got, want, atol=tol), (got - want))
        # the direction is gone from those columns, and the others did not move
        self.assertTrue(np.allclose(u @ got[:, cols], 0, atol=tol * 3))
        self.assertTrue(np.array_equal(got[:, [1, 2, 4]], w0[:, [1, 2, 4]]))

    def test_every_edit_leaves_the_original_file_alone(self):
        before = digest(self.file)
        self.s.call("ablate_direction", {"tensor": "mlp.down", "vector": [1, 0, 0, 0], "columns": [0]})
        self.s.call("scale", {"tensor": "mlp.up", "rows": [0, 1], "factor": 0.5})
        self.s.call("add_rank1", {"tensor": "mlp.up", "u": [1] * 6, "v": [0.5] * 4, "alpha": 0.25})
        self.s.call("set_slice", {"tensor": "norm", "index": "0:2", "values": [9, 9]})
        self.assertEqual(digest(self.file), before)
        self.assertEqual(len(self.s.edits()), 4)
        # the edit list is on disk, with each edit's numbers beside it
        self.assertTrue(os.path.exists(os.path.join(self.sess, "edits.json")))
        self.assertTrue(os.path.exists(os.path.join(self.sess, "deltas", "0001_vector.npy")))
        self.assertEqual(float(self.s.tensor("norm")[0]), 9.0)

    def test_scale_rank1_and_set_slice_do_what_they_say(self):
        w = self.original("mlp.up").astype(np.float64)
        self.s.call("scale", {"tensor": "mlp.up", "cols": [1], "factor": 2})
        self.s.call("add_rank1", {"tensor": "mlp.up", "u": [1, 0, 0, 0, 0, 0], "v": [0, 0, 0, 1], "alpha": 3})
        got = self.s.tensor("mlp.up").astype(np.float64)
        tol = 1e-5 if self.dtype == "F32" else 5e-2
        self.assertTrue(np.allclose(got[:, 1], 2 * w[:, 1], atol=tol))
        self.assertAlmostEqual(got[0, 3], w[0, 3] + 3, delta=tol)

    def test_undo_restores_exactly(self):
        self.s.call("scale", {"tensor": "mlp.down", "cols": [2], "factor": 3})
        one = self.s.tensor("mlp.down").copy()
        self.s.call("ablate_direction", {"tensor": "mlp.down", "vector": [1, 1, 1, 1], "columns": [0, 1]})
        self.assertFalse(np.array_equal(self.s.tensor("mlp.down"), one))
        # what `tempo-launch retune undo` does: the last entry moves to undone.jsonl
        edits = self.s.edits()
        last = edits.pop()
        with open(os.path.join(self.sess, "edits.json"), "w") as f:
            json.dump({"edits": edits}, f)
        with open(os.path.join(self.sess, "undone.jsonl"), "a") as f:
            f.write(json.dumps(last) + "\n")
        self.assertTrue(np.array_equal(self.s.tensor("mlp.down"), one))
        edits = []
        with open(os.path.join(self.sess, "edits.json"), "w") as f:
            json.dump({"edits": edits}, f)
        self.assertTrue(np.array_equal(self.s.tensor("mlp.down"), self.original("mlp.down")))
        # a number is never reused, and the undone edit's file is still there
        r = self.s.call("ablate_direction", {"tensor": "mlp.down", "vector": [1, 0, 0, 0], "columns": [0]})
        self.assertEqual(r["edit"], 3)
        self.assertTrue(os.path.exists(os.path.join(self.sess, "deltas", "0002_vector.npy")))

    def test_apply_writes_a_new_folder_and_counts_the_weights(self):
        before = digest(self.file)
        self.s.call("ablate_direction", {"tensor": "mlp.down", "vector": [1, 0, 0, 0], "columns": [0, 1]})
        out = os.path.join(self.tmp.name, "out")
        r = rb.apply(self.model, self.sess, out)
        self.assertEqual(digest(self.file), before)
        self.assertEqual(r["total"], 24 + 24 + 4)
        self.assertGreaterEqual(r["changed"], 1)
        self.assertLessEqual(r["changed"], 2)  # row 0 of two columns
        w = rb.Weights(out)
        self.assertTrue(np.array_equal(w.get("mlp.down"), self.s.tensor("mlp.down")))
        self.assertTrue(np.array_equal(w.get("mlp.up"), self.original("mlp.up")))
        self.assertTrue(os.path.exists(os.path.join(out, "config.json")))
        with self.assertRaises(rb.Refused):  # never into a folder with files in it
            rb.apply(self.model, self.sess, out)

    def test_reads_are_capped_and_bad_asks_are_refused(self):
        r = self.s.call("read_slice", {"tensor": "mlp.up", "index": "0:2, 1:3"})
        self.assertEqual((r["shape"], len(r["values"])), ([2, 2], 4))
        big = os.path.join(self.tmp.name, "big")
        os.makedirs(big)
        rb.write_safetensors(os.path.join(big, "m.safetensors"),
                             [("w", "F32", (200, 200), np.zeros((200, 200), np.float32).tobytes())], None)
        b = rb.Session(big, os.path.join(self.tmp.name, "bs"))
        with self.assertRaises(rb.Refused):
            b.call("read_slice", {"tensor": "w", "index": "0:200, 0:200"})
        for tool, args in [
            ("run", {"cmd": "ls"}),                                    # there is no such tool
            ("read_slice", {"tensor": "../../etc/passwd", "index": "0:1"}),
            ("ablate_direction", {"tensor": "mlp.down", "vector": [1, 2], "columns": [0]}),
            ("ablate_direction", {"tensor": "mlp.down", "vector": [0, 0, 0, 0], "columns": [0]}),
            ("ablate_direction", {"tensor": "mlp.down", "vector": [1, 0, 0, 0], "columns": [99]}),
            ("set_slice", {"tensor": "norm", "index": "0:2", "values": [float("nan"), 1]}),
            ("scale", {"tensor": "mlp.up", "rows": [0], "cols": [0], "factor": 2}),
        ]:
            with self.assertRaises(rb.Refused, msg=tool):
                self.s.call(tool, args)
        self.assertEqual(self.s.edits(), [])

    def test_stats_and_directions(self):
        st = self.s.call("tensor_stats", {"tensor": "mlp.down", "vector": [1, 0, 0, 0]})
        full = np.linalg.svd(self.original("mlp.down"), compute_uv=False)
        self.assertAlmostEqual(st["top_singular_values"][0], float(full[0]), places=4)
        self.assertEqual(len(st["columns_most_aligned"]), 6)
        d = self.s.call("top_directions", {"tensor": "mlp.down", "k": 2, "side": "output"})
        self.assertEqual((len(d["directions"]), len(d["directions"][0])), (2, 4))
        info = self.s.call("model_info", {})
        self.assertEqual(info["parameters"], 52)

    def test_forward_sees_edits_and_loops_are_counted(self):
        r = self.s.call("forward", {"prompts": ["a", "b"], "max_tokens": 8})
        self.assertEqual(r["stats"]["loop_pct"], 100.0)
        # take the loud number out and the loop goes
        self.s.call("set_slice", {"tensor": "mlp.down", "index": "0:1, 0:1", "values": [0.1]})
        r = self.s.call("forward", {"prompts": ["a", "b"], "max_tokens": 8})
        self.assertEqual(r["stats"]["loop_pct"], 0.0)
        d = self.s.call("activation_diff", {"prompts_a": ["aaaa", "bbbbbb"], "prompts_b": ["c"], "layer": 1})
        self.assertAlmostEqual(d["mean_diff_unit"][0], 1.0)

    def test_loop_detection(self):
        self.assertTrue(rb.looped("x", True))
        self.assertFalse(rb.looped("a plain short answer", False))
        self.assertTrue(rb.looped("one two three four five six seven eight nine ten " * 4, False))
        self.assertFalse(rb.looped(" ".join(f"w{i}" for i in range(200)), False))
        s = rb.loop_stats([("ok", False), ("cut", True)])
        self.assertEqual((s["hit_limit_pct"], s["loop_pct"]), (50.0, 50.0))


class BenchBf16(Bench):
    dtype = "BF16"


class Format(unittest.TestCase):
    def test_the_files_are_real_safetensors_when_the_library_is_there(self):
        try:
            from safetensors.numpy import load_file
        except ImportError:
            self.skipTest("safetensors is not installed here; the cross-check with it did not run")
        with tempfile.TemporaryDirectory() as d:
            t = make_model(d)
            got = load_file(os.path.join(d, "model.safetensors"))
            for k in t:
                self.assertTrue(np.array_equal(got[k], t[k]))


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "serve-fake":
        rb.serve(sys.argv[2], sys.argv[3], FakeRunner())
    elif len(sys.argv) == 3 and sys.argv[1] == "make-model":
        make_model(sys.argv[2])
    else:
        unittest.main()
