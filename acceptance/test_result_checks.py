import ast
import json
from pathlib import Path
import unittest


SOURCE = Path(__file__).with_name("windows_acceptance.py")
TREE = ast.parse(SOURCE.read_text(encoding="utf-8"))
RESULT_CLASS = next(node for node in TREE.body if isinstance(node, ast.ClassDef) and node.name == "Result")
NAMESPACE = {
    "Path": Path,
    "now": lambda: "fixture-time",
    "jdump": lambda *_args: None,
    "json": json,
    "log": lambda *_args: None,
    "redact": lambda value: value,
    "Fail": type("Fail", (Exception,), {}),
}
exec(compile(ast.Module(body=[RESULT_CLASS], type_ignores=[]), str(SOURCE), "exec"), NAMESPACE)
Result = NAMESPACE["Result"]
Fail = NAMESPACE["Fail"]


def new_result():
    result = Result.__new__(Result)
    result.doc = {"checks": {}, "failure_reason": None}
    result.save = lambda: None
    return result


class ResultChecksTests(unittest.TestCase):
    def test_health_details_may_include_ok_and_name(self):
        result = new_result()
        health = {"ok": True, "name": "gateway readiness", "http": 200}
        self.assertTrue(result.check("gateway_health", health["ok"], **health))
        self.assertEqual(result.doc["checks"]["gateway_health"], {"passed": True, **health})

    def test_failed_require_keeps_health_details_and_raises(self):
        result = new_result()
        health = {"ok": False, "name": "gateway readiness", "http": 503, "reason": "not ready"}
        with self.assertRaises(Fail):
            result.require("gateway_health", health["ok"], **health)
        self.assertEqual(result.doc["checks"]["gateway_health"], {"passed": False, **health})
        self.assertIn("not ready", result.doc["failure_reason"])


if __name__ == "__main__":
    unittest.main()
