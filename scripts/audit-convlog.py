"""Count convlog dialogue lines and dump the tail from a given line index."""

import sys

PATH = ".pi/decision-auditor/convlog.md"


def lines():
    with open(PATH, encoding="utf-8") as f:
        return f.read().splitlines()


def main() -> int:
    raw = lines()
    idx = []
    for i, line in enumerate(raw):
        if line.startswith("## 👤") or line.startswith("## 🤖"):
            idx.append(i)
    total = len(idx)
    print("convlines:", total)
    start = int(sys.argv[1]) if len(sys.argv) > 1 else max(0, total - 25)
    for n in range(start + 1, total + 1):
        line = raw[idx[n - 1]]
        tag = "run:" + (line.split("run:")[1][:14] if "run:" in line else "NO-MARK")
        text = line[:160].replace("\n", " ")
        print(f"[{n}][{tag}] {text}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
