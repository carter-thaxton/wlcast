use std::path::PathBuf;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};

/// Stream the Wayland desktop and system audio to an RTSP server (MediaMTX).
#[derive(Parser, Debug, Clone)]
#[command(version)]
pub struct Config {
    /// RTSP URL to publish to.
    #[arg(
        long,
        env = "WLCAST_RTSP_URL",
        default_value = "rtsp://127.0.0.1:8554/screen"
    )]
    pub rtsp_url: String,

    /// Output width in pixels.
    #[arg(long, env = "WLCAST_WIDTH", default_value_t = 1280)]
    pub width: u32,

    /// Output height in pixels.
    #[arg(long, env = "WLCAST_HEIGHT", default_value_t = 720)]
    pub height: u32,

    /// Output frame rate.
    #[arg(long, env = "WLCAST_FPS", default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=120))]
    pub fps: u32,

    /// Video bitrate in kbit/s (CBR modes and x264).
    #[arg(long, env = "WLCAST_BITRATE", default_value_t = 3000)]
    pub bitrate: u32,

    /// I-frame QP for CQP modes; P-frames use QP+2.
    #[arg(long, env = "WLCAST_QP", default_value_t = 26, value_parser = clap::value_parser!(u32).range(0..=49))]
    pub qp: u32,

    /// Opus bitrate in bit/s.
    #[arg(long, env = "WLCAST_AUDIO_BITRATE", default_value_t = 128_000)]
    pub audio_bitrate: u32,

    /// Positive delays audio, negative delays video (ms).
    #[arg(
        long,
        env = "WLCAST_AUDIO_DELAY_MS",
        default_value_t = 0,
        allow_negative_numbers = true
    )]
    pub audio_delay_ms: i32,

    /// Encoder mode; `auto` probes them in order and uses the first that works.
    #[arg(long, env = "WLCAST_ENCODER", value_enum, default_value_t = EncoderChoice::Auto)]
    pub encoder: EncoderChoice,

    /// Screen capture method; `auto` picks the best the compositor offers.
    #[arg(long, env = "WLCAST_CAPTURE", value_enum, default_value_t = CaptureChoice::Auto)]
    pub capture: CaptureChoice,

    /// Output (monitor) to capture, by name, e.g. HDMI-A-1 [default: the
    /// first]. Not used with the portal, which asks the user.
    #[arg(long, env = "WLCAST_OUTPUT")]
    pub output: Option<String>,

    /// Draw the mouse cursor into the video.
    #[arg(long, env = "WLCAST_CURSOR", default_value_t = true, action = clap::ArgAction::Set)]
    pub cursor: bool,

    /// Capture system audio; false sends silence instead.
    #[arg(long, env = "WLCAST_AUDIO", default_value_t = true, action = clap::ArgAction::Set)]
    pub audio: bool,

    /// Capture this sink's monitor (its node.name) instead of the default sink's.
    #[arg(long, env = "WLCAST_AUDIO_TARGET")]
    pub audio_target: Option<String>,

    /// Where to keep the ScreenCast portal's restore token
    /// [default: $XDG_STATE_HOME/wlcast/portal-restore-token].
    #[arg(long, env = "WLCAST_RESTORE_TOKEN_FILE")]
    pub restore_token_file: Option<PathBuf>,

    /// Publish a generated A/V sync pattern instead of capturing the desktop:
    /// a white flash and a 1 kHz beep at the start of every second.
    #[arg(long)]
    pub test_pattern: bool,

    /// Whether `--bitrate` was given (on the command line or in the
    /// environment), not just defaulted.
    #[arg(skip)]
    pub bitrate_set: bool,

    /// Whether `--qp` was given.
    #[arg(skip)]
    pub qp_set: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureChoice {
    Auto,
    /// ext-image-copy-capture-v1 (wlroots 0.19+)
    Ext,
    /// wlr-screencopy-unstable-v1, version 3
    Wlr,
    /// xdg-desktop-portal ScreenCast (GNOME, KDE, ...)
    Portal,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    Auto,
    VaLpCbr,
    VaLpCqp,
    VaCbr,
    VaCqp,
    X264,
}

impl Config {
    /// Parses the command line and environment, noting which rate options
    /// were set explicitly.
    pub fn load() -> Self {
        let matches = Self::command().get_matches();
        let mut cfg = Self::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
        let given = |id| {
            matches!(
                matches.value_source(id),
                Some(ValueSource::CommandLine | ValueSource::EnvVariable)
            )
        };
        cfg.bitrate_set = given("bitrate");
        cfg.qp_set = given("qp");
        cfg
    }

    /// (audio delay, video delay) in ms, both >= 0.
    pub fn branch_delays_ms(&self) -> (u64, u64) {
        let d = self.audio_delay_ms;
        if d >= 0 {
            (d as u64, 0)
        } else {
            (0, d.unsigned_abs() as u64)
        }
    }

    pub fn restore_token_path(&self) -> PathBuf {
        if let Some(p) = &self.restore_token_file {
            return p.clone();
        }
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."));
        state.join("wlcast/portal-restore-token")
    }
}
