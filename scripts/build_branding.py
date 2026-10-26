#!/usr/bin/env python3
"""Deterministic size/format conversion of the approved master (Pillow dev tool).

No image generation here. Source master and portrait are never overwritten.
Run: python3 scripts/build_branding.py [--check]
"""
import argparse
import io
from pathlib import Path
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SIZES = (16, 24, 32, 48, 64, 128, 256, 512)


def outputs():
    master = Image.open(ROOT / 'assets/branding/mascot-master.png').convert('RGBA')
    if master.width != master.height or master.getextrema()[3][0] != 0:
        raise ValueError('Square master with real alpha required')
    results = {}
    for size in SIZES:
        image = master.resize((size, size), Image.Resampling.LANCZOS)
        buffer = io.BytesIO()
        image.save(buffer, format='PNG', optimize=False)
        results[f'assets/branding/icons/bunny-and-size-{size}.png'] = buffer.getvalue()
    buffer = io.BytesIO()
    master.save(buffer, format='ICO', sizes=[(size, size) for size in (16,24,32,48,64,256)])
    results['assets/branding/bunny-and-size.ico'] = buffer.getvalue()
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    for name, raw in outputs().items():
        target = ROOT / name
        if args.check:
            if target.read_bytes() != raw:
                raise SystemExit('Stale icon: '+name)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(raw)
        print('PASS', name)


if __name__ == '__main__':
    main()
