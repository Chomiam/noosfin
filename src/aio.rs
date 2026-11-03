//! Client Stremio pour AIOStreams (ou Cinemeta), en appoint de TMDB et du moteur intégré.

use anyhow::{Result, bail};
use reqwest::Client;
use serde::Deserialize;

use crate::config::Prefs;
use crate::tmdb::Tmdb;

#[derive(Clone)]
pub struct Aio {
    http: Client,
    base: String,
    /// Avec une clé TMDB, catalogues et fiches viennent de TMDB (en français).
    tmdb: Option<Tmdb>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "releaseInfo")]
    pub release_info: Option<String>,
    #[serde(rename = "imdbRating")]
    pub imdb_rating: Option<String>,
    pub genres: Vec<String>,
    pub videos: Vec<Video>,
    pub imdb_id: Option<String>,
    /// Autres titres (anglais, original) pour la recherche de sources.
    #[serde(skip)]
    pub alt_names: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Video {
    pub id: String,
    pub title: Option<String>,
    pub name: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub thumbnail: Option<String>,
    pub overview: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawStream {
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    url: Option<String>,
    #[serde(rename = "behaviorHints")]
    hints: Hints,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Hints {
    filename: Option<String>,
    #[serde(rename = "videoSize")]
    video_size: Option<u64>,
}

/// Source analysée et notée, prête à être affichée.
#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub url: String,
    pub provider: String,
    pub quality: String,
    pub hdr: String,
    pub source: String,
    pub codec: String,
    pub audio: String,
    pub langs: String,
    pub size: String,
    pub filename: String,
    pub tags: Vec<String>,
    pub score: i64,
}

impl Aio {
    pub fn new(http: Client, manifest_url: &str) -> Self {
        Self { http, base: manifest_url.trim().trim_end_matches("/manifest.json").to_string(), tmdb: None }
    }

    pub fn with_tmdb(mut self, tmdb: Option<Tmdb>) -> Self {
        self.tmdb = tmdb;
        self
    }

    pub fn tmdb_active(&self) -> bool {
        self.tmdb.is_some()
    }

    pub fn enabled(&self) -> bool {
        self.tmdb.is_some() || self.base.starts_with("http")
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        if !self.enabled() {
            bail!("AIOStreams non configuré");
        }
        let resp = self.http.get(format!("{}{}", self.base, path)).send().await?;
        if !resp.status().is_success() {
            bail!("AIOStreams {path}: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    pub async fn catalog(&self, kind: &str, id: &str, search: Option<&str>) -> Result<Vec<Meta>> {
        if let (Some(t), true) = (&self.tmdb, id.starts_with("tmdb.")) {
            return t.catalog(kind, id, search).await;
        }
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            metas: Vec<Meta>,
        }
        let path = match search {
            Some(q) => format!("/catalog/{kind}/{id}/search={}.json", urlencoding::encode(q)),
            None => format!("/catalog/{kind}/{id}.json"),
        };
        Ok(self.get::<R>(&path).await?.metas)
    }

    pub async fn meta(&self, kind: &str, id: &str) -> Result<Meta> {
        if let Some(t) = &self.tmdb {
            if id.starts_with("tmdb:") || id.starts_with("tt") {
                return t.meta(kind, id).await;
            }
        }
        #[derive(Deserialize)]
        struct R {
            meta: Meta,
        }
        Ok(self.get::<R>(&format!("/meta/{kind}/{}.json", urlencoding::encode(id))).await?.meta)
    }

    /// `id` au format Stremio : `tt123`, `tt123:1:2`, `kitsu:3936:1`, `mal:5114:1`…
    pub async fn streams(&self, kind: &str, id: &str, prefs: &Prefs, anime: bool) -> Result<Vec<StreamInfo>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            streams: Vec<RawStream>,
        }
        let raw = self.get::<R>(&format!("/stream/{kind}/{id}.json")).await?.streams;
        let mut out: Vec<StreamInfo> = raw.into_iter().filter_map(|s| parse_stream(s, prefs, anime)).collect();
        out.sort_by(|a, b| b.score.cmp(&a.score));
        Ok(out)
    }
}

