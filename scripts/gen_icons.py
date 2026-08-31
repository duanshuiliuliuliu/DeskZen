"""从源 PNG 生成 DeskZen 全套应用图标：
源文件: C:\\Users\\Baosong.Nan\\Downloads\\DeskZen.png
输出:   resources/icons/ (32/128/256 png + icon.ico)

直接运行: python scripts/gen_icons.py
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SRC = Path(r"C:\Users\Baosong.Nan\Downloads\DeskZen.png")
ICONS = ROOT / "resources" / "icons"


def main():
    if not SRC.exists():
        raise SystemExit(f"source icon not found: {SRC}")
    img = Image.open(SRC).convert("RGBA")
    print(f"source: {SRC} {img.size}")

    ICONS.mkdir(parents=True, exist_ok=True)
    for name, size in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)]:
        img.resize((size, size), Image.LANCZOS).save(ICONS / name)
    img.resize((256, 256), Image.LANCZOS).save(
        ICONS / "icon.ico",
        sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print(f"icons -> {ICONS}")


if __name__ == "__main__":
    main()
