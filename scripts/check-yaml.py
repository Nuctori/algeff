"""Minimal YAML validator for CI workflow files."""

import sys

import yaml


def main() -> int:
    """Validate that each file argument parses as YAML."""
    if len(sys.argv) < 2:
        print("usage: check-yaml.py <file>...", file=sys.stderr)
        return 2
    for path in sys.argv[1:]:
        try:
            with open(path, encoding="utf-8") as f:
                yaml.safe_load(f)
        except (OSError, yaml.YAMLError) as e:
            print(f"yaml FAIL {path}: {e}", file=sys.stderr)
            return 1
        print(f"yaml OK: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
