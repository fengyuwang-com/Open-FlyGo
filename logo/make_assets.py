#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Generate FlyGo brand raster assets from the flydog-logo.svg geometry.
Outputs:
  logo/flygo-logo-512.png       - master 512x512 (og-image, store, favicon base)
  logo/flygo-logo-1024.png      - 1024x1024 (play store 1024)
  logo/flygo.ico                - multi-res ICO (16/24/32/48/64/128/256) for exe
  logo/favicon-32.png           - 32x32 favicon
  logo/apple-touch-icon.png     - 180x180
  logo/og-image.png             - 1200x1200 (OpenGraph, keeps rounded square style)
Renders the v5 logo: blue gradient rounded-square tile + white dog-head
silhouette + dark-blue eye/nose + 3 speed lines, exactly per flydog-logo.svg.
Fully self-contained (only Pillow). No SVG rasterizer dependency.
"""
import os
from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "")

def lerp(a, b, t):
    return a + (b - a) * t

def point_on_cubic(p0, p1, p2, p3, t):
    """De Casteljau for a cubic bezier."""
    mt = 1.0 - t
    a = (mt*mt*mt*p0[0], mt*mt*mt*p0[1])
    b = (3*mt*mt*t*p1[0], 3*mt*mt*t*p1[1])
    c = (3*mt*t*t*p2[0], 3*mt*t*t*p2[1])
    d = (t*t*t*p3[0], t*t*t*p3[1])
    return (a[0]+b[0]+c[0]+d[0], a[1]+b[1]+c[1]+d[1])

def sample_cubic(p0, p1, p2, p3, steps=40):
    return [point_on_cubic(p0, p1, p2, p3, i/steps) for i in range(steps+1)]

# Dog head silhouette path (from flydog-logo.svg), as a list of ("L"|"C", pts)
def dog_head_points():
    pts = []
    pts.append((469, 255))
    # The path is: M 469 255 L 427 232 L 383 224 L 334 191 L 288 178 L 267 167
    # L 237 68 L 199 170  C 170 199 153 242 159 283  L 151 342  C 159 383 183 402 209 394
    # L 264 348 L 310 332 L 348 310 L 369 291 L 469 255 Z
    l = [(427,232),(383,224),(334,191),(288,178),(267,167),(237,68),(199,170)]
    for p in l: pts.append(p)
    # C 170 199 153 242 159 283   (p0=199,170 ; c1=170,199 ; c2=153,242 ; end=159,283)
    pts += sample_cubic((199,170),(170,199),(153,242),(159,283))[1:]
    pts.append((151,342))
    # C 159 383 183 402 209 394   (p0=151,342 ; c1=159,383 ; c2=183,402 ; end=209,394)
    pts += sample_cubic((151,342),(159,383),(183,402),(209,394))[1:]
    l2 = [(264,348),(310,332),(348,310),(369,291),(469,255)]
    for p in l2: pts.append(p)
    return pts

def draw_logo(size, corner_radius=None):
    """Render the FlyGo v5 logo at `size` pixels. Returns RGBA Image."""
    s = float(size)
    if corner_radius is None:
        corner_radius = s * (112.0/512.0)  # rx=112 on 512 canvas
    img = Image.new("RGBA", (size, size), (0,0,0,0))
    draw = ImageDraw.Draw(img)

    # ---- gradient background (#0A2E6E -> #1A73E8, top-left -> bottom-right) ----
    c0 = (0x0A,0x2E,0x6E); c1 = (0x1A,0x73,0xE8)
    # Build gradient by drawing many 1px horizontal bands (cheap, correct enough)
    grad = Image.new("RGBA", (size,size), (0,0,0,0))
    gd = ImageDraw.Draw(grad)
    for y in range(size):
        t = y / max(1,size-1)
        r = int(lerp(c0[0],c1[0],t))
        g = int(lerp(c0[1],c1[1],t))
        b = int(lerp(c0[2],c1[2],t))
        gd.line([(0,y),(size,y)], fill=(r,g,b,255))
    # Apply rounded-corner mask + composite
    mask = Image.new("L", (size,size), 0)
    md = ImageDraw.Draw(mask)
    md.rounded_rectangle([0,0,size-1,size-1], radius=corner_radius, fill=255)
    img = Image.composite(grad, img, mask)
    draw = ImageDraw.Draw(img)

    # ---- speed lines (lower-left, white, straight) ----
    # scale from 512 coords
    def P(x,y):
        return (x*s/512.0, y*s/512.0)
    w8 = 8*s/512.0; w6=6*s/512.0; w5=5*s/512.0
    draw.line([P(120,444),P(170,394)], fill=(255,255,255,255), width=max(1,int(round(w8))))
    draw.line([P(124,478),P(168,434)], fill=(255,255,255,230), width=max(1,int(round(w6))))
    draw.line([P(140,496),P(176,460)], fill=(255,255,255,220), width=max(1,int(round(w5))))

    # ---- dog head silhouette (white fill) ----
    head = [P(x,y) for (x,y) in dog_head_points()]
    draw.polygon(head, fill=(255,255,255,255))

    # ---- eye (circle) & nose tip (ellipse) in dark blue #0A2E6E ----
    eye_c = P(321,227); eye_r = 8.5*s/512.0
    draw.ellipse([eye_c[0]-eye_r, eye_c[1]-eye_r, eye_c[0]+eye_r, eye_c[1]+eye_r], fill=(0x0A,0x2E,0x6E,255))
    nose_c = P(464,247); nrx=7*s/512.0; nry=5*s/512.0
    draw.ellipse([nose_c[0]-nrx, nose_c[1]-nry, nose_c[0]+nrx, nose_c[1]+nry], fill=(0x0A,0x2E,0x6E,255))

    return img

def save_ico(img512, path):
    """Build a multi-resolution ICO from the 512 master."""
    sizes = [16,24,32,48,64,128,256]
    # Pillow's ICO writer generates each size internally from the source.
    img512.save(path, format="ICO", sizes=[(sz,sz) for sz in sizes])

if __name__ == "__main__":
    img512 = draw_logo(512)
    img512.save(OUT+"flygo-logo-512.png")
    img1024 = draw_logo(1024)
    img1024.save(OUT+"flygo-logo-1024.png")
    save_ico(img512, OUT+"flygo.ico")

    # favicon 32 & 48
    img512.resize((32,32), Image.LANCZOS).save(OUT+"favicon-32.png")
    img512.resize((180,180), Image.LANCZOS).save(OUT+"apple-touch-icon.png")
    # og-image: 1200x1200 centered logo on transparent bg (rounded square style)
    img512.resize((1200,1200), Image.LANCZOS).save(OUT+"og-image.png")
    print("done")
