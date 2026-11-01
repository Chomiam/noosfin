//! Configuration du moteur de sources : ~/.config/noosfin/sources.json (droits 600).
//! Éditée depuis le téléphone via la page web locale (voir `web.rs`).

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DebridKind {
    None,
    AllDebrid,
    TorBox,
    RealDebrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Debrid {
    pub service: DebridKind,
    pub api_key: String,
    /// Lancer le téléchargement sur le débrideur quand la source n'est pas en cache.
    pub download_uncached: bool,
}

/// Type d'indexeur privé / Torznab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IndexerKind {
    /// Torznab standard (C411, Tr4ker, Jackett, UNIT3D…).
    Torznab,
    /// API JSON de Prowlarr (tous ses indexeurs, FlareSolverr compris).
    Prowlarr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Indexer {
    pub name: String,
    pub kind: IndexerKind,
    pub url: String,
    pub api_key: String,
    pub enabled: bool,
    /// Tracker privé : on transmet le .torrent (passkey) au débrideur plutôt qu'un magnet.
    pub private: bool,
    /// Délai minimal entre deux requêtes (quota du tracker).
    pub min_interval_ms: u64,
    /// Interrogé aussi pour les animes.
    pub anime: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Public {
    pub yts: bool,
    pub eztv: bool,
    pub tpb: bool,
    pub nyaa: bool,
    /// Méta-moteur public (1337x, TorrentGalaxy, LimeTorrents, RuTracker…).
    pub knaben: bool,
    pub torrentscsv: bool,
    /// Animes : sorties SubsPlease (magnets par résolution).
    pub subsplease: bool,
    pub animetosho: bool,
    /// Trackers publics intégrés (définitions Cardigann embarquées), par identifiant.
    pub trackers: Vec<String>,
    /// Ancienne instance AIOStreams, en source d'appoint.
    pub aiostreams: bool,
}

/// Catalogues et fiches (affiches, résumés en français) via TMDB.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub tmdb_key: String,
    pub language: String,
}

impl Default for Metadata {
    fn default() -> Self {
        Self { tmdb_key: String::new(), language: "fr-FR".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Dns {
    System,
    Cloudflare,
    Quad9,
    Google,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    /// DNS chiffré (DoH) pour contourner les blocages DNS des FAI.
    pub dns: Dns,
    /// FlareSolverr pour les pages protégées par Cloudflare : « local » (conteneur podman géré
    /// par l'app), une URL, ou vide pour le désactiver.
    pub flaresolverr: String,
    pub timeout_secs: u64,
}

/// Critères de tri, appliqués dans l'ordre (le premier départage tout).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Criterion {
    Cached,
    Language,
    Subtitles,
    Resolution,
    Quality,
    Hdr,
    Codec,
    Size,
    Seeders,
    Group,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ranking {
    pub order: Vec<Criterion>,
    /// Langues audio par ordre de préférence : vff, vfq, vf2, truefrench, multi, vostfr, vo.
    pub languages: Vec<String>,
    pub excluded_languages: Vec<String>,
    /// Sous-titres souhaités (fr, en…).
    pub subtitles: Vec<String>,
    /// 2160p, 1080p, 720p, 480p
    pub resolutions: Vec<String>,
    pub excluded_resolutions: Vec<String>,
    /// remux, bluray, web-dl, webrip, hdtv
    pub qualities: Vec<String>,
    pub exclude_cam: bool,
    /// dv, hdr10+, hdr10, hlg, sdr
    pub hdr: Vec<String>,
    /// hevc, av1, avc
    pub codecs: Vec<String>,
    /// Taille maximale (Go), 0 = illimitée.
    pub max_size_gb: f64,
    pub cached_only: bool,
    pub max_results: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Anime {
    pub languages: Vec<String>,
    /// Groupes de release favoris (fansubs FR, raws…).
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub debrid: Debrid,
    pub metadata: Metadata,
    pub indexers: Vec<Indexer>,
    pub public: Public,
    pub network: Network,
    pub ranking: Ranking,
    pub anime: Anime,
}

impl Default for Debrid {
    fn default() -> Self {
        Self { service: DebridKind::None, api_key: String::new(), download_uncached: false }
    }
}

impl Default for Indexer {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: IndexerKind::Torznab,
            url: String::new(),
            api_key: String::new(),
            enabled: true,
            private: true,
            min_interval_ms: 1000,
            anime: true,
        }
    }
}

/// Trackers intégrés actifs par défaut : ceux qui répondent vite sans FlareSolverr
/// (Knaben couvre déjà 1337x, KickassTorrents, TPB…).
pub const DEFAULT_TRACKERS: &[&str] = &["limetorrents", "nekobt", "bangumi-moe", "shanaproject", "anibt"];

/// FlareSolverr lancé par l'application dans un conteneur podman local.
pub const LOCAL_FLARESOLVERR: &str = "local";

impl Default for Public {
    fn default() -> Self {
        Self { yts: true, eztv: true, tpb: true, nyaa: true, knaben: true, torrentscsv: true, subsplease: true, animetosho: false, trackers: DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect(), aiostreams: false }
    }
}

impl Default for Network {
    fn default() -> Self {
        Self { dns: Dns::Cloudflare, flaresolverr: LOCAL_FLARESOLVERR.into(), timeout_secs: 15 }
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl Default for Ranking {
    fn default() -> Self {
        use Criterion::*;
        Self {
            order: vec![Cached, Language, Resolution, Quality, Hdr, Subtitles, Codec, Seeders, Size],
            languages: strings(&["multi", "vff", "truefrench", "vfq", "vf2", "vostfr", "vo"]),
            excluded_languages: vec![],
            subtitles: strings(&["fr"]),
            resolutions: strings(&["2160p", "1080p", "720p", "480p"]),
            excluded_resolutions: vec![],
            qualities: strings(&["remux", "bluray", "web-dl", "webrip", "hdtv"]),
            exclude_cam: true,
            hdr: strings(&["dv", "hdr10+", "hdr10", "hlg", "sdr"]),
            codecs: strings(&["hevc", "av1", "avc"]),
            max_size_gb: 0.0,
            cached_only: false,
            max_results: 60,
        }
    }
}

impl Default for Anime {
    fn default() -> Self {
        Self {
            languages: strings(&["vostfr", "multi", "vff", "vo"]),
            groups: strings(&["Tsundere-Raws", "FRiTCHi571", "VEFA", "Erai-raws", "SubsPlease", "Kaerizaki-Fansub"]),
        }
    }
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            debrid: Debrid::default(),
            metadata: Metadata::default(),
            indexers: vec![],
            public: Public::default(),
            network: Network::default(),
            ranking: Ranking::default(),
            anime: Anime::default(),
        }
    }
}

pub fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("noosfin/sources.json")
}

impl Sources {
    pub fn load() -> Sources {
        let mut s: Sources = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        // Prowlarr n'est plus utilisé : les trackers sont intégrés à l'application.
        s.indexers.retain(|i| i.kind != IndexerKind::Prowlarr);
        // L'ancien FlareSolverr du NAS est remplacé par l'instance locale gérée par l'app.
        if s.network.flaresolverr.contains("192.168.1.15") {
            s.network.flaresolverr = LOCAL_FLARESOLVERR.into();
        }
        s
    }

    pub fn save(&self) -> Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn has_debrid(&self) -> bool {
        self.debrid.service != DebridKind::None && !self.debrid.api_key.trim().is_empty()
    }

    pub fn summary(&self) -> String {
        let debrid = match (&self.debrid.service, self.has_debrid()) {
            (DebridKind::AllDebrid, true) => "AllDebrid",
            (DebridKind::TorBox, true) => "TorBox",
            (DebridKind::RealDebrid, true) => "Real-Debrid",
            _ => "aucun débrideur",
        };
        let private = self.indexers.iter().filter(|i| i.enabled).count();
        let p = &self.public;
        let public = [p.yts, p.eztv, p.tpb, p.nyaa, p.knaben, p.torrentscsv, p.subsplease].iter().filter(|b| **b).count();
        let builtin = self.public.trackers.len();
        let tmdb = if self.metadata.tmdb_key.trim().is_empty() { "" } else { " · TMDB" };
        format!("{debrid} · {private} tracker(s) privé(s) · {} source(s) publique(s){tmdb}", public + builtin)
    }
}
