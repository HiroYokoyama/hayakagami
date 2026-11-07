#!/usr/bin/env python3
"""Draws the application icon into assets/. Requires Pillow."""

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageOps

SCALE = 4
SIZE = 1024 * SCALE
GOLD = (240, 190, 90)
GOLD_TAB = (214, 158, 62)


def px(value):
    return int(value * SCALE)


def vertical_gradient(top, bottom):
    strip = Image.new("RGB", (1, 256))
    for y in range(256):
        t = y / 255
        strip.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return strip.resize((SIZE, SIZE))


def folder_mask(tab):
    """Folder on the left half. `tab` selects the tab instead of the front."""
    mask = Image.new("L", (SIZE, SIZE), 0)
    draw = ImageDraw.Draw(mask)
    if tab:
        draw.rounded_rectangle((px(112), px(300), px(300), px(450)), radius=px(34), fill=255)
    else:
        draw.rounded_rectangle((px(112), px(360), px(472), px(724)), radius=px(44), fill=255)
    return mask


def main():
    canvas = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    tile = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(tile).rounded_rectangle(
        (px(48), px(48), px(976), px(976)), radius=px(210), fill=255
    )
    canvas.paste(vertical_gradient((125, 211, 252), (56, 176, 240)), mask=tile)

    # The original folder.
    canvas.paste(Image.new("RGB", (SIZE, SIZE), GOLD_TAB), mask=folder_mask(tab=True))
    canvas.paste(Image.new("RGB", (SIZE, SIZE), GOLD), mask=folder_mask(tab=False))

    # Its reflection: the same shape flipped across the mirror, in glass colours.
    mirrored_tab = ImageOps.mirror(folder_mask(tab=True))
    mirrored_front = ImageOps.mirror(folder_mask(tab=False))
    canvas.paste(Image.new("RGB", (SIZE, SIZE), (190, 228, 248)), mask=mirrored_tab)
    canvas.paste(vertical_gradient((245, 252, 255), (214, 240, 252)), mask=mirrored_front)

    # Slanted glints on the reflection suggest both glass and speed.
    glints = Image.new("L", (SIZE, SIZE), 0)
    glint_draw = ImageDraw.Draw(glints)
    for x, width in ((640, 64), (736, 26)):
        glint_draw.polygon(
            [
                (px(x + 110), px(340)),
                (px(x + 110 + width), px(340)),
                (px(x - 60 + width), px(740)),
                (px(x - 60), px(740)),
            ],
            fill=255,
        )
    glints = ImageChops.multiply(glints, mirrored_front)
    canvas.paste(Image.new("RGB", (SIZE, SIZE), (178, 224, 248)), mask=glints)

    # The mirror line.
    ImageDraw.Draw(canvas).rounded_rectangle(
        (px(494), px(208), px(530), px(816)), radius=px(18), fill=(255, 255, 255, 255)
    )

    canvas.putalpha(ImageChops.multiply(canvas.getchannel("A"), tile))

    assets = Path(__file__).resolve().parent.parent / "assets"
    assets.mkdir(exist_ok=True)
    final = canvas.resize((1024, 1024), Image.LANCZOS)
    final.resize((256, 256), Image.LANCZOS).save(assets / "icon-256.png", optimize=True)
    final.save(
        assets / "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
