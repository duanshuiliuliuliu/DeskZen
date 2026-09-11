# -*- coding: utf-8 -*-
"""把 clips/ 里的动作帧条做「几何归一 + 循环缝优化」。

做两件事：
1. 几何归一（解决动作切换时角色忽大忽小、左右跳、脚线不齐）：
   按每个动作所有帧的**中位包围盒**推导统一的缩放与偏移，把「站立高度 / 脚底线 /
   身体中线」对齐到同一基准；用中位数而非逐帧，保留动作本身的位移。
2. 循环缝优化：在首尾各若干帧里搜一对最接近的帧作为新的循环窗口，改善明显才采用。

只改这两项，不碰颜色/时长；persona.json 只回写被重剪动作的 frames。

用法：
    python scripts/normalize_link_clips.py [--dry-run] [--report out.png]
"""

import argparse
import json
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

from json_format import dumps as json_dumps


FRAME_SIZE = (192, 208)
# 归一化基准：站立高度、脚底线、身体中线
TARGET_HEIGHT = 200
TARGET_BOTTOM = 203
TARGET_CENTER_X = 96
# 缩放钳制：避免把明显躺/坐的姿势放大成巨人
SCALE_MIN, SCALE_MAX = 0.85, 1.25
# 允许的最多越界像素：个别动作手臂/武器顶到画布边缘时不至于卡住对齐
CLIP_TOLERANCE = 4
# 中位高度低于此值时视为「非站立姿势」，只对齐不缩放
STANDING_MIN_HEIGHT = 150
# 循环缝：改善达到该比例且保留帧数达标才采用
LOOP_GAIN_MIN = 0.20
# 重剪后至少保留 75% 的帧：避免为了追求循环缝把动作本身剪掉
LOOP_KEEP_MIN = 0.75
LOOP_SEARCH_EDGE = 8
ALPHA_THRESHOLD = 16


def split_frames(strip: Image.Image, frames: int) -> list[Image.Image]:
    frame_w = strip.width // frames
    return [
        strip.crop((i * frame_w, 0, (i + 1) * frame_w, strip.height))
        for i in range(frames)
    ]


def bbox(frame: Image.Image) -> tuple[int, int, int, int] | None:
    alpha = np.asarray(frame)[:, :, 3] > ALPHA_THRESHOLD
    ys, xs = np.where(alpha)
    if len(xs) == 0:
        return None
    return int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())


def frame_feature(frame: Image.Image) -> np.ndarray:
    """预乘 alpha 的 RGBA，用于比较两帧的姿势与轮廓差异"""
    rgba = np.asarray(frame, dtype=np.float32) / 255.0
    alpha = rgba[:, :, 3:4]
    return np.dstack([rgba[:, :, :3] * alpha, alpha])


def seam_distance(a: Image.Image, b: Image.Image) -> float:
    return float(np.mean((frame_feature(a) - frame_feature(b)) ** 2))


def trim_loop(frames: list[Image.Image]) -> tuple[list[Image.Image], float, float]:
    """返回（新的帧序列, 原缝距离, 新缝距离）"""
    count = len(frames)
    current = seam_distance(frames[0], frames[-1])
    best = (current, 0, count - 1)
    min_keep = max(int(count * LOOP_KEEP_MIN), 2)
    max_start = min(LOOP_SEARCH_EDGE, count - min_keep)
    for start in range(0, max_start + 1):
        last_allowed = count - 1
        first_allowed = max(count - 1 - LOOP_SEARCH_EDGE, start + min_keep - 1)
        for end in range(first_allowed, last_allowed + 1):
            distance = seam_distance(frames[start], frames[end])
            if distance < best[0]:
                best = (distance, start, end)
    gain = (current - best[0]) / current if current > 0 else 0.0
    if gain < LOOP_GAIN_MIN or best[2] - best[1] + 1 >= count:
        return frames, current, current
    return frames[best[1]: best[2] + 1], current, best[0]


