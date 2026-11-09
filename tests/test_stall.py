"""Stall detector: stale heartbeat flagged once per stall, cleared by a fresh change, one alert file per item.
Run: python -m unittest discover -s tests"""
import os
import pathlib
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import tempo_stall as S  # noqa: E402

H = 3600


class Check(unittest.TestCase):
    def item(self, beat, **kw):
        return {"id": "u1", "label": "unit one", "last_beat": beat, **kw}

    def test_fresh_beat_is_quiet(self):
        self.assertEqual(S.check([self.item(1000 - 5 * H)], {}, 1000)[0], [])

    def test_stale_beat_alerts_with_its_age(self):
        alerts, state = S.check([self.item(1000 - 15 * H)], {}, 1000)
        self.assertEqual(alerts[0]["age_hours"], 15.0)
        self.assertIn("u1", state)

    def test_the_same_stall_alerts_once_not_every_scan(self):
        _, state = S.check([self.item(0)], {}, 7 * H)
        for hour in range(8, 30):
            alerts, state = S.check([self.item(0)], state, hour * H)
            self.assertEqual(alerts, [])

    def test_a_fresh_change_clears_it_and_a_later_stall_alerts_again(self):
        _, state = S.check([self.item(0)], {}, 7 * H)
        _, state = S.check([self.item(10 * H)], state, 11 * H)   # wrote again: cleared
        self.assertEqual(state, {})
        alerts, _ = S.check([self.item(10 * H)], state, 20 * H)  # stalled again
        self.assertEqual(len(alerts), 1)

    def test_per_item_limit_wins_over_the_default(self):
        self.assertEqual(S.check([self.item(0, hours=1)], {}, 2 * H)[0][0]["hours"], 1)
        self.assertEqual(S.check([self.item(0, hours=12)], {}, 7 * H)[0], [])

    def test_finished_or_unreadable_items_are_never_stalled(self):
        self.assertEqual(S.check([self.item(0, done=True), self.item(None)], {}, 99 * H)[0], [])

    def test_an_item_that_vanished_drops_out_of_the_state(self):
        _, state = S.check([self.item(0)], {}, 7 * H)
        self.assertEqual(S.check([], state, 8 * H)[1], {})


class Scan(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.report = self.root / "run" / "report.md"
        self.report.parent.mkdir()
        self.report.write_text("progress")
        self.kw = dict(watchfile=self.root / "w.json", statefile=self.root / "s.json", alert_dir=self.root / "alerts")

    def tearDown(self):
        self.tmp.cleanup()

    def age(self, hours):
        t = time.time() - hours * H
        os.utime(self.report, (t, t))
        os.utime(self.report.parent, (t, t))

    def test_newest_change_looks_inside_a_folder(self):
        self.age(20)
        (self.report.parent / "log.txt").write_text("fresh")
        self.assertGreater(S.newest_change(self.report.parent), time.time() - 60)
        self.assertIsNone(S.newest_change(self.root / "missing"))

    def test_end_to_end_one_alert_file_rewritten_in_place(self):
        S.watch("s56", self.report.parent, 6, self.kw["watchfile"])
        self.age(15)
        first = S.scan(**self.kw)
        self.assertEqual(len(first), 1)
        self.assertEqual(S.scan(**self.kw), [])   # second scan: quiet
        files = list((self.root / "alerts").iterdir())
        self.assertEqual([f.name for f in files], ["STUCK-stall-s56.md"])
        self.assertIn("15.0 h ago", files[0].read_text(encoding="utf-8"))
        self.assertIn("Nothing has been stopped", files[0].read_text(encoding="utf-8"))

    def test_unwatch_forgets_the_item_and_its_alert_state(self):
        S.watch("s56", self.report, 6, self.kw["watchfile"])
        self.age(15)
        S.scan(**self.kw)
        S.unwatch("s56", self.kw["watchfile"], self.kw["statefile"])
        self.assertEqual(S._read(self.kw["watchfile"], None), [])
        self.assertEqual(S._read(self.kw["statefile"], None), {})


if __name__ == "__main__":
    unittest.main()
