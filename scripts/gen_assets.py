"""生成 DeskZen MVP 像素资源（16x16 网格 + 自动描边）：
- public/sprites/dog.png        (64x96 spritesheet, 3 states x 2 frames)

应用图标由 scripts/gen_icons.py 从外部 PNG 生成，本脚本不再覆盖。

直接运行: python scripts/gen_assets.py
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent

SIZE = 16

OUTLINE = (70, 44, 26, 255)
BROWN = (205, 133, 80, 255)
DARK = (138, 88, 50, 255)
LIGHT = (240, 205, 160, 255)
WHITE = (250, 248, 240, 255)
BLACK = (45, 38, 34, 255)
RED = (160, 72, 62, 255)
PINK = (240, 150, 155, 255)
GRAY = (150, 150, 162, 255)


def canvas():
    return [[None] * SIZE for _ in range(SIZE)]


def ellipse_pixels(cx, cy, rx, ry):
    pts = set()
    for y in range(int(cy - ry) - 1, int(cy + ry) + 2):
        for x in range(int(cx - rx) - 1, int(cx + rx) + 2):
            dx = (x - cx) / rx
            dy = (y - cy) / ry
            if dx * dx + dy * dy <= 1.0:
                pts.add((x, y))
    return pts


def rect_pixels(x0, y0, x1, y1):
    return {(x, y) for x in range(x0, x1 + 1) for y in range(y0, y1 + 1)}


def blit(img, pixels, color):
    for x, y in pixels:
        if 0 <= x < SIZE and 0 <= y < SIZE:
            img[y][x] = color


def draw_z(img, x0, y0):
    pts = {
        (x0, y0),
        (x0 + 1, y0),
        (x0 + 2, y0),
        (x0 + 2, y0 + 1),
        (x0 + 1, y0 + 2),
        (x0, y0 + 2),
    }
    blit(img, pts, GRAY)


def outline_pass(img):
    for y in range(SIZE):
        for x in range(SIZE):
            if img[y][x] is None:
                continue
            for nx, ny in ((x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)):
                if nx < 0 or ny < 0 or nx >= SIZE or ny >= SIZE or img[ny][nx] is None:
                    img[y][x] = OUTLINE
                    break


def dog_frame(eyes="open", tail="up", sleeping=False, sway=0, zzz=0):
    img = canvas()
    dx = sway
    ear_y = 5.2 if sleeping else 4.2
    head_y = 6.6 if sleeping else 6.2
    body_y = 11.4 if sleeping else 11.0

    # 耳朵（先画，头部会覆盖底部）
    blit(img, ellipse_pixels(3.2 + dx, ear_y, 2.0, 3.0), DARK)
    blit(img, ellipse_pixels(12.8 + dx, ear_y, 2.0, 3.0), DARK)

    # 身体 + 白肚皮
    blit(img, ellipse_pixels(8 + dx, body_y, 5.2, 3.8), BROWN)
    blit(img, ellipse_pixels(8 + dx, body_y + 0.9, 3.2, 2.1), WHITE)

    # 头 + 白色口鼻
    blit(img, ellipse_pixels(8 + dx, head_y, 5.3, 4.5), BROWN)
    blit(img, ellipse_pixels(8 + dx, head_y + 2.0, 3.1, 1.9), WHITE)

    # 眼睛
    eye_y = 5
    if eyes == "open":
        blit(img, rect_pixels(5 + dx, eye_y, 6 + dx, eye_y + 1), BLACK)
        blit(img, rect_pixels(9 + dx, eye_y, 10 + dx, eye_y + 1), BLACK)
    else:
        # closed / half 都画成一条横线，效果接近
        blit(img, rect_pixels(5 + dx, eye_y + 1, 6 + dx, eye_y + 1), BLACK)
        blit(img, rect_pixels(9 + dx, eye_y + 1, 10 + dx, eye_y + 1), BLACK)

    # 鼻子
    blit(img, rect_pixels(7 + dx, 8, 8 + dx, 8), BLACK)

    # 嘴
    if eyes == "open":
        blit(img, rect_pixels(6 + dx, 9, 9 + dx, 9), RED)
        blit(img, rect_pixels(7 + dx, 10, 8 + dx, 10), PINK)
    else:
        blit(img, rect_pixels(7 + dx, 9, 8 + dx, 9), RED)

    # 尾巴
    if tail == "up":
        blit(img, {(13 + dx, 8), (13 + dx, 9), (14 + dx, 9)}, BROWN)
        blit(img, {(14 + dx, 8)}, LIGHT)
    elif tail == "down":
        blit(img, {(13 + dx, 10), (13 + dx, 11), (14 + dx, 11)}, BROWN)

    outline_pass(img)

    # Zzz 与阴影放在描边之后，避免被描边吞掉
    if zzz == 1:
        draw_z(img, 9 + dx, 1)
    elif zzz == 2:
        draw_z(img, 10 + dx, 0)
        draw_z(img, 8 + dx, 2)
    blit(img, ellipse_pixels(8 + dx, 14.6, 6.0, 1.2), (0, 0, 0, 42))
    return img


FRAMES = {
    "Awake": [
        dog_frame(eyes="open", tail="up"),
        dog_frame(eyes="closed", tail="up"),
    ],
    "Sleeping": [
        dog_frame(eyes="closed", tail="down", sleeping=True, zzz=1),
        dog_frame(eyes="closed", tail="down", sleeping=True, zzz=2),
    ],
    "Dazing": [
        dog_frame(eyes="half", tail="up"),
        dog_frame(eyes="half", tail="up", sway=1),
    ],
}

STATE_ORDER = ["Awake", "Sleeping", "Dazing"]

CHAR_MAP = {
    OUTLINE: "O",
    BROWN: "B",
    DARK: "D",
    LIGHT: "L",
    WHITE: "W",
    BLACK: "e",
    RED: "m",
    PINK: "p",
    GRAY: "z",
}


def preview(img):
    for row in img:
        print("".join(CHAR_MAP.get(px, " ") if px is not None else " " for px in row))


def to_image(grid):
    data = bytearray()
    for row in grid:
        for px in row:
            data.extend(px if px is not None else (0, 0, 0, 0))
    return Image.frombytes("RGBA", (SIZE, SIZE), bytes(data))


def main():
    sheet = Image.new("RGBA", (SIZE * 2, SIZE * 3), (0, 0, 0, 0))
    for row, state in enumerate(STATE_ORDER):
        for col, frame in enumerate(FRAMES[state]):
            sheet.paste(to_image(frame), (col * SIZE, row * SIZE))

    out_sheet = ROOT / "public" / "sprites" / "dog.png"
    out_sheet.parent.mkdir(parents=True, exist_ok=True)
    sheet.resize((64, 96), Image.NEAREST).save(out_sheet)
    print(f"spritesheet -> {out_sheet}")

    print("\n--- Awake frame0 ---")
    preview(FRAMES["Awake"][0])
    print("\n--- Sleeping frame0 ---")
    preview(FRAMES["Sleeping"][0])


if __name__ == "__main__":
    main()
