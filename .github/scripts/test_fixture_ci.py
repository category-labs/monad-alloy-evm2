"""Acceptance-policy regressions using a small independent fixture inventory."""

import contextlib
import copy
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import fixture_ci


class FixtureGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.file = self.root / "cases.json"
        self.file.write_text(json.dumps({
            "pass": {"config": {"network": "MONAD_NINE"}},
            "known": {"config": {"network": "MONAD_EIGHT"}},
        }))
        actual = {
            "status": "reserve_balance_violation", "gas_spent": 21000,
            "refunded": 0, "receipt_gas": 50000, "output": "0x", "logs": [],
            "created_address": None, "state_root": "0x" + "11" * 32,
        }
        difference = {
            "at": "block 1, transaction 0", "actual": actual,
            "reference": {**actual, "status": "success", "state_root": "0x" + "22" * 32},
            "reference_error": None,
        }
        self.report = {
            "schema_version": 1, "verification": "valid_block_execution_and_reference_parity",
            "summary": {
                "total": 2, "passed": 1, "failed": 1, "unsupported": 0,
                "parity_checks": 2, "parity_differences": 1, "corrected_reference_receipts": 2,
            },
            "cases": [
                {"file": str(self.file), "name": "pass", "network": "MONAD_NINE", "status": "passed",
                 "message": "execution commitments matched",
                 "parity": {"checks": 1, "corrected_receipts": 1, "differences": [difference]}},
                {"file": str(self.file), "name": "known", "network": "MONAD_EIGHT", "status": "failed",
                 "message": "known receipts root mismatch",
                 "parity": {"checks": 1, "corrected_receipts": 1, "differences": []}},
            ],
        }
        self.expected = {
            "spec_revision": "a" * 40, "reference": "monad-revm 0.8.0",
            "summary": self.report["summary"].copy(),
            "fixture_failures": [{"file": "cases.json", "name": "known", "network": "MONAD_EIGHT",
                                  "message": "known receipts root mismatch"}],
            "reserve_differences": [{"file": "cases.json", "name": "pass", "network": "MONAD_NINE",
                                     "count": 1, "sha256": fixture_ci.digest([difference])}],
        }

    def check(self, report=None, exit_code=1):
        fixture_ci.check_report(self.report if report is None else report, self.root, self.expected, exit_code)

    def test_accepts_exact_results_without_rewriting_failures(self):
        original = copy.deepcopy(self.report)
        self.check()
        self.assertEqual(original, self.report)

    def test_rejects_missing_extra_duplicate_and_substituted_cases(self):
        variants = [[], self.report["cases"][:1], self.report["cases"] * 2]
        substituted = copy.deepcopy(self.report["cases"])
        substituted[0]["name"] = "different"
        variants.append(substituted)
        for cases in variants:
            with self.subTest(cases=cases):
                report = {**self.report, "cases": cases}
                with self.assertRaises(ValueError):
                    self.check(report)

    def test_rejects_new_fixture_outcomes_and_changed_known_failure(self):
        for index, key, value in [
            (0, "status", "failed"), (0, "status", "unsupported"),
            (1, "status", "passed"), (1, "message", "different mismatch"),
            (1, "network", "MONAD_NINE"), (0, "file", "cases.json"),
            (0, "file", str(self.root.parent / "outside.json")),
        ]:
            with self.subTest(index=index, key=key, value=value):
                report = copy.deepcopy(self.report)
                report["cases"][index][key] = value
                with self.assertRaises(ValueError):
                    self.check(report)

    def test_rejects_any_changed_parity_observation(self):
        for side in ("actual", "reference"):
            for key, value in {
                "status": "revert", "gas_spent": 21001, "refunded": 1,
                "receipt_gas": 21000, "state_root": "0x" + "33" * 32,
                "output": "0x01", "logs": [{"address": "changed"}],
                "created_address": "0x" + "44" * 20,
            }.items():
                with self.subTest(side=side, key=key):
                    report = copy.deepcopy(self.report)
                    report["cases"][0]["parity"]["differences"][0][side][key] = value
                    with self.assertRaises(ValueError):
                        self.check(report)

    def test_rejects_new_missing_and_failed_reference_comparisons(self):
        for change in ("new", "missing", "error", "location"):
            report = copy.deepcopy(self.report)
            differences = report["cases"][0]["parity"]["differences"]
            if change == "new":
                report["cases"][1]["parity"]["differences"] = differences
            elif change == "missing":
                differences.clear()
            elif change == "error":
                differences[0]["reference"] = None
                differences[0]["reference_error"] = "rejected"
            else:
                differences[0]["at"] = "block 0, transaction 0"
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.check(report)

    def test_rejects_changed_counters_schema_and_exit_code(self):
        for counter in self.report["summary"]:
            report = copy.deepcopy(self.report)
            report["summary"][counter] += 1
            with self.subTest(counter=counter), self.assertRaises(ValueError):
                self.check(report)
        for key, value in [("checks", 0), ("checks", -1), ("corrected_receipts", 2), ("checks", True)]:
            report = copy.deepcopy(self.report)
            report["cases"][0]["parity"][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                self.check(report)
        for key in ("verification", "schema_version"):
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.check({**self.report, key: "wrong"})
        for code in (0, 2, -1):
            with self.subTest(code=code), self.assertRaises(ValueError):
                self.check(exit_code=code)

    def test_main_preserves_raw_report_on_success_and_failure(self):
        scripts = self.root / "scripts"
        scripts.mkdir()
        (scripts / "fixture_expectations.json").write_text(json.dumps(self.expected))
        fixtures = self.root / "fixtures"
        fixtures.mkdir()
        self.file.rename(fixtures / "cases.json")
        for case in self.report["cases"]:
            case["file"] = str(fixtures / "cases.json")
        for passes in (True, False):
            report = copy.deepcopy(self.report)
            if not passes:
                report["cases"][1]["message"] = "unexpected"
            output = self.root / "output" / "report.json"
            summary = self.root / "summary.md"

            def run(command, stdout, check):
                stdout.write(json.dumps(report).encode())
                return subprocess.CompletedProcess(command, 1)

            with (
                patch.object(fixture_ci, "__file__", str(scripts / "fixture_ci.py")),
                patch.object(fixture_ci.subprocess, "check_output", return_value="a" * 40),
                patch.object(fixture_ci.subprocess, "run", side_effect=run),
                patch.dict(fixture_ci.os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                code = fixture_ci.main(["--runner", "runner", "--fixtures", str(fixtures), "--report", str(output)])
            self.assertEqual(code, 0 if passes else 1)
            self.assertEqual(json.loads(output.read_text()), report)
            self.assertIn("Fixture gate passed" if passes else "Fixture gate failed", summary.read_text())


if __name__ == "__main__":
    unittest.main()
