//! État local : progression des animes regardés en streaming (hors Jellyfin),
//! positions de reprise des flux et historique de lecture (10 jours).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Durée de conservation de l'historique.
pub const HISTORY_DAYS: i64 = 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimeProgress {
    pub title: String,
    pub cover: Option<String>,
    pub last_watched: i64,
    pub total: Option<i64>,
    pub updated: i64,
}

/// Comment rouvrir la fiche d'une entrée d'historique.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum OpenRef {
    Anime { id: i64 },
    Jf { id: String },
    Meta { kind: String, id: String },
    Iptv { profile: String, kind: String, id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Clé du flux lu (une entrée par épisode / film).
    pub key: String,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub backdrop: Option<String>,
    pub open: OpenRef,
    /// Résumé affiché dans le bandeau (anciennes entrées : complété au lancement).
    #[serde(default)]
    pub overview: String,
    pub position: f64,
    pub duration: f64,
    pub finished: bool,
    pub watched_at: i64,
}

/// Favori enregistré localement (films, séries, animes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub open: OpenRef,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub backdrop: Option<String>,
    #[serde(default)]
    pub overview: String,
    pub added: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub anime: HashMap<i64, AnimeProgress>,
    /// clé de flux → secondes
    pub positions: HashMap<String, f64>,
    /// clé de flux → durée totale (pour afficher la progression)
    pub durations: HashMap<String, f64>,
    pub history: Vec<HistoryEntry>,
    /// Favoris, du plus récent au plus ancien.
    pub favorites: Vec<Favorite>,
}

fn path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("noosfin/state.json")
}

impl State {
    pub fn load() -> State {
        let mut s: State =
            std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        s.prune_history();
        s
    }

    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }

    /// Animes en cours, du plus récent au plus ancien.
    pub fn watching(&self) -> Vec<(i64, AnimeProgress)> {
        let mut v: Vec<_> = self
            .anime
            .iter()
            .filter(|(_, p)| p.total.is_none_or(|t| p.last_watched < t))
            .map(|(k, p)| (*k, p.clone()))
            .collect();
        v.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
        v
    }

    pub fn prune_history(&mut self) {
        let limit = chrono::Local::now().timestamp() - HISTORY_DAYS * 86_400;
        self.history.retain(|h| h.watched_at >= limit);
        self.history.sort_by(|a, b| b.watched_at.cmp(&a.watched_at));
    }

    /// Ajoute (ou remonte) une entrée ; une seule entrée par clé.
    pub fn record(&mut self, entry: HistoryEntry) {
        self.history.retain(|h| h.key != entry.key);
        self.history.insert(0, entry);
        self.prune_history();
    }

    pub fn remove_history(&mut self, key: &str) {
        self.history.retain(|h| h.key != key);
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    pub fn is_favorite(&self, open: &OpenRef) -> bool {
        self.favorites.iter().any(|f| &f.open == open)
    }

    /// Ajoute ou retire ; renvoie vrai si l'élément est désormais favori.
    pub fn toggle_favorite(&mut self, fav: Favorite) -> bool {
        if self.is_favorite(&fav.open) {
            self.favorites.retain(|f| f.open != fav.open);
            false
        } else {
            self.favorites.insert(0, fav);
            true
        }
    }

    /// Progression (0..1) d'un flux, ou -1.
    pub fn progress_of(&self, key: &str) -> f32 {
        match (self.positions.get(key), self.durations.get(key)) {
            (Some(p), Some(d)) if *d > 0.0 => (p / d) as f32,
            _ => -1.0,
        }
    }
}
