#!/usr/bin/env python3
"""Audit the block-area material constants against the official APK.

The block-area shader draws with a fixed set of material parameters
(``_FillColor``, ``_EdgeOpacity``, ...). They live in ``ActiveBlock.mat``,
``DisabledBlock.mat`` and ``ReadyBlock.mat`` inside the game's Unity data, and
this project hard-codes them in ``phire/src/core/block_shader.rs``.

Hard-coded numbers rot silently, so this script re-reads them from the APK and
diffs them against the Rust source. Run it after touching any constant.

Usage:
    python docs/block-area/audit_material_constants.py <path/to/Phigros.apk>
    python docs/block-area/audit_material_constants.py <path/to/Phigros.apk> --dump

Reference APK: Phigros 4.0.1 (versionCode 157).

Naming note
-----------
Phira Pro renamed several DisabledBlock material fields when porting the
official shader into a single ``block_shader_full.frag``. ``DisabledBlock.mat``
only has the Unity names, so this script maps them back:

    uDisabledFillColor      <- DisabledBlock._FillColor
    uDisabledFillOpacity    <- DisabledBlock._FillOpacity
    uDisabledSparkOpacity   <- DisabledBlock._SparkMapOpacity
    uDisabledSparkIntensity <- DisabledBlock._SparkDisplaceIntensity
    uDisabledSparkTint      <- DisabledBlock._SparkTint
    uDisabledSpeed          <- DisabledBlock._DisplaceSpeed

Also note ``_SparkHueShiftAmount`` differs per material (ActiveBlock 0.2,
DisabledBlock 0.73). The active layer uses the ActiveBlock value; the disabled
layer in ``block_shader_full.frag`` has its hue shift pre-folded into
``uDisabledSparkTint``, so only the ActiveBlock value is loaded.
"""

import argparse
import re
import sys
import zipfile
from pathlib import Path

try:
    import UnityPy
except ImportError:  # pragma: no cover - optional review dependency
    sys.exit("UnityPy is required: pip install UnityPy")

# Rust name -> (material, property, kind) where kind is "float" / "color" / "vec3"
EXPECTED = {
    "_EdgeOpacity": ("ActiveBlock", "_EdgeOpacity", "float"),
    "_FillStrength": ("ActiveBlock", "_FillStrength", "float"),
    "_FillOpacity": ("ActiveBlock", "_FillOpacity", "float"),
    "_GlowIntensity": ("ActiveBlock", "_GlowIntensity", "float"),
    "_SparkMapOpacity": ("ActiveBlock", "_SparkMapOpacity", "float"),
    "_SparkHueShiftAmount": ("ActiveBlock", "_SparkHueShiftAmount", "float"),
    "_SparkDisplaceIntensity": ("ActiveBlock", "_SparkDisplaceIntensity", "float"),
    "_DisplaceBlendIntensity": ("ActiveBlock", "_DisplaceBlendIntensity", "float"),
    "_DisplaceSpeed": ("ActiveBlock", "_DisplaceSpeed", "float"),
    "_DisplaceStrength": ("ActiveBlock", "_DisplaceStrength", "float"),
    "_TouchPosRadius": ("ActiveBlock", "_TouchPosRadius", "float"),
    "_TouchPosSDFSmoothness": ("ActiveBlock", "_TouchPosSDFSmoothness", "float"),
    "_TouchPosSDFFalloff": ("ActiveBlock", "_TouchPosSDFFalloff", "float"),
    "_BackgroundPixelScale": ("ActiveBlock", "_BackgroundPixelScale", "float"),
    "_ShineSpeed": ("ActiveBlock", "_ShineSpeed", "float"),
    "_ShineBrightness": ("ActiveBlock", "_ShineBrightness", "float"),
    "_TouchDisplaceSpeed": ("ActiveBlock", "_TouchDisplaceSpeed", "float"),
    "_TouchDisplaceStrength": ("ActiveBlock", "_TouchDisplaceStrength", "float"),
    "_NoiseEvoSpeed": ("ActiveBlock", "_NoiseEvoSpeed", "float"),
    "_NoiseDirChangeSpeed": ("ActiveBlock", "_NoiseDirChangeSpeed", "float"),
    "_NoiseDisplaceStrength": ("ActiveBlock", "_NoiseDisplaceStrength", "float"),
    "_NoiseRadius": ("ActiveBlock", "_NoiseRadius", "float"),
    "_NoiseSmoothness": ("ActiveBlock", "_NoiseSmoothness", "float"),
    "_SDFCellSize": ("ActiveBlock", "_SDFCellSize", "float"),
    "_SDFSmoothness": ("ActiveBlock", "_SDFSmoothness", "float"),
    "_SDFFalloff": ("ActiveBlock", "_SDFFalloff", "float"),
    "_SDFMoveSpeed": ("ActiveBlock", "_SDFMoveSpeed", "float"),
    "_TouchBackgroundPixelScale": ("ActiveBlock", "_TouchBackgroundPixelScale", "float"),
    "_TouchPosShineSpeed": ("ActiveBlock", "_TouchPosShineSpeed", "float"),
    "_TouchPosBrightness": ("ActiveBlock", "_TouchPosBrightness", "float"),
    "_TouchPosDarkness": ("ActiveBlock", "_TouchPosDarkness", "float"),
    "_TouchPosLowThreshold": ("ActiveBlock", "_TouchPosLowThreshold", "float"),
    # DisabledBlock, renamed by Phira Pro
    "uDisabledFillOpacity": ("DisabledBlock", "_FillOpacity", "float"),
    "uDisabledSparkOpacity": ("DisabledBlock", "_SparkMapOpacity", "float"),
    "uDisabledSparkIntensity": ("DisabledBlock", "_SparkDisplaceIntensity", "float"),
    "uDisabledSpeed": ("DisabledBlock", "_DisplaceSpeed", "float"),
    # Colors
    "_EdgeColor": ("ActiveBlock", "_EdgeColor", "color"),
    "_FillColor": ("ActiveBlock", "_FillColor", "color"),
    "_GlowColor": ("ActiveBlock", "_GlowColor", "color"),
    "_DisplaceDirection": ("ActiveBlock", "_DisplaceDirection", "color"),
    "_ShineColor": ("ActiveBlock", "_ShineColor", "color"),
    "_TouchDisplaceDirection": ("ActiveBlock", "_TouchDisplaceDirection", "color"),
    "_NoiseTint": ("ActiveBlock", "_NoiseTint", "color"),
    "_TouchGlowColor": ("ActiveBlock", "_TouchGlowColor", "color"),
    "_SparkTint": ("ActiveBlock", "_SparkTint", "vec3"),
    "uDisabledFillColor": ("DisabledBlock", "_FillColor", "color"),
    "uDisabledSparkTint": ("DisabledBlock", "_SparkTint", "vec3"),
}

