#!/usr/bin/env python3
"""Fail on high/critical CodeQL findings, including failures outside a PR diff."""
import json
from pathlib import Path
import sys


def check(directory):
    files = list(Path(directory).rglob("*.sarif"))
    if not files:
        raise ValueError("CodeQL did not produce a SARIF report")
    findings = []
    for path in files:
        report = json.loads(path.read_text())
        runs = report.get("runs", [])
        if not runs:
            raise ValueError(f"Empty CodeQL report: {path}")
        for run in runs:
            if any(i.get("executionSuccessful") is False for i in run.get("invocations", [])):
                raise ValueError(f"CodeQL analysis failed: {path}")
            driver = run["tool"]["driver"]
            rules = {r["id"]: r for r in driver.get("rules", [])}
            for result in run.get("results", []):
                if any(s.get("status") == "accepted" for s in result.get("suppressions", [])):
                    continue
                rule_id = result.get("ruleId", "unknown")
                severity = rules.get(rule_id, {}).get("properties", {}).get("security-severity", "0")
                if float(severity) >= 7:
                    findings.append(rule_id)
    for rule in findings:
        print(f"High/critical CodeQL finding: {rule}", file=sys.stderr)
    return 1 if findings else 0


if __name__ == "__main__":
    try:
        sys.exit(check(sys.argv[1]))
    except (ValueError, KeyError, IndexError, OSError) as error:
        print(f"CodeQL gate failed: {error}", file=sys.stderr)
        sys.exit(1)
