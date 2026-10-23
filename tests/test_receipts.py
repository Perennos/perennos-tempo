"""The receipt chain must detect any edit or deletion. Run: python -m unittest discover -s tests"""
import json
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import tempo_receipts as R  # noqa: E402


class ReceiptChain(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = pathlib.Path(self.dir.name) / "receipts.jsonl"
        for i in range(3):
            R.append_receipt(self.path, {"job": f"job-{i}", "engine": "qwen3:4b", "route": "local"})

    def tearDown(self):
        self.dir.cleanup()

    def test_intact_chain_verifies(self):
        v = R.verify_chain(self.path)
        self.assertTrue(v["ok"], v)
        self.assertEqual(v["length"], 3)

    def test_edited_receipt_is_detected(self):
        lines = self.path.read_text(encoding="utf-8").splitlines()
        rec = json.loads(lines[1])
        rec["engine"] = "someone-else"
        lines[1] = json.dumps(rec)
        self.path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        self.assertFalse(R.verify_chain(self.path)["ok"])

    def test_deleted_receipt_is_detected(self):
        lines = self.path.read_text(encoding="utf-8").splitlines()
        self.path.write_text("\n".join([lines[0], lines[2]]) + "\n", encoding="utf-8")
        self.assertFalse(R.verify_chain(self.path)["ok"])

    def test_reserved_fields_and_non_objects_are_refused(self):
        with self.assertRaises(ValueError):
            R.append_receipt(self.path, {"receipt_hash": "forged"})
        with self.assertRaises(TypeError):
            R.append_receipt(self.path, ["not", "an", "object"])


if __name__ == "__main__":
    unittest.main()