TOLERANCE = 2e-4


def load_materials(apk_path: Path) -> dict:
    with zipfile.ZipFile(apk_path) as apk:
        environment = UnityPy.load(apk.read("assets/bin/Data/data.unity3d"))
    materials = {}
    for obj in environment.objects:
        if obj.type.name != "Material":
            continue
        try:
            material = obj.read()
        except Exception:
            continue
        name = getattr(material, "m_Name", None)
        if name not in {"ActiveBlock", "DisabledBlock", "ReadyBlock"}:
            continue
        props = material.m_SavedProperties
        floats, colors = {}, {}
        for key, value in (props.m_Floats or {}).items():
            floats[str(key)] = float(value)
        for key, value in (props.m_Colors or {}).items():
            components = [getattr(value, attr, None) for attr in ("r", "g", "b", "a")]
            if any(component is None for component in components):
                components = [getattr(value, attr, None) for attr in ("R", "G", "B", "A")]
            colors[str(key)] = [round(float(component), 6) for component in components]
        materials[name] = {"floats": floats, "colors": colors}
    return materials


def parse_rust(path: Path):
    """Pull the FLOATS / COLORS tables out of block_shader.rs."""
    source = path.read_text(encoding="utf-8")
    floats, colors = {}, {}

    block = re.search(r"const FLOATS:\s*&\[\(&str, f32\)\]\s*=\s*&\[(.*?)\n\];", source, re.S)
    if block:
        for name, value in re.findall(r'\("([^"]+)",\s*([-\d.eE_]+)\)', block.group(1)):
            floats[name] = float(value.replace("_", ""))

    block = re.search(r"const COLORS:\s*&\[\(&str, \[f32; 4\]\)\]\s*=\s*&\[(.*?)\n\];", source, re.S)
    if block:
        for name, body in re.findall(r'\("([^"]+)",\s*\[([^\]]*)\]\)', block.group(1)):
            colors[name] = [float(part.strip().replace("_", "")) for part in body.split(",") if part.strip()]

    # vec3 uniforms are set with set_uniform(...) instead of living in the tables
    for name, body in re.findall(r'set_uniform\("([^"]+)",\s*vec3\(([^)]*)\)\)', source):
        colors.setdefault(name, [float(part.strip().replace("_", "")) for part in body.split(",") if part.strip()])

    return floats, colors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("apk", type=Path)
    parser.add_argument("--rust", type=Path, default=Path("phire/src/core/block_shader.rs"), help="Rust file holding the constants")
    parser.add_argument("--dump", action="store_true", help="print every value instead of only the mismatches")
    args = parser.parse_args()

    if not args.apk.is_file():
        sys.exit(f"APK not found: {args.apk}")
    if not args.rust.is_file():
        sys.exit(f"Rust source not found: {args.rust} (run from the workspace root, or pass --rust)")

    materials = load_materials(args.apk)
    floats, colors = parse_rust(args.rust)

    missing = {"ActiveBlock", "DisabledBlock", "ReadyBlock"} - set(materials)
    if missing:
        sys.exit(f"materials not found in the APK: {sorted(missing)}")

    failures = 0
    checked = 0
    for rust_name, (material, prop, kind) in sorted(EXPECTED.items()):
        table, label = (floats, "float") if kind == "float" else (colors, kind)
        if rust_name not in table:
            print(f"  ?  {rust_name:32s} not present in {args.rust.name}")
            failures += 1
            continue

        got = table[rust_name]
        if kind == "float":
            want = materials[material]["floats"].get(prop)
            if want is None:
                print(f"  ?  {rust_name:32s} {material}.{prop} missing from APK")
                failures += 1
                continue
            ok = abs(got - want) <= TOLERANCE
            got_show, want_show = f"{got!r}", f"{want!r}"
        else:
            want_full = materials[material]["colors"].get(prop)
            if want_full is None:
                print(f"  ?  {rust_name:32s} {material}.{prop} missing from APK")
                failures += 1
                continue
            want = want_full[: len(got)]
            ok = all(abs(a - b) <= TOLERANCE for a, b in zip(got, want))
            got_show, want_show = str(got), str(want)

        checked += 1
        if ok and not args.dump:
            continue
        mark = "ok " if ok else "!! "
        print(f"  {mark} {rust_name:32s} {label:6s} rust={got_show:34s} apk({material}.{prop})={want_show}")
        if not ok:
            failures += 1

    print(f"\nchecked {checked} constants, {failures} problem(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
