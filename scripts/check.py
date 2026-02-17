#!/usr/bin/env python3

from __future__ import annotations

import subprocess
import sys


def run(argv: list[str]) -> None:
    proc = subprocess.run(argv)
    if proc.returncode != 0:
        raise SystemExit(proc.returncode)


def main() -> None:
    run(["cargo", "fmt", "--all", "--check"])

    clippy_args = [
        "cargo",
        "clippy",
        "--all-targets",
        "--all-features",
        "--",
        "-D",
        "warnings",
        "-D",
        "clippy::all",
        "-D",
        "clippy::pedantic",
        "-D",
        "clippy::nursery",
        "-A",
        "clippy::module_name_repetitions",
        "-A",
        "clippy::missing_errors_doc",
        "-A",
        "clippy::missing_panics_doc",
        "-A",  # style noise; not a correctness signal
        "clippy::needless_pass_by_value",
        "-A",
        "clippy::uninlined_format_args",
        "-A",
        "clippy::map_unwrap_or",
        "-A",
        "clippy::option_if_let_else",
        "-A",
        "clippy::too_many_lines",
        "-A",
        "clippy::print_literal",
    ]

    run(clippy_args)
    run(["cargo", "test"])
    run(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "orchd",
            "--",
            "--dispatch-config",
            "config/orchd-dispatch.toml",
            "role",
            "check",
            "--offline",
        ]
    )
    run([sys.executable, "scripts/verify_skill_sync.py"])


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
