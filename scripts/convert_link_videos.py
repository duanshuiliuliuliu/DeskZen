import argparse
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage


CLIPS = {
    1: "observe",
    2: "stretch",
    3: "sheikah_slate",
    4: "eat_apple",
    5: "standing_doze",
    6: "polish_shield",
    7: "korok_leaf",
    8: "zonai_hand",
    9: "task_cheer",
    10: "greet_wave",
}

SOURCE_FPS = 12
FRAME_MS = round(1000 / SOURCE_FPS)
FRAME_SIZE = (192, 208)
CROP_BOX = (60, 35, 660, 685)


def extract_frames(video: Path, directory: Path) -> list[Path]:
    pattern = directory / "%04d.png"
    subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(video),
            "-vf",
            f"fps={SOURCE_FPS}",
            str(pattern),
        ],
        check=True,
    )
    return sorted(directory.glob("*.png"))


def fit_background(rgb: np.ndarray) -> np.ndarray:
    height, width, _ = rgb.shape
    yy, xx = np.mgrid[0:height, 0:width]
    xn = xx / max(width - 1, 1)
    yn = yy / max(height - 1, 1)
    features = np.stack(
        [np.ones_like(xn), xn, yn, xn * yn, xn * xn, yn * yn], axis=-1
    )

    border = (xx < 90) | (xx >= width - 90) | (yy < 90) | (yy >= height - 90)
    sampled = border & ((xx % 4 == 0) & (yy % 4 == 0))
    design = features[sampled]
    values = rgb[sampled].astype(np.float32)

    keep = np.ones(len(values), dtype=bool)
    coefficients = None
    for _ in range(3):
        coefficients, *_ = np.linalg.lstsq(design[keep], values[keep], rcond=None)
        residual = np.linalg.norm(design @ coefficients - values, axis=1)
        cutoff = np.percentile(residual, 78)
        keep = residual <= max(cutoff, 4.0)

    return np.clip(features @ coefficients, 0, 255).astype(np.float32)


def component_distance(first: tuple[int, int, int, int], second: tuple[int, int, int, int]) -> float:
    ay0, ay1, ax0, ax1 = first
    by0, by1, bx0, bx1 = second
    dx = max(ax0 - bx1, bx0 - ax1, 0)
    dy = max(ay0 - by1, by0 - ay1, 0)
    return float((dx * dx + dy * dy) ** 0.5)


def foreground_support(distance: np.ndarray) -> np.ndarray:
    labels, count = ndimage.label(distance > 22.0)
    if count == 0:
        return np.zeros(distance.shape, dtype=bool)

    objects = ndimage.find_objects(labels)
    sizes = np.bincount(labels.ravel())
    height, width = distance.shape
    center = np.zeros(distance.shape, dtype=bool)
    center[height // 6 : height - 20, width // 5 : width - width // 5] = True

    candidates = []
    for label in range(1, count + 1):
        overlap = np.count_nonzero((labels == label) & center)
        if overlap:
            candidates.append((sizes[label], label))
    if not candidates:
        return np.zeros(distance.shape, dtype=bool)

    main_label = max(candidates)[1]
    main_slice = objects[main_label - 1]
    main_box = (
        main_slice[0].start,
        main_slice[0].stop,
        main_slice[1].start,
        main_slice[1].stop,
    )

    keep = labels == main_label
    for label in range(1, count + 1):
        if label == main_label or sizes[label] < 12:
            continue
        item = objects[label - 1]
        box = (item[0].start, item[0].stop, item[1].start, item[1].stop)
        if component_distance(main_box, box) <= 115:
            keep |= labels == label

    return ndimage.binary_dilation(keep, iterations=2)


def remove_background(image: Image.Image) -> Image.Image:
    rgb = np.asarray(image.convert("RGB"), dtype=np.float32)
    background = fit_background(rgb)
    distance = np.linalg.norm(rgb - background, axis=2)
    support = foreground_support(distance)

    alpha = np.clip((distance - 16.0) / 28.0, 0.0, 1.0)
    alpha = np.power(alpha, 0.78) * support
    alpha[distance >= 48.0] = support[distance >= 48.0]

    rgba = np.dstack([rgb, alpha * 255.0]).astype(np.uint8)
    result = Image.fromarray(rgba, "RGBA").crop(CROP_BOX)
    return result.resize(FRAME_SIZE, Image.Resampling.LANCZOS)


def frame_distance(first: Image.Image, second: Image.Image) -> float:
    first_rgba = np.asarray(first, dtype=np.float32) / 255.0
    second_rgba = np.asarray(second, dtype=np.float32) / 255.0
    first_alpha = first_rgba[:, :, 3:4]
    second_alpha = second_rgba[:, :, 3:4]
    first_feature = np.dstack([first_rgba[:, :, :3] * first_alpha, first_alpha])
    second_feature = np.dstack([second_rgba[:, :, :3] * second_alpha, second_alpha])
    return float(np.mean((first_feature - second_feature) ** 2))


def trim_to_loop(frames: list[Image.Image]) -> tuple[list[Image.Image], float]:
    first = frames[0]
    candidate_start = max(len(frames) - 8, 1)
    candidates = [
        (frame_distance(first, frames[index]), index)
        for index in range(candidate_start, len(frames))
    ]
    score, end_index = min(candidates)
    return frames[: end_index + 1], score


def save_strip(frames: list[Image.Image], destination: Path) -> None:
    width, height = FRAME_SIZE
    strip = Image.new("RGBA", (width * len(frames), height), (0, 0, 0, 0))
    for index, frame in enumerate(frames):
        strip.alpha_composite(frame, (index * width, 0))
    strip.save(destination, "WEBP", quality=88, method=6, exact=True)


def convert(input_dir: Path, output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)

    for number, clip_name in CLIPS.items():
        video = input_dir / f"{number}.mp4"
        if not video.exists():
            raise FileNotFoundError(f"Missing source video: {video}")

        with tempfile.TemporaryDirectory(prefix=f"deskzen-{number}-") as temp:
            paths = extract_frames(video, Path(temp))
            frames = [remove_background(Image.open(path)) for path in paths]

        frames, loop_score = trim_to_loop(frames)
        destination = output_dir / f"{clip_name}.webp"
        save_strip(frames, destination)
        # loop_score 越小表示首尾帧越接近、循环越顺；只打印不落盘
        print(
            f"{number:02d} {clip_name}: {len(frames)} frames "
            f"-> {destination.name} (loop_score={loop_score:.6f})"
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("input_dir", type=Path)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("resources/characters/link/clips"),
    )
    args = parser.parse_args()
    convert(args.input_dir.resolve(), args.output_dir.resolve())


if __name__ == "__main__":
    main()
