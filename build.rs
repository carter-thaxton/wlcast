fn main() {
    // With static-gst, gst-static.toml turns off the -sys crates' own
    // pkg-config lookups and this one probe supplies every link flag.
    #[cfg(feature = "static-gst")]
    pkg_config::Config::new()
        .statik(true)
        .probe("gstreamer-full-1.0")
        .expect("gstreamer-full-1.0 not found; set PKG_CONFIG_PATH to the gst-full build");
}
