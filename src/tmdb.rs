//! TMDB : catalogues (tendances, populaires…), recherche et fiches en français.
//! Renvoie les mêmes structures que le protocole Stremio (`aio::Meta`) pour que l'interface
//! n'ait pas à distinguer la provenance.

use anyhow::{Result, bail};
use reqwest::Client;
use serde_json::Value;

use crate::aio::{CatalogDef, ExtraDef, Manifest, Meta, Video};

const API: &str = "https://api.themoviedb.org/3";
const IMG: &str = "https://image.tmdb.org/t/p";

#[derive(Clone)]
pub struct Tmdb {
    http: Client,
    key: String,
    lang: String,
}

/// Catalogues proposés : (type, identifiant, nom, chemin de l'API).
const CATALOGS: &[(&str, &str, &str, &str)] = &[
    ("movie", "tmdb.trending", "Tendances", "/trending/movie/week"),
    ("movie", "tmdb.now_playing", "Au cinéma", "/movie/now_playing"),
    ("movie", "tmdb.popular", "Populaires", "/movie/popular"),
    ("movie", "tmdb.top_rated", "Les mieux notés", "/movie/top_rated"),
    ("series", "tmdb.trending", "Tendances", "/trending/tv/week"),
    ("series", "tmdb.on_the_air", "En cours de diffusion", "/tv/on_the_air"),
    ("series", "tmdb.popular", "Populaires", "/tv/popular"),
    ("series", "tmdb.top_rated", "Les mieux notées", "/tv/top_rated"),
];

impl Tmdb {
    pub fn new(http: Client, key: &str, lang: &str) -> Option<Self> {
        let key = key.trim();
        (!key.is_empty()).then(|| Self { http, key: key.to_string(), lang: if lang.is_empty() { "fr-FR".into() } else { lang.to_string() } })
    }

    async fn get(&self, path: &str, extra: &[(&str, &str)]) -> Result<Value> {
        let mut q: Vec<(&str, &str)> = vec![("api_key", &self.key), ("language", &self.lang)];
        q.extend_from_slice(extra);
        let r = self.http.get(format!("{API}{path}")).query(&q).send().await?;
        let status = r.status();
        if status.as_u16() == 401 {
            bail!("clé TMDB refusée");
        }
        if !status.is_success() {
            bail!("TMDB {path} : HTTP {status}");
        }
        Ok(r.json().await?)
    }

    pub async fn check(&self) -> Result<String> {
        let v = self.get("/configuration", &[]).await?;
        let _ = v;
        Ok("Clé TMDB valide".into())
    }

    pub fn manifest(&self) -> Manifest {
        let mut catalogs: Vec<CatalogDef> = CATALOGS
            .iter()
            .map(|(kind, id, name, _)| CatalogDef { kind: kind.to_string(), id: id.to_string(), name: name.to_string(), extra: vec![] })
            .collect();
        for kind in ["movie", "series"] {
            catalogs.push(CatalogDef { kind: kind.into(), id: "tmdb.search".into(), name: "Recherche".into(), extra: vec![ExtraDef { name: "search".into() }] });
        }
        Manifest { name: "TMDB".into(), version: String::new(), catalogs }
    }

    pub async fn catalog(&self, kind: &str, id: &str, search: Option<&str>) -> Result<Vec<Meta>> {
        let tv = kind == "series";
        let v = match search {
            Some(q) => self.get(if tv { "/search/tv" } else { "/search/movie" }, &[("query", q)]).await?,
            None => {
                let Some((_, _, _, path)) = CATALOGS.iter().find(|c| c.0 == kind && c.1 == id) else { bail!("catalogue TMDB inconnu : {id}") };
                self.get(path, &[]).await?
            }
        };
        Ok(v["results"].as_array().cloned().unwrap_or_default().iter().map(|r| preview(r, tv)).collect())
    }

