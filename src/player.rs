//! Lecture via le wrapper mpv de l'utilisateur (HDR : gpu-next + Vulkan Wayland sous KWin).
//! La position est suivie par l'IPC JSON de mpv et remontée à Jellyfin.

use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::config::PlayerConfig;
use crate::jellyfin::{Jellyfin, PlayReport, ReportStage};

#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub url: String,
    pub title: String,
    pub start_secs: f64,
    pub http_headers: Vec<String>,
    /// Remontée de progression à Jellyfin (élément de la médiathèque ou Gelato).
    pub report: Option<PlayReport>,
    /// Flux en direct (IPTV) : reconnexion automatique, pas de position à suivre.
    pub live: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlayOutcome {
    pub position: f64,
    pub duration: f64,
}

impl PlayOutcome {
    pub fn finished(&self) -> bool {
        self.duration > 0.0 && (self.position / self.duration > 0.88 || self.duration - self.position < 150.0)
    }
}

pub async fn play(cfg: &PlayerConfig, req: PlayRequest, jf: Option<Jellyfin>) -> Result<PlayOutcome> {
    let sock = cfg.ipc_socket.clone();
    let _ = std::fs::remove_file(&sock);

    let mut cmd = Command::new(&cfg.command);
    cmd.arg(&req.url)
        .arg(format!("--force-media-title={}", req.title))
        .arg(format!("--input-ipc-server={sock}"))
        .args(&cfg.extra_args)
        .kill_on_drop(true);
    if req.start_secs > 5.0 {
        cmd.arg(format!("--start=+{:.0}", req.start_secs));
    }
    let hwdec = if cfg.hwdec == "auto" { crate::iptv::hwdec_for_gpu().to_string() } else { cfg.hwdec.clone() };
    cmd.arg(format!("--hwdec={hwdec}"));
    cmd.arg("--cursor-autohide=always");
    if let Some(osc) = overlay_script().filter(|_| cfg.overlay) {
        cmd.arg(format!("--script={}", osc.display())).arg("--osc=no").arg("--osd-bar=no");
    }
    if req.live {
        cmd.arg("--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5")
            .arg("--cache=yes")
            .arg("--demuxer-readahead-secs=10");
    }
    if !req.http_headers.is_empty() {
        cmd.arg(format!("--http-header-fields={}", req.http_headers.join(",")));
    }
    log::info!("lecture : {} ({})", req.title, req.url.split('?').next().unwrap_or_default());
    let mut child = cmd.spawn()?;

    let mut out = PlayOutcome { position: req.start_secs, duration: 0.0 };
    let mut paused = false;
    let mut started = false;
    let mut conn: Option<(tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>, tokio::net::unix::OwnedWriteHalf)> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut report_every = 0u32;

    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tick.tick() => {
                if conn.is_none() {
                    if let Ok(s) = UnixStream::connect(&sock).await {
                        let (r, w) = s.into_split();
                        conn = Some((BufReader::new(r).lines(), w));
                    }
                }
                let Some((lines, w)) = conn.as_mut() else { continue };
                let mut ok = true;
                for (rid, prop) in [(1, "time-pos"), (2, "duration"), (3, "pause")] {
                    let msg = json!({ "command": ["get_property", prop], "request_id": rid }).to_string() + "\n";
                    if w.write_all(msg.as_bytes()).await.is_err() { ok = false; }
                }
                // Lit les réponses disponibles (événements ignorés).
                let deadline = tokio::time::sleep(Duration::from_millis(400));
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        l = lines.next_line() => match l {
                            Ok(Some(l)) => {
                                let v: Value = serde_json::from_str(&l).unwrap_or_default();
                                match v["request_id"].as_i64() {
                                    Some(1) => if let Some(p) = v["data"].as_f64() { out.position = p; },
                                    Some(2) => if let Some(d) = v["data"].as_f64() { out.duration = d; },
                                    Some(3) => { paused = v["data"].as_bool().unwrap_or(false); },
                                    _ => {}
                                }
                            }
                            _ => { ok = false; break; }
                        }
                    }
                }
                if !ok { conn = None; continue; }

                if let (Some(jf), Some(r)) = (&jf, &req.report) {
                    if !started && out.duration > 0.0 {
                        started = true;
                        let _ = jf.report(ReportStage::Start, r, out.position, paused).await;
                    } else if started {
                        report_every += 1;
                        if report_every % 5 == 0 {
                            let _ = jf.report(ReportStage::Progress, r, out.position, paused).await;
                        }
                    }
                }
            }
        }
    }

    log::info!("fin de lecture : {:.0} s / {:.0} s", out.position, out.duration);
    if let (Some(jf), Some(r)) = (&jf, &req.report) {
        if let Err(e) = jf.report(ReportStage::Stop, r, out.position, false).await {
            log::warn!("rapport d'arrêt Jellyfin : {e:#}");
        }
        if out.finished() {
            let _ = jf.set_played(&r.item_id, true).await;
        }
    }
    let _ = std::fs::remove_file(&sock);
    Ok(out)
}

fn overlay_script() -> Option<std::path::PathBuf> {
    let p = dirs::data_dir()?.join("noosfin/mpv/noosfin-osc.lua");
    p.exists().then_some(p)
}

/// Relaie un bouton de manette à l'overlay mpv (script-message noosfin-pad).
pub async fn send_pad(sock: &str, button: &str) {
    if let Ok(mut s) = UnixStream::connect(sock).await {
        let msg = json!({ "command": ["script-message", "noosfin-pad", button] }).to_string() + "\n";
        let _ = s.write_all(msg.as_bytes()).await;
    }
}
