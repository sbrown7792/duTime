#!/usr/bin/env python3
"""Python port of the dataviz skill's validate_palette.js.

Same thresholds, same Machado-Oliveira-Fernandes (2009) severity-1.0 CVD
transforms, same OKLab Delta-E x100 metric. Exists because this project has no
Node toolchain and the palette must be computed, not eyeballed.
"""
import math, sys

BAND = {"light": (0.43, 0.77), "dark": (0.48, 0.67)}
CHROMA_FLOOR = 0.10
CVD_TARGET, CVD_FLOOR = 8.0, 6.0
NORMAL_FLOOR = 15.0
CONTRAST_MIN = 3.0
SURFACE = {"light": "#fcfcfb", "dark": "#1a1a19"}

MACHADO = {
 "protan": [[0.152286,1.052583,-0.204868],[0.114503,0.786281,0.099216],[-0.003882,-0.048116,1.051998]],
 "deutan": [[0.367322,0.860646,-0.227968],[0.280085,0.672501,0.047413],[-0.011820,0.042940,0.968881]],
 "tritan": [[1.255528,-0.076749,-0.178779],[-0.078411,0.930809,0.147602],[0.004733,0.691367,0.303900]],
}

def hex2srgb(h):
    h = h.strip().lstrip('#')
    return [int(h[i:i+2], 16)/255 for i in (0, 2, 4)]

def s2lin(c): return c/12.92 if c <= 0.04045 else ((c+0.055)/1.055)**2.4
def lin(h): return [s2lin(c) for c in hex2srgb(h)]
def rellum(h):
    r, g, b = lin(h); return 0.2126*r + 0.7152*g + 0.0722*b
def contrast(a, b):
    hi, lo = sorted([rellum(a), rellum(b)], reverse=True)
    return (hi+0.05)/(lo+0.05)

def oklab_from_lin(rgb):
    r, g, b = rgb
    l = (0.4122214708*r + 0.5363325363*g + 0.0514459929*b) ** (1/3)
    m = (0.2119034982*r + 0.6806995451*g + 0.1073969566*b) ** (1/3)
    s = (0.0883024619*r + 0.2817188376*g + 0.6299787005*b) ** (1/3)
    return [0.2104542553*l + 0.7936177850*m - 0.0040720468*s,
            1.9779984951*l - 2.4285922050*m + 0.4505937099*s,
            0.0259040371*l + 0.7827717662*m - 0.8086757660*s]

def oklch(h):
    L, a, b = oklab_from_lin(lin(h)); return L, math.hypot(a, b)

def simulate(h, kind):
    r, g, b = lin(h); M = MACHADO[kind]
    return [min(1, max(0, M[i][0]*r + M[i][1]*g + M[i][2]*b)) for i in range(3)]

def deltaE(h1, h2, kind=None):
    a = oklab_from_lin(simulate(h1, kind) if kind else lin(h1))
    b = oklab_from_lin(simulate(h2, kind) if kind else lin(h2))
    return 100*math.dist(a, b)

def validate(palette, mode="light", surface=None, pairs="adjacent"):
    surface = surface or SURFACE[mode]
    lo, hi = BAND[mode]; ok = True
    print(f"\n=== {mode} mode, {pairs} pairs, surface {surface} ===")

    off = [(c, round(oklch(c)[0], 3)) for c in palette if not (lo <= oklch(c)[0] <= hi)]
    ok &= not off
    print(f"{'Lightness band':<22} {'PASS' if not off else 'FAIL':<6} " +
          (f"outside L {lo}-{hi}: {off}" if off else f"all {len(palette)} inside L {lo}-{hi}"))

    lowc = [(c, round(oklch(c)[1], 3)) for c in palette if oklch(c)[1] < CHROMA_FLOOR]
    ok &= not lowc
    print(f"{'Chroma floor':<22} {'PASS' if not lowc else 'FAIL':<6} " +
          (f"reads gray: {lowc}" if lowc else f"all {len(palette)} >= {CHROMA_FLOOR}"))

    n = len(palette)
    pl = ([(i, j) for i in range(n) for j in range(i+1, n)] if pairs == "all"
          else [(i, i+1) for i in range(n-1)])
    worst = min(((deltaE(palette[i], palette[j], k), k, palette[i], palette[j])
                 for k in ("protan", "deutan") for i, j in pl), default=None)
    tri = min((deltaE(palette[i], palette[j], "tritan") for i, j in pl), default=99)
    wd = worst[0] if worst else 99
    state = "PASS" if wd >= CVD_TARGET else ("FLOOR" if wd >= CVD_FLOOR else "FAIL")
    ok &= state != "FAIL"
    print(f"{'CVD separation':<22} {state:<6} worst {worst[3]}<->{worst[2]} dE {wd:.1f} ({worst[1]}) - tritan {tri:.1f}")

    nworst = min(((deltaE(palette[i], palette[j]), palette[i], palette[j]) for i, j in pl), default=None)
    nd = nworst[0] if nworst else 99
    nstate = "PASS" if nd >= NORMAL_FLOOR else "FAIL"
    ok &= nstate == "PASS"
    print(f"{'Normal-vision floor':<22} {nstate:<6} worst {nworst[2]}<->{nworst[1]} dE {nd:.1f}")

    low = [(c, round(contrast(c, surface), 2)) for c in palette if contrast(c, surface) < CONTRAST_MIN]
    print(f"{'Contrast vs surface':<22} {'PASS' if not low else 'WARN':<6} " +
          (f"sub-3:1 (needs labels/table): {low}" if low else f"all >= {CONTRAST_MIN}:1"))
    print(f"{'RESULT':<22} {'OK' if ok else 'FAILED'}")
    return ok

if __name__ == "__main__":
    CAT_LIGHT = ["#2a78d6","#eb6834","#1baf7a","#eda100","#e87ba4","#008300","#4a3aa7","#e34948"]
    CAT_DARK  = ["#3987e5","#d95926","#199e70","#c98500","#d55181","#008300","#9085e9","#e66767"]
    allok = True
    # Stacked area: segments are adjacent, so the adjacent pairlist applies.
    allok &= validate(CAT_LIGHT, "light")
    allok &= validate(CAT_DARK, "dark")
    # Diff treemap: tiles touch arbitrarily, so the diverging arms get the
    # all-pairs treatment against their own surface.
    DIV_LIGHT = ["#0d366b","#256abf","#86b6ef","#f0efec","#f0a3a3","#d03b3b","#8c1f1f"]
    DIV_DARK  = ["#9ec5f4","#3987e5","#1c5cab","#383835","#8c1f1f","#d03b3b","#e89a9a"]
    print("\n--- diverging ramps: contrast + endpoint separation only ---")
    for name, ramp, mode in (("light", DIV_LIGHT, "light"), ("dark", DIV_DARK, "dark")):
        s = SURFACE[mode]
        print(f"{name}: endpoints dE {deltaE(ramp[0], ramp[-1]):.1f} | "
              f"midpoint contrast {contrast(ramp[3], s):.2f}:1 | "
              f"extreme contrast {contrast(ramp[0], s):.2f}:1 / {contrast(ramp[-1], s):.2f}:1")
        for kind in ("protan", "deutan"):
            print(f"   {kind}: cool-arm vs warm-arm dE {deltaE(ramp[1], ramp[5], kind):.1f}")
    sys.exit(0 if allok else 1)
