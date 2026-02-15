#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path


def percentile(sorted_values: list[float], p: float) -> float:
    if not sorted_values:
        raise ValueError("percentile called with empty data")
    if p <= 0:
        return sorted_values[0]
    if p >= 100:
        return sorted_values[-1]

    # Nearest-rank style percentile over sorted samples.
    rank = int(round((p / 100.0) * (len(sorted_values) - 1)))
    rank = max(0, min(rank, len(sorted_values) - 1))
    return sorted_values[rank]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Summarize orchd live integration timing JSONL output"
    )
    parser.add_argument(
        "--input",
        default="target/live-test-timings.jsonl",
        help="Path to timing JSONL file (default: %(default)s)",
    )
    parser.add_argument(
        "--last",
        type=int,
        default=0,
        help="Only analyze the last N timing rows (0 = all rows)",
    )
    return parser.parse_args()


def load_rows(path: Path) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    with path.open("r", encoding="utf-8") as handle:
        for line_no, raw in enumerate(handle, start=1):
            line = raw.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as exc:
                raise SystemExit(
                    f"invalid JSON at {path}:{line_no}: {exc}"
                ) from exc

            if not isinstance(value, dict):
                raise SystemExit(f"invalid row at {path}:{line_no}: expected object")
            rows.append(value)
    return rows


def main() -> None:
    args = parse_args()
    input_path = Path(args.input)
    if not input_path.exists():
        raise SystemExit(f"timing file not found: {input_path}")

    rows = load_rows(input_path)
    if args.last > 0:
        rows = rows[-args.last :]

    if not rows:
        print(f"no rows to summarize in {input_path}")
        return

    by_step: dict[str, list[float]] = defaultdict(list)
    by_test: dict[str, list[float]] = defaultdict(list)

    for row in rows:
        step = row.get("step")
        test = row.get("test")
        elapsed = row.get("elapsed_ms")
        if not isinstance(step, str) or not isinstance(test, str):
            continue
        if not isinstance(elapsed, (int, float)):
            continue
        by_step[step].append(float(elapsed))
        by_test[test].append(float(elapsed))

    def render_table(title: str, series: dict[str, list[float]]) -> None:
        print(title)
        print(
            f"{'name':40} {'count':>5} {'avg':>8} {'p50':>8} {'p95':>8} {'max':>8}"
        )
        print(f"{'-' * 40} {'-' * 5} {'-' * 8} {'-' * 8} {'-' * 8} {'-' * 8}")

        ranked = sorted(
            series.items(),
            key=lambda item: percentile(sorted(item[1]), 95),
            reverse=True,
        )

        for name, samples in ranked:
            ordered = sorted(samples)
            avg = sum(ordered) / len(ordered)
            p50 = percentile(ordered, 50)
            p95 = percentile(ordered, 95)
            max_v = ordered[-1]
            print(
                f"{name:40} {len(ordered):5d} {avg:8.1f} {p50:8.1f} {p95:8.1f} {max_v:8.1f}"
            )
        print()

    print(f"input: {input_path}")
    print(f"rows analyzed: {len(rows)}")
    print()
    render_table("By step", by_step)
    render_table("By test", by_test)


if __name__ == "__main__":
    main()
