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
    /// Langues audio par ordre de préférence (codes : ja, fr, en…), animes puis films / séries.
    pub audio_anime: Vec<String>,
    pub audio_video: Vec<String>,
    /// Langues de sous-titres par ordre de préférence ; vide = sans sous-titres.
    pub subs_anime: Vec<String>,
    pub subs_video: Vec<String>,
    /// Pas de sous-titres quand l'audio choisi est déjà dans la langue des sous-titres (VF).
    pub subs_off_with_dub_anime: bool,
    pub subs_off_with_dub_video: bool,
    /// Taille des sous-titres (1.0 = normale).
    pub sub_scale: f64,
    /// Sous-titres en haut de l'image plutôt qu'en bas.
    pub sub_top: bool,
    /// Sons de navigation de l'interface.
    pub ui_sounds: bool,
}

/// Langues proposées dans les paramètres Audio / Sous-titres : (code, libellé).
pub const LANGUAGES: &[(&str, &str)] = &[
    ("fr", "Français"),
    ("ja", "Japonais"),
    ("en", "Anglais"),
    ("es", "Espagnol"),
    ("de", "Allemand"),
    ("it", "Italien"),
    ("ko", "Coréen"),
    ("zh", "Chinois"),
];

pub fn language_label(code: &str) -> &str {
    LANGUAGES.iter().find(|l| l.0 == code).map_or(code, |l| l.1)
}

/// Codes ISO 639-1 et 639-2 reconnus par mpv pour une langue.
fn mpv_codes(code: &str) -> &'static str {
    match code {
        "fr" => "fre,fra,fr",
        "ja" => "jpn,ja,jp",
        "en" => "eng,en",
        "es" => "spa,es",
        "de" => "ger,deu,de",
        "it" => "ita,it",
        "ko" => "kor,ko",
        "zh" => "chi,zho,zh",
        _ => "",
    }
}

impl PlayerConfig {
    /// Options mpv de langue et de sous-titres pour une lecture (anime ou film / série).
    pub fn language_args(&self, anime: bool) -> Vec<String> {
        let (audio, subs, off) =
            if anime { (&self.audio_anime, &self.subs_anime, self.subs_off_with_dub_anime) } else { (&self.audio_video, &self.subs_video, self.subs_off_with_dub_video) };
        let join = |l: &Vec<String>| l.iter().map(|c| mpv_codes(c)).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(",");
        let mut v = vec![];
        if !audio.is_empty() {
            v.push(format!("--alang={}", join(audio)));
        }
        if subs.is_empty() {
            v.push("--sid=no".into());
        } else {
            v.push(format!("--slang={}", join(subs)));
            // « forced » : on garde les sous-titres forcés (panneaux, chansons) même en VF.
            v.push(format!("--subs-with-matching-audio={}", if off { "forced" } else { "yes" }));
        }
        if (self.sub_scale - 1.0).abs() > 0.01 {
            v.push(format!("--sub-scale={:.2}", self.sub_scale));
        }
        if self.sub_top {
            v.push("--sub-align-y=top".into());
            v.push("--sub-pos=6".into());
        }
        v
    }
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
            audio_anime: vec!["ja".into(), "fr".into(), "en".into()],
            audio_video: vec!["fr".into(), "en".into()],
            subs_anime: vec!["fr".into(), "en".into()],
            subs_video: vec!["fr".into()],
            subs_off_with_dub_anime: true,
            subs_off_with_dub_video: true,
            sub_scale: 1.0,
            sub_top: false,
            ui_sounds: true,
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
