#!/usr/bin/env python3
"""Summarize one JSONL run; 95% percentile CIs resample whole sample batches."""
import argparse
import json
import math
from pathlib import Path
import random
import statistics


def percentile(values, fraction):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * fraction) - 1)]


def metrics(rows):
    calls = [duration for row in rows for duration in row["latency_ns"]]
    operations = sum(row["iterations"] for row in rows)
    elapsed = sum(row["elapsed_ns"] for row in rows)
    return {
        "mean_operation_ns": statistics.mean(calls),
        "p95_operation_ns": percentile(calls, .95),
        "operations_per_second": operations * 1e9 / elapsed,
        "items_per_second": sum(row["iterations"] * row["items_per_operation"] for row in rows) * 1e9 / elapsed,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--bootstrap", type=int, default=2000)
    args = parser.parse_args()
    if args.bootstrap < 100:
        parser.error("at least 100 bootstrap replicates required")
    groups = {}
    runs = []
    completed = []
    for line in args.log.read_text().splitlines():
        if not line.startswith("{"):
            continue
        record = json.loads(line)
        if record.get("kind") == "run":
            runs.append(record)
        if record.get("kind") == "complete":
            completed.append(record)
        if record.get("kind") == "sample":
            assert len(record["latency_ns"]) == record["iterations"]
            assert all(value > 0 for value in record["latency_ns"])
            key = (record["backend"], record["case"], record["items_per_operation"])
            groups.setdefault(key, []).append(record)
    if not groups:
        parser.error("no valid samples found")
    if len(runs) != 1 or len(completed) != 1:
        parser.error("expected exactly one successfully completed run; do not summarize partial/concatenated logs")
    run = runs[0]
    expected_cases = 4 if run["build"]["backend"] == "jni" else 9
    if len(groups) != (expected_cases if run["case"] == "all" else 1):
        parser.error("incomplete workload coverage")
    for (backend, case, items), rows in groups.items():
        if sorted(r["sample"] for r in rows) != list(range(run["samples"])):
            parser.error("missing or duplicate sample indices")
        rng = random.Random(0)
        values = metrics(rows)
        bootstrap = [metrics(rng.choices(rows, k=len(rows))) for _ in range(args.bootstrap)]
        intervals = {name: [percentile([r[name] for r in bootstrap], .025),
                            percentile([r[name] for r in bootstrap], .975)] for name in values}
        print(json.dumps({"backend": backend, "case": case, "items_per_operation": items,
                          "samples": len(rows), "calibration_only": len(rows) < 30 or "calibration" in run["label"],
                          **values, "ci95": intervals, "ci_method": "percentile bootstrap of whole batches; seed=0"}))


if __name__ == "__main__":
    main()