fn parse_stream(s: RawStream, prefs: &Prefs, anime: bool) -> Option<StreamInfo> {
    let url = s.url?;
    let name = s.name.unwrap_or_default();
    let desc = s.description.or(s.title).unwrap_or_default();
    let filename = s.hints.filename.clone().unwrap_or_else(|| {
        line_after(&desc, "📁").unwrap_or_default()
    });
    let hay = format!("{name} {desc} {filename}").to_lowercase();

    // Résolution : nom du flux (AIOStreams y met la résolution), puis nom de fichier, puis
    // description. On compare des mots entiers : « 4k » ou « uhd » au milieu d'un nom de
    // groupe ou d'un titre ne doivent pas l'emporter sur un « 1080p » explicite.
    let height = [name.as_str(), filename.as_str(), desc.as_str()]
        .iter()
        .find_map(|t| resolution_of(t))
        .unwrap_or(0);
    let quality = match height {
        2160 => "4K",
        1440 => "1440p",
        1080 => "1080p",
        720 => "720p",
        480 => "SD",
        _ => "?",
    }
    .to_string();

    let has = |k: &str| hay.contains(k);
    let dv = has(" dv ") || has(".dv.") || has("dolby vision") || has("| dv") || has("dv |") || has("📺 dv");
    let hdr10p = has("hdr10+") || has("hdr10plus");
    let hdr = has("hdr");
    let hdr_label = match (dv, hdr10p, hdr) {
        (true, _, true) => "DV · HDR10",
        (true, _, false) => "DV",
        (false, true, _) => "HDR10+",
        (false, false, true) => "HDR10",
        _ => "",
    }
    .to_string();

    // « [TB⚡] UwU-FR 1080p » → « UwU-FR »
    let is_res = |w: &str| {
        w.eq_ignore_ascii_case("4k") || (w.ends_with('p') && w[..w.len() - 1].chars().all(|c| c.is_ascii_digit()))
    };
    let provider = name
        .rsplit(']')
        .next()
        .unwrap_or(&name)
        .split_whitespace()
        .filter(|w| !is_res(w))
        .collect::<Vec<_>>()
        .join(" ");
    let cached = name.contains('⚡');

    let source = line_field(&desc, "🎥").unwrap_or_default();
    let codec = line_field(&desc, "🎞️").or_else(|| line_field(&desc, "🎞")).unwrap_or_default();
    let audio = line_field(&desc, "🎧").unwrap_or_default();
    let langs = line_field(&desc, "🌎").unwrap_or_default();
    let size = match s.hints.video_size {
        Some(b) if b > 0 => format!("{:.1} Go", b as f64 / 1e9),
        _ => line_field(&desc, "📦").unwrap_or_default(),
    };

    let mut tags = Vec::new();
    let fl = filename.to_lowercase();
    for (k, label) in [("vostfr", "VOSTFR"), ("multi", "MULTI"), ("truefrench", "TRUEFRENCH"), ("vff", "VFF"), ("vf2", "VF2")] {
        if fl.contains(k) || hay.contains(k) {
            tags.push(label.to_string());
        }
    }
    if has("remux") {
        tags.push("REMUX".into());
    }
    if has("atmos") {
        tags.push("Atmos".into());
    }
    if has("imax") {
        tags.push("IMAX".into());
    }
    if cached {
        tags.push("⚡ Cache".into());
    }

    // --- Score -------------------------------------------------------------------------
    let mut score: i64 = 0;
    if height > prefs.max_height {
        score -= 5000;
    } else {
        score += height as i64 / 2;
    }
    if prefs.prefer_hdr && !hdr_label.is_empty() {
        score += if dv { 250 } else { 200 };
    }
    let langs_pref = if anime { &prefs.anime_langs } else { &prefs.movie_langs };
    for (i, l) in langs_pref.iter().enumerate() {
        if hay.contains(&l.to_lowercase()) {
            score += 600 - (i as i64) * 100;
            break;
        }
    }
    if !hay.contains("french") && !hay.contains("multi") && !hay.contains("vostfr") && !hay.contains("vf") {
        score -= 400;
    }
    if has("remux") {
        score += 60;
    }
    if cached {
        score += 300;
    }
    // Les packs de saison complets sont plus lents à démarrer.
    if anime && (fl.contains(".s01.") || fl.contains("integrale") || fl.contains("complete")) && !fl.contains("e0") {
        score -= 150;
    }
    if hay.contains("cam") && hay.contains("hdcam") {
        score -= 3000;
    }

    Some(StreamInfo {
        url,
        provider: if provider.is_empty() { name.clone() } else { provider },
        quality,
        hdr: hdr_label,
        source,
        codec,
        audio,
        langs,
        size,
        filename,
        tags,
        score,
    })
}

