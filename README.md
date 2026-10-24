# wlcast

wlcast streams a live Wayland desktop, with video and system audio, to web
browsers.  It puts readable text, A/V sync, and low latency ahead of picture
quality.  It's one-way: viewers watch and listen, with no input handling.

It's intended for small Wayland environments, such as appliances, kiosks,
and embedded or headless devices running a lightweight compositor.  That's
why it can be built as a single binary with GStreamer linked in (see
[static-gst](#static-gst-gstreamer-built-in)).  It also runs on desktops
such as GNOME.

```
screen capture ─► H.264 (VA-API or x264) ─┐
                                          ├─► RTSP ─► MediaMTX ─► WebRTC ─► browsers
system audio   ─► Opus ───────────────────┘
```

wlcast captures and encodes, then publishes over RTSP to
[MediaMTX](https://github.com/bluenviron/mediamtx), a separate program.
MediaMTX serves the viewer page and fans the stream out to browsers over
WebRTC.

- **Screen capture:** wlcast picks the best method the compositor offers:
  1. `ext-image-copy-capture-v1` (labwc, sway, and other wlroots 0.19+
     compositors);
  2. `wlr-screencopy-unstable-v1` version 3 (older wlroots compositors);
  3. the xdg-desktop-portal ScreenCast portal (GNOME, KDE).  On the first run
     the desktop asks which screen to share.  wlcast saves the portal's restore
     token, so later runs start without asking.

  With the first two, wlcast captures an output directly, with no portal or
  other services needed.  If the output goes away (unplugged, or replaced by a
  compositor's fallback output), wlcast picks an output again.
- **Audio:** the default PipeWire sink's monitor, so viewers hear whatever the
  machine is playing.  Audio never holds up video.  If PipeWire isn't running,
  restarts, or has no sink to monitor, wlcast sends silence and keeps retrying
  PipeWire in the background.
- **Encoding:** at startup wlcast probes for a working H.264 encoder: VA-API
  low-power, then VA-API, each with CBR and then CQP, then x264.  It logs the
  one it picks.  On an Intel NUC7 with Debian's driver it picks VA-API
  low-power CQP, which uses about 40% less CPU than x264.
- **Recovery:** if MediaMTX goes away, wlcast reconnects.  If capture fails (for
  example PipeWire restarts), it restarts the capture.  If the user stops
  sharing from the desktop, wlcast exits.

## Building

wlcast needs Rust 1.88 or newer (install with [rustup](https://rustup.rs)) and
a Linux system with PipeWire.  Debian 13 is the tested platform; the package
names below are Debian's.

There are two build modes.

### Default: GStreamer from the system

This is a plain cargo build, dynamically linked against the distribution's
GStreamer.  Use it for development.

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

The binary is `target/release/wlcast`.  At startup it checks that every
GStreamer element it needs is installed and names any that are missing.

### static-gst: GStreamer built in

For deployment on small systems that shouldn't need a full GStreamer
installation.  GStreamer 1.26, glib, x264, Opus, and orc are linked statically
from a custom "gst-full" build that contains only the plugins wlcast uses.  The
binary then needs only these on the target machine: glibc, libpipewire-0.3,
libva, and libva-drm (plus libdrm and libgcc_s, which those and every Debian
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

From the crate root:

1. Build gst-full once:

   ```
   scripts/build-gst-full.sh
   ```

   The script clones GStreamer 1.26.11, downloads the meson subprojects (glib,
   orc, opus, x264), and builds and installs everything under `target/gst-full`.
   That takes a while.  Running it again only rebuilds what changed.

2. Build wlcast against it:

   ```
   cargo static-gst
   ```

The binary is `target/static/release/wlcast`.  Both commands must run in the
crate root.

The gst-full install also has `gst-inspect-1.0` and `gst-launch-1.0` in
`target/gst-full/prefix/bin`, built against the same plugins.  They're handy for
checking what the static build contains (on a VA-API machine,
`gst-inspect-1.0 va` lists the encoders the driver supports).

Cleaning:

- `cargo static-gst-clean` removes the static build (`target/static`) and keeps
  gst-full.  Plain `cargo clean`, with or without `--release` or `-p`, works on
  the default target directory and doesn't touch the static build.
- After gst-full changes, the whole static build has to be redone, not just
  wlcast, because the gstreamer `-sys` crates bundle copies of gst-full's
  static libraries.  `scripts/build-gst-full.sh` does this for you: when its
  run changes the installed gst-full, it removes `target/static`.  If you change
  gst-full some other way, run `cargo static-gst-clean`.
- Plain `cargo clean` deletes `target/gst-full` along with everything else, so
  the next static build needs gst-full rebuilt first.

To keep gst-full somewhere else, pass a
directory to the script (`scripts/build-gst-full.sh ~/gst-full`) and point the
build at it with
`PKG_CONFIG_PATH=~/gst-full/prefix/lib/pkgconfig cargo static-gst`.  An existing
`PKG_CONFIG_PATH` always overrides the `target/gst-full` default, so unset it if
your shell sets it for something else.

`cargo static-gst` is a cargo alias (in `.cargo/config.toml`) for
`cargo build --release --features static-gst --config .cargo/static-gst.toml` with a
separate target directory.  The feature makes `build.rs` link gst-full and
register its plugins.  `.cargo/static-gst.toml` points pkg-config at
`target/gst-full` and makes the gstreamer `-sys` crates link gst-full's
libraries statically.  See the comments in those files and in the script.

### Release packages

`scripts/package.sh` builds a release tarball for the machine's architecture
(`target/dist/wlcast_vVERSION_linux_ARCH.tar.gz`, with a `.sha256` file).  It
runs `cargo static-gst`, checks the binary (linked libraries, the glibc
version it needs, `--version`, and a test-pattern run), and adds
`THIRD_PARTY_LICENSES.txt`.  That file lists the bundled C libraries with links
to their source, and includes the license texts of those libraries and of the
Rust crates.  The notices need
[cargo-about](https://github.com/EmbarkStudios/cargo-about):
`cargo install --locked --features cli cargo-about`.

Release builds use a fixed Rust version, set by `RELEASE_RUST` in the
script, so that both architectures and later rebuilds use the same compiler.
It can be your default `rustc`, or installed alongside it with rustup
(`rustup toolchain install <version>`).
Other builds use whatever Rust version you have (1.88 or newer).

Release binaries are built natively on each architecture, not cross-compiled.

### Runtime requirements

- A Wayland compositor offering `ext-image-copy-capture-v1` or
  `wlr-screencopy-unstable-v1` v3, or else xdg-desktop-portal with a
  ScreenCast backend such as xdg-desktop-portal-gnome.
- PipeWire, for audio (and for video with the portal).  Without it, the stream
  carries silence.  libpipewire must still be installed, because the binary
  links against it.
- For hardware encoding, a VA-API driver, e.g. `intel-media-va-driver` on Intel.
  Without one, wlcast falls back to x264.
  - Debian's `intel-media-va-driver` has no video-processing entrypoint, so the
    scaling and conversion to NV12 ahead of the encoder happen on the CPU.  With
    a driver that has one, wlcast does them on the GPU.
  - Without HuC firmware loaded (the default on older Intel GPUs), the encoder
    supports only constant QP, not CBR.  Then `--qp` sets the quality, and
    `--bitrate` is ignored.

  wlcast logs the bitrate or QP it applies along with the encoder.  It warns
  if you set only `--bitrate` and a constant-QP mode is picked, or only `--qp`
  and a bitrate mode is picked.  Set both to cover either case without a
  warning.

## MediaMTX

wlcast needs a MediaMTX server to publish to.  Download a release for your
architecture (`linux_amd64` or `linux_arm64`) from the
[MediaMTX releases page](https://github.com/bluenviron/mediamtx/releases);
it's a single binary.  wlcast is tested with v1.21.2.

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

For larger audiences, MediaMTX can run on a server instead.  The device then
publishes to it (for example through a reverse tunnel); point `--rtsp-url` at
it.  wlcast itself is the same either way.

## Running

Start MediaMTX first, then run wlcast inside the desktop session (as the same
user, with `WAYLAND_DISPLAY` set, and the session D-Bus for the portal):

```
wlcast
```

`wlcast --test-pattern` publishes a generated pattern instead of capturing the
desktop: a white flash and a 1 kHz beep at the start of every second.  It's
useful for checking MediaMTX, the network path, and A/V sync without a portal.
`tools/syncmeter.py` reads that stream back and reports the audio-to-video
offset (it needs `python3-gi` and `gstreamer1.0-libav`).

To try the native capture backends on a desktop that lacks them (GNOME, for
example), `tools/labwc-headless.sh` starts a headless labwc and prints its
`WAYLAND_DISPLAY`.  Run wlcast with that `WAYLAND_DISPLAY`, plus a client that
draws something, such as `weston-simple-shm`.  See the script for details.

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
| `--capture` | `auto` | `auto`, `ext`, `wlr`, or `portal` (see above) |
| `--output` | first output | Output to capture, by name (e.g. `HDMI-A-1`); not used with the portal |
| `--cursor` | `true` | Draw the mouse cursor |
| `--audio` | `true` | Capture system audio; `false` sends silence |
| `--audio-target` | default sink | Capture this sink's monitor (its `node.name`) |
| `--restore-token-file` | `$XDG_STATE_HOME/wlcast/portal-restore-token` | Portal restore token |

Logging goes to stderr.  Use `RUST_LOG=debug` for more detail and `GST_DEBUG`
for GStreamer's own logs.

To get the screen picker again, delete the restore token file.

## License

GPL-3.0-or-later.  See [LICENSE](LICENSE).
