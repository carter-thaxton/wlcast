fn main() {
    #[cfg(feature = "static-gst")]
    static_gst();
}

/// Links the static gst-full build that PKG_CONFIG_PATH points at, by
/// default target/gst-full (see scripts/build-gst-full.sh and
/// .cargo/static-gst.toml).  The gstreamer -sys crates link the core libraries from
/// the same prefix; this adds the plugins and gst_init_static_plugins().
/// Plugin .pc files live in a subdirectory of libdir (gstreamer-full-1.0.pc
/// requires them), so that goes on the pkg-config path too.
#[cfg(feature = "static-gst")]
fn static_gst() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let hint = "static-gst: gst-full not found; run scripts/build-gst-full.sh, \
                or set PKG_CONFIG_PATH=<gst-full prefix>/lib/pkgconfig";
    let libdir = pkg_config::get_variable("gstreamer-1.0", "libdir")
        .unwrap_or_else(|e| panic!("{hint}: {e}"));
    let path = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
    // SAFETY: build scripts are single-threaded.
    unsafe {
        std::env::set_var(
            "PKG_CONFIG_PATH",
            format!("{path}:{libdir}/gstreamer-1.0/pkgconfig"),
        )
    };
    pkg_config::Config::new()
        .statik(true)
        .probe("gstreamer-full-1.0")
        .unwrap_or_else(|e| panic!("{hint}: {e}"));
}
