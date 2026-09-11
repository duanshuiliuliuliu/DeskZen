# -*- coding: utf-8 -*-
"""把旧的 8x9 / 20x6 精灵图逐行切成 clips 动作帧条。

只处理“语义上仍有归属、且与新动作帧同尺寸（192x208）”的行；语义重复或没有
归属的行不抽取（见 ROWS / SKIPPED_ROWS），避免把不合适的动作塞进场景池。

用法：
    python scripts/extract_spritesheet_clips.py <spritesheet.webp>

输出：与 `convert_link_videos.py` 相同的横向 WebP 帧条（帧数与帧时长需在 persona.json 声明）。
"""

import argparse
from pathlib import Path

from PIL import Image


FRAME_SIZE = (192, 208)
FRAME_MS = 150

# 行号 -> (动作名, 中文说明, 原始状态名)
ROWS = {
    0: ("walk", "在桌面上走动", "walking"),
    1: ("ride_motorcycle", "骑摩托兜风", "motorcycle"),
    2: ("sit_idle", "坐着发呆", "idle"),
    4: ("fight", "挥剑战斗", "fight"),
    5: ("lie_sleep", "躺下睡觉", "sleep"),
}

# 有意不抽取的行：原因写在这里，避免以后误以为漏掉
SKIPPED_ROWS = {
    3: "吃苹果：与视频动作 eat_apple 语义重复，不重复收录",
}


def slice_row(sheet: Image.Image, row: int) -> Image.Image:
    frame_w, frame_h = FRAME_SIZE
    frames = sheet.width // frame_w
    top = row * frame_h
    strip = Image.new("RGBA", (frame_w * frames, frame_h), (0, 0, 0, 0))
    for index in range(frames):
        frame = sheet.crop(
            (index * frame_w, top, (index + 1) * frame_w, top + frame_h)
        ).convert("RGBA")
        strip.alpha_composite(frame, (index * frame_w, 0))
    return strip


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("sheet", type=Path)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("resources/characters/link/clips"),
    )
    args = parser.parse_args()

    sheet = Image.open(args.sheet).convert("RGBA")
    expected_cols, rows = sheet.width // FRAME_SIZE[0], sheet.height // FRAME_SIZE[1]
    if sheet.width % FRAME_SIZE[0] or sheet.height % FRAME_SIZE[1]:
        raise SystemExit(f"精灵图尺寸 {sheet.size} 不是 {FRAME_SIZE} 的整数倍")

    output_dir: Path = args.output_dir
    output_dir.mkdir(parents=True, exist_ok=True)

    for row, (name, label, origin) in ROWS.items():
        if row >= rows:
            continue
        strip = slice_row(sheet, row)
        destination = output_dir / f"{name}.webp"
        strip.save(destination, "WEBP", quality=88, method=6, exact=True)
        print(
            f"row {row} -> {name}: {expected_cols} frames -> {destination.name} "
            f"（{label}，原状态 {origin}，frame_ms={FRAME_MS}）"
        )
    for row, reason in SKIPPED_ROWS.items():
        print(f"row {row} 跳过：{reason}")


if __name__ == "__main__":
    main()
