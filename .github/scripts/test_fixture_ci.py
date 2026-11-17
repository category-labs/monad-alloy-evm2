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
        self.report = {
            "schema_version": 3, "verification": "valid_block_execution",
            "summary": {
                "total": 2, "passed": 1, "failed": 1, "unsupported": 0,
                "known_spec_model_divergences": 1,
            },
            "cases": [
                {"file": str(self.file), "name": "pass", "network": "MONAD_NINE", "status": "passed",
                 "message": "execution commitments matched",
                 "known_spec_model_divergence": None},
                {"file": str(self.file), "name": "known", "network": "MONAD_EIGHT", "status": "failed",
                 "message": "known receipts root mismatch",
                 "known_spec_model_divergence": "gas_fees_exceed_reserve"},
            ],
        }
        self.expected = {
            "spec_revision": "a" * 40,
            "summary": self.report["summary"].copy(),
            "spec_model_divergences": [{"file": "cases.json", "name": "known", "network": "MONAD_EIGHT",
                                        "message": "known receipts root mismatch",
                                        "known_spec_model_divergence": "gas_fees_exceed_reserve"}],
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

    def test_requires_the_exact_spec_model_classification(self):
        for index, classification in [(0, "gas_fees_exceed_reserve"), (1, None), (1, "other")]:
            report = copy.deepcopy(self.report)
            report["cases"][index]["known_spec_model_divergence"] = classification
            with self.subTest(index=index, classification=classification), self.assertRaises(ValueError):
                self.check(report)
        for index in (0, 1):
            report = copy.deepcopy(self.report)
            del report["cases"][index]["known_spec_model_divergence"]
            with self.subTest(missing=index), self.assertRaises(ValueError):
                self.check(report)

    def test_rejects_changed_counters_schema_and_exit_code(self):
        for counter in self.report["summary"]:
            report = copy.deepcopy(self.report)
            report["summary"][counter] += 1
            with self.subTest(counter=counter), self.assertRaises(ValueError):
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
            summary.unlink(missing_ok=True)

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
                arguments = ["--runner", "runner", "--fixtures", str(fixtures), "--report", str(output)]
                code = fixture_ci.main(arguments)
            self.assertEqual(code, 0 if passes else 1)
            self.assertEqual(json.loads(output.read_text()), report)
            if passes:
                message = (
                    "Fixture gate passed: 1 passes; known spec-model divergences: 1, "
                    "unexpected failures: 0, unsupported: 0."
                )
            else:
                message = (
                    "Fixture gate failed: known spec-model divergence changed: "
                    "('cases.json', 'known', 'MONAD_EIGHT')"
                )
            self.assertEqual(summary.read_text(), f"### Monad execution fixtures\n\n{message}\n")


if __name__ == "__main__":
    unittest.main()
