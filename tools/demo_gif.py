#!/usr/bin/env python3
"""Render a real serial transcript as an animated demo GIF.

The terminal content is authentic (captured from QEMU); what this tool adds is
presentation: a human-like mouse move + click before typing, per-character
typing with jitter and pauses, and a fast smooth camera zoom to the line being
written.

Usage:
    python3 tools/demo_gif.py --log /tmp/pkg55.log --out docs/demo-pkg.gif \
        --title "pagh-riscv 0.15 — pkg + applets"
"""
from __future__ import annotations

import argparse
import random
import re
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

W, H = 960, 540               # output GIF size
SUP = 2                       # supersampling factor for the camera crop
FONT_PATH = "/usr/share/fonts/google-noto/NotoSansMono-Regular.ttf"
FONT_SIZE = 17
LINE_H = 25
MARGIN_X = 26
MARGIN_TOP = 54
PROMPT = b"pagh:rv64>"

BG = (14, 18, 24)
PANEL = (22, 28, 36)
FG = (238, 238, 238)
DIM = (150, 150, 150)
GREEN = (255, 255, 255)
CYAN = (235, 235, 235)
MAGENTA = (210, 210, 210)
CURSOR = (255, 255, 255)


def load_transcript(path: Path):
    raw = path.read_bytes().replace(b"\r", b"\n")
    raw = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", raw)
    text = raw.decode("utf-8", "replace")
    lines = []
    started = False
    for line in text.splitlines():
        line = line.rstrip()
        if PROMPT.decode() in line:
            started = True
            cmd = line.split(PROMPT.decode(), 1)[1].strip()
            lines.append(("input", cmd))
        elif started and line and not line.startswith(("[debug]", "[dbg]", "[EXC]", "[WARN]")):
            lines.append(("output", line))
    return lines


def font():
    return ImageFont.truetype(FONT_PATH, FONT_SIZE)


def ease(t: float) -> float:
    return 1 - (1 - t) ** 3


class Camera:
    def __init__(self):
        self.z = 1.0
        self.cx = W / 2
        self.cy = H / 2
        self.tz = 1.0
        self.tcx = W / 2
        self.tcy = H / 2

    def target(self, line_y: float, zoom: float):
        self.tz = zoom
        # Keep the left margin visible while zooming: put the viewport's left
        # edge a little left of the terminal margin, so text never gets clipped.
        self.tcx = 12.0 + (W / zoom) / 2
        self.tcy = max((H / zoom) / 2, min(H - (H / zoom) / 2, line_y + 90.0))

    def step(self, n=1, total=8):
        k = ease(n / total)
        self.z += (self.tz - self.z) * k
        self.cx += (self.tcx - self.cx) * k
        self.cy += (self.tcy - self.cy) * k

    @property
    def settled(self):
        return abs(self.z - self.tz) < 0.01 and abs(self.cy - self.tcy) < 1.0