def normalize_geometry(frames: list[Image.Image]) -> tuple[list[Image.Image], float, int, int, tuple | None]:
    boxes = [b for b in (bbox(f) for f in frames) if b]
    if not boxes:
        return frames, 1.0, 0, 0, None
    arr = np.array(boxes, dtype=float)
    mx0, my0, mx1, my1 = np.median(arr, axis=0)
    median_h = my1 - my0 + 1
    # 画布只有 192 宽：放大到超出宽度会裁掉张开的武器/手臂，因此缩放还要受宽度约束
    max_width = (arr[:, 2] - arr[:, 0] + 1).max()
    width_fit = (FRAME_SIZE[0] - 2) / max_width
    scale = 1.0
    if median_h >= STANDING_MIN_HEIGHT:
        scale = float(
            np.clip(min(TARGET_HEIGHT / median_h, width_fit), SCALE_MIN, SCALE_MAX)
        )

    # 缩放后所有帧的极值（用于把偏移钳制在画布内，避免裁掉脚尖/头顶）
    min_x = arr[:, 0].min() * scale
    max_x = arr[:, 2].max() * scale
    min_y = arr[:, 1].min() * scale
    max_y = arr[:, 3].max() * scale

    def clamp(offset: float, low: float, high: float) -> int:
        if low > high:  # 内容比画布还大：居中
            return int(round((low + high) / 2))
        return int(round(min(max(offset, low), high)))

    offset_x = clamp(
        TARGET_CENTER_X - (mx0 + mx1) / 2 * scale,
        -min_x - CLIP_TOLERANCE,
        FRAME_SIZE[0] - 1 - max_x + CLIP_TOLERANCE,
    )
    offset_y = clamp(
        TARGET_BOTTOM - my1 * scale,
        1 - min_y - CLIP_TOLERANCE,
        FRAME_SIZE[1] - 1 - max_y + CLIP_TOLERANCE,
    )

    output = []
    for frame in frames:
        scaled = frame.resize(
            (max(1, round(frame.width * scale)), max(1, round(frame.height * scale))),
            Image.Resampling.LANCZOS,
        )
        canvas = Image.new("RGBA", FRAME_SIZE, (0, 0, 0, 0))
        canvas.alpha_composite(scaled, (offset_x, offset_y))
        output.append(canvas)
    return output, scale, offset_x, offset_y, (mx0, my0, mx1, my1)


def save_strip(frames: list[Image.Image], destination: Path) -> None:
    width, height = FRAME_SIZE
    strip = Image.new("RGBA", (width * len(frames), height), (0, 0, 0, 0))
    for index, frame in enumerate(frames):
        strip.alpha_composite(frame, (index * width, 0))
    strip.save(destination, "WEBP", quality=88, method=6, exact=True)


def build_report(pairs: list[tuple[str, Image.Image, Image.Image]], destination: Path) -> None:
    """前后对比图：每个动作一行，左=归一前，右=归一后"""
    cell_w, cell_h, label_h, pad = FRAME_SIZE[0], FRAME_SIZE[1], 26, 8
    columns = 2
    rows = len(pairs)
    width = columns * (cell_w + pad) + pad + 90
    height = rows * (cell_h + label_h + pad) + pad
    sheet = Image.new("RGB", (width, height), (245, 243, 238))
    draw = ImageDraw.Draw(sheet)
    for row, (name, before, after) in enumerate(pairs):
        y = pad + row * (cell_h + label_h + pad)
        draw.text((pad, y + cell_h // 2), name, fill=(30, 30, 30))
        for column, frame in enumerate((before, after)):
            tile = Image.new("RGB", FRAME_SIZE, (255, 255, 255))
            tile.paste(frame, (0, 0), frame)
            x = pad + 90 + column * (cell_w + pad)
            sheet.paste(tile, (x, y))
            draw.rectangle([x, y, x + cell_w - 1, y + cell_h - 1], outline=(200, 196, 188))
        draw.text((pad + 90, y + cell_h + 6), "before / after", fill=(120, 116, 108))
    sheet.save(destination)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--clips-dir", type=Path, default=Path("resources/characters/link/clips"))
    parser.add_argument("--persona", type=Path, default=Path("resources/characters/link/persona.json"))
    parser.add_argument("--dry-run", action="store_true", help="只统计，不写回素材与 persona.json")
    parser.add_argument("--report", type=Path, help="输出前后对比图")
    args = parser.parse_args()

    persona = json.loads(args.persona.read_text(encoding="utf-8"))
    report_pairs: list[tuple[str, Image.Image, Image.Image]] = []
    print(f"{'clip':16s}{'帧数':>8}{'循环缝':>18}{'缩放':>7}{'偏移':>12}")

    for name, clip in persona["clips"].items():
        source = args.clips_dir / f"{name}.webp"
        strip = Image.open(source).convert("RGBA")
        frames = split_frames(strip, clip["frames"])
        middle = frames[len(frames) // 2]

        frames, seam_before, seam_after = trim_loop(frames)
        frames, scale, offset_x, offset_y, _ = normalize_geometry(frames)
        print(
            f"{name:16s}{len(frames):8d}"
            f"{f'{seam_before:.4f}->{seam_after:.4f}':>18}"
            f"{scale:7.2f}{f'{offset_x:+d},{offset_y:+d}':>12}"
        )
        if args.report:
            report_pairs.append((name, middle, frames[len(frames) // 2]))
        if not args.dry_run:
            save_strip(frames, source)
            clip["frames"] = len(frames)

    if args.report:
        build_report(report_pairs, args.report)
        print(f"对比图: {args.report}")
    if args.dry_run:
        print("dry-run：未写回素材与 persona.json")
        return
    # 用紧凑格式化器回写，避免把 persona.json 又写回「每个字段一行」的碎格式
    args.persona.write_text(json_dumps(persona), encoding="utf-8")
    print(f"persona.json 已更新: {args.persona}")


if __name__ == "__main__":
    main()
