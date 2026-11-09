"""Outcome log (did Tempo help?) and external-job kind. Run: python -m unittest discover -s tests"""
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import tempo_outcomes as O  # noqa: E402
import tempo_receipts as R  # noqa: E402


class OutcomeLog(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = pathlib.Path(self.tmp.name) / "outcomes.jsonl"

    def tearDown(self):
        self.tmp.cleanup()

    def use(self, tool="tempo_ask_local", seconds=10, ok=True):
        return O.record_use(tool, "claude", seconds, ok, path=self.path)

    def test_a_use_row_has_what_an_outcome_needs_to_refer_to(self):
        row = self.use()
        self.assertTrue(row["id"].startswith("u"))
        self.assertEqual(O.rows(self.path)[0]["tool"], "tempo_ask_local")

    def test_outcome_is_one_of_three_words(self):
        with self.assertRaises(ValueError):
            O.record_outcome("u1", "great", path=self.path)

    def test_summary_counts_helped_partly_no_and_unrated(self):
        ids = [self.use(seconds=s)["id"] for s in (10, 20, 30, 40)]
        O.record_outcome(ids[0], "helped", path=self.path)
        O.record_outcome(ids[1], "partly", seconds=5, note="needed a rewrite", path=self.path)
        O.record_outcome(ids[2], "no", path=self.path)
        t = O.summary(O.rows(self.path))["tools"]["tempo_ask_local"]
        self.assertEqual((t["calls"], t["helped"], t["partly"], t["no"], t["unrated"]), (4, 1, 1, 1, 1))
        self.assertEqual(t["helped_pct"], 33)          # of the three rated
        self.assertEqual((t["total_seconds"], t["median_seconds"]), (100.0, 25.0))

    def test_the_latest_word_on_a_use_wins(self):
        u = self.use()["id"]
        O.record_outcome(u, "no", path=self.path)
        O.record_outcome(u, "helped", path=self.path)
        self.assertEqual(O.summary(O.rows(self.path))["tools"]["tempo_ask_local"]["helped"], 1)

    def test_nothing_rated_gives_no_percentage_not_zero(self):
        self.use()
        self.assertIsNone(O.summary(O.rows(self.path))["tools"]["tempo_ask_local"]["helped_pct"])

    def test_note_is_capped_and_a_bad_line_is_skipped(self):
        O.record_outcome("u1", "helped", note="x" * 1000, path=self.path)
        with self.path.open("a") as f:
            f.write("not json\n")
        self.assertEqual(len(O.rows(self.path)), 1)
        self.assertEqual(len(O.rows(self.path)[0]["note"]), O.NOTE_MAX)

    def test_a_rating_for_a_receipt_id_counts_under_its_own_line(self):
        O.record_outcome("receipt-abc", "helped", seconds=12, path=self.path)
        O.record_outcome("receipt-def", "no", path=self.path)
        t = O.summary(O.rows(self.path))["tools"]["(receipts and other refs)"]
        self.assertEqual((t["calls"], t["helped"], t["no"], t["total_seconds"]), (2, 1, 1, 12.0))

    def test_text_view_lists_each_tool(self):
        self.use("tempo_admit")
        self.assertIn("tempo_admit", O.text(O.summary(O.rows(self.path))))


class External(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = pathlib.Path(self.tmp.name) / "outcomes.jsonl"

    def tearDown(self):
        self.tmp.cleanup()

    def test_start_then_done_records_time_and_outcome(self):
        s = O.external_start("claude", "cloud review of PR", "claude-cloud", path=self.path, now=1000)
        d = O.external_done(s["id"], outcome="helped", note="found two bugs", path=self.path, now=1090)
        self.assertEqual(d["seconds"], 90.0)
        t = O.summary(O.rows(self.path))["tools"]["external:claude-cloud"]
        self.assertEqual((t["calls"], t["helped"], t["total_seconds"]), (1, 1, 90.0))

    def test_an_unfinished_job_is_counted_as_open_not_as_a_call(self):
        O.external_start("codex", "cloud task", "codex-cloud", path=self.path)
        rep = O.summary(O.rows(self.path))
        self.assertEqual((rep["tools"], rep["external_still_open"]), ({}, 1))

    def test_finishing_an_unknown_job_is_an_error(self):
        with self.assertRaises(KeyError):
            O.external_done("x-nope", path=self.path)

    def test_done_can_also_append_a_metadata_only_receipt(self):
        receipts = pathlib.Path(self.tmp.name) / "receipts.jsonl"
        s = O.external_start("gemini", "summary", "gemini-hosted", path=self.path, now=0)
        O.external_done(s["id"], ok=False, path=self.path, receipts_path=receipts, now=2)
        check = R.verify_chain(receipts)
        self.assertTrue(check["ok"], check)
        row = R.verified_snapshot(receipts)[0]
        self.assertEqual((row["route"], row["status"], row["engine_id"], row["elapsed_ms"]), ("external", "failed", "gemini-hosted", 2000.0))
        self.assertNotIn("note", row)

    def test_no_receipt_is_written_unless_asked(self):
        s = O.external_start("gemini", "summary", "gemini-hosted", path=self.path)
        O.external_done(s["id"], path=self.path)
        self.assertFalse((pathlib.Path(self.tmp.name) / "receipts.jsonl").exists())


if __name__ == "__main__":
    unittest.main()
