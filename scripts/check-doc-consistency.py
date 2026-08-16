#!/usr/bin/env python3
"""文档一致性**常驻门禁**（R6 块 A6 创建，CI 三平台自动执行；D-074）。

核对事实集合在以下四处是否一致：
  - README.md
  - docs/src/derivation.md
  - spec/resource-notes.md
  - spec/proof-obligations.md

事实集合（canonical = 实测 + main 33704cc 统一口径，supervisor 已裁决）：
  1. 测试计数：410 个测试函数（约 399 二进制 + 11 doc-test），48 个测试二进制 + 3 个 doc-test 运行
  2. 对抗 E2E：182 个（R1-R6=167、R7=15；旧口径 120/125/157/162/164 均不得出现）
  3. RFC-10 状态：已修复
  4. RFC-11 状态：已修复（深度守卫阈值 96 → 迭代 1 复测裁决 64）
  5. CI 三平台：ubuntu / windows / macos
  6. 性能数字：103.1 / 366.2 / 570.9 / 39.1（24.3 仅允许以「修复前旧数」语境出现）
  7. 深度守卫：阈值 64 / 错误码 105 / 左结合 ≥65 步（96/97 仅历史语境）

原则：某文件提及某事实时不得与 canonical 冲突（缺失 ≠ 冲突）；旧口径残留一律报。

退出码：0 = 无差异；1 = 存在差异（打印差异清单）。
"""

import sys

FILES = [
    "README.md",
    "docs/src/derivation.md",
    "spec/resource-notes.md",
    "spec/proof-obligations.md",
]

issues = []


def load(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def check_absent(path, text, label, needle):
    """needle 若出现则报差异（旧口径残留）。"""
    if needle in text:
        issues.append(f"[{path}] 事实「{label}」存在旧口径残留：「{needle}」")


def main():
    texts = {p: load(p) for p in FILES}

    # ---- 1. 测试计数（旧口径残留：305/297/41/44）----
    for p in FILES:
        t = texts[p]
        for stale in (
            "305 个测试函数",
            "305 测试全绿",
            "297 二进制",
            "297 个二进制",
            "44 个测试二进制",
            "41 个测试二进制",
            "309 个测试函数",
            "309 测试全绿",
            "42 个测试二进制",
            "120 个 E2E",
            "125 个测试",
            "157 个",
            "162 个 E2E",
            "164 个 E2E",
            "162 个测试",
        ):
            check_absent(p, t, "测试计数", stale)
        # 新口径应存在（至少一处）
        if any(s in t for s in ("测试函数", "测试全绿", "测试二进制")):
            if "410 个测试函数" not in t and "410 测试全绿" not in t:
                issues.append(f"[{p}] 提及测试计数但缺少 canonical 口径「410」")

        # E2E 正向门禁：提及 E2E 计数即须含 canonical 164
        if any(s in t for s in ("个 E2E", "E2E 测试", "对抗 E2E")):
            if "182" not in t:
                issues.append(f"[{p}] 提及对抗 E2E 但缺少 canonical 口径「182」")

    # ---- 2. RFC-10 状态：提及即须体现已修复 ----
    for p in FILES:
        t = texts[p]
        if "RFC-10" in t and "已修复" not in t:
            issues.append(
                f"[{p}] RFC-10 被提及但无「已修复」标记（历史轮次行除外，人工核对）"
            )

    # ---- 3. RFC-11 状态：提及即须体现已修复 ----
    for p in FILES:
        t = texts[p]
        if "RFC-11" in t and "已修复" not in t and "已修" not in t:
            issues.append(
                f"[{p}] RFC-11 被提及但无「已修复」标记（历史轮次行除外，人工核对）"
            )

    # ---- 4. CI 三平台：提及 CI/三平台时不得出现少于三平台的表述 ----
    for p in FILES:
        t = texts[p]
        if "CI" in t or "三平台" in t:
            if "双平台" in t or "两平台" in t:
                issues.append(f"[{p}] CI 平台表述出现「双平台/两平台」")
            # 若在 CI/三平台语境下枚举平台，必须含三平台（RFC 语境提及 windows 不算）
            ci_lines = [
                l
                for l in t.splitlines()
                if ("CI" in l or "三平台" in l)
                and ("ubuntu" in l or "windows" in l or "macos" in l)
            ]
            for l in ci_lines:
                if not ("ubuntu" in l and "windows" in l and "macos" in l):
                    issues.append(f"[{p}] CI 平台枚举不完整：{l.strip()[:60]}")

    # ---- 5. 性能数字（canonical 含 R6 复测 39.1；24.3 仅历史语境）----
    for p in FILES:
        t = texts[p]
        if any(x in t for x in ("103.1", "366.2", "570.9")):
            for num in ("103.1", "366.2", "570.9", "39.1"):
                if num not in t:
                    issues.append(f"[{p}] 提及性能数据但缺 canonical 数字「{num}」")

    # ---- 6. 深度守卫（64/105/65；96/97 仅历史语境）----
    for p in FILES:
        t = texts[p]
        if any(x in t for x in ("深度守卫", "阈值 64", "Other(105)", "65 步")):
            if "64" not in t or "105" not in t or "65" not in t:
                issues.append(f"[{p}] 提及深度守卫但缺 64/105/65 之一")
        stale96 = ("≥97 步" in t) or ("≥ 97 步" in t) or ("阈值 **96**（CTO" in t) or ("阈值 96（CTO" in t)
        if stale96 and "迭代 1" not in t and "→ **64**" not in t:
            issues.append(f"[{p}] 深度守卫旧口径（97 步/阈值 96）出现但无迭代 1 更新注记")

    # ---- 7. append 24.3% 仅历史语境（canonical 39.1）----
    for p in FILES:
        t = texts[p]
        if "24.3%" in t and "修复前旧数" not in t and "39.1" not in t:
            issues.append(f"[{p}] 24.3% 出现但无「修复前旧数」/39.1 语境（append canonical 现为 39.1）")

    # ---- 8. 合并冲突标记门禁（审查 Note：dx-design 残留教训）----
    for p in FILES + ["docs/src/dx-design.md"]:
        t = texts.get(p) or load(p)
        for marker in ("<<<<<<<", "=======", ">>>>>>>"):
            if marker in t:
                issues.append(f"[{p}] 残留合并冲突标记「{marker}」")

    if issues:
        print("=== 差异清单（{} 项）===".format(len(issues)))
        for i in issues:
            print(" -", i)
        return 1
    print("=== 核对通过：四处文档在 7 组事实集合上一致（410/48+3 口径）===")
    return 0


if __name__ == "__main__":
    sys.exit(main())
