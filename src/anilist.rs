//! Métadonnées anime : AniList (catalogues, saisons) + Kitsu (identifiants & épisodes).
//! AIOStreams accepte les identifiants `kitsu:ID:EP`, qui donnent des fichiers par épisode.

use std::collections::HashMap;

use anyhow::{Result, bail};
use chrono::Datelike;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

const ANILIST: &str = "https://graphql.anilist.co";
const KITSU: &str = "https://kitsu.app/api/edge";

#[derive(Clone)]
pub struct AniList {
    http: Client,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Title {
    pub romaji: Option<String>,
    pub english: Option<String>,
    pub native: Option<String>,
    #[serde(rename = "userPreferred")]
    pub user_preferred: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Cover {
    #[serde(rename = "extraLarge")]
    pub extra_large: Option<String>,
    pub large: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Airing {
    pub episode: i64,
    #[serde(rename = "airingAt")]
    pub airing_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StreamingEpisode {
    pub title: Option<String>,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Studios {
    pub nodes: Vec<Studio>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Studio {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Anime {
    pub id: i64,
    #[serde(rename = "idMal")]
    pub id_mal: Option<i64>,
    pub title: Title,
    #[serde(rename = "coverImage")]
    pub cover: Cover,
    #[serde(rename = "bannerImage")]
    pub banner: Option<String>,
    pub description: Option<String>,
    pub episodes: Option<i64>,
    pub status: Option<String>,
    pub season: Option<String>,
    #[serde(rename = "seasonYear")]
    pub season_year: Option<i64>,
    pub format: Option<String>,
    #[serde(rename = "averageScore")]
    pub average_score: Option<i64>,
    pub genres: Vec<String>,
    #[serde(rename = "nextAiringEpisode")]
    pub next_airing: Option<Airing>,
    #[serde(rename = "streamingEpisodes")]
    pub streaming_episodes: Vec<StreamingEpisode>,
    pub studios: Studios,
    pub trailer: Option<Trailer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Trailer {
    pub id: Option<String>,
    pub site: Option<String>,
}

impl Anime {
    /// Bande-annonce YouTube (AniList ne référence parfois que Dailymotion : ignorée).
    pub fn trailer_url(&self) -> Option<String> {
        let t = self.trailer.as_ref()?;
        (t.site.as_deref() == Some("youtube")).then(|| format!("https://www.youtube.com/watch?v={}", t.id.clone().unwrap_or_default()))
    }
}

#[derive(Debug, Clone)]
pub struct SeasonEntry {
    pub id: i64,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct Mappings {
    pub tmdb: Option<i64>,
    pub tvdb: Option<i64>,
    pub imdb: Option<String>,
    pub movie: bool,
    /// Épisode AniList → (saison, épisode) TVDB.
    pub episodes: HashMap<i64, (i64, i64)>,
}

#[derive(Debug, Clone, Default)]
pub struct Episode {
    pub number: i64,
    pub title: String,
    pub thumbnail: Option<String>,
    pub overview: String,
}

const FIELDS: &str = "id idMal title { romaji english native userPreferred } coverImage { extraLarge large } \
    bannerImage description(asHtml: false) episodes status season seasonYear format averageScore genres \
    nextAiringEpisode { episode airingAt } studios(isMain: true) { nodes { name } }";

impl Anime {
    pub fn display_title(&self) -> String {
        self.title
            .user_preferred
            .clone()
            .or_else(|| self.title.romaji.clone())
            .or_else(|| self.title.english.clone())
            .unwrap_or_default()
    }

    pub fn cover_url(&self) -> Option<String> {
        self.cover.extra_large.clone().or_else(|| self.cover.large.clone())
    }

    /// Nombre d'épisodes disponibles (diffusés) à ce jour.
    pub fn aired_episodes(&self) -> i64 {
        match (&self.next_airing, self.episodes) {
            (Some(n), _) => (n.episode - 1).max(0),
            (None, Some(e)) => e,
            (None, None) => 0,
        }
    }

    pub fn clean_description(&self) -> String {
        let d = self.description.clone().unwrap_or_default();
        let mut out = String::with_capacity(d.len());
        let mut in_tag = false;
        for c in d.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&quot;", "\"").replace("&amp;", "&").replace("&#039;", "'").trim().to_string()
    }

    pub fn meta_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(f) = &self.format {
            parts.push(f.replace('_', " "));
        }
        if let (Some(s), Some(y)) = (&self.season, self.season_year) {
            parts.push(format!("{} {y}", season_fr(s)));
        }
        if let Some(e) = self.episodes {
            parts.push(format!("{e} ép."));
        }
        if let Some(s) = self.average_score {
            parts.push(format!("★ {:.1}", s as f64 / 10.0));
        }
        if let Some(st) = self.studios.nodes.first() {
            parts.push(st.name.clone());
        }
        if self.status.as_deref() == Some("RELEASING") {
            parts.push("En cours de diffusion".into());
        }
        parts.join("  ·  ")
    }
}

pub fn season_fr(s: &str) -> &'static str {
    match s {
        "WINTER" => "Hiver",
        "SPRING" => "Printemps",
        "SUMMER" => "Été",
        _ => "Automne",
    }
}

pub fn current_season() -> (&'static str, i32) {
    let now = chrono::Local::now();
    let s = match now.month() {
        1..=3 => "WINTER",
        4..=6 => "SPRING",
        7..=9 => "SUMMER",
        _ => "FALL",
    };
    (s, now.year())
}

impl AniList {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    async fn query(&self, q: &str, vars: serde_json::Value) -> Result<serde_json::Value> {
        let resp = self.http.post(ANILIST).json(&json!({ "query": q, "variables": vars })).send().await?;
        if !resp.status().is_success() {
            bail!("AniList: HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        if let Some(e) = v.get("errors") {
            bail!("AniList: {e}");
        }
        Ok(v["data"].clone())
    }

    async fn page(&self, args: &str, vars: serde_json::Value) -> Result<Vec<Anime>> {
        // AniList refuse les variables déclarées mais inutilisées.
        let decl: Vec<&str> = [("$season", "$season: MediaSeason"), ("$year", "$year: Int"), ("$search", "$search: String")]
            .iter()
            .filter(|(v, _)| args.contains(v))
            .map(|(_, d)| *d)
            .collect();
        let decl = if decl.is_empty() { String::new() } else { format!("({})", decl.join(", ")) };
        let q = format!(
            "query{decl} {{ Page(perPage: 30) {{ media({args}, type: ANIME, isAdult: false) {{ {FIELDS} }} }} }}"
        );
        let data = self.query(&q, vars).await?;
        Ok(serde_json::from_value(data["Page"]["media"].clone())?)
    }

    pub async fn trending(&self) -> Result<Vec<Anime>> {
        self.page("sort: TRENDING_DESC", json!({})).await
    }

    pub async fn seasonal(&self) -> Result<Vec<Anime>> {
        let (season, year) = current_season();
        self.page("season: $season, seasonYear: $year, sort: POPULARITY_DESC", json!({ "season": season, "year": year }))
            .await
    }

    pub async fn popular(&self) -> Result<Vec<Anime>> {
        self.page("sort: POPULARITY_DESC", json!({})).await
    }

    pub async fn search(&self, text: &str) -> Result<Vec<Anime>> {
        self.page("search: $search, sort: SEARCH_MATCH", json!({ "search": text })).await
    }

    pub async fn by_id(&self, id: i64) -> Result<Anime> {
        let q = format!(
            "query($id: Int) {{ Media(id: $id, type: ANIME) {{ {FIELDS} streamingEpisodes {{ title thumbnail }} trailer {{ id site }} }} }}"
        );
        let data = self.query(&q, json!({ "id": id })).await?;
        Ok(serde_json::from_value(data["Media"].clone())?)
    }

    /// Identifiant Kitsu correspondant à un identifiant MyAnimeList.
    pub async fn kitsu_id(&self, mal: i64) -> Result<Option<String>> {
        let url = format!(
            "{KITSU}/mappings?filter[externalSite]=myanimelist/anime&filter[externalId]={mal}&include=item"
        );
        let v: serde_json::Value = self.http.get(url).send().await?.json().await?;
        Ok(v["data"][0]["relationships"]["item"]["data"]["id"].as_str().map(String::from))
    }

    /// Saisons d'une même série : on suit les liens PREQUEL / SEQUEL d'AniList (chaque saison
    /// y est une fiche à part). Les films et OVA intercalés sont traversés mais pas listés.
    pub async fn franchise(&self, id: i64) -> Result<Vec<SeasonEntry>> {
        static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<i64, Vec<SeasonEntry>>>> = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(v) = cache.lock().ok().and_then(|c| c.get(&id).cloned()) {
            return Ok(v);
        }
        let q = "query($id: Int) { Media(id: $id, type: ANIME) { id format seasonYear startDate { year month } \
            title { romaji english userPreferred } \
            relations { edges { relationType node { id type format } } } } }";
        let mut nodes: HashMap<i64, serde_json::Value> = HashMap::new();
        let mut todo = vec![id];
        // Une requête par saison ; borne de sécurité pour les très longues franchises.
        while let Some(cur) = todo.pop() {
            if nodes.contains_key(&cur) || nodes.len() >= 16 {
                continue;
            }
            let m = self.query(q, json!({ "id": cur })).await?["Media"].clone();
            for e in m["relations"]["edges"].as_array().into_iter().flatten() {
                let rel = e["relationType"].as_str().unwrap_or("");
                let n = &e["node"];
                if (rel == "SEQUEL" || rel == "PREQUEL") && n["type"].as_str() == Some("ANIME") {
                    if let Some(nid) = n["id"].as_i64() {
                        todo.push(nid);
                    }
                }
            }
            nodes.insert(cur, m);
        }
        let mut list: Vec<&serde_json::Value> = nodes.values().filter(|m| matches!(m["format"].as_str(), Some("TV" | "TV_SHORT" | "ONA" | "MOVIE"))).collect();
        let date = |m: &serde_json::Value| (m["startDate"]["year"].as_i64().unwrap_or(9999), m["startDate"]["month"].as_i64().unwrap_or(12));
        list.sort_by_key(|m| date(m));
        // Numéro de saison TheTVDB (via ani.zip) : même découpage que TMDB / Plex / Jellyfin,
        // les « Part 2 » d'une même saison y portent le même numéro.
        let mut set = tokio::task::JoinSet::new();
        for (i, m) in list.iter().enumerate() {
            let (me, mid) = (self.clone(), m["id"].as_i64().unwrap_or(0));
            set.spawn(async move { (i, me.mappings(mid).await.ok().and_then(|x| x.episodes.get(&1).map(|e| e.0)).filter(|s| *s > 0)) });
        }
        let mut tvdb: HashMap<usize, i64> = HashMap::new();
        while let Some(Ok((i, s))) = set.join_next().await {
            if let Some(s) = s {
                tvdb.insert(i, s);
            }
        }
        let mut out = vec![];
        let mut n = 0;
        let mut parts: HashMap<i64, i64> = HashMap::new();
        for (i, m) in list.into_iter().enumerate() {
            let movie = m["format"].as_str() == Some("MOVIE");
            let year = m["startDate"]["year"].as_i64();
            let label = if movie {
                "Film".to_string()
            } else {
                n = tvdb.get(&i).copied().unwrap_or(n + 1);
                let part = parts.entry(n).or_insert(0);
                *part += 1;
                let base = if *part > 1 { format!("Saison {n} · partie {part}") } else { format!("Saison {n}") };
                match year {
                    Some(y) => format!("{base} · {y}"),
                    None => base,
                }
            };
            out.push(SeasonEntry { id: m["id"].as_i64().unwrap_or(0), label });
        }
        // Une seule saison et pas de film : rien à regrouper.
        if out.len() <= 1 {
            out.clear();
        }
        if let Ok(mut c) = cache.lock() {
            for e in &out {
                c.insert(e.id, out.clone());
            }
            c.insert(id, out.clone());
        }
        Ok(out)
    }

    /// Correspondances d'identifiants (TMDB, TVDB, IMDb) et numérotation des épisodes (ani.zip).
    pub async fn mappings(&self, id: i64) -> Result<Mappings> {
        let v: serde_json::Value = self.http.get(format!("https://api.ani.zip/mappings?anilist_id={id}")).send().await?.error_for_status()?.json().await?;
        let m = &v["mappings"];
        let mut episodes = HashMap::new();
        for (k, e) in v["episodes"].as_object().into_iter().flatten() {
            if let (Ok(n), Some(s), Some(ep)) = (k.parse::<i64>(), e["seasonNumber"].as_i64(), e["episodeNumber"].as_i64()) {
                episodes.insert(n, (s, ep));
            }
        }
        Ok(Mappings {
            tmdb: m["themoviedb_id"].as_i64().or_else(|| m["themoviedb_id"].as_str().and_then(|x| x.parse().ok())),
            tvdb: m["thetvdb_id"].as_i64(),
            imdb: m["imdb_id"].as_str().map(String::from),
            movie: m["type"].as_str() == Some("MOVIE"),
            episodes,
        })
    }

    /// Titres/vignettes des épisodes depuis Kitsu (jusqu'à `max` épisodes).
    pub async fn kitsu_episodes(&self, kitsu: &str, max: i64) -> Result<Vec<Episode>> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < max.min(500) {
            let url = format!("{KITSU}/anime/{kitsu}/episodes?page[limit]=20&page[offset]={offset}&sort=number");
            let v: serde_json::Value = self.http.get(url).send().await?.json().await?;
            let Some(list) = v["data"].as_array() else { break };
            if list.is_empty() {
                break;
            }
            for e in list {
                let a = &e["attributes"];
                let Some(n) = a["number"].as_i64() else { continue };
                let title = a["titles"]["en_us"]
                    .as_str()
                    .or(a["titles"]["en"].as_str())
                    .or(a["canonicalTitle"].as_str())
                    .or(a["titles"]["en_jp"].as_str())
                    .unwrap_or_default()
                    .to_string();
                out.push(Episode {
                    number: n,
                    title,
                    thumbnail: a["thumbnail"]["original"].as_str().map(String::from),
                    overview: a["synopsis"].as_str().unwrap_or_default().to_string(),
                });
            }
            offset += 20;
        }
        Ok(out)
    }
}
