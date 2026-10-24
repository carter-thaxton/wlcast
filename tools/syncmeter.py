#!/usr/bin/env python3
"""Measure A/V sync of a `wlcast --test-pattern` stream.

Reads the stream back over RTSP, finds each white flash and each beep, and
prints the audio-minus-video offset for every second, both by PTS (what a
player syncing by timestamps sees) and by arrival time at this process.
With --audio-delay-ms N, both should come out near N.

Usage: syncmeter.py [RTSP_URL] [SECONDS]
       (default rtsp://127.0.0.1:8554/screen, 8 s)
Needs: python3-gi, gstreamer1.0-libav (avdec_h264), plugins good/bad.
"""
import struct
import sys
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

FLASH_LUMA = 180     # mean luma above this is a flash
BEEP_LEVEL = 3000    # |sample| above this is a beep
BEEP_GAP = 480       # this many quiet samples (10 ms) end a beep
RATE = 48000


class Meter:
    def __init__(self):
        self.frames = 0
        self.lit = False
        self.loud = False
        self.quiet = 0
        self.flashes = []  # (pts s, arrival s)
        self.beeps = []

    def on_video(self, sink):
        buf = sink.emit("pull-sample").get_buffer()
        ok, m = buf.map(Gst.MapFlags.READ)
        sample = m.data[::997]
        lit = sum(sample) / len(sample) > FLASH_LUMA
        buf.unmap(m)
        self.frames += 1
        if lit and not self.lit:
            self.flashes.append((buf.pts / Gst.SECOND, time.monotonic()))
        self.lit = lit
        return Gst.FlowReturn.OK

    def on_audio(self, sink):
        buf = sink.emit("pull-sample").get_buffer()
        ok, m = buf.map(Gst.MapFlags.READ)
        samples = struct.unpack(f"<{len(m.data) // 2}h", m.data)
        buf.unmap(m)
        now = time.monotonic()
        for i, x in enumerate(samples):
            if abs(x) > BEEP_LEVEL:
                if not self.loud:
                    self.beeps.append((buf.pts / Gst.SECOND + i / RATE, now))
                self.loud, self.quiet = True, 0
            else:
                self.quiet += 1
                if self.quiet > BEEP_GAP:
                    self.loud = False
        return Gst.FlowReturn.OK


def offsets_ms(beeps, flashes):
    """For each beep, its offset from the nearest flash."""
    if not flashes:
        return []
    return [(b - min(flashes, key=lambda f: abs(f - b))) * 1000 for b in beeps]


def main():
    url = sys.argv[1] if len(sys.argv) > 1 else "rtsp://127.0.0.1:8554/screen"
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0

    Gst.init(None)
    pipeline = Gst.parse_launch(
        f"rtspsrc location={url} protocols=tcp latency=0 name=r "
        "r. ! application/x-rtp,media=video ! rtph264depay ! h264parse ! avdec_h264 "
        "! videoconvert ! video/x-raw,format=GRAY8 ! appsink name=v emit-signals=true sync=false "
        "r. ! application/x-rtp,media=audio ! rtpopusdepay ! opusdec ! audioconvert "
        f"! audio/x-raw,format=S16LE,channels=1,rate={RATE} "
        "! appsink name=a emit-signals=true sync=false")
    meter = Meter()
    pipeline.get_by_name("v").connect("new-sample", meter.on_video)
    pipeline.get_by_name("a").connect("new-sample", meter.on_audio)

    loop = GLib.MainLoop()
    failed = []

    def on_error(_bus, msg):
        err, _ = msg.parse_error()
        failed.append(err.message)
        loop.quit()

    bus = pipeline.get_bus()
    bus.add_signal_watch()
    bus.connect("message::error", on_error)
    pipeline.set_state(Gst.State.PLAYING)
    GLib.timeout_add(int(secs * 1000), loop.quit)
    loop.run()
    pipeline.set_state(Gst.State.NULL)
    if failed:
        sys.exit(f"error: {failed[0]}")

    fmt = lambda xs: " ".join(f"{x:+.0f}" for x in xs) or "(none)"  # noqa: E731
    print(f"frames: {meter.frames} in {secs:g} s wall (includes connect time)")
    print(f"flashes: {len(meter.flashes)}, beeps: {len(meter.beeps)}")
    print("audio-minus-video by PTS (ms):    ",
          fmt(offsets_ms([b[0] for b in meter.beeps], [f[0] for f in meter.flashes])))
    print("audio-minus-video by arrival (ms):",
          fmt(offsets_ms([b[1] for b in meter.beeps], [f[1] for f in meter.flashes])))
    if not meter.flashes or not meter.beeps:
        sys.exit(1)


if __name__ == "__main__":
    main()
