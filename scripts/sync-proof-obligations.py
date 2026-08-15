"""Sync docs/src/proof-obligations.md obligation tables from spec/."""

SPEC = "spec/proof-obligations.md"
DOC = "docs/src/proof-obligations.md"

with open(SPEC, encoding="utf-8") as f:
    spec = f.read()
with open(DOC, encoding="utf-8") as f:
    doc = f.read()

start = doc.find("## 义务明细")
end = doc.find("## 更新规则")
spec_start = spec.find("## 义务明细")
spec_end = spec.find("## 更新规则")
if start == -1 or end == -1 or spec_start == -1 or spec_end == -1:
    raise SystemExit(
        f"markers missing: doc {start}/{end}, spec {spec_start}/{spec_end}"
    )

new_doc = doc[:start] + spec[spec_start:spec_end] + doc[end:]
with open(DOC, "w", encoding="utf-8", newline="\n") as f:
    f.write(new_doc)
print("docs obligation tables synced from spec")
