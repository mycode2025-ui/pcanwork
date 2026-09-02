#!/usr/bin/env python3
"""Generate the dedicated Windows icon for PcanWork .pcprj project files."""

from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter


ROOT = Path(__file__).resolve().parent.parent
ICON_PATH = ROOT / "assets" / "project.ico"
PREVIEW_PATH = ROOT / "artifacts" / "project-icon-preview.png"
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)


def scaled_points(points: list[tuple[float, float]], size: int) -> list[tuple[int, int]]:
    return [(round(x * size), round(y * size)) for x, y in points]


def draw_project_icon(size: int) -> Image.Image:
    # Render above target resolution so curves and diagonal folds remain clean.
    scale = 4 if size >= 64 else 8
    canvas_size = size * scale
    image = Image.new("RGBA", (canvas_size, canvas_size), (0, 0, 0, 0))

    def px(value: float) -> int:
        return round(value * canvas_size)

    # Soft depth shadow, kept inside the icon canvas.
    shadow = Image.new("RGBA", image.size, (0, 0, 0, 0))
    shadow_draw = ImageDraw.Draw(shadow)
    shadow_draw.rounded_rectangle(
        (px(0.13), px(0.07), px(0.89), px(0.96)),
        radius=px(0.10),
        fill=(6, 43, 91, 105),
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(px(0.025)))
    image.alpha_composite(shadow)

    # Folded document silhouette.
    mask = Image.new("L", image.size, 0)
    mask_draw = ImageDraw.Draw(mask)
    mask_draw.rounded_rectangle(
        (px(0.11), px(0.035), px(0.88), px(0.93)),
        radius=px(0.105),
        fill=255,
    )
    mask_draw.polygon(
        scaled_points([(0.68, 0.03), (0.89, 0.03), (0.89, 0.25)], canvas_size),
        fill=0,
    )

    body = Image.new("RGBA", image.size, (0, 0, 0, 0))
    body_pixels = body.load()
    for y in range(px(0.03), px(0.94)):
        fy = y / canvas_size
        for x in range(px(0.10), px(0.89)):
            fx = x / canvas_size
            # PcanWork blue with restrained modern depth.
            light = max(0.0, 1.0 - math.hypot(fx - 0.40, fy - 0.22) / 0.72)
            r = round(17 + 13 * light)
            g = round(75 + 48 * light)
            b = round(151 + 50 * light)
            body_pixels[x, y] = (r, g, b, 255)
    body.putalpha(mask)
    image.alpha_composite(body)

    draw = ImageDraw.Draw(image)

    # Folded corner.
    draw.polygon(
        scaled_points([(0.68, 0.035), (0.88, 0.25), (0.76, 0.25), (0.68, 0.17)], canvas_size),
        fill=(93, 168, 240, 255),
    )
    draw.line(
        scaled_points([(0.68, 0.035), (0.68, 0.17), (0.76, 0.25), (0.88, 0.25)], canvas_size),
        fill=(137, 200, 250, 190),
        width=max(1, px(0.006)),
        joint="curve",
    )

    # Official PcanWork identity badge: digital square wave + sine wave.
    badge_box = (px(0.275), px(0.245), px(0.715), px(0.615))
    badge_shadow = Image.new("RGBA", image.size, (0, 0, 0, 0))
    badge_shadow_draw = ImageDraw.Draw(badge_shadow)
    badge_shadow_draw.rounded_rectangle(
        (badge_box[0], badge_box[1] + px(0.018), badge_box[2], badge_box[3] + px(0.018)),
        radius=px(0.075),
        fill=(0, 25, 78, 105),
    )
    badge_shadow = badge_shadow.filter(ImageFilter.GaussianBlur(px(0.012)))
    image.alpha_composite(badge_shadow)
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle(
        badge_box,
        radius=px(0.075),
        fill=(37, 112, 209, 255),
        outline=(102, 170, 238, 255),
        width=max(1, px(0.006)),
    )

    wave_width = max(2, px(0.026))
    square_wave = [
        (0.325, 0.395),
        (0.395, 0.395),
        (0.395, 0.315),
        (0.465, 0.315),
        (0.465, 0.395),
        (0.535, 0.395),
        (0.535, 0.315),
        (0.605, 0.315),
        (0.605, 0.395),
        (0.665, 0.395),
    ]
    draw.line(
        scaled_points(square_wave, canvas_size),
        fill=(255, 255, 255, 255),
        width=wave_width,
        joint="curve",
    )

    sine_points: list[tuple[float, float]] = []
    for i in range(97):
        t = i / 96
        x = 0.325 + 0.34 * t
        y = 0.505 - 0.058 * math.sin(t * math.pi * 3)
        sine_points.append((x, y))
    draw.line(
        scaled_points(sine_points, canvas_size),
        fill=(143, 203, 249, 255),
        width=wave_width,
        joint="curve",
    )

    # Engineering/circuit motif separates a project file from the executable.
    circuit_color = (185, 218, 248, 238)
    circuit_width = max(1, px(0.010))
    traces = [
        [(0.115, 0.73), (0.28, 0.73), (0.35, 0.80), (0.52, 0.80), (0.59, 0.73), (0.88, 0.73)],
        [(0.22, 0.86), (0.48, 0.86), (0.55, 0.92), (0.78, 0.92)],
        [(0.53, 0.80), (0.53, 0.68)],
        [(0.60, 0.73), (0.67, 0.66), (0.80, 0.66)],
    ]
    for trace in traces:
        draw.line(
            scaled_points(trace, canvas_size),
            fill=circuit_color,
            width=circuit_width,
            joint="curve",
        )
    for x, y, radius in [
        (0.28, 0.73, 0.024),
        (0.22, 0.86, 0.024),
        (0.53, 0.68, 0.026),
        (0.67, 0.66, 0.022),
        (0.80, 0.66, 0.020),
        (0.78, 0.92, 0.027),
    ]:
        draw.ellipse(
            (px(x - radius), px(y - radius), px(x + radius), px(y + radius)),
            fill=(28, 91, 174, 255),
            outline=circuit_color,
            width=circuit_width,
        )

    return image.resize((size, size), Image.Resampling.LANCZOS)


def build_preview(icon: Image.Image) -> Image.Image:
    preview = Image.new("RGBA", (960, 520), (246, 248, 251, 255))
    preview.alpha_composite(icon.resize((360, 360), Image.Resampling.LANCZOS), (82, 70))
    draw = ImageDraw.Draw(preview)
    draw.text((500, 105), "PcanWork Project", fill=(25, 53, 88))
    draw.text((500, 145), ".pcprj", fill=(31, 94, 172))
    x = 500
    for size in (64, 48, 32, 16):
        sample = draw_project_icon(size)
        preview.alpha_composite(sample, (x, 230 + (64 - size)))
        draw.text((x, 310), f"{size}px", fill=(85, 105, 130))
        x += 105
    return preview


def main() -> None:
    ICON_PATH.parent.mkdir(parents=True, exist_ok=True)
    PREVIEW_PATH.parent.mkdir(parents=True, exist_ok=True)

    master = draw_project_icon(256)
    master.save(ICON_PATH, format="ICO", sizes=[(size, size) for size in ICO_SIZES])
    build_preview(master).save(PREVIEW_PATH, format="PNG")
    print(ICON_PATH)
    print(PREVIEW_PATH)


if __name__ == "__main__":
    main()
