//! Screen capture through the xdg-desktop-portal ScreenCast portal, for
//! compositors without a capture protocol we speak (GNOME).
//!
//! The portal is asked to persist the permission; the restore token it
//! returns is saved, so the picker only appears the first time. Tokens are
//! single use: every session returns a new one, which replaces the old.

use std::fs;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
};
use futures_lite::future::block_on;
use log::{info, warn};

use super::{VideoBackend, pwvideo};
use crate::config::Config;
use crate::feed::{Feed, Running};

/// Repeat the last frame after this many frame periods without a new one,
/// so the stream keeps its frame rate while the screen is still.
const KEEPALIVE_FRAMES: f64 = 1.5;

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
        let session = opened.session;
        running.on_stop(move || {
            let _ = block_on(session.close());
        });
        pwvideo::start(&mut running, opened.fd, opened.node, cfg, feed.clone())?;
        running.keepalive(
            &feed,
            Duration::from_secs_f64(KEEPALIVE_FRAMES / cfg.fps as f64),
        );
        Ok(running)
    }
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
