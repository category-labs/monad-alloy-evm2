"""Check the pinned corpus and exact known spec-model divergences."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def identity(case):
    return case["file"], case["name"], case["network"]


def inventory(root):
    """Read case identities independently of the runner's report."""
    result = set()
    for file in sorted(root.rglob("*.json")):
        if file.name == "index.json":
            continue
        entries = json.loads(file.read_text())
        for name, case in entries.items():
            network = case.get("config", {}).get("network", case.get("network"))
            result.add((file.relative_to(root).as_posix(), name, network))
    return result


def check_report(report, root, expected, runner_exit_code):
    """Reject missing coverage, new failures and any changed known outcome."""
    if (
        not isinstance(report, dict)
        or report.get("schema_version") != 3
        or report.get("verification") != "valid_block_execution"
    ):
        raise ValueError("unsupported fixture report schema or verification scope")
    cases = report.get("cases")
    if not isinstance(cases, list):
        raise ValueError("missing fixture cases")
    root = root.resolve()
    expected_cases = inventory(root)
    divergences = {identity(case): case for case in expected["spec_model_divergences"]}
    seen = set()
    totals = dict.fromkeys(expected["summary"], 0)
    for case in cases:
        file = Path(case["file"])
        if not file.is_absolute():
            raise ValueError("report paths must be absolute")
        relative = file.resolve().relative_to(root).as_posix()
        key = relative, case["name"], case["network"]
        if key in seen or key not in expected_cases:
            raise ValueError(f"duplicate or unexpected fixture result: {key}")
        seen.add(key)
        if "known_spec_model_divergence" not in case:
            raise ValueError(f"missing spec-model classification: {key}")
        classification = case["known_spec_model_divergence"]
        known_divergence = divergences.get(key)
        if known_divergence:
            if (
                case["status"] != "failed"
                or case["message"] != known_divergence["message"]
                or classification != known_divergence["known_spec_model_divergence"]
            ):
                raise ValueError(f"known spec-model divergence changed: {key}")
        elif case["status"] != "passed":
            raise ValueError(f"unexpected fixture outcome: {key}: {case['message']}")
        elif classification is not None:
            raise ValueError(f"unexpected spec-model classification: {key}")

        totals["total"] += 1
        totals[case["status"]] += 1
        totals["known_spec_model_divergences"] += int(classification is not None)
    if seen != expected_cases or not seen:
        raise ValueError("fixture results are incomplete or empty")
    if not divergences.keys() <= seen:
        raise ValueError("known fixture identities are missing")
    if report.get("summary") != totals or totals != expected["summary"]:
        raise ValueError(f"unexpected fixture totals: {totals}")
    expected_exit = int(totals["failed"] > 0)
    if runner_exit_code != expected_exit:
        raise ValueError(f"runner exited {runner_exit_code}; expected {expected_exit}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        expected = json.loads(Path(__file__).with_name("fixture_expectations.json").read_text())
        revision = subprocess.check_output(
            ["git", "-C", str(args.fixtures.resolve()), "rev-parse", "HEAD"], text=True
        ).strip()
        if revision != expected["spec_revision"]:
            raise ValueError(f"expected fixture revision {expected['spec_revision']}, got {revision}")
        args.report.parent.mkdir(parents=True, exist_ok=True)
        with args.report.open("wb") as output:
            result = subprocess.run(
                [str(args.runner.resolve()), str(args.fixtures.resolve()), "--json", "--fail-on-unsupported"],
                stdout=output,
                check=False,
            )
        report = json.loads(args.report.read_text())
        check_report(report, args.fixtures, expected, result.returncode)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        message = f"Fixture gate failed: {error}"
        exit_code = 1
    else:
        summary = report["summary"]
        message = (
            f"Fixture gate passed: {summary['passed']} passes; "
            f"known spec-model divergences: {summary['known_spec_model_divergences']}, "
            f"unexpected failures: {summary['failed'] - summary['known_spec_model_divergences']}, "
            f"unsupported: {summary['unsupported']}."
        )
        exit_code = 0
    print(message, file=sys.stderr if exit_code else sys.stdout)
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a") as output:
            output.write(f"### Monad execution fixtures\n\n{message}\n")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
