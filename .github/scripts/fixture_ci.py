"""Check the pinned fixture corpus and optional, documented reference differences."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def digest(value):
    """Commit to every observation, including logs, output and both state roots."""
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


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


def check_report(report, root, expected, runner_exit_code, compare_monad_revm=False):
    """Reject missing coverage, new differences and any changed known outcome."""
    verification = (
        "valid_block_execution_and_reference_parity" if compare_monad_revm else "valid_block_execution"
    )
    if (
        not isinstance(report, dict)
        or report.get("schema_version") != 1
        or report.get("verification") != verification
    ):
        raise ValueError("unsupported fixture report schema or verification scope")
    cases = report.get("cases")
    if not isinstance(cases, list):
        raise ValueError("missing fixture cases")
    root = root.resolve()
    expected_cases = inventory(root)
    failures = {identity(case): case for case in expected["fixture_failures"]}
    differences = {identity(case): case for case in expected["reserve_differences"]}
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
        known_failure = failures.get(key)
        if known_failure:
            if case["status"] != "failed" or case["message"] != known_failure["message"]:
                raise ValueError(f"known fixture failure changed: {key}")
        elif case["status"] != "passed":
            raise ValueError(f"unexpected fixture outcome: {key}: {case['message']}")

        parity = case["parity"]
        observed = parity["differences"]
        checks = parity["checks"]
        corrections = parity["corrected_receipts"]
        if not all(type(n) is int and n >= 0 for n in (checks, corrections)):
            raise ValueError(f"invalid parity counters: {key}")
        if not isinstance(observed, list) or len(observed) > checks or corrections > checks:
            raise ValueError(f"inconsistent parity counters: {key}")
        known_difference = differences.get(key)
        if not compare_monad_revm:
            if checks != 0 or corrections != 0 or observed:
                raise ValueError(f"unexpected reference comparison in independent mode: {key}")
        elif known_difference:
            for item in observed:
                actual, reference = item["actual"], item["reference"]
                if item["reference_error"] is not None or reference is None:
                    raise ValueError(f"reference execution failed: {key}")
                if (
                    actual["status"] != "reserve_balance_violation"
                    or reference["status"] not in ("success", "halt")
                    or actual["logs"] != []
                    or actual["output"] != "0x"
                    or any(actual[field] != reference[field] for field in (
                        "gas_spent", "refunded", "receipt_gas", "created_address"
                    ))
                ):
                    raise ValueError(f"difference is not the known reserve behavior: {key}")
            if len(observed) != known_difference["count"] or digest(observed) != known_difference["sha256"]:
                raise ValueError(f"known reserve observations changed: {key}")
        elif observed:
            raise ValueError(f"unexpected reference difference: {key}")
        totals["total"] += 1
        totals[case["status"]] += 1
        totals["parity_checks"] += checks
        totals["parity_differences"] += len(observed)
        totals["corrected_reference_receipts"] += corrections
    if seen != expected_cases or not seen:
        raise ValueError("fixture results are incomplete or empty")
    if not failures.keys() <= seen or not differences.keys() <= seen:
        raise ValueError("known fixture identities are missing")
    expected_summary = expected["summary"].copy()
    if not compare_monad_revm:
        for counter in ("parity_checks", "parity_differences", "corrected_reference_receipts"):
            expected_summary[counter] = 0
    if report.get("summary") != totals or totals != expected_summary:
        raise ValueError(f"unexpected fixture totals: {totals}")
    expected_exit = int(totals["failed"] > 0 or totals["parity_differences"] > 0)
    if runner_exit_code != expected_exit:
        raise ValueError(f"runner exited {runner_exit_code}; expected {expected_exit}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument(
        "--compare-monad-revm", action="store_true",
        help="require the comparison-enabled runner and exact reference outcomes",
    )
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
        check_report(report, args.fixtures, expected, result.returncode, args.compare_monad_revm)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        message = f"Fixture gate failed: {error}"
        exit_code = 1
    else:
        summary = report["summary"]
        message = (
            f"Fixture gate passed: {summary['passed']} passes, {summary['failed']} known fixture mismatch, "
            f"{summary['unsupported']} unsupported."
        )
        if args.compare_monad_revm:
            message += (
                f" {summary['parity_checks']} engine comparisons, "
                f"{summary['parity_differences']} exact reserve-enforcement differences; "
                f"{summary['corrected_reference_receipts']} reference receipts normalized."
            )
        exit_code = 0
    print(message, file=sys.stderr if exit_code else sys.stdout)
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a") as output:
            output.write(f"### Monad execution fixtures\n\n{message}\n")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
