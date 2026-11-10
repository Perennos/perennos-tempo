from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "perennos-tempo-launcher" / "src" / "foundry" / "fixtures"
SPEC = spec_from_file_location("code_exam_evaluate", FIXTURES / "code_exam_evaluate.py")
CHECKER = module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)
ORIGINAL = (FIXTURES / "budget_before.rs").read_bytes()


def candidate_with(extra_token_line=b""):
    prefix, _, suffix = CHECKER.region(ORIGINAL)
    methods = b"""    pub fn charge_tokens(&self, n: u64) {
        self.chain().for_each(|x| {
            let _ = x.tokens.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| Some(current.saturating_add(n).min(u64::MAX)));
            EXTRA_TOKEN_LINE
        });
    }

    pub fn charge_tool(&self) {
        self.chain().for_each(|x| {
            let _ = x.tools.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| Some(current.saturating_add(1).min(u64::MAX)));
        });
    }

    pub fn charge_cents(&self, n: u64) {
        self.chain().for_each(|x| {
            let _ = x.cents.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| Some(current.saturating_add(n).min(u64::MAX)));
        });
    }
""".replace(b"            EXTRA_TOKEN_LINE\n", extra_token_line)
    return prefix + methods + suffix


class FoundryCodeCheckerTests(unittest.TestCase):
    def test_saturating_replacement_passes(self):
        CHECKER.validate_candidate(ORIGINAL, candidate_with())

    def test_malformed_qualified_path_is_refused(self):
        candidate = candidate_with(b"            let _ = usize::MAX;\n")
        with self.assertRaises(ValueError):
            CHECKER.validate_candidate(ORIGINAL, candidate)

    def test_changes_outside_charge_methods_are_refused(self):
        candidate = candidate_with()
        with self.assertRaises(ValueError):
            CHECKER.validate_candidate(ORIGINAL, b"x" + candidate[1:])


if __name__ == "__main__":
    unittest.main()
