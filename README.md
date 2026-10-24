# wlcast

wlcast streams a live Wayland desktop, with video and system audio, to web
browsers. It's built for remote viewing and demos of a karaoke/media player, so
it puts readable text, A/V sync and low latency ahead of picture quality. It's
one-way: viewers watch and listen, with no input handling.

```
screen capture ─► H.264 (VA-API or x264) ─┐
                                          ├─► RTSP ─► MediaMTX ─► WebRTC ─► browsers
system audio   ─► Opus ───────────────────┘
```

wlcast captures and encodes, then publishes over RTSP to
[MediaMTX](https://github.com/bluenviron/mediamtx), a separate program. MediaMTX
serves the viewer page and fans the stream out to browsers over WebRTC.

- **Screen capture:** currently via the xdg-desktop-portal ScreenCast portal
  (GNOME and others). On the first run the desktop asks which screen to share.
  wlcast saves the portal's restore token, so later runs start without asking.
  Direct wlroots capture (`ext-image-copy-capture-v1`, `wlr-screencopy`) is
  planned.
- **Audio:** the default PipeWire sink's monitor, so viewers hear whatever the
  machine is playing.
- **Encoding:** at startup wlcast probes for a working H.264 encoder: VA-API
  low-power, then VA-API, each with CBR and then CQP, then x264. It logs the one
  it picks.
- **Recovery:** if MediaMTX goes away, wlcast reconnects. If capture fails (for
  example PipeWire restarts), it restarts the capture. If the user stops
  sharing from the desktop, wlcast exits.

## Building

wlcast needs Rust 1.88 or newer (install with [rustup](https://rustup.rs)) and
a Linux system with PipeWire. Debian 13 is the tested platform; the package
names below are Debian's.

There are two build modes.

### Default: GStreamer from the system

This is a plain cargo build, dynamically linked against the distribution's
GStreamer. Use it for development.

Build dependencies:

```
sudo apt install --no-install-recommends \
  build-essential pkg-config libclang-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libpipewire-0.3-dev
```

GStreamer plugins needed at runtime:

```
sudo apt install --no-install-recommends \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-rtsp
```

Build:

```
cargo build --release
```

The binary is `target/release/wlcast`. At startup it checks that every
GStreamer element it needs is installed and names any that are missing.

### static-gst: GStreamer built in

For deployment. GStreamer 1.26, glib, x264, Opus and orc are linked statically
from a custom "gst-full" build that contains only the plugins wlcast uses. The
binary then needs only these on the target machine: glibc, libpipewire-0.3,
libva and libva-drm (plus libdrm and libgcc_s, which those and every Debian
system already provide).

Build dependencies:

```
sudo apt install --no-install-recommends \
  build-essential pkg-config libclang-dev libpipewire-0.3-dev \
  git meson ninja-build flex bison libva-dev libdrm-dev
# x86_64 only, for x264's assembly:
sudo apt install --no-install-recommends nasm
```

The system GStreamer packages aren't needed for this mode.

1. Build gst-full once. The script clones GStreamer 1.26.11 into a work
   directory of your choice, downloads the meson subprojects (glib, orc, opus,
   x264), and installs everything into `<workdir>/prefix`. That takes a while.

   ```
   scripts/build-gst-full.sh ~/gst-full
   ```

2. Build wlcast against it:

   ```
   PKG_CONFIG_PATH=~/gst-full/prefix/lib/pkgconfig cargo static-gst
   ```

The binary is `target/static/release/wlcast`. `cargo static-gst` is a cargo
alias (in `.cargo/config.toml`) for
`cargo build --release --features static-gst --config static-gst.toml` with a
separate target directory. The feature makes `build.rs` link gst-full and
register its plugins. `static-gst.toml` makes the gstreamer `-sys` crates link
gst-full's libraries statically. See the comments in those files and in the
script.

### Runtime requirements

- A Wayland session with xdg-desktop-portal and a ScreenCast backend, such as
  xdg-desktop-portal-gnome or xdg-desktop-portal-wlr.
- PipeWire.
- For hardware encoding, a VA-API driver, e.g. `intel-media-va-driver` on Intel.
  Without one, wlcast falls back to x264.

## MediaMTX

wlcast needs a MediaMTX server to publish to. Download a release for your
architecture (`linux_amd64` or `linux_arm64`) from the
[MediaMTX releases page](https://github.com/bluenviron/mediamtx/releases);
it's a single binary. wlcast is tested with v1.21.2.

Start it with the config in this repository:

```
./mediamtx /path/to/wlcast/mediamtx.yml
```

This config:

- accepts RTSP on `127.0.0.1:8554`, which is wlcast's default
  `--rtsp-url rtsp://127.0.0.1:8554/screen`;
- serves the viewer page at `http://<host>:8889/screen`;
- uses WebRTC ICE on UDP port 8189 (LAN) and TCP port 8190, which works
  through an SSH tunnel.

It has **no authentication**: anyone who can reach port 8889 can watch.
Restrict access with a firewall or an SSH tunnel, for example:

```
ssh -L 8889:127.0.0.1:8889 -L 8190:127.0.0.1:8190 user@device
# then open http://localhost:8889/screen
```

For larger audiences, MediaMTX can run on a server instead. The device then
publishes to it (for example through a reverse tunnel); point `--rtsp-url` at
it. wlcast itself is the same either way.

## Running

Start MediaMTX first, then run wlcast inside the desktop session (as the same
user, with `WAYLAND_DISPLAY` and the session D-Bus available):

```
wlcast
```

`wlcast --test-pattern` publishes a generated pattern instead of capturing the
desktop: a white flash and a 1 kHz beep at the start of every second. It's
useful for checking MediaMTX, the network path and A/V sync without a portal.
`tools/syncmeter.py` reads that stream back and reports the audio-to-video
offset (it needs `python3-gi` and `gstreamer1.0-libav`).

Options (each can also be set through the environment variable shown in
`wlcast --help`, e.g. `WLCAST_FPS`):

| Option | Default | |
|---|---|---|
| `--rtsp-url` | `rtsp://127.0.0.1:8554/screen` | Where to publish |
| `--width`, `--height` | 1280×720 | Output size; the screen is scaled to fit |
| `--fps` | 30 | Output frame rate |
| `--bitrate` | 3000 | Video kbit/s (CBR modes and x264) |
| `--qp` | 26 | I-frame QP for CQP modes (P-frames use QP+2) |
| `--audio-bitrate` | 128000 | Opus bit/s |
| `--audio-delay-ms` | 0 | Positive delays audio, negative delays video |
| `--encoder` | `auto` | `auto`, `va-lp-cbr`, `va-lp-cqp`, `va-cbr`, `va-cqp`, `x264` |
| `--cursor` | `true` | Draw the mouse cursor |
| `--audio-target` | default sink | Capture this sink's monitor (its `node.name`) |
| `--restore-token-file` | `$XDG_STATE_HOME/wlcast/portal-restore-token` | Portal restore token |

Logging goes to stderr. Use `RUST_LOG=debug` for more detail and `GST_DEBUG`
for GStreamer's own logs.

To get the screen picker again, delete the restore token file.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
