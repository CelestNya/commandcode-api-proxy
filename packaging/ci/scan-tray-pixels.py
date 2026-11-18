#!/usr/bin/env python3
"""托盘图标像素验收脚本（Linux KDE CI 用）。

只依赖 python3 + Pillow(PIL)，不引入 numpy / scipy。

背景：托盘图标是程序在面板上程序化绘制的 22×22 ARGB32 圆点——代理在跑
时为绿色 #2ECC71 (46,204,113)，停止时为灰色 #808080。面板主题图标名不可
靠，因此像素颜色是验收托盘是否真实渲染出来的唯一依据。

两种模式：

模式 1（默认）：在整幅桌面截图里找绿色圆点。
  - 每通道距 #2ECC71 容差 ≤ 40；
  - 连通域至少 25 个像素；
  - 包围盒宽、高均 ≤ 60（排除大面积绿色背景 / 壁纸误判）；
  - 找到退出 0，否则退出 1。

模式 2（--diff-region）：对比前后两张截图在指定圆形区域内的像素差异，
  用于 X11 右键菜单弹出后“托盘附近区域是否发生可见变化”的断言；
  差异像素 > 2000 退出 0，否则退出 1。
"""

import argparse
import json
import sys
from collections import Counter, deque

from PIL import Image

# ---- 基准常量（与托盘绘制代码对齐，见 crates/ccproxy-tray 托盘后端） ----
GREEN = (46, 204, 113)          # #2ECC71，代理运行时圆点颜色
COLOR_TOLERANCE = 40            # 每通道容差，抗截图压缩与抗锯齿
MIN_COMPONENT_PIXELS = 25       # 连通域最小像素数（22×22 圆点远大于此）
MAX_BBOX = 60                   # 包围盒宽/高上限，防大面积绿色背景误判
DIFF_CHANNEL_THRESHOLD = 12     # 单通道差异超过该值才算“变了”（容忍噪声）
DIFF_MIN_PIXELS = 2000          # 模式 2 判定区域发生变化的差异像素下限


def _load_rgb(path):
    """打开图片并统一转 RGB；尺寸不一致由调用方取交集处理。"""
    return Image.open(path).convert("RGB")


