#!/usr/bin/env python3
"""修复 runtime.rs 冲突标记 + Timeout 臂融合（VC + 取消传播）。"""
import re

p = "crates/algeff-core/src/runtime.rs"
with open(p, encoding="utf-8") as f:
    src = f.read()

# 1) 删除所有冲突标记行
lines = src.split("\n")
out = []
for ln in lines:
    if ln.strip() in ("<<<<<<< HEAD", "=======", ">>>>>>> iter1/it1-rfc0809"):
        continue
    out.append(ln)
src = "\n".join(out)

# 2) HEAD 旧墙钟 match 块删除：从 "#[allow(unreachable_code)]" 后的
#    "match tokio::time::timeout(" 到 "}"（旧 Elapsed 分支结束）与取消传播
#    注释之间；保留取消传播块本身。
pat_old_wall = re.compile(
    r"                #\[allow\(unreachable_code\)\]\n"
    r"                match tokio::time::timeout\(\n"
    r"(?:.*?\n)*?"
    r"                \}\n"
    r"                // ── 取消传播协议",
    re.DOTALL,
)
m = pat_old_wall.search(src)
if not m:
    print("PATTERN-OLD-WALL: NOT FOUND")
else:
    src = src[: m.start()] + "                #[allow(unreachable_code)]\n" + src[m.end() :]
    print("PATTERN-OLD-WALL: removed", len(m.group(0)), "chars")

# 3) VC 块内 run_virtual_timeout 调用补 cancel 参数
pat_vc = re.compile(
    r"                    return run_virtual_timeout\(\n"
    r"                        \*inner,\n"
    r"                        \*on_timeout,\n"
    r"                        duration,\n"
    r"                        ctx,\n"
    r"                        undo,\n"
    r"                        reg,\n"
    r"                        access\.reborrow\(\),\n"
    r"                        depth,\n"
    r"                    \)"
)
m = pat_vc.search(src)
if not m:
    print("PATTERN-VC-CALL: NOT FOUND")
else:
    src = src[: m.end()] + "\n                        cancel.as_deref_mut()," + src[m.end() :]
    print("PATTERN-VC-CALL: patched")

with open(p, "w", encoding="utf-8", newline="\n") as f:
    f.write(src)
print("done")
