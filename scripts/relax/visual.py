#!/usr/bin/env python3
"""Original procedural coastal-room illustration and a precisely periodic loop.

No photographs, stock footage, fonts, assets, or third-party image content.
Requires Python, Pillow, numpy and FFmpeg. Copyright 2026 Alexey Safonov.
"""
from __future__ import annotations

import argparse
import json
import math
import random
import subprocess
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

W, H, SS = 1280, 720, 2
FPS, SECONDS, SEED = 16, 90, 6042026
TAU = math.tau
HERE = Path(__file__).resolve().parent
WINDOW = (89, 55, 833, 528)


def gradient(size, upper, lower):
    width, height = size
    factor = np.linspace(0, 1, height)[:, None, None]
    rgb = np.array(upper)[None, None, :] * (1 - factor) + np.array(lower)[None, None, :] * factor
    return Image.fromarray(np.repeat(rgb, width, axis=1).astype('uint8'), 'RGB')


def scene():
    """Draw oversampled static artwork, then downsample for smooth shapes."""
    size = (W * SS, H * SS)
    im = gradient(size, (83, 71, 60), (123, 91, 63)).convert('RGBA')

    def layer():
        return Image.new('RGBA', size, (0, 0, 0, 0))

    def draw(target):
        return ImageDraw.Draw(target)

    def box(x):
        return tuple(round(v * SS) for v in x)

    def points(x):
        return [(round(a * SS), round(b * SS)) for a, b in x]

    def ellipse(target, bounds, color, outline=None, width=1):
        draw(target).ellipse(box(bounds), fill=color, outline=outline, width=width * SS)

    def line(target, xy, color, width=1):
        draw(target).line(points(xy), fill=color, width=round(width * SS), joint='curve')

    # Broad lamplight on textured plaster; all texture is deterministic.
    light = layer()
    ellipse(light, (575, 13, 1475, 865), (246, 174, 83, 109))
    light = light.filter(ImageFilter.GaussianBlur(130 * SS))
    im = Image.alpha_composite(im, light)
    plaster = layer()
    p = draw(plaster)
    rng = random.Random(SEED)
    for _ in range(12000):
        x, y = rng.randrange(W * SS), rng.randrange(H * SS)
        p.point((x, y), fill=(230, 213, 177, rng.randrange(2, 13)))
    im = Image.alpha_composite(im, plaster)

    # Recessed timber surround and the cool nocturnal view.
    draw(im).rounded_rectangle(box((64, 29, 858, 557)), radius=8*SS, fill=(45, 36, 33, 255))
    view = gradient((744*SS, 473*SS), (21, 43, 62), (79, 101, 108)).convert('RGBA')
    im.paste(view, (89*SS, 55*SS))
    clouds = layer()
    for x, y, a, b in ((104, 111, 408, 195), (264, 86, 696, 171), (516, 143, 991, 212), (32, 258, 803, 320)):
        ellipse(clouds, (x, y, a, b), (132, 150, 157, 20))
    clouds = clouds.filter(ImageFilter.GaussianBlur(25*SS))
    window_mask = Image.new('L', size, 0)
    ImageDraw.Draw(window_mask).rectangle(box(WINDOW), fill=255)
    clouds.putalpha(Image.composite(clouds.getchannel('A'), Image.new('L', size, 0), window_mask))
    im = Image.alpha_composite(im, clouds)

    moon = layer()
    ellipse(moon, (584, 129, 656, 201), (221, 225, 200, 27))
    moon = moon.filter(ImageFilter.GaussianBlur(17*SS))
    im = Image.alpha_composite(im, moon)
    ellipse(im, (607, 151, 633, 177), (184, 200, 193, 175))
    haze = layer()
    ellipse(haze, (341, 240, 873, 313), (157, 184, 184, 24))
    im = Image.alpha_composite(im, haze.filter(ImageFilter.GaussianBlur(19*SS)))

    # A coast rather than a copied real location.
    draw(im).polygon(points([(89, 289), (122, 282), (161, 276), (200, 282), (251, 266), (278, 270), (317, 282), (366, 286), (418, 300), (89, 312)]), fill=(31, 49, 55, 255))
    draw(im).rectangle(box((89, 306, 833, 528)), fill=(26, 50, 66, 255))
    ocean = gradient((744*SS, 222*SS), (43, 68, 81), (22, 42, 53)).convert('RGBA')
    im.paste(ocean, (89*SS, 306*SS))
    for k in range(30):
        y = 322 + k * 6.5
        for _ in range(7):
            x = rng.uniform(91, 802)
            length = rng.uniform(5, 38) * (1+k/35)
            line(im, [(x, y), (min(x+length, 832), y)], (125, 160, 164, rng.randint(13, 28)), .6)
    # An unoccupied tiny lighthouse and distant windows.
    draw(im).polygon(points([(185, 281), (187, 251), (195, 251), (199, 283)]), fill=(104, 122, 117, 255))
    draw(im).rectangle(box((184, 246, 198, 253)), fill=(190, 148, 80, 255))
    draw(im).polygon(points([(182, 246), (191, 240), (201, 246)]), fill=(41, 49, 51, 255))
    for x, y in ((130, 287), (149, 286), (229, 281), (240, 279), (286, 287), (314, 291)):
        ellipse(im, (x, y, x+2, y+1.2), (229, 173, 88, 240))

    # Glass and three pane divisions. Light stays quiet and translucent.
    glass = layer()
    draw(glass).polygon(points([(115, 57), (199, 57), (509, 527), (422, 527)]), fill=(172, 194, 196, 8))
    draw(glass).polygon(points([(617, 57), (656, 57), (833, 312), (833, 373)]), fill=(200, 198, 160, 8))
    im = Image.alpha_composite(im, glass)
    frame = layer()
    d = draw(frame)
    for x in (80, 327, 574, 833):
        d.rectangle(box((x, 43, x+11, 542)), fill=(50, 44, 39, 255))
        d.rectangle(box((x+1, 43, x+3, 542)), fill=(137, 102, 65, 255))
    for y in (44, 535):
        d.rectangle(box((74, y, 851, y+11)), fill=(72, 55, 41, 255))
        line(frame, [(75, y+2), (850, y+2)], (156, 116, 73, 255), 1)
    d.polygon(points([(61, 544), (855, 544), (875, 562), (47, 562)]), fill=(152, 113, 75, 255))
    line(frame, [(48, 559), (875, 559)], (187, 142, 91, 255), 2)
    im = Image.alpha_composite(im, frame)

    # Quiet curtains framing the view, with folds.
    curtain = layer()
    d = draw(curtain)
    d.polygon(points([(0, 0), (85, 0), (77, 368), (48, 548), (0, 596)]), fill=(116, 110, 99, 255))
    d.polygon(points([(859, 0), (900, 0), (891, 470), (912, 555), (869, 549)]), fill=(116, 96, 77, 255))
    for x in (12, 28, 49, 64):
        line(curtain, [(x, 0), (x+6, 160), (x-3, 380), (max(0, x-16), 545)], (64, 69, 66, 80), 5)
    for x in (872, 884):
        line(curtain, [(x, 0), (x+4, 222), (x+7, 472), (x+18, 548)], (72, 68, 60, 60), 3)
    im = Image.alpha_composite(im, curtain)

    # Warm timber desk, with restrained grain.
    d = draw(im)
    d.polygon(points([(0, 575), (1120, 550), (1280, 611), (1280, 720), (0, 720)]), fill=(118, 80, 49, 255))
    wood = layer()
    d = draw(wood)
    d.polygon(points([(0, 587), (1119, 563), (1280, 622), (1280, 720), (0, 720)]), fill=(169, 114, 66, 255))
    for _ in range(180):
        y = rng.uniform(581, 720)
        x = rng.uniform(-70, 1180)
        length = rng.uniform(60, 420)
        line(wood, [(x, y), (x+length*.5, y-1), (x+length, y+1)], (83, 52, 34, rng.randint(9, 26)), .6)
    im = Image.alpha_composite(im, wood)
    line(im, [(0, 580), (1120, 555), (1280, 616)], (211, 155, 94, 255), 2)

    # Lamp, shade, and a pool of gold on the desk.
    pool = layer()
    ellipse(pool, (774, 554, 1216, 682), (247, 179, 83, 67))
    im = Image.alpha_composite(im, pool.filter(ImageFilter.GaussianBlur(34*SS)))
    ellipse(im, (976, 578, 1100, 602), (68, 49, 33, 255))
    ellipse(im, (982, 575, 1095, 591), (171, 126, 65, 255))
    line(im, [(1038, 389), (1038, 580)], (53, 54, 46, 255), 8)
    line(im, [(1040, 398), (1040, 576)], (203, 153, 80, 255), 1.5)
    shade = layer()
    d = draw(shade)
    d.polygon(points([(973, 306), (1100, 306), (1142, 408), (931, 408)]), fill=(237, 197, 129, 255))
    ellipse(shade, (931, 396, 1142, 420), (255, 211, 134, 255))
    ellipse(shade, (973, 299, 1100, 313), (178, 137, 83, 255))
    for x in range(947, 1131, 12):
        line(shade, [(1000+(x-1000)*.58, 311), (x, 404)], (184, 144, 92, 42), 1)
    im = Image.alpha_composite(im, shade)
    glow = layer()
    ellipse(glow, (911, 379, 1161, 460), (255, 197, 100, 42))
    im = Image.alpha_composite(im, glow.filter(ImageFilter.GaussianBlur(27*SS)))

    # A stack of unlabelled books; no words or trademarks.
    d = draw(im)
    for x, y, width, color in ((714, 608, 161, (65, 80, 79, 255)), (731, 586, 155, (179, 147, 104, 255)), (722, 568, 152, (84, 66, 55, 255))):
        d.rounded_rectangle(box((x, y, x+width, y+18)), radius=2*SS, fill=color)
        d.rectangle(box((x+12, y+3, x+width-4, y+13)), fill=(198, 183, 146, 255))
        for yy in range(y+5, y+13, 3):
            line(im, [(x+15, yy), (x+width-6, yy)], (150, 136, 107, 255), .5)

    # Open sketchbook and a pencil, without UI text.
    shadow = layer()
    draw(shadow).polygon(points([(187, 644), (374, 622), (581, 658), (389, 704)]), fill=(41, 34, 28, 76))
    im = Image.alpha_composite(im, shadow.filter(ImageFilter.GaussianBlur(7*SS)))
    d = draw(im)
    d.polygon(points([(165, 623), (363, 610), (369, 673), (190, 697)]), fill=(222, 208, 173, 255))
    d.polygon(points([(363, 610), (557, 640), (553, 691), (369, 673)]), fill=(242, 218, 175, 255))
    line(im, [(363, 612), (369, 672)], (116, 91, 62, 160), 1)
    for k in range(6):
        line(im, [(200, 639+k*7), (334, 627+k*7)], (150, 137, 105, 35), .5)
    line(im, [(443, 650), (530, 673)], (77, 74, 63, 255), 4)
    draw(im).polygon(points([(530, 671), (539, 675), (530, 675)]), fill=(217, 174, 108, 255))

    # Ceramic mug and saucer, the animation will provide the steam.
    ellipse(im, (477, 601, 618, 631), (57, 40, 31, 110))
    ellipse(im, (469, 593, 607, 622), (190, 169, 130, 255))
    ellipse(im, (476, 592, 601, 615), (224, 203, 163, 255))
    draw(im).arc(box((569, 539, 614, 586)), start=265, end=95, fill=(195, 178, 141, 255), width=10*SS)
    draw(im).rounded_rectangle(box((494, 534, 574, 604)), radius=16*SS, fill=(209, 191, 151, 255))
    ellipse(im, (494, 524, 574, 550), (235, 215, 173, 255))
    ellipse(im, (501, 529, 567, 547), (59, 38, 28, 255))
    line(im, [(500, 550), (503, 583)], (242, 220, 179, 180), 2)
    line(im, [(538, 594), (562, 591)], (132, 112, 78, 55), 1)

    # Leaf silhouettes provide a softer organic right edge.
    plant = layer()
    d = draw(plant)
    branches = [((1190, 570), (1166, 374)), ((1198, 573), (1240, 350)), ((1194, 570), (1179, 454)), ((1200, 570), (1271, 470))]
    for start, end in branches:
        line(plant, [start, end], (62, 76, 51, 255), 3)
    leaves = [(1164, 388, -1), (1170, 421, 1), (1162, 466, -1), (1236, 373, 1), (1225, 419, -1), (1211, 461, 1), (1181, 481, -1), (1187, 516, 1), (1251, 484, 1), (1228, 517, -1)]
    for x, y, direction in leaves:
        d.polygon(points([(x, y+16), (x+direction*19, y-19), (x+direction*49, y-31), (x+direction*41, y-2), (x, y+16)]), fill=(63, 86, 57, 255))
        line(plant, [(x, y+13), (x+direction*41, y-23)], (145, 143, 84, 115), 1)
    d.polygon(points([(1157, 555), (1239, 555), (1226, 632), (1172, 632)]), fill=(140, 91, 56, 255))
    ellipse(plant, (1157, 548, 1239, 563), (168, 115, 69, 255))
    ellipse(plant, (1165, 551, 1232, 560), (56, 45, 32, 255))
    im = Image.alpha_composite(im, plant)

    # Gentle fixed vignette. No changing grain: restful motion only.
    arr = np.asarray(im.convert('RGB'), dtype=np.float32)
    yy, xx = np.mgrid[:H*SS, :W*SS]
    radius = ((xx-W*SS*.52)/(W*SS*.7))**2 + ((yy-H*SS*.51)/(H*SS*.85))**2
    arr *= (1 - np.minimum(.28, .16*radius))[:, :, None]
    return Image.fromarray(np.clip(arr, 0, 255).astype('uint8'), 'RGB').resize((W, H), Image.Resampling.LANCZOS)