def _dominant_colors(img, sample_step=2, top_n=5):
    """整屏主色桶统计（失败时写进证据 JSON，便于判断是不是纯色空图）。

    按 16 级量化颜色并隔点采样，避免在 1920×1080 图上跑满双重循环。
    """
    w, h = img.size
    px = img.load()
    buckets = Counter()
    for y in range(0, h, sample_step):
        for x in range(0, w, sample_step):
            r, g, b = px[x, y]
            buckets[(r // 16 * 16, g // 16 * 16, b // 16 * 16)] += 1
    return [
        {"rgb": list(color), "count": count}
        for color, count in buckets.most_common(top_n)
    ]


def scan_green(image_path, json_path):
    """模式 1：整图搜索绿色托盘圆点。"""
    img = _load_rgb(image_path)
    w, h = img.size
    px = img.load()

    # 第一遍：收集所有落在容差内的像素坐标。
    mask = set()
    for y in range(h):
        for x in range(w):
            r, g, b = px[x, y]
            if (
                abs(r - GREEN[0]) <= COLOR_TOLERANCE
                and abs(g - GREEN[1]) <= COLOR_TOLERANCE
                and abs(b - GREEN[2]) <= COLOR_TOLERANCE
            ):
                mask.add((x, y))

    # 四邻域泛洪连通域（纯 Python，不依赖 scipy.ndimage）。
    components = []
    while mask:
        seed = mask.pop()
        queue = deque([seed])
        comp = [seed]
        while queue:
            x, y = queue.popleft()
            for nx, ny in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
                if (nx, ny) in mask:
                    mask.remove((nx, ny))
                    queue.append((nx, ny))
                    comp.append((nx, ny))
        components.append(comp)

    # 按尺寸 + 包围盒筛选，取最大的合格连通域作为托盘图标。
    best = None
    for comp in components:
        xs = [p[0] for p in comp]
        ys = [p[1] for p in comp]
        bbox_w = max(xs) - min(xs) + 1
        bbox_h = max(ys) - min(ys) + 1
        if (
            len(comp) >= MIN_COMPONENT_PIXELS
            and bbox_w <= MAX_BBOX
            and bbox_h <= MAX_BBOX
        ):
            if best is None or len(comp) > len(best):
                best = comp

    result = {
        "found": False,
        "count": 0,
        "bbox": None,
        "centroid": None,
        "image_size": [w, h],
        "green_candidates": len(components),
        "dominant_colors": _dominant_colors(img),
    }

    if best is not None:
        xs = [p[0] for p in best]
        ys = [p[1] for p in best]
        n = len(best)
        result.update(
            found=True,
            count=n,
            bbox=[min(xs), min(ys), max(xs), max(ys)],
            centroid=[round(sum(xs) / n, 2), round(sum(ys) / n, 2)],
        )

    with open(json_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)

    if result["found"]:
        print(
            "tray-icon: found %d green pixels at centroid %s"
            % (result["count"], result["centroid"])
        )
        return 0
    print(
        "tray-icon: NOT found (candidate components=%d, image=%dx%d)"
        % (len(components), w, h)
    )
    return 1


def diff_region(before_path, after_path, cx, cy, radius, json_path):
    """模式 2：对比圆形区域内前后两帧的可见差异。"""
    before = _load_rgb(before_path)
    after = _load_rgb(after_path)

    # 截图尺寸可能不一致（DPI / 重排），取交集区域逐像素对齐。
    iw = min(before.width, after.width)
    ih = min(before.height, after.height)
    before = before.crop((0, 0, iw, ih))
    after = after.crop((0, 0, iw, ih))
    pb = before.load()
    pa = after.load()

    cx_i = int(round(cx))
    cy_i = int(round(cy))
    r_i = int(round(radius))

    region_pixels = 0
    changed_pixels = 0
    x0 = max(0, cx_i - r_i)
    x1 = min(iw, cx_i + r_i + 1)
    y0 = max(0, cy_i - r_i)
    y1 = min(ih, cy_i + r_i + 1)

    for y in range(y0, y1):
        for x in range(x0, x1):
            # 圆形掩码：超出半径的边角不计入区域。
            if (x - cx_i) ** 2 + (y - cy_i) ** 2 > r_i * r_i:
                continue
            region_pixels += 1
            rb, gb, bb = pb[x, y]
            ra, ga, ba = pa[x, y]
            if max(abs(rb - ra), abs(gb - ga), abs(bb - ba)) > DIFF_CHANNEL_THRESHOLD:
                changed_pixels += 1

    ratio = changed_pixels / region_pixels if region_pixels else 0.0
    result = {
        "changed_pixels": changed_pixels,
        "ratio": round(ratio, 4),
        "region_pixels": region_pixels,
        "threshold_pixels": DIFF_MIN_PIXELS,
        "channel_threshold": DIFF_CHANNEL_THRESHOLD,
        "radius": r_i,
        "center": [cx_i, cy_i],
        "image_size": [iw, ih],
    }

    with open(json_path, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)

    if changed_pixels > DIFF_MIN_PIXELS:
        print("tray-menu: region changed %d pixels (%.2f%%)" % (changed_pixels, ratio * 100))
        return 0
    print(
        "tray-menu: region unchanged (%d / %d pixels changed)"
        % (changed_pixels, region_pixels)
    )
    return 1


def main(argv):
    parser = argparse.ArgumentParser(description="托盘图标 / 菜单像素验收")
    parser.add_argument("image", nargs="?", help="模式 1：待扫描的桌面截图")
    parser.add_argument("--json", required=True, help="结果 JSON 输出路径")
    parser.add_argument(
        "--diff-region",
        nargs=3,
        metavar=("CX", "CY", "R"),
        type=float,
        help="模式 2：圆形区域中心坐标与半径（像素）",
    )
    parser.add_argument("--before", help="模式 2：点击前截图")
    parser.add_argument("--after", help="模式 2：点击后截图")
    args = parser.parse_args(argv)

    if args.diff_region is not None:
        if not args.before or not args.after:
            print("error: --diff-region 需要同时提供 --before 与 --after", file=sys.stderr)
            return 2
        cx, cy, radius = args.diff_region
        return diff_region(args.before, args.after, cx, cy, radius, args.json)

    if not args.image:
        print("error: 模式 1 需要传入截图路径", file=sys.stderr)
        return 2
    return scan_green(args.image, args.json)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
