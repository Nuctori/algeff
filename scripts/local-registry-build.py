#!/usr/bin/env python3
"""发布演练辅助：把 `cargo package` 产物组装成本地 sparse registry。

用法（工作区根目录执行）：
    python scripts/local-registry-build.py <registry-dir> [crate...]

产物布局（consumer 侧 .cargo/config.toml 用 `sparse+file://` 引用 index）：
    <registry-dir>/index/config.json          # {"dl": "file://<registry-dir>/dl", "api": null}
    <registry-dir>/index/al/ge/algeff-core    # 每版本一行 JSON（crates.io index v2）
    <registry-dir>/dl/algeff-core/algeff-core-0.1.0.crate

仅做组装，不真实发布、不联网。包内容来自 `cargo package` 产物
（target/package/<name>-<version>.crate，已是发布归一化清单）。
"""
import hashlib
import json
import os
import re
import shutil
import sys
import tarfile
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # Python 3.10 及以下回退 tomli
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ModuleNotFoundError:
        raise SystemExit("需要 tomllib（Python ≥3.11）或 tomli 包")

WORKSPACE = Path.cwd().resolve()
TARGET_PKG = WORKSPACE / "target" / "package"

# 本地 registry 内的 crate 名单：这些名字的依赖解析到本地（deps.registry 置 null）；
# 其余依赖（tokio/syn/…）指向 crates.io 规范索引 URL（经本机 source 替换走镜像）。
LOCAL_CRATES = {"algeff-core", "algeff-std", "algeff-macro"}
CRATES_IO_INDEX = "https://github.com/rust-lang/crates.io-index"


def sha256_hex(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def dep_entry(name: str, spec: dict, default_features: bool) -> dict:
    """把 Cargo.toml 的依赖条目映射为 index 的 deps 条目。"""
    req = spec.get("version", "*")
    if req == "*" and not spec.get("path"):
        raise SystemExit(f"依赖 {name} 无 version 且无 path，无法写入 index")
    features = spec.get("features", [])
    optional = spec.get("optional", False)
    kind = "dev" if spec.get("dev", False) else "normal"
    # path-only 依赖（dev-dep）在发布清单中会被 cargo 剔除，这里兜底跳过
    if "path" in spec and "version" not in spec:
        return None
    entry = {
        "name": name,
        "req": req,
        "features": features,
        "optional": optional,
        "default_features": default_features,
        "target": None,
        "kind": kind,
        "registry": None if name in LOCAL_CRATES else CRATES_IO_INDEX,
    }
    # 同一 crate 内多个同 name 条目（如 dev+normal）需区分
    if "package" in spec:
        entry["package"] = spec["package"]
    return entry


def parse_dep_tables(manifest: dict, crate_name: str) -> list:
    deps = []
    for table, is_dev in (("dependencies", False), ("dev-dependencies", True), ("build-dependencies", False)):
        for name, spec in manifest.get(table, {}).items():
            if isinstance(spec, str):
                spec = {"version": spec}
            else:
                spec = dict(spec)
            if "path" in spec and "version" not in spec:
                # 发布清单不应残留 path-only 依赖；剔除并记录
                print(f"  [跳过 path-only {table} 依赖] {crate_name} -> {name}")
                continue
            spec["dev"] = is_dev
            entry = dep_entry(name, spec, default_features=spec.get("default-features", True))
            if entry:
                deps.append(entry)
    return deps


def build_index(crate_file: Path, index_dir: Path) -> str:
    name_vers = crate_file.stem  # <name>-<version>
    name, vers = name_vers.rsplit("-", 1)
    with tarfile.open(crate_file, "r:gz") as tf:
        manifest_path = f"{name_vers}/Cargo.toml"
        with tf.extractfile(manifest_path) as f:
            manifest = tomllib.loads(f.read().decode("utf-8"))
    deps = parse_dep_tables(manifest, name)
    cksum = sha256_hex(crate_file)
    features = manifest.get("features", {})
    entry = {
        "name": name,
        "vers": vers,
        "deps": deps,
        "cksum": cksum,
        "features": features,
        "yanked": False,
        "links": manifest.get("links"),
        "v": 2,
    }
    # crates.io 索引路径规则：1 字符→1/name，2→2/name，3→3/x/name，4+→ab/cd/name
    if len(name) == 1:
        prefix = "1"
    elif len(name) == 2:
        prefix = "2"
    elif len(name) == 3:
        prefix = f"3/{name[0]}"
    else:
        prefix = f"{name[:2]}/{name[2:4]}"
    idx_file = index_dir / prefix / name
    idx_file.parent.mkdir(parents=True, exist_ok=True)
    with open(idx_file, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    return name_vers


def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    registry_dir = Path(sys.argv[1]).resolve()
    crates = sys.argv[2:] or ["algeff-core", "algeff-std", "algeff-macro"]
    index_dir = registry_dir / "index"
    index_dir.mkdir(parents=True, exist_ok=True)

    # cargo 本地 registry 下载约定：dl 指向 registry 根，.crate 内容置于
    # <dl>/<name>/<version>/download（本地 registry 布局，见 cargo-local-registry）。
    dl_url = "file:///" + registry_dir.as_posix().lstrip("/")
    (index_dir / "config.json").write_text(
        json.dumps({"dl": dl_url, "api": None}), encoding="utf-8"
    )
    print(f"index: {index_dir.resolve()}  (dl = {dl_url})")

    for name in crates:
        # 精确匹配 <name>-<version>.crate（可能命中同名不同版）
        candidates = sorted(
            (p for p in TARGET_PKG.glob(f"{name}-*.crate")),
            key=lambda p: p.name,
        )
        if not candidates:
            print(f"  [缺失] target/package 无 {name}-*.crate")
            sys.exit(1)
        for crate_file in candidates:
            name_vers = build_index(crate_file, index_dir)
            vers = name_vers.rsplit("-", 1)[1]
            dest_dir = registry_dir / name / vers
            dest_dir.mkdir(parents=True, exist_ok=True)
            shutil.copy2(crate_file, dest_dir / "download")
            shutil.copy2(crate_file, dest_dir / crate_file.name)
            print(f"  [OK] {name_vers} -> {dest_dir.relative_to(WORKSPACE)}/download")


if __name__ == "__main__":
    main()
