#!/usr/bin/env python3
"""Extract the four official block-area textures from a Phigros 4.0.x APK.

Read-only: the APK is never modified. Output goes to ``assets/blockarea/``,
which is git-ignored (these are decoded official assets; same repository policy
as Phira Pro).

Usage:
    python docs/block-area/extract_official_assets.py <path/to/Phigros.apk>

Reference APK: Phigros 4.0.1 (versionCode 157). With the default settings the
produced files are byte-identical to Phira Pro's ``assets/blockarea`` export:

    BlockNoise1.png      256x256  RGBA  78a6d7e8113f0844...
    PointNoise.png       128x128  RGBA  335d31458f34db5f...
    FD_Noise_00000.png   256x256  RGBA  b04a8a943d202511...
    TouchHover.png        44x44   RGBA  59171418baf7bd52...   (source: Round10_Blur4)

RGB565 expansion (read this before "fixing" FD_Noise_00000)
-----------------------------------------------------------
``FD_Noise_00000`` is stored as RGB565. Two reasonable 5/6/5 -> 8/8/8
expansions exist:

* ``scale`` (default) - ``floor(code * 255 / max)``. This is what UnityPy's
  ``Texture2D.image`` does, and therefore what ships in Phira Pro's PNG.
* ``replicate`` - ``(code << shift) | (code >> (8 - 2*shift))``. This is what a
  GLES driver does when it samples an RGB565 texture natively, i.e. what the
  official client actually renders.

The two differ by at most 1/255 (max 7/3/7 against a naive shift-only
expansion). We default to ``scale`` so that the asset is byte-identical to
Phira Pro's and any later pixel diff is attributable to our pipeline rather
than to the texture. Pass ``--rgb565-expand replicate`` for the GPU-exact
variant.

Note: Phira Pro's ``docs/block-area/README.md`` states that its PNG was
regenerated from the raw APK RGB565 words with bit replication. The artifact it
actually ships matches UnityPy's ``scale`` output instead, so the two disagree
by <=1/255. This script can produce either; ``--verify-against`` checks the
result against the recorded hashes.
"""

import argparse
import hashlib
import sys
import zipfile
from pathlib import Path

import numpy as np
from PIL import Image

try:
    import UnityPy
except ImportError:  # pragma: no cover - optional review dependency
    sys.exit("UnityPy is required: pip install UnityPy")

# Output name -> (source texture name, width, height, UnityPy TextureFormat id)
TARGETS = {
    "BlockNoise1.png": ("BlockNoise1", 256, 256, 63),
    "PointNoise.png": ("PointNoise", 128, 128, 63),
    "FD_Noise_00000.png": ("FD_Noise_00000", 256, 256, 7),
    "TouchHover.png": ("Round10_Blur4", 44, 44, 4),
}

FORMAT_NOTE = {63: "Alpha8", 7: "RGB565", 4: "RGBA32"}

# Recorded hashes for the "scale" variant extracted from Phigros 4.0.1.
REFERENCE_SHA256 = {
    "BlockNoise1.png": "78a6d7e8113f084452f3efe31c24940cf925f8fbf0685399eca20c6032b82070",
    "PointNoise.png": "335d31458f34db5f7611ddc6cd6e959fad6a415c0dfb4761a1299ed44d8add72",
    "FD_Noise_00000.png": "b04a8a943d202511807d7f6f1f69080434c5b2b42368577edeb5fba7d4ee2d8e",
    "TouchHover.png": "59171418baf7bd52e384b3df17e38ef4ae84e81055a6fcb3f5df202f51299540",
}


def expand_rgb565(words: np.ndarray, mode: str) -> np.ndarray:
    """RGB565 words -> RGB888, either by bit replication or by 255/max scaling."""
    r5 = (words >> 11) & 0x1F
    g6 = (words >> 5) & 0x3F
    b5 = words & 0x1F
    if mode == "replicate":
        r, g, b = (r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2)
    else:
        r, g, b = r5 * 255 // 31, g6 * 255 // 63, b5 * 255 // 31
    return np.stack([r, g, b], axis=-1).astype(np.uint8)