class Demo:
    def __init__(self, events, title):
        self.events = events
        self.title = title
        self.lines = []          # (text, kind)
        self.typing = ""
        self.prompt = "pagh:rv64> "
        self.cam = Camera()
        self.frames = []
        self.durations = []
        self.rng = random.Random(7)
        self.mouse = (-80, -60)
        self.mouse_target = (MARGIN_X + 120, MARGIN_TOP + 12)
        self.click = 0

    # ---- canvas -------------------------------------------------------
    def base(self):
        img = Image.new("RGB", (W, H), BG)
        d = ImageDraw.Draw(img)
        d.rectangle([0, 0, W, 34], fill=(31, 38, 48))
        for i, c in enumerate(((255, 95, 86), (255, 189, 46), (39, 201, 63))):
            d.ellipse([12 + i * 20, 11, 24 + i * 20, 23], fill=c)
        d.text((88, 9), self.title, font=ImageFont.truetype(FONT_PATH, 14), fill=DIM)
        d.rectangle([0, 34, W, H], fill=BG)
        return img

    def line_y(self, idx):
        return MARGIN_TOP + idx * LINE_H

    def render(self):
        """Draw the whole terminal at SUP scale, then crop the camera."""
        img = self.base()
        d = ImageDraw.Draw(img)
        f = font()
        rows = self.lines[:]
        if self.typing:
            rows.append((self.prompt + self.typing, "input"))
        for i, (text, kind) in enumerate(rows):
            y = self.line_y(i)
            if y < 0 or y > H + LINE_H:
                continue
            color = (255, 255, 255) if kind == "input" else (215, 215, 215)
            if text.startswith("pagh:rv64>"):
                color = (255, 255, 255)
            d.text((MARGIN_X, y), text, font=f, fill=color)
        # camera crop
        big = img.resize((W * SUP, H * SUP), Image.LANCZOS)
        vw = W / self.cam.z
        vh = H / self.cam.z
        cx = min(max(self.cam.cx, vw / 2), W - vw / 2)
        cy = min(max(self.cam.cy, vh / 2), H - vh / 2)
        box = (int((cx - vw / 2) * SUP), int((cy - vh / 2) * SUP),
               int((cx + vw / 2) * SUP), int((cy + vh / 2) * SUP))
        crop = big.crop(box).resize((W, H), Image.LANCZOS)
        # mouse cursor + click ripple in screen space. While typing, the
        # cursor sits at the end of the text being written, transformed through
        # the current camera, so it stays on the zoomed line.
        d2 = ImageDraw.Draw(crop)
        if self.typing:
            vw = W / self.cam.z
            vh = H / self.cam.z
            cx = min(max(self.cam.cx, vw / 2), W - vw / 2)
            cy = min(max(self.cam.cy, vh / 2), H - vh / 2)
            tx = MARGIN_X + 9 * (len(self.prompt) + len(self.typing))
            ty = self.line_y(len(self.lines)) + 6
            self.mouse = ((tx - (cx - vw / 2)) * self.cam.z,
                          (ty - (cy - vh / 2)) * self.cam.z)
        mx, my = self.mouse
        pts = [(mx, my), (mx + 1, my + 17), (mx + 5, my + 13), (mx + 9, my + 18),
               (mx + 12, my + 16), (mx + 8, my + 11), (mx + 13, my + 10)]
        d2.polygon(pts, fill=CURSOR, outline=(0, 0, 0))
        if self.click > 0:
            r = 6 + (10 - self.click) * 3
            d2.ellipse([mx - r, my - r, mx + r, my + r], outline=(220, 220, 220), width=2)
        return crop

    def capture(self, ms=45):
        self.frames.append(self.render())
        self.durations.append(ms)

    def zoom_for(self, idx):
        self.cam.target(self.line_y(idx), 1.22)

    # ---- animation steps ---------------------------------------------
    def mouse_to(self, x, y, steps=14, ms=30):
        sx, sy = self.mouse
        for i in range(1, steps + 1):
            t = ease(i / steps)
            self.mouse = (sx + (x - sx) * t, sy + (y - sy) * t)
            self.capture(ms)
        self.click = 10
        for _ in range(10):
            self.capture(22)
            self.click -= 1

    def type_command(self, cmd):
        idx = len(self.lines)
        self.typing = ""
        self.zoom_for(idx)
        for _ in range(7):
            self.cam.step(total=7)
            self.capture(30)
        i = 0
        while i < len(cmd):
            self.typing += cmd[i]
            i += 1
            self.capture(self.rng.randint(38, 72))
            if cmd[i - 1] in " /-:":
                self.capture(self.rng.randint(70, 120))
            if i % self.rng.randint(9, 16) == 0:
                self.capture(self.rng.randint(140, 260))
            if i % 5 == 0 and not self.cam.settled:
                self.cam.step(total=7)
        self.lines.append((self.prompt + cmd, "input"))
        self.typing = ""
        self.capture(160)
        self.zoom_for(len(self.lines) - 1)

    def output(self, lines):
        for line in lines:
            self.lines.append((line, "output"))
            idx = len(self.lines) - 1
            if idx % 3 == 0:
                self.zoom_for(idx)
                for _ in range(5):
                    self.cam.step(total=5)
                    self.capture(24)
            self.capture(55)

    def run(self):
        # Poster: first frames show the complete transcript, so a still viewer
        # (PDF/README thumbnail) sees the result; then the terminal clears and
        # the live typing/camera animation starts.
        preview = []
        for kind, text in self.events:
            if kind == "input":
                preview.append((self.prompt + text, "input"))
            else:
                preview.append((text, "output"))
        self.lines = preview
        for _ in range(10):
            self.capture(90)
        self.lines = []
        for _ in range(4):
            self.capture(50)
        first = True
        pending = []
        for kind, text in self.events:
            if kind == "input":
                if pending:
                    self.output(pending)
                    pending = []
                if first:
                    self.mouse_to(MARGIN_X + 120, MARGIN_TOP + 10)
                    first = False
                self.type_command(text)
            elif kind == "output":
                pending.append(text)
        if pending:
            self.output(pending)
        # final hold
        for _ in range(16):
            self.capture(60)
        return self.frames, self.durations


def main():
    global W, H, FONT_SIZE
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--title", default="pagh-riscv — live QEMU transcript")
    ap.add_argument("--max-events", type=int, default=60)
    ap.add_argument("--width", type=int, default=W)
    ap.add_argument("--height", type=int, default=H)
    ap.add_argument("--font-size", type=int, default=FONT_SIZE)
    a = ap.parse_args()
    W, H, FONT_SIZE = a.width, a.height, a.font_size
    events = load_transcript(a.log)[: a.max_events]
    # keep complete command + its output, drop the boot tail noise
    demo = Demo(events, a.title)
    frames, durations = demo.run()
    a.out.parent.mkdir(parents=True, exist_ok=True)
    frames[0].save(a.out, save_all=True, append_images=frames[1:],
                   duration=durations, loop=0, optimize=True)
    print(f"wrote {a.out} ({a.out.stat().st_size / 1e6:.2f} MB, "
          f"{len(frames)} frames, {sum(durations)/1000:.1f}s)")


if __name__ == "__main__":
    main()