BASE = None
RAINDROPS = None


def frame(index: int):
    global BASE, RAINDROPS
    if BASE is None:
        BASE = scene()
        rng = random.Random(SEED + 20000)
        RAINDROPS = [(rng.uniform(96, 825), rng.uniform(0, 490), rng.uniform(8, 24), rng.randint(24, 54), rng.choice((1, 2, 3))) for _ in range(81)]
    t = (index % (FPS * SECONDS)) / FPS
    phase = TAU*t/SECONDS
    im = BASE.convert('RGBA')
    motion = Image.new('RGBA', (W, H), 0)
    d = ImageDraw.Draw(motion)

    # Wrapped rain: every particle period divides the full loop exactly.
    # Painting its translated copies on both boundaries avoids visual popping.
    for x, offset, length, alpha, laps in RAINDROPS:
        yy = 55 + ((offset + t*490*laps/SECONDS) % 490)
        for y in (yy-490, yy, yy+490):
            x1 = x + 1.4*math.sin(phase + offset)
            d.line([(x1, y), (x1-1.8, y+length)], fill=(196, 216, 215, alpha), width=1)
            d.line([(x1+1, y+length-3), (x1+1, y+length)], fill=(225, 220, 196, alpha//2), width=1)
    mask = Image.new('L', (W, H), 0)
    md = ImageDraw.Draw(mask)
    for x1, x2 in ((92, 326), (339, 573), (586, 832)):
        md.rectangle((x1, 58, x2, 528), fill=255)
    motion.putalpha(Image.composite(motion.getchannel('A'), Image.new('L', (W, H), 0), mask))

    # Restrained lunar reflections: integer cycles over the full loop.
    sea = Image.new('RGBA', (W, H), 0)
    sd = ImageDraw.Draw(sea)
    for j in range(22):
        y = 315+j*8.6
        width = 3+j*1.75
        x = 620 + math.sin(j*2.3+phase*2)*width*.53
        alpha = int((15 + 4*math.sin(j*1.7+phase*3))*(1-j/36))
        sd.line([(x-width*.5, y), (x+width*.5, y)], fill=(166, 191, 184, alpha), width=1)
    sea.putalpha(Image.composite(sea.getchannel('A'), Image.new('L', (W, H), 0), mask))
    im = Image.alpha_composite(im, sea)
    im = Image.alpha_composite(im, motion)

    # Three continuous steam ribbons, not a burst/fade animation.
    steam = Image.new('RGBA', (W, H), 0)
    st = ImageDraw.Draw(steam)
    for k in range(3):
        for j in range(52):
            age = j/52
            x = 520+k*12 + 5*math.sin(age*7 + phase*3 + k*1.7) + 3*age*math.sin(phase*2+k)
            y = 528-age*60
            alpha = int(24*(math.sin(math.pi*age)**1.6))
            st.ellipse((x-1.0, y-1.5, x+1.0, y+1.5), fill=(234, 219, 189, alpha))
    steam = steam.filter(ImageFilter.GaussianBlur(.75))
    return Image.alpha_composite(im, steam).convert('RGB')


def main():
    global HERE, FPS, SECONDS, SEED
    parser = argparse.ArgumentParser()
    parser.add_argument('--poster-only', action='store_true')
    parser.add_argument('--output-dir', type=Path, default=HERE)
    parser.add_argument('--duration', type=int, default=90, help='Whole seconds, at least 30')
    parser.add_argument('--fps', type=int, default=16, help='Frame rate from 12 to 24')
    parser.add_argument('--seed', type=int, default=6042026)
    args = parser.parse_args()
    if args.duration < 30 or not 12 <= args.fps <= 24:
        parser.error('duration must be >=30 seconds and fps between 12 and 24')
    HERE, FPS, SECONDS, SEED = args.output_dir.resolve(), args.fps, args.duration, args.seed
    HERE.mkdir(parents=True, exist_ok=True)
    frame(0).save(HERE/'coastal-room-poster.png')
    if args.poster_only:
        return
    output = HERE/f'coastal-room-loop-{SECONDS}s.mp4'
    cmd = ['ffmpeg', '-hide_banner', '-loglevel', 'error', '-y', '-f', 'rawvideo', '-pixel_format', 'rgb24', '-video_size', f'{W}x{H}', '-framerate', str(FPS), '-i', 'pipe:0', '-an', '-c:v', 'libx264', '-preset', 'fast', '-crf', '20', '-pix_fmt', 'yuv420p', '-r', str(FPS), '-g', str(FPS*4), '-keyint_min', str(FPS*4), '-sc_threshold', '0', '-bf', '2', '-movflags', '+faststart', str(output)]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    try:
        for i in range(FPS*SECONDS):
            p.stdin.write(frame(i).tobytes())
            if i % (FPS*10) == 0:
                print(f'{i//FPS}/{SECONDS} seconds', flush=True)
        p.stdin.close()
        if p.wait() != 0:
            raise RuntimeError('FFmpeg encode failed')
    finally:
        if p.poll() is None:
            p.kill()
    first = np.asarray(frame(0), dtype=np.int16)
    last = np.asarray(frame(FPS*SECONDS-1), dtype=np.int16)
    next_frame = np.asarray(frame(FPS*SECONDS), dtype=np.int16)
    before = np.asarray(frame(FPS*SECONDS-2), dtype=np.int16)
    report = {
        'width': W, 'height': H, 'fps': FPS, 'duration_seconds': SECONDS,
        'frame_count': FPS*SECONDS, 'audio': False, 'codec': 'h264', 'pixel_format': 'yuv420p',
        'seed': SEED,
        'periodicity': f'frame({FPS*SECONDS}) is byte-identical to frame(0); every animated phase is an integer cycle over {SECONDS} seconds',
        'period_frame_equal': bool(np.array_equal(first, next_frame)),
        'loop_join_pixel_mean_absolute_delta': float(np.abs(first-last).mean()),
        'previous_frame_pixel_mean_absolute_delta': float(np.abs(before-last).mean()),
        'content': 'Original procedural illustration: warm quiet room, ocean window at dusk/night, gentle glass rain, lamp, books, mug steam, plant.',
        'rights': 'No third-party photos, music, sampled artwork, fonts, footage, or trademarks.',
        'output_file': output.name,
    }
    (HERE/'visual-verification.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    frame(FPS*SECONDS-1).save(HERE/'loop-last-frame.png')
    frame(FPS*SECONDS).save(HERE/'loop-period-frame.png')
    print(json.dumps(report, ensure_ascii=False), flush=True)


if __name__ == '__main__':
    main()
