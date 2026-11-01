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
    /// Options mpv propres à cette lecture (périphérique du disque optique…).
    pub mpv_args: Vec<String>,
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
        // Menu « Image » (bouton Start) : filtres proposés selon la puissance du GPU.
        let (tier, name) = gpu_profile();
        let shaders = dirs::data_dir().unwrap_or_default().join("noosfin/shaders");
        cmd.arg(format!("--script-opts=noosfin-gpu={tier},noosfin-gpuname={name},noosfin-shaders={}", shaders.display()));
    }
    cmd.args(&req.mpv_args);
    // Audio bit-perfect : PCM à sa fréquence d'origine, formats compressés envoyés tels quels
    // (bitstream) à l'ampli HDMI, sans normalisation ni volume logiciel.
    let restore_rates = if cfg.audio == "bitperfect" { bitperfect_prepare(&mut cmd).await } else { None };
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
    if let Some(rates) = restore_rates {
        let _ = tokio::process::Command::new("pw-metadata").args(["-n", "settings", "0", "clock.allowed-rates", &rates]).output().await;
    }
    Ok(out)
}

async fn output(cmd: &str, args: &[&str]) -> String {
    // Sortie en anglais : les outils PipeWire traduisent leurs libellés (« Destination n° »).
    tokio::process::Command::new(cmd).args(args).env("LC_ALL", "C").output().await.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
}

/// Prépare PipeWire et mpv pour une sortie bit-perfect ; renvoie les fréquences à rétablir.
async fn bitperfect_prepare(cmd: &mut Command) -> Option<String> {
    // Sortie HDMI : celle par défaut si c'en est une, sinon la première trouvée.
    let default = output("pactl", &["get-default-sink"]).await.trim().to_string();
    let sinks = output("pactl", &["list", "short", "sinks"]).await;
    let names: Vec<&str> = sinks.lines().filter_map(|l| l.split('\t').nth(1)).collect();
    let hdmi = if default.contains("hdmi") { Some(default.as_str()) } else { names.iter().copied().find(|n| n.contains("hdmi")) };
    // Formats déclarés pour la sortie HDMI (réglages audio de KDE / PipeWire) : on n'envoie en
    // bitstream que ce que l'écran ou l'ampli sait décoder, sinon c'est le silence.
    let mut spdif = vec![];
    if let Some(sink) = hdmi {
        cmd.arg(format!("--audio-device=pipewire/{sink}"));
        let info = output("pactl", &["list", "sinks"]).await;
        let block = info.split("Sink #").find(|b| b.contains(&format!("Name: {sink}"))).unwrap_or_default();
        let codecs = block.lines().find(|l| l.contains("iec958.codecs")).unwrap_or_default().to_uppercase();
        for (name, opt) in [("\"AC3\"", "ac3"), ("\"EAC3\"", "eac3"), ("\"DTS\"", "dts"), ("\"DTS-HD\"", "dts-hd"), ("\"TRUEHD\"", "truehd")] {
            if codecs.contains(name) {
                spdif.push(opt);
            }
        }
    }
    if !spdif.is_empty() {
        cmd.arg(format!("--audio-spdif={}", spdif.join(",")));
    }
    cmd.args([
        "--audio-channels=auto",
        "--audio-normalize-downmix=no",
        "--volume=100",
        "--volume-max=100",
        "--replaygain=no",
        "--audio-pitch-correction=no",
    ]);
    // PipeWire suit la fréquence du fichier (44,1 / 96 / 192 kHz…) au lieu de rééchantillonner.
    let settings = output("pw-metadata", &["-n", "settings", "0", "clock.allowed-rates"]).await;
    let previous = settings.split("value:'").nth(1).and_then(|v| v.split('\'').next()).map(String::from);
    let _ = tokio::process::Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.allowed-rates", "[ 44100 48000 88200 96000 176400 192000 ]"])
        .output()
        .await;
    Some(previous.unwrap_or_else(|| "[ 48000 ]".into()))
}

/// Puissance de la carte graphique pour les filtres d'image : « low » (iGPU, Steam Deck),
/// « mid » (APU / dGPU modeste) ou « high » (carte dédiée), et son nom court.
pub fn gpu_profile() -> (&'static str, String) {
    let mut best: (u8, String) = (0, "GPU".into());
    for e in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = e.path().join("device");
        let vendor = std::fs::read_to_string(dev.join("vendor")).unwrap_or_default();
        let vram = std::fs::read_to_string(dev.join("mem_info_vram_total")).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        // Les iGPU Intel sont toujours sur le bus 00:02.0 ; ailleurs, c'est une carte Arc.
        let slot = std::fs::canonicalize(&dev).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let (rank, label) = match vendor.trim() {
            "0x10de" => (3, "NVIDIA".to_string()),
            "0x1002" if vram >= 6 << 30 => (3, format!("AMD {} Go", (vram + (1 << 29)) >> 30)),
            "0x1002" if vram >= 2 << 30 => (2, format!("AMD {} Go", (vram + (1 << 29)) >> 30)),
            "0x1002" => (1, "AMD intégré".to_string()),
            "0x8086" if !slot.ends_with("0000:00:02.0") => (3, "Intel Arc".to_string()),
            "0x8086" => (1, "Intel intégré".to_string()),
            _ => continue,
        };
        if rank > best.0 {
            best = (rank, label);
        }
    }
    let tier = match best.0 {
        3 => "high",
        2 => "mid",
        _ => "low",
    };
    (tier, best.1)
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
