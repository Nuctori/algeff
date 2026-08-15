"""Sync docs/src/proof-obligations.md sections (round log + obligation tables) from spec/."""

SPEC = "spec/proof-obligations.md"
DOC = "docs/src/proof-obligations.md"

with open(SPEC, encoding="utf-8") as f:
    spec = f.read()
with open(DOC, encoding="utf-8") as f:
    doc = f.read()

# Section 1: 轮次日志 (## 轮次日志 → ## 义务明细)
log_start = doc.find("## 轮次日志")
log_end = doc.find("## 义务明细")
spec_log_start = spec.find("## 轮次日志")
spec_log_end = spec.find("## 义务明细")
if -1 in (log_start, log_end, spec_log_start, spec_log_end):
    raise SystemExit(f"round-log markers missing: doc {log_start}/{log_end}, spec {spec_log_start}/{spec_log_end}")
doc = doc[:log_start] + spec[spec_log_start:spec_log_end] + doc[log_end:]

# Section 2: 义务明细 (## 义务明细 → ## 更新规则)
start = doc.find("## 义务明细")
end = doc.find("## 更新规则")
spec_start = spec.find("## 义务明细")
spec_end = spec.find("## 更新规则")
if -1 in (start, end, spec_start, spec_end):
    raise SystemExit(f"obligation-table markers missing: doc {start}/{end}, spec {spec_start}/{spec_end}")
doc = doc[:start] + spec[spec_start:spec_end] + doc[end:]

with open(DOC, "w", encoding="utf-8", newline="\n") as f:
    f.write(doc)
print("docs round log + obligation tables synced from spec")