    /// `id` : `tmdb:123` ou un identifiant IMDb (`tt123`).
    pub async fn meta(&self, kind: &str, id: &str) -> Result<Meta> {
        let tv = kind == "series";
        let tmdb_id = match id.strip_prefix("tmdb:") {
            Some(t) => t.split(':').next().unwrap_or(t).to_string(),
            None if id.starts_with("tt") => {
                let imdb = id.split(':').next().unwrap_or(id);
                let v = self.get(&format!("/find/{imdb}"), &[("external_source", "imdb_id")]).await?;
                let list = if tv { &v["tv_results"] } else { &v["movie_results"] };
                match list[0]["id"].as_i64() {
                    Some(n) => n.to_string(),
                    None => bail!("{imdb} introuvable sur TMDB"),
                }
            }
            None => bail!("identifiant non géré par TMDB : {id}"),
        };
        let path = format!("/{}/{tmdb_id}", if tv { "tv" } else { "movie" });
        let v = self.get(&path, &[("append_to_response", "external_ids,translations")]).await?;
        let mut m = preview(&v, tv);
        // Titre anglais : c'est souvent celui des releases sur les trackers.
        let en = v["translations"]["translations"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["iso_639_1"].as_str() == Some("en"))
            .and_then(|t| t["data"][if tv { "name" } else { "title" }].as_str())
            .filter(|t| !t.is_empty() && *t != m.name)
            .map(String::from);
        if let Some(en) = en {
            m.alt_names.retain(|a| *a != en);
            m.alt_names.insert(0, en);
        }
        m.genres = v["genres"].as_array().into_iter().flatten().filter_map(|g| g["name"].as_str().map(String::from)).collect();
        m.imdb_id = v["external_ids"]["imdb_id"].as_str().or(v["imdb_id"].as_str()).filter(|s| !s.is_empty()).map(String::from);
        if let Some(rt) = v["runtime"].as_i64().filter(|r| *r > 0) {
            m.release_info = Some(format!("{} · {}h{:02}", m.release_info.clone().unwrap_or_default(), rt / 60, rt % 60));
        }
        if tv {
            m.videos = self.episodes(&tmdb_id, &v, m.imdb_id.as_deref()).await;
        }
        Ok(m)
    }

    async fn episodes(&self, tmdb_id: &str, show: &Value, imdb: Option<&str>) -> Vec<Video> {
        let seasons: Vec<i64> = show["seasons"].as_array().into_iter().flatten().filter_map(|s| s["season_number"].as_i64()).filter(|n| *n > 0).collect();
        let mut out = vec![];
        // 20 saisons maximum par requête (append_to_response).
        for chunk in seasons.chunks(20) {
            let append: Vec<String> = chunk.iter().map(|n| format!("season/{n}")).collect();
            let Ok(v) = self.get(&format!("/tv/{tmdb_id}"), &[("append_to_response", &append.join(","))]).await else { continue };
            for n in chunk {
                for e in v[format!("season/{n}")]["episodes"].as_array().into_iter().flatten() {
                    let ep = e["episode_number"].as_i64().unwrap_or(0);
                    let base = imdb.map(String::from).unwrap_or_else(|| format!("tmdb:{tmdb_id}"));
                    out.push(Video {
                        id: format!("{base}:{n}:{ep}"),
                        name: e["name"].as_str().map(String::from),
                        title: e["name"].as_str().map(String::from),
                        season: Some(*n),
                        episode: Some(ep),
                        thumbnail: e["still_path"].as_str().map(|p| format!("{IMG}/w300{p}")),
                        overview: e["overview"].as_str().filter(|s| !s.is_empty()).map(clean),
                        description: None,
                    });
                }
            }
        }
        out
    }
}

/// Espaces insécables (fines) absentes de la police de l'interface.
fn clean(s: &str) -> String {
    s.replace(['\u{202f}', '\u{a0}'], " ")
}

fn preview(r: &Value, tv: bool) -> Meta {
    let name = if tv { r["name"].as_str() } else { r["title"].as_str() }.unwrap_or_default().to_string();
    let original = if tv { r["original_name"].as_str() } else { r["original_title"].as_str() }.unwrap_or_default().to_string();
    let date = if tv { r["first_air_date"].as_str() } else { r["release_date"].as_str() }.unwrap_or_default();
    Meta {
        id: format!("tmdb:{}", r["id"].as_i64().unwrap_or(0)),
        kind: if tv { "series".into() } else { "movie".into() },
        poster: r["poster_path"].as_str().map(|p| format!("{IMG}/w342{p}")),
        background: r["backdrop_path"].as_str().map(|p| format!("{IMG}/w1280{p}")),
        description: r["overview"].as_str().filter(|s| !s.is_empty()).map(clean),
        release_info: date.get(..4).map(String::from),
        imdb_rating: r["vote_average"].as_f64().filter(|v| *v > 0.0).map(|v| format!("{v:.1}")),
        // Titre original seulement s'il est en alphabet latin (sinon inutile pour les trackers).
        alt_names: if !original.is_empty() && original != name && original.chars().all(|c| (c as u32) < 0x250) { vec![original] } else { vec![] },
        name,
        ..Default::default()
    }
}

