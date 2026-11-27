#!/usr/bin/env python3
"""核对补丁镜像：
  1. 与官方系统相比，内容改动集合必须与预期清单完全一致（不多、不少）；
  2. 条目数不变；
  3. 所有条目的权限/属主/链接目标不变（大小可以因补丁而变）。

只看元数据发现不了「同样大小的内容改动」，所以内容比较用解开的目录做 MD5；
元数据比较用两个 squashfs 镜像的 `unsquashfs -lls`。

用法：python3 -I verify_build.py <官方rootfs目录> <补丁rootfs目录> <官方.squashfs> <补丁.squashfs> <预期清单.txt>
预期清单：每行一个相对路径，空行和 # 开头的行忽略。

退出码：0 全部通过；1 有不符（逐条打印原因）。
"""
import hashlib
import os
import re
import stat
import subprocess
import sys

LINE = re.compile(r"^(\S+) (\S+)\s+(\S+(?:,\s*\d+)?) \S+ \S+ squashfs-root(.*)$")


def listing(img):
    out = subprocess.run(["unsquashfs", "-lls", img], capture_output=True, text=True, check=True).stdout
    items = {}
    for line in out.splitlines():
        m = LINE.match(line)
        if not m:
            continue
        mode, owner, size, rest = m.groups()
        path, _, target = rest.partition(" -> ")
        path = path.lstrip("/")
        if mode[0] == "d":
            size = ""  # 目录大小随打包方式变，不比
        items[path or "/"] = (mode, owner, size, target)
    return items


def scan(root):
    out = {}
    for d, dirs, files in os.walk(root, followlinks=False):
        for n in dirs + files:
            p = os.path.join(d, n)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            st = os.lstat(p)
            if stat.S_ISLNK(st.st_mode):
                out[rel] = ("link", os.readlink(p))
            elif stat.S_ISREG(st.st_mode):
                h = hashlib.md5()
                with open(p, "rb") as f:
                    for chunk in iter(lambda: f.read(1 << 20), b""):
                        h.update(chunk)
                out[rel] = ("file", h.hexdigest(), st.st_size)
            elif stat.S_ISDIR(st.st_mode):
                out[rel] = ("dir",)
            else:
                out[rel] = ("other",)
    return out


def main(old_dir, new_dir, old_img, new_img, expected_file):
    expected = set()
    with open(expected_file, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#"):
                expected.add(line)

    errors = []

    old_tree, new_tree = scan(old_dir), scan(new_dir)
    added = set(new_tree) - set(old_tree)
    removed = set(old_tree) - set(new_tree)
    content_changed = {p for p in set(old_tree) & set(new_tree) if old_tree[p] != new_tree[p]}

    for p in sorted(added):
        errors.append(f"新增条目：{p}")
    for p in sorted(removed):
        errors.append(f"删除条目：{p}")
    for p in sorted(content_changed - expected):
        errors.append(f"意外改动：{p}")
    for p in sorted(expected - content_changed):
        errors.append(f"预期改动缺失：{p}")

    old_list, new_list = listing(old_img), listing(new_img)
    if len(old_list) != len(new_list):
        errors.append(f"条目数变了：{len(old_list)} -> {len(new_list)}")
    for p in sorted(set(old_list) & set(new_list)):
        o, n = old_list[p], new_list[p]
        if o[0] != n[0]:
            errors.append(f"{p}：权限变了 {o[0]} -> {n[0]}")
        if o[1] != n[1]:
            errors.append(f"{p}：属主变了 {o[1]} -> {n[1]}")
        if o[3] != n[3]:
            errors.append(f"{p}：链接目标变了 {o[3]} -> {n[3]}")

    print(f"条目数 {len(old_list)} -> {len(new_list)}")
    print(f"内容改动 {len(content_changed)} 个（预期 {len(expected)} 个）：")
    for p in sorted(content_changed):
        o = old_tree.get(p)
        n = new_tree.get(p)
        osz = o[2] if o and o[0] == "file" else "-"
        nsz = n[2] if n and n[0] == "file" else "-"
        print(f"  {p} {osz} -> {nsz}")

    if errors:
        print("\n核对失败：")
        for e in errors:
            print("  " + e)
        return 1
    print("\n核对通过：改动集合、权限、属主、条目数都与预期一致。")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 6:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(*sys.argv[1:]))
