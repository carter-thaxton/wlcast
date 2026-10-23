use clap::{Parser, ValueEnum};

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

    /// Publish a generated A/V sync pattern instead of capturing the desktop:
    /// a white flash and a 1 kHz beep at the start of every second.
    #[arg(long)]
    pub test_pattern: bool,
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
    /// (audio delay, video delay) in ms, both >= 0.
    pub fn branch_delays_ms(&self) -> (u64, u64) {
        let d = self.audio_delay_ms;
        if d >= 0 {
            (d as u64, 0)
        } else {
            (0, d.unsigned_abs() as u64)
        }
    }
}