def decode(texture, rgb565_expand: str) -> Image.Image:
    """Decode a Texture2D to a top-down RGBA image.

    The Alpha8 / RGBA32 textures go through UnityPy unchanged; only RGB565 needs
    the explicit expansion above. UnityPy already flips texture rows back to the
    top-down convention, so no extra flip is applied here.
    """
    fmt = int(texture.m_TextureFormat)
    if fmt == 7:
        w, h = texture.m_Width, texture.m_Height
        raw = texture.image_data
        expected = w * h * 2
        if len(raw) < expected:
            raise ValueError(f"RGB565 payload too short: {len(raw)} < {expected}")
        words = np.frombuffer(raw[:expected], dtype="<u2").reshape(h, w)
        # Unity stores rows bottom-up; flip so the PNG is top-down, matching
        # what UnityPy returns for the other formats.
        image = Image.fromarray(np.flipud(expand_rgb565(words, rgb565_expand)), "RGB")
        return image.convert("RGBA")
    return texture.image.convert("RGBA")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("apk", type=Path, nargs="?", default=None, help="Phigros 4.0.x APK")
    parser.add_argument("--out", type=Path, default=Path("assets/blockarea"), help="output directory (default: assets/blockarea)")
    parser.add_argument("--rgb565-expand", choices=("scale", "replicate"), default="scale", help="5/6/5 -> 8/8/8 expansion for FD_Noise_00000 (default: scale, byte-identical to Phira Pro)")
    parser.add_argument("--verify", action="store_true", help="compare each output against the recorded hash")
    args = parser.parse_args()

    if args.apk is None:
        parser.error("an APK path is required")
    if not args.apk.is_file():
        sys.exit(f"APK not found: {args.apk}")

    args.out.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(args.apk) as apk:
        environment = UnityPy.load(apk.read("assets/bin/Data/data.unity3d"))

    found = {}
    for obj in environment.objects:
        if obj.type.name != "Texture2D":
            continue
        try:
            texture = obj.read()
        except Exception:
            continue
        if texture.m_Name in {name for name, *_ in TARGETS.values()} and texture.m_Name not in found:
            found[texture.m_Name] = texture

    missing = {name for name, *_ in TARGETS.values()} - set(found)
    if missing:
        sys.exit(f"missing textures in this APK: {sorted(missing)}")

    status = 0
    for out_name, (source, want_w, want_h, want_fmt) in TARGETS.items():
        texture = found[source]
        fmt = int(texture.m_TextureFormat)
        if (texture.m_Width, texture.m_Height, fmt) != (want_w, want_h, want_fmt):
            print(
                f"  ! {source}: got {texture.m_Width}x{texture.m_Height} fmt={fmt} ({FORMAT_NOTE.get(fmt, '?')}), "
                f"expected {want_w}x{want_h} fmt={want_fmt} ({FORMAT_NOTE[want_fmt]})",
                file=sys.stderr,
            )
            status = 1

        image = decode(texture, args.rgb565_expand)
        path = args.out / out_name
        image.save(path)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()

        note = ""
        if args.verify:
            want = REFERENCE_SHA256[out_name]
            if digest == want:
                note = "  verified"
            elif args.rgb565_expand == "replicate" and out_name == "FD_Noise_00000.png":
                note = "  (replicate variant; differs from the recorded 'scale' hash by <=1/255)"
            else:
                note = f"  !! MISMATCH, expected {want[:16]}..."
                status = 1

        print(f"  {source:18s} -> {path}  {image.size[0]}x{image.size[1]}  {FORMAT_NOTE.get(fmt, fmt):7s} {digest[:16]}...{note}")

    return status


if __name__ == "__main__":
    raise SystemExit(main())
