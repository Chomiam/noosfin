//! IPTV : comptes Xtream Codes et playlists M3U, profils enregistrés et favoris.
//! Stockage local : ~/.local/share/noosfin/iptv.json (droits 600, contient les identifiants).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Live,
    Movie,
    Series,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Live => "live",
            Kind::Movie => "movie",
            Kind::Series => "series",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Source {
    Xtream { server: String, user: String, pass: String },
    M3u { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub source: Source,
    #[serde(default)]
    pub last_used: i64,
    /// Favoris : "kind:id"
    #[serde(default)]
    pub favorites: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub profiles: Vec<Profile>,
}

fn store_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("noosfin/iptv.json")
}

impl Store {
    pub fn load() -> Store {
        let mut s: Store =
            std::fs::read_to_string(store_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        s.profiles.sort_by(|a, b| b.last_used.cmp(&a.last_used));
        s
    }

    pub fn save(&self) -> Result<()> {
        let p = store_path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

impl Profile {
    pub fn kind_label(&self) -> &'static str {
        match self.source {
            Source::Xtream { .. } => "Xtream Codes",
            Source::M3u { .. } => "Playlist M3U",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub logo: Option<String>,
    pub category: String,
    /// URL directe (M3U, ou calculée pour Xtream live / film).
    pub url: Option<String>,
    pub rating: Option<String>,
    pub year: Option<String>,
    pub plot: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub categories: HashMap<Kind, Vec<Category>>,
    pub entries: HashMap<Kind, Vec<Entry>>,
    /// Épisodes des séries issues d'une playlist (identifiant de série → épisodes).
    #[serde(default)]
    pub episodes: HashMap<String, Vec<EpLite>>,
    /// Préfixe commun des URL d'épisodes (les épisodes ne stockent que la fin).
    #[serde(default)]
    pub ep_prefix: String,
}

/// Épisode compact (les gros exports Xtream dépassent le million d'épisodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpLite {
    #[serde(rename = "s")]
    pub season: u16,
    #[serde(rename = "n")]
    pub number: u32,
    #[serde(rename = "u")]
    pub url: String,
}

impl Catalog {
    pub fn total(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    pub fn episode_url(&self, ep: &EpLite) -> String {
        if ep.url.starts_with("http") { ep.url.clone() } else { format!("{}{}", self.ep_prefix, ep.url) }
    }

    /// Épisodes d'une série de playlist, au format de la fiche.
    pub fn series_episodes(&self, series_id: &str) -> Vec<SeriesEpisode> {
        let mut v: Vec<SeriesEpisode> = self
            .episodes
            .get(series_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|e| SeriesEpisode {
                season: e.season as i64,
                number: e.number as i64,
                title: format!("Épisode {}", e.number),
                plot: String::new(),
                image: None,
                url: self.episode_url(e),
                id: e.url.rsplit('/').next().unwrap_or(&e.url).to_string(),
            })
            .collect();
        v.sort_by_key(|e| (e.season, e.number));
        v
    }
}

#[derive(Debug, Clone)]
pub struct SeriesEpisode {
    pub season: i64,
    pub number: i64,
    pub title: String,
    pub plot: String,
    pub image: Option<String>,
    pub url: String,
    pub id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SeriesInfo {
    pub plot: String,
    pub cover: Option<String>,
    pub backdrop: Option<String>,
    pub genre: String,
    pub year: String,
    pub episodes: Vec<SeriesEpisode>,
}

pub struct Account {
    pub status: String,
    pub expires: Option<i64>,
    pub max_connections: Option<String>,
}

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn base(server: &str) -> String {
    let s = server.trim().trim_end_matches('/');
    if s.starts_with("http") { s.to_string() } else { format!("http://{s}") }
}

pub struct Xtream<'a> {
    http: &'a Client,
    /// Client sans délai global pour les listes complètes (plusieurs dizaines de Mo).
    long: Client,
    server: String,
    user: &'a str,
    pass: &'a str,
}

impl<'a> Xtream<'a> {
    pub fn new(http: &'a Client, server: &str, user: &'a str, pass: &'a str) -> Self {
        Self { http, long: download_client(), server: base(server), user, pass }
    }

    async fn api(&self, action: Option<&str>, extra: &[(&str, &str)]) -> Result<Value> {
        self.api_with(self.http, action, extra, None).await
    }

    async fn api_with(&self, http: &Client, action: Option<&str>, extra: &[(&str, &str)], progress: Option<&AtomicU64>) -> Result<Value> {
        let mut q: Vec<(&str, &str)> = vec![("username", self.user), ("password", self.pass)];
        if let Some(a) = action {
            q.push(("action", a));
        }
        q.extend_from_slice(extra);
        let mut r = http.get(format!("{}/player_api.php", self.server)).query(&q).send().await?;
        if !r.status().is_success() {
            bail!("serveur IPTV : HTTP {}", r.status());
        }
        let mut body = Vec::new();
        while let Some(chunk) = r.chunk().await? {
            if let Some(p) = progress {
                p.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body).context("réponse IPTV illisible")?)
    }

    pub async fn account(&self) -> Result<Account> {
        let v = self.api(None, &[]).await?;
        let ui = &v["user_info"];
        if ui["auth"].as_i64() == Some(0) || s(&ui["auth"]).as_deref() == Some("0") {
            bail!("identifiants refusés");
        }
        if ui.is_null() {
            bail!("ce serveur ne répond pas comme un serveur Xtream Codes");
        }
        Ok(Account {
            status: s(&ui["status"]).unwrap_or_else(|| "?".into()),
            expires: s(&ui["exp_date"]).and_then(|e| e.parse().ok()),
            max_connections: s(&ui["max_connections"]),
        })
    }

    pub async fn catalog(&self, progress: &AtomicU64) -> Result<Catalog> {
        let mut cat = Catalog::default();
        for (kind, cats, streams) in [
            (Kind::Live, "get_live_categories", "get_live_streams"),
            (Kind::Movie, "get_vod_categories", "get_vod_streams"),
            (Kind::Series, "get_series_categories", "get_series"),
        ] {
            let (c, e) = tokio::join!(self.api(Some(cats), &[]), self.api_with(&self.long, Some(streams), &[], Some(progress)));
            if let Err(err) = &e {
                log::warn!("IPTV {streams} : {err:#}");
            }
            let categories = c
                .unwrap_or_default()
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|c| Some(Category { id: s(&c["category_id"])?, name: s(&c["category_name"]).unwrap_or_default() }))
                .collect();
            let entries = e
                .unwrap_or_default()
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|e| {
                    let id = s(&e["stream_id"]).or_else(|| s(&e["series_id"]))?;
                    let ext = s(&e["container_extension"]);
                    let url = match kind {
                        Kind::Live => Some(format!("{}/live/{}/{}/{id}.ts", self.server, self.user, self.pass)),
                        Kind::Movie => Some(format!(
                            "{}/movie/{}/{}/{id}.{}",
                            self.server,
                            self.user,
                            self.pass,
                            ext.clone().unwrap_or_else(|| "mp4".into())
                        )),
                        Kind::Series => None,
                    };
                    Some(Entry {
                        id,
                        name: clean_name(&s(&e["name"]).unwrap_or_default()),
                        logo: s(&e["stream_icon"]).or_else(|| s(&e["cover"])),
                        category: s(&e["category_id"]).unwrap_or_default(),
                        url,
                        rating: s(&e["rating"]).filter(|r| r != "0"),
                        year: s(&e["year"]).or_else(|| s(&e["releaseDate"]).map(|d| d.chars().take(4).collect())),
                        plot: s(&e["plot"]),
                    })
                })
                .collect();
            cat.categories.insert(kind, categories);
            cat.entries.insert(kind, entries);
        }
        if cat.total() == 0 {
            // Certains panels renvoient des listes vides via l'API alors que l'export M3U est complet.
            log::info!("API Xtream vide : repli sur l'export M3U (get.php)");
            let url = format!("{}/get.php?username={}&password={}&type=m3u_plus&output=ts", self.server, urlencoding::encode(self.user), urlencoding::encode(self.pass));
            return load_m3u(&url, progress).await;
        }
        Ok(cat)
    }

    /// Détails d'un film : (résumé, fond, genre, durée).
    pub async fn vod_info(&self, vod_id: &str) -> Result<(String, Option<String>, String, String)> {
        let v = self.api(Some("get_vod_info"), &[("vod_id", vod_id)]).await?;
        let i = &v["info"];
        Ok((
            s(&i["plot"]).or_else(|| s(&i["description"])).unwrap_or_default(),
            i["backdrop_path"].get(0).and_then(s).or_else(|| s(&i["cover_big"])),
            s(&i["genre"]).unwrap_or_default(),
            s(&i["duration"]).unwrap_or_default(),
        ))
    }

    pub async fn series_info(&self, series_id: &str) -> Result<SeriesInfo> {
        let v = self.api(Some("get_series_info"), &[("series_id", series_id)]).await?;
        let info = &v["info"];
        let mut episodes = vec![];
        if let Some(map) = v["episodes"].as_object() {
            for (season, list) in map {
                for e in list.as_array().cloned().unwrap_or_default() {
                    let Some(id) = s(&e["id"]) else { continue };
                    let ext = s(&e["container_extension"]).unwrap_or_else(|| "mp4".into());
                    episodes.push(SeriesEpisode {
                        season: s(&e["season"]).and_then(|x| x.parse().ok()).or_else(|| season.parse().ok()).unwrap_or(1),
                        number: s(&e["episode_num"]).and_then(|x| x.parse().ok()).unwrap_or(0),
                        title: clean_name(&s(&e["title"]).unwrap_or_default()),
                        plot: s(&e["info"]["plot"]).unwrap_or_default(),
                        image: s(&e["info"]["movie_image"]),
                        url: format!("{}/series/{}/{}/{id}.{ext}", self.server, self.user, self.pass),
                        id,
                    });
                }
            }
        }
        episodes.sort_by_key(|e| (e.season, e.number));
        Ok(SeriesInfo {
            plot: s(&info["plot"]).unwrap_or_default(),
            cover: s(&info["cover"]),
            backdrop: info["backdrop_path"].get(0).and_then(s),
            genre: s(&info["genre"]).unwrap_or_default(),
            year: s(&info["releaseDate"]).map(|d| d.chars().take(4).collect()).unwrap_or_default(),
            episodes,
        })
    }
}

// ─────────────────────────────── M3U ───────────────────────────────

/// Client sans délai global : un export complet peut dépasser 400 Mo.
fn download_client() -> Client {
    Client::builder()
        .user_agent("VLC/3.0.20 LibVLC/3.0.20")
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

/// Charge une playlist (URL ou fichier local) en la lisant au fil de l'eau.
/// `progress` reçoit le nombre d'octets lus.
pub async fn load_m3u(url: &str, progress: &AtomicU64) -> Result<Catalog> {
    let mut b = M3uBuilder::default();
    let mut pending = Vec::<u8>::new();
    let mut feed = |chunk: &[u8], b: &mut M3uBuilder| {
        pending.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(pos) = pending[start..].iter().position(|&c| c == b'\n') {
            b.line(&String::from_utf8_lossy(&pending[start..start + pos]));
            start += pos + 1;
        }
        pending.drain(..start);
    };
    if url.starts_with("http") {
        let mut r = download_client().get(url).send().await?;
        if !r.status().is_success() {
            bail!("playlist : HTTP {}", r.status());
        }
        while let Some(chunk) = r.chunk().await? {
            progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            feed(&chunk, &mut b);
        }
    } else {
        let data = tokio::fs::read(url.trim_start_matches("file://")).await?;
        progress.fetch_add(data.len() as u64, Ordering::Relaxed);
        feed(&data, &mut b);
    }
    feed(b"\n", &mut b);
    b.finish()
}

fn attr(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let start = line.find(&pat)? + pat.len();
    let end = start + line[start..].find('"')?;
    let v = line[start..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// « FR| 10 Couples Parfaits » → « 10 Couples Parfaits »
pub fn clean_name(name: &str) -> String {
    match name.split_once('|') {
        Some((pre, rest)) if pre.len() <= 6 && !pre.contains(' ') => rest.trim().to_string(),
        _ => name.trim().to_string(),
    }
}

/// « Série S01 E04 » / « Série S01E04 » → (« Série », 1, 4)
fn split_episode(name: &str) -> Option<(String, u16, u32)> {
    let bytes = name.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if (bytes[i] == b'S' || bytes[i] == b's') && (i == 0 || bytes[i - 1] == b' ') {
            let rest = &name[i + 1..];
            let sd: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if sd.is_empty() {
                continue;
            }
            let after = rest[sd.len()..].trim_start();
            let Some(after) = after.strip_prefix(['E', 'e']) else { continue };
            let ed: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if ed.is_empty() {
                continue;
            }
            let series = name[..i].trim().trim_end_matches(['-', '·', ':']).trim().to_string();
            return Some((series, sd.parse().ok()?, ed.parse().ok()?));
        }
    }
    None
}

#[derive(Default)]
struct M3uBuilder {
    cat: Catalog,
    groups: HashMap<Kind, Vec<String>>,
    group_seen: HashSet<(Kind, String)>,
    series_index: HashMap<(String, String), String>,
    pending: Option<(String, Option<String>, String)>,
    prefix: Option<String>,
    n: usize,
}

impl M3uBuilder {
    fn line(&mut self, line: &str) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF") {
            let name = rest.rsplit_once(',').map(|(_, n)| n.trim().to_string()).unwrap_or_default();
            let name = if name.is_empty() { attr(rest, "tvg-name").unwrap_or_default() } else { name };
            let group = attr(rest, "group-title").unwrap_or_else(|| "Sans catégorie".into());
            self.pending = Some((name, attr(rest, "tvg-logo"), group));
            return;
        }
        if line.is_empty() || line.starts_with('#') {
            return;
        }
        let Some((name, logo, group)) = self.pending.take() else { return };
        let lower = line.to_lowercase();
        let kind = if lower.contains("/movie/") {
            Kind::Movie
        } else if lower.contains("/series/") {
            Kind::Series
        } else {
            Kind::Live
        };
        if self.group_seen.insert((kind, group.clone())) {
            self.groups.entry(kind).or_default().push(group.clone());
        }
        self.n += 1;
        if kind == Kind::Series {
            if let Some((series, season, number)) = split_episode(&name) {
                let key = (group.clone(), series.clone());
                let id = match self.series_index.get(&key) {
                    Some(id) => id.clone(),
                    None => {
                        let id = format!("s{}", self.series_index.len() + 1);
                        self.series_index.insert(key, id.clone());
                        self.cat.entries.entry(Kind::Series).or_default().push(Entry {
                            id: id.clone(),
                            name: clean_name(&series),
                            logo,
                            category: group,
                            ..Default::default()
                        });
                        id
                    }
                };
                // Préfixe commun « http://hôte/series/user/pass/ » : seule la fin est gardée.
                let cut = line.rfind('/').map_or(0, |p| p + 1);
                let (pre, tail) = line.split_at(cut);
                let prefix = self.prefix.get_or_insert_with(|| pre.to_string());
                let url = if pre == prefix { tail.to_string() } else { line.to_string() };
                self.cat.episodes.entry(id).or_default().push(EpLite { season, number, url });
                return;
            }
        }
        self.cat.entries.entry(kind).or_default().push(Entry {
            id: format!("m{}", self.n),
            name: clean_name(&name),
            logo,
            category: group,
            url: Some(line.to_string()),
            ..Default::default()
        });
    }

    fn finish(mut self) -> Result<Catalog> {
        if self.n == 0 {
            bail!("playlist vide ou illisible");
        }
        for (kind, list) in self.groups {
            self.cat.categories.insert(kind, list.into_iter().map(|g| Category { id: g.clone(), name: g }).collect());
        }
        self.cat.ep_prefix = self.prefix.unwrap_or_default();
        Ok(self.cat)
    }
}

#[cfg(test)]
pub fn parse_m3u(text: &str) -> Result<Catalog> {
    let mut b = M3uBuilder::default();
    for l in text.lines() {
        b.line(l);
    }
    b.finish()
}

// ─────────────────────────────── Cache du catalogue ───────────────────────────────

/// Durée de validité du catalogue en cache.
pub const CACHE_HOURS: u64 = 24;

fn cache_path(profile_id: &str) -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(format!("noosfin/iptv-{profile_id}.json"))
}

/// Catalogue en cache s'il a moins de `CACHE_HOURS` heures.
pub fn load_cache(profile_id: &str) -> Option<Catalog> {
    let p = cache_path(profile_id);
    let age = std::fs::metadata(&p).ok()?.modified().ok()?.elapsed().ok()?;
    if age.as_secs() > CACHE_HOURS * 3600 {
        return None;
    }
    let data = std::fs::read(&p).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn save_cache(profile_id: &str, cat: &Catalog) {
    let p = cache_path(profile_id);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(data) = serde_json::to_vec(cat) {
        if std::fs::write(&p, data).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

pub fn drop_cache(profile_id: &str) {
    let _ = std::fs::remove_file(cache_path(profile_id));
}

/// Décodage matériel selon le GPU détecté (repli logiciel automatique de mpv).
pub fn hwdec_for_gpu() -> &'static str {
    let mut vendors = vec![];
    if let Ok(dir) = std::fs::read_dir("/sys/class/drm") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("card") && !name.contains('-') {
                if let Ok(v) = std::fs::read_to_string(e.path().join("device/vendor")) {
                    vendors.push(v.trim().to_lowercase());
                }
            }
        }
    }
    if vendors.iter().any(|v| v == "0x10de") {
        "nvdec,vaapi,auto-safe"
    } else if vendors.iter().any(|v| v == "0x1002" || v == "0x8086") {
        "vaapi,auto-safe"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u() {
        let t = "#EXTM3U\n#EXTINF:-1 tvg-id=\"tf1.fr\" tvg-logo=\"http://l/tf1.png\" group-title=\"France\",TF1 HD\nhttp://s/live/u/p/1.ts\n#EXTINF:-1 group-title=\"Films\",Dune\nhttp://s/movie/u/p/9.mkv\n";
        let c = parse_m3u(t).unwrap();
        assert_eq!(c.entries[&Kind::Live][0].name, "TF1 HD");
        assert_eq!(c.entries[&Kind::Live][0].logo.as_deref(), Some("http://l/tf1.png"));
        assert_eq!(c.entries[&Kind::Movie][0].category, "Films");
        assert_eq!(c.categories[&Kind::Live][0].name, "France");
    }

    #[test]
    fn m3u_series() {
        let t = "#EXTM3U\n#EXTINF:-1 tvg-logo=\"l\" group-title=\"[FR] SERIES\",FR| 10 Couples Parfaits S01 E02\nhttp://h:80/series/u/p/933810.mkv\n#EXTINF:-1 group-title=\"[FR] SERIES\",FR| 10 Couples Parfaits S01E01\nhttp://h:80/series/u/p/933809.mkv\n";
        let c = parse_m3u(t).unwrap();
        let s = &c.entries[&Kind::Series];
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "10 Couples Parfaits");
        let eps = c.series_episodes(&s[0].id);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].number, 1);
        assert_eq!(eps[0].url, "http://h:80/series/u/p/933809.mkv");
    }
}