/// Texte suivant `emoji` jusqu'au prochain emoji / fin de ligne.
fn line_field(desc: &str, emoji: &str) -> Option<String> {
    let start = desc.find(emoji)? + emoji.len();
    let rest = &desc[start..];
    let end = rest
        .char_indices()
        .find(|&(_, c)| c == '\n' || (c as u32) >= 0x1F000 || c == '🏷' || c == '🔊' || c == '⏱')
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let v = rest[..end].trim().trim_matches(|c: char| c == '️' || c == ':').trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn line_after(desc: &str, emoji: &str) -> Option<String> {
    let start = desc.find(emoji)? + emoji.len();
    desc[start..].lines().next().map(|l| l.trim().to_string())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CatalogDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub extra: Vec<ExtraDef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExtraDef {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub catalogs: Vec<CatalogDef>,
}

impl CatalogDef {
    pub fn is_search(&self) -> bool {
        self.extra.iter().any(|e| e.name == "search")
    }
}

impl Aio {
    pub async fn manifest(&self) -> Result<Manifest> {
        if let Some(t) = &self.tmdb {
            return Ok(t.manifest());
        }
        self.get("/manifest.json").await
    }
}


/// Résolution annoncée par un texte, d'après ses mots (`1080p`, `2160p`, `4K`, `UHD`…).
/// Un « 1080p » explicite l'emporte sur un « 4K » isolé (ex. « 4K Remaster 1080p »).
pub fn resolution_of(text: &str) -> Option<u32> {
    let mut found: Option<u32> = None;
    let mut explicit: Option<u32> = None;
    for w in text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()) {
        let w = w.to_ascii_lowercase();
        let digits = w.trim_end_matches(['p', 'i']);
        if (w.ends_with('p') || w.ends_with('i')) && digits.len() >= 3 && digits.chars().all(|c| c.is_ascii_digit()) {
            let h: u32 = digits.parse().unwrap_or(0);
            if matches!(h, 360 | 480 | 540 | 576 | 720 | 1080 | 1440 | 2160 | 4320) {
                explicit = Some(explicit.map_or(h, |e| e.max(h)));
            }
        } else if w == "4k" || w == "uhd" || w == "2160" {
            found = Some(2160);
        }
    }
    explicit.or(found)
}

#[cfg(test)]
mod tests {
    use super::resolution_of;

    #[test]
    fn resolution() {
        assert_eq!(resolution_of("[TB⚡] UwU-FR 1080p"), Some(1080));
        assert_eq!(resolution_of("Interstellar.2014.2160p.IMAX.REMUX"), Some(2160));
        assert_eq!(resolution_of("interstellar.2014.imax.4k-kc.mkv"), Some(2160));
        assert_eq!(resolution_of("Movie.4K.Remaster.1080p.BluRay"), Some(1080));
        assert_eq!(resolution_of("Group-jennaortegaUHD 1080p"), Some(1080));
        assert_eq!(resolution_of("Show.S01E01.MULTi.WEB"), None);
    }
}
