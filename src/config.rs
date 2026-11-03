//! Configuration persistante : ~/.config/noosfin/config.toml

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub jellyfin: JellyfinConfig,
    pub aiostreams: AioConfig,
    pub player: PlayerConfig,
    pub session: SessionConfig,
    pub prefs: Prefs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JellyfinConfig {
    pub url: String,
    pub token: String,
    pub user_id: String,
    pub user_name: String,
    pub device_id: String,
    /// Nom (insensible à la casse) de la bibliothèque contenant les animes.
    pub anime_library: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AioConfig {
    /// URL complète du manifest Stremio (AIOStreams).
    pub manifest_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    /// Wrapper mpv (gère déjà Wayland/HDR et Gamescope).
    pub command: String,
    pub ipc_socket: String,
    pub extra_args: Vec<String>,
    /// Overlay Noosfin (boutons, pistes, infos) à la place de l'OSC de mpv.
    pub overlay: bool,
    /// « auto » : selon le GPU détecté (vaapi / nvdec), repli logiciel automatique.
    pub hwdec: String,
    /// « standard » (mixage PipeWire) ou « bitperfect » (fréquence d'origine + bitstream HDMI).
    pub audio: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Client gamescope-session-plus à lancer pour Steam.
    pub steam_client: String,
    /// Disposition du clavier physique dans la session (XKB), ex. « fr », « us », « ch(fr) ».
    pub keyboard: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Langues recherchées dans les sources des animes (VOSTFR en tête).
    pub anime_langs: Vec<String>,
    /// Langues recherchées dans les sources films/séries.
    pub movie_langs: Vec<String>,
    /// Résolution maximale acceptée par le tri automatique.
    pub max_height: u32,
    pub prefer_hdr: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jellyfin: JellyfinConfig::default(),
            aiostreams: AioConfig::default(),
            player: PlayerConfig::default(),
            session: SessionConfig::default(),
            prefs: Prefs::default(),
        }
    }
}

impl Default for JellyfinConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            user_id: String::new(),
            user_name: String::new(),
            device_id: uuid::Uuid::new_v4().simple().to_string(),
            anime_library: "anime".into(),
        }
    }
}

impl Default for AioConfig {
    fn default() -> Self {
        Self { manifest_url: String::new() }
    }
}

impl Default for PlayerConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            command: home.join(".local/bin/noosfin-mpv").to_string_lossy().into_owned(),
            ipc_socket: "/tmp/noosfin-mpv.sock".into(),
            extra_args: vec![],
            overlay: true,
            hwdec: "auto".into(),
            audio: "standard".into(),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { steam_client: "ogui-steam".into(), keyboard: "fr".into() }
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            anime_langs: vec!["vostfr".into(), "multi".into(), "japanese".into(), "french".into()],
            movie_langs: vec!["multi".into(), "french".into(), "vff".into(), "truefrench".into(), "vf".into()],
            max_height: 2160,
            prefer_hdr: true,
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("noosfin/config.toml")
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        let cfg = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                log::error!("config invalide {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        // Ancien lanceur (avec le pont manette Python) → lanceur Noosfin s'il est installé.
        let mut cfg = cfg;
        let home = dirs::home_dir().unwrap_or_default();
        if cfg.player.command == home.join(".local/bin/mpv").to_string_lossy() && home.join(".local/bin/noosfin-mpv").exists() {
            cfg.player.command = home.join(".local/bin/noosfin-mpv").to_string_lossy().into_owned();
        }
        if let Err(e) = cfg.save() {
            log::warn!("impossible d'écrire la config : {e:#}");
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, toml::to_string_pretty(self)?).with_context(|| path.display().to_string())?;
        Ok(())
    }

    pub fn jellyfin_ready(&self) -> bool {
        !self.jellyfin.token.is_empty() && !self.jellyfin.user_id.is_empty()
    }
}
