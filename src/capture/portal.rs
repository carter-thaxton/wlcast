//! Screen capture through the xdg-desktop-portal ScreenCast portal, for
//! compositors without a capture protocol we speak (GNOME).
//!
//! The portal is asked to persist the permission; the restore token it
//! returns is saved, so the picker only appears the first time.  Tokens are
//! single use: every session returns a new one, which replaces the old.

use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
};
use async_io::Timer;
use futures_lite::future::{FutureExt, block_on};
use futures_lite::stream::StreamExt;
use log::{info, warn};

use super::{VideoBackend, keepalive_interval, pwvideo};
use crate::config::Config;
use crate::feed::{Feed, Running};

pub struct Portal;

impl VideoBackend for Portal {
    fn name(&self) -> &'static str {
        "portal"
    }

    fn start(&self, cfg: &Config, feed: Feed) -> Result<Running> {
        let token_path = cfg.restore_token_path();
        let token = load_token(&token_path);
        if token.is_none() {
            info!("portal: no restore token yet; the desktop may ask which screen to share");
        }
        let opened = block_on(open(cfg.cursor, token.as_deref()))?;
        match &opened.restore_token {
            Some(t) => save_token(&token_path, t),
            None => warn!("portal: no restore token returned; the picker will appear next time"),
        }
        info!("portal: streaming PipeWire node {}", opened.node);

        let mut running = Running::new();
        // Stop actions run in reverse order: the PipeWire stream stops
        // before the session closes.
        let session = Arc::new(opened.session);
        let closer = session.clone();
        running.on_stop(move || {
            let _ = block_on(closer.close());
        });
        watch_closed(&mut running, session, feed.clone());
        pwvideo::start(&mut running, opened.fd, opened.node, cfg, feed.clone())?;
        running.keepalive(&feed, keepalive_interval(cfg.fps));
        Ok(running)
    }
}

/// Treats the portal closing the session (the user pressed "Stop sharing")
/// as a request to stop, unless we're the ones stopping.
fn watch_closed(running: &mut Running, session: Arc<Session<Screencast>>, feed: Feed) {
    let stop = running.stop_flag();
    let t = thread::Builder::new()
        .name("portal-closed".into())
        .spawn(move || {
            block_on(async {
                let mut closed = match session.receive_closed().await {
                    Ok(s) => Box::pin(s),
                    Err(e) => {
                        warn!("portal: can't watch for the session closing: {e}");
                        return;
                    }
                };
                loop {
                    let next = async { Some(closed.next().await) };
                    let tick = async {
                        Timer::after(Duration::from_millis(100)).await;
                        None
                    };
                    match next.or(tick).await {
                        Some(Some(_)) if !stop.load(Ordering::SeqCst) => {
                            feed.request_stop("screen sharing was stopped from the desktop");
                            return;
                        }
                        Some(_) => return,
                        None if stop.load(Ordering::SeqCst) => return,
                        None => {}
                    }
                }
            })
        })
        .expect("spawn portal watcher thread");
    running.add_thread(t);
}

struct Opened {
    session: Session<Screencast>,
    node: u32,
    fd: OwnedFd,
    restore_token: Option<String>,
}

async fn open(cursor: bool, token: Option<&str>) -> Result<Opened> {
    let proxy = Screencast::new()
        .await
        .context("ScreenCast portal unavailable")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("CreateSession")?;
    let cursor_mode = if cursor {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    let options = SelectSourcesOptions::default()
        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
        .set_multiple(false)
        .set_cursor_mode(cursor_mode)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(token);
    proxy
        .select_sources(&session, options)
        .await
        .and_then(|r| r.response())
        .context("SelectSources")?;
    let streams = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .and_then(|r| r.response())
        .context("Start (was the share dialog cancelled?)")?;
    let node = streams
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no streams"))?
        .pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("OpenPipeWireRemote")?;
    Ok(Opened {
        session,
        node,
        fd,
        restore_token: streams.restore_token().map(str::to_owned),
    })
}

fn load_token(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

fn save_token(path: &Path, token: &str) {
    let result = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(path, token));
    if let Err(e) = result {
        warn!(
            "portal: can't save restore token to {}: {e}",
            path.display()
        );
    }
}