/// Textes français d'un anime (fiche AniList → TMDB).
#[derive(Debug, Clone, Default)]
pub struct AnimeFr {
    pub overview: String,
    /// Épisode AniList → (titre, résumé, vignette).
    pub episodes: std::collections::HashMap<i64, (String, String, Option<String>)>,
}

/// TMDB met souvent « Épisode 3 » faute de vrai titre français.
fn placeholder(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n.strip_prefix("Épisode").or_else(|| n.strip_prefix("Episode")).is_some_and(|r| r.trim().chars().all(|c| c.is_ascii_digit()))
}

impl Tmdb {
    async fn find(&self, external: &str, source: &str, movie: bool) -> Option<i64> {
        let v = self.get(&format!("/find/{external}"), &[("external_source", source)]).await.ok()?;
        v[if movie { "movie_results" } else { "tv_results" }][0]["id"].as_i64()
    }

    /// Résumé et épisodes en français. `titles` : titres anglais / romaji, pour une recherche
    /// en dernier recours quand ani.zip ne connaît pas l'identifiant TMDB.
    pub async fn anime_fr(&self, map: &crate::anilist::Mappings, titles: &[String], year: Option<i64>, movie: bool) -> Result<Option<AnimeFr>> {
        let mut id = map.tmdb.filter(|_| movie == map.movie || !map.movie);
        if id.is_none() {
            if let Some(t) = map.tvdb {
                id = self.find(&t.to_string(), "tvdb_id", movie).await;
            }
        }
        if id.is_none() {
            if let Some(i) = &map.imdb {
                id = self.find(i, "imdb_id", movie).await;
            }
        }
        if id.is_none() {
            let y = year.map(|y| y.to_string()).unwrap_or_default();
            for t in titles.iter().filter(|t| !t.is_empty()).take(2) {
                let mut q = vec![("query", t.as_str())];
                if !y.is_empty() && movie {
                    q.push(("year", y.as_str()));
                }
                let Ok(v) = self.get(if movie { "/search/movie" } else { "/search/tv" }, &q).await else { continue };
                // Animation (genre 16) en priorité.
                let list = v["results"].as_array().cloned().unwrap_or_default();
                id = list.iter().find(|r| r["genre_ids"].as_array().is_some_and(|g| g.iter().any(|x| x.as_i64() == Some(16)))).and_then(|r| r["id"].as_i64());
                if id.is_some() {
                    break;
                }
            }
        }
        let Some(id) = id else { return Ok(None) };
        if movie {
            let v = self.get(&format!("/movie/{id}"), &[]).await?;
            let overview = clean(v["overview"].as_str().unwrap_or_default());
            return Ok((!overview.is_empty()).then_some(AnimeFr { overview, ..Default::default() }));
        }
        let show = self.get(&format!("/tv/{id}"), &[]).await?;
        let mut out = AnimeFr { overview: clean(show["overview"].as_str().unwrap_or_default()), ..Default::default() };
        let mut seasons: Vec<i64> = map.episodes.values().map(|(s, _)| *s).filter(|s| *s > 0).collect();
        seasons.sort();
        seasons.dedup();
        for s in seasons.iter().take(4) {
            let Ok(v) = self.get(&format!("/tv/{id}/season/{s}"), &[]).await else { continue };
            // Le résumé propre à la saison est plus juste que celui de la série.
            if let Some(o) = v["overview"].as_str().filter(|o| !o.is_empty()) {
                if seasons.len() == 1 {
                    out.overview = clean(o);
                }
            }
            let eps: std::collections::HashMap<i64, &Value> =
                v["episodes"].as_array().into_iter().flatten().filter_map(|e| Some((e["episode_number"].as_i64()?, e))).collect();
            for (n, (es, ee)) in &map.episodes {
                if es != s {
                    continue;
                }
                if let Some(e) = eps.get(ee) {
                    let name = e["name"].as_str().unwrap_or_default();
                    let ov = clean(e["overview"].as_str().unwrap_or_default());
                    let still = e["still_path"].as_str().map(|p| format!("{IMG}/w300{p}"));
                    out.episodes.insert(*n, (if placeholder(name) { String::new() } else { name.to_string() }, ov, still));
                }
            }
        }
        Ok((!out.overview.is_empty() || !out.episodes.is_empty()).then_some(out))
    }
}
