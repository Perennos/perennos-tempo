import ast
from pathlib import Path, PureWindowsPath
import re
import unittest


SOURCE = Path(__file__).with_name("windows_acceptance.py")
TREE = ast.parse(SOURCE.read_text(encoding="utf-8"))
HELPER = next(node for node in TREE.body if isinstance(node, ast.FunctionDef) and node.name == "parse_drive_space")
NAMESPACE = {"re": re}
exec(compile(ast.Module(body=[HELPER], type_ignores=[]), str(SOURCE), "exec"), NAMESPACE)
parse_drive_space = NAMESPACE["parse_drive_space"]


class ConsentSizeParsingTests(unittest.TestCase):
    def test_parses_actual_disk_plan_drive_and_plan_sentence(self):
        # _disk_plan uses path.drive.lower(); plan appends ': require'.
        drive = PureWindowsPath('C:/TempoCI/app').drive.lower()
        text = f"Required space by drive: {drive}: require 16,670,151,461 bytes, 30,316,859,392 bytes free"
        self.assertIn('c:: require', text)
        self.assertEqual(parse_drive_space(text), [
            {"drive": "C", "required": 16670151461, "free": 30316859392},
        ])

    def test_keeps_compatible_rooted_drive_format(self):
        text = "Required space by drive: C:\\: require 16,670,151,461 bytes, 30,316,859,392 bytes free"
        self.assertEqual(parse_drive_space(text), [
            {"drive": "C", "required": 16670151461, "free": 30316859392},
        ])

    def test_keeps_accepting_existing_drive_format_and_multiple_drives(self):
        text = "C: require 100 bytes, 200 bytes free; d:\\: require 300 bytes, 400 bytes free"
        self.assertEqual(parse_drive_space(text), [
            {"drive": "C", "required": 100, "free": 200},
            {"drive": "D", "required": 300, "free": 400},
        ])


if __name__ == "__main__":
    unittest.main()
