//! PipeWire plumbing shared by the video and audio streams.

use std::any::Any;
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Pod, Value, serialize::PodSerializer};

use crate::feed::{Feed, Running};

/// Runs a PipeWire main loop on its own thread until `running` stops.
/// `setup` connects and creates the streams, returning whatever must live as
/// long as the loop. Returns once setup has finished, with its error if any.
pub fn spawn<F>(name: &str, running: &mut Running, setup: F) -> Result<()>
where
    F: FnOnce(&pw::main_loop::MainLoopRc) -> Result<Box<dyn Any>> + Send + 'static,
{
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let t = thread::Builder::new().name(name.into()).spawn(move || {
        pw::init();
        let setup = || -> Result<_> {
            let mainloop = pw::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
            let keep = setup(&mainloop)?;
            Ok((mainloop, keep))
        };
        let (mainloop, keep) = match setup() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let ml = mainloop.clone();
        let _quit = quit_rx.attach(mainloop.loop_(), move |()| ml.quit());
        let _ = ready_tx.send(Ok(()));
        mainloop.run();
        drop(keep);
    })?;
    running.add_thread(t);
    running.on_stop(move || {
        let _ = quit_tx.send(());
    });
    ready_rx
        .recv()
        .map_err(|_| anyhow!("{name} thread exited during setup"))?
}

/// Reports a core error (e.g. the PipeWire daemon going away) as a feed
/// failure. Keep the returned listener alive.
pub fn watch_core(core: &pw::core::CoreRc, feed: &Feed, what: &'static str) -> pw::core::Listener {
    let feed = feed.clone();
    core.add_listener_local()
        .error(move |id, _seq, res, msg| {
            // id 0 is the core itself: the connection is broken.
            if id == pw::core::PW_ID_CORE {
                feed.fail(format!("{what}: PipeWire error {res}: {msg}"));
            }
        })
        .register()
}

/// Reports a stream that errors out or gets disconnected as a feed failure.
pub fn check_state(
    feed: &Feed,
    what: &str,
    old: &pw::stream::StreamState,
    new: &pw::stream::StreamState,
) {
    use pw::stream::StreamState as S;
    log::debug!("{what}: stream {old:?} -> {new:?}");
    match new {
        S::Error(e) => feed.fail(format!("{what}: stream error: {e}")),
        S::Unconnected if !matches!(old, S::Unconnected) => {
            feed.fail(format!("{what}: stream disconnected"))
        }
        _ => {}
    }
}

/// Serializes a format object into a pod for `Stream::connect`.
pub fn pod_bytes(obj: spa::pod::Object) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serialize pod")
        .0
        .into_inner()
}

pub fn as_pod(bytes: &[u8]) -> &Pod {
    Pod::from_bytes(bytes).expect("valid pod")
}
