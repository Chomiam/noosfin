//! Fournisseurs de résultats : Torznab (C411, Tr4ker, Jackett…), Prowlarr, YTS, EZTV,
//! The Pirate Bay (apibay), Nyaa et AnimeTosho.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;

use super::config::{Indexer, IndexerKind, Network};
use super::net::{self, unescape};
use super::parse;
use super::{Candidate, Query};

// ─────────────────────────────── Outils ───────────────────────────────

/// Élément RSS / Torznab, extrait sans dépendance XML (flux simples et bien formés).
#[derive(Debug, Default)]
struct Item {
    tags: HashMap<String, String>,
    attrs: HashMap<String, String>,
    enclosure: Option<(String, u64)>,
}

fn strip_cdata(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(s);
    unescape(s.trim())
}

fn attr_of(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let start = tag.find(&pat)? + pat.len();
    let end = start + tag[start..].find('"')?;
    Some(unescape(&tag[start..end]))
}

fn rss_items(xml: &str) -> Vec<Item> {
    let mut out = vec![];
    for chunk in xml.split("<item").skip(1) {
        let Some(body) = chunk.find('>').map(|i| &chunk[i + 1..]) else { continue };
        let body = body.split("</item>").next().unwrap_or(body);
        let mut it = Item::default();
        let mut rest = body;
        while let Some(lt) = rest.find('<') {
            rest = &rest[lt + 1..];
            let Some(gt) = rest.find('>') else { break };
            let tag = &rest[..gt];
            if tag.starts_with('/') || tag.starts_with('!') || tag.starts_with('?') {
                rest = &rest[gt + 1..];
                continue;
            }
            let name = tag.split_whitespace().next().unwrap_or_default().trim_end_matches('/').to_string();
            if name.ends_with(":attr") {
                if let (Some(n), Some(v)) = (attr_of(tag, "name"), attr_of(tag, "value")) {
                    it.attrs.insert(n.to_lowercase(), v);
                }
                rest = &rest[gt + 1..];
                continue;
            }
            if name == "enclosure" {
                if let Some(u) = attr_of(tag, "url") {
                    let len = attr_of(tag, "length").and_then(|l| l.parse().ok()).unwrap_or(0);
                    it.enclosure = Some((u, len));
                }
                rest = &rest[gt + 1..];
                continue;
            }
            if tag.ends_with('/') {
                rest = &rest[gt + 1..];
                continue;
            }
            let after = &rest[gt + 1..];
            let close = format!("</{name}>");
            if let Some(end) = after.find(&close) {
                it.tags.entry(name.to_lowercase()).or_insert_with(|| strip_cdata(&after[..end]));
                rest = &after[end + close.len()..];
            } else {
                rest = after;
            }
        }
        out.push(it);
    }
    out
}

pub fn hash_from_magnet(m: &str) -> Option<String> {
    let p = m.find("btih:")? + 5;
    let h: String = m[p..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    if h.len() == 40 { Some(h.to_lowercase()) } else if h.len() == 32 { base32_to_hex(&h) } else { None }
}

fn base32_to_hex(s: &str) -> Option<String> {
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = String::new();
    for c in s.chars() {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => return None,
        };
        bits = (bits << 5) | v;
        n += 5;
        while n >= 4 {
            n -= 4;
            out.push(std::char::from_digit(((bits >> n) & 0xF) as u32, 16)?);
        }
    }
    Some(out)
}

pub fn magnet(hash: &str, name: &str) -> String {
    const TRACKERS: &[&str] = &[
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://exodus.desync.com:6969/announce",
    ];
    let mut m = format!("magnet:?xt=urn:btih:{hash}&dn={}", urlencoding::encode(name));
    for t in TRACKERS {
        m.push_str("&tr=");
        m.push_str(&urlencoding::encode(t));
    }
    m
}

pub fn size_from_text(s: &str) -> u64 {
    let s = s.trim().replace(',', ".");
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().unwrap_or(0.0);
    let mul = match unit.trim().to_lowercase().as_str() {
        "kib" | "kb" | "ko" => 1024.0,
        "mib" | "mb" | "mo" => 1024.0 * 1024.0,
        "gib" | "gb" | "go" => 1024.0 * 1024.0 * 1024.0,
        "tib" | "tb" | "to" => 1024f64.powi(4),
        _ => 1.0,
    };
    (n * mul) as u64
}

pub fn candidate_from(title: String, source: &str, size: u64, seeders: Option<u32>, hash: Option<String>, private: bool) -> Candidate {
    Candidate {
        info: parse::parse(&title),
        title,
        source: source.to_string(),
        size,
        seeders,
        hash: hash.map(|h| h.to_lowercase()),
        magnet: None,
        torrent_url: None,
        private,
        cached: None,
        score: 0,
        page_def: None,
    }
}

/// Mots significatifs d'un titre, chiffres isolés compris (« Dune 2 »).
fn title_words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| (w.len() > 1 || w.chars().all(|c| c.is_ascii_digit())) && !w.is_empty())
        .filter(|w| !["the", "le", "la", "les", "de", "du", "des", "a", "an", "of", "no", "wa", "to"].contains(w))
        .map(String::from)
        .collect()
}

/// La release semble-t-elle correspondre à l'un des titres ?
pub fn title_ok(release: &str, titles: &[String]) -> bool {
    // Les tags de groupe en tête (« [Erai-raws] ») ne comptent pas.
    let mut rel = release.trim();
    while rel.starts_with(['[', '(']) {
        match rel.find([']', ')']) {
            Some(end) => rel = rel[end + 1..].trim_start(),
            None => break,
        }
    }
    let r = title_words(rel);
    titles.iter().any(|t| {
        let w = title_words(t);
        if w.is_empty() || r.len() < w.len() {
            return false;
        }
        // Le titre doit ouvrir le nom de la release…
        if r[..w.len()] == w[..] {
            // …sans être suivi d'un numéro de suite (« Lupin III », « Dune 2 »).
            let sequel = r.get(w.len()).is_some_and(|n| {
                ["ii", "iii", "iv", "vi", "vii", "viii", "ix"].contains(&n.as_str()) || (n.len() == 1 && n.chars().all(|c| ('2'..='9').contains(&c)))
            });
            return !sequel;
        }
        // Titres longs : les releases abrègent ou réordonnent souvent.
        w.len() >= 4 && w.iter().filter(|x| r.contains(x)).count() * 10 >= w.len() * 8 && r.iter().position(|x| *x == w[0]).is_some_and(|p| p <= 1)
    })
}

#[cfg(test)]
mod title_tests {
    use super::title_ok;

    #[test]
    fn titles() {
        let t = |r: &str, q: &str| title_ok(r, &[q.to_string()]);
        assert!(t("Lupin S01E01 Chapter 1 1080p NF WEB DL", "Lupin"));
        assert!(t("Lupin.2021.S01E01.1080p.WEB.x264", "Lupin"));
        assert!(!t("Lupin III - Prima Serie S01E01-23 (1972) WEBRip", "Lupin"));
        assert!(!t("Le avventure di Lupin III S01e01-23", "Lupin"));
        assert!(t("[Erai-raws] Sousou no Frieren 2nd Season - 05 [1080p]", "Sousou no Frieren"));
        assert!(t("Interstellar.2014.MULTI.VFF.2160p", "Interstellar"));
        assert!(t("The Dark Knight 2008 1080p", "The Dark Knight"));
        assert!(!t("Dune 2 2024 1080p", "Dune"));
    }
}

// ─────────────────────────────── Torznab / Prowlarr ───────────────────────────────

/// Respect du quota de chaque indexeur (ex. C411 : 15 requêtes / minute).
#[derive(Default)]
pub struct Throttle {
    last: Mutex<HashMap<String, Instant>>,
}

impl Throttle {
    pub async fn wait(&self, key: &str, min: Duration) {
        let mut g = self.last.lock().await;
        if let Some(t) = g.get(key) {
            let el = t.elapsed();
            if el < min {
                tokio::time::sleep(min - el).await;
            }
        }
        g.insert(key.to_string(), Instant::now());
    }
}

fn torznab_url(ix: &Indexer, params: &[(&str, String)]) -> String {
    let base = ix.url.trim_end_matches('/');
    let mut u = format!("{base}?apikey={}", urlencoding::encode(&ix.api_key));
    for (k, v) in params {
        if !v.is_empty() {
            u.push_str(&format!("&{k}={}", urlencoding::encode(v)));
        }
    }
    u
}

async fn torznab_request(http: &Client, net: &Network, ix: &Indexer, throttle: &Throttle, params: &[(&str, String)]) -> Result<Vec<Candidate>> {
    throttle.wait(&ix.name, Duration::from_millis(ix.min_interval_ms)).await;
    let xml = net::get_text(http, net, &torznab_url(ix, params)).await?;
    if let Some(p) = xml.find("<error") {
        let msg = attr_of(&xml[p..], "description").unwrap_or_else(|| "erreur".into());
        bail!("{} : {msg}", ix.name);
    }
    Ok(rss_items(&xml)
        .into_iter()
        .filter_map(|it| {
            let title = it.tags.get("title")?.clone();
            let size = it.attrs.get("size").and_then(|s| s.parse().ok()).or_else(|| it.tags.get("size").and_then(|s| s.parse().ok())).or(it.enclosure.as_ref().map(|e| e.1)).unwrap_or(0);
            let seeders = it.attrs.get("seeders").and_then(|s| s.parse().ok());
            let magnet_url = it.attrs.get("magneturl").cloned().or_else(|| it.tags.get("link").filter(|l| l.starts_with("magnet:")).cloned());
            let hash = it.attrs.get("infohash").cloned().or_else(|| magnet_url.as_deref().and_then(hash_from_magnet));
            let mut c = candidate_from(title, &ix.name, size, seeders, hash, ix.private);
            c.magnet = magnet_url;
            c.torrent_url = it.enclosure.map(|e| e.0).filter(|u| u.starts_with("http"));
            Some(c)
        })
        .collect())
}

pub async fn torznab(http: &Client, net: &Network, ix: &Indexer, throttle: &Throttle, q: &Query) -> Result<Vec<Candidate>> {
    let mut out = vec![];
    if !q.anime {
        if let Some(imdb) = &q.imdb {
            let params: Vec<(&str, String)> = match (q.season, q.episode) {
                (Some(s), Some(e)) => vec![("t", "tvsearch".into()), ("imdbid", imdb.clone()), ("season", s.to_string()), ("ep", e.to_string()), ("limit", "100".into())],
                _ => vec![("t", "movie".into()), ("imdbid", imdb.clone()), ("limit", "100".into())],
            };
            out = torznab_request(http, net, ix, throttle, &params).await?;
        }
    }
    if out.is_empty() {
        // Recherche textuelle (anime, ou indexeur sans identifiant IMDb).
        // Anime : titre seul (les trackers FR nomment « S01E05 », Nyaa « Titre - 05 ») puis filtre ;
        // titre anglais en second essai si le romaji ne donne presque rien.
        let texts: Vec<String> = if q.anime { q.anime_titles().into_iter().map(|t| super::strip_season(&t)).collect() } else { vec![q.text_query()] };
        let mut seen = std::collections::HashSet::new();
        for text in texts.into_iter().filter(|t| seen.insert(t.to_lowercase())).take(2) {
            let mut list = torznab_request(http, net, ix, throttle, &[("t", "search".into()), ("q", text), ("limit", "100".into())]).await?;
            list.retain(|c| title_ok(&c.title, &q.titles()));
            out.extend(list);
            if out.len() >= 5 {
                break;
            }
        }
    }
    Ok(out)
}

pub async fn indexer(http: &Client, net: &Network, ix: &Indexer, throttle: &Throttle, q: &Query) -> Result<Vec<Candidate>> {
    match ix.kind {
        IndexerKind::Torznab => torznab(http, net, ix, throttle, q).await,
        IndexerKind::Prowlarr => Ok(vec![]),
    }
}

/// Test de connexion (page de configuration).
pub async fn test_indexer(http: &Client, net: &Network, ix: &Indexer) -> Result<String> {
    match ix.kind {
        IndexerKind::Torznab => {
            let xml = net::get_text(http, net, &torznab_url(ix, &[("t", "caps".into())])).await?;
            if let Some(p) = xml.find("<error") {
                bail!("{}", attr_of(&xml[p..], "description").unwrap_or_else(|| "erreur".into()));
            }
            if !xml.contains("<caps") {
                bail!("réponse inattendue (pas de <caps>)");
            }
            Ok(attr_of(&xml[xml.find("<server").unwrap_or(0)..], "title").unwrap_or_else(|| "Torznab OK".into()))
        }
        IndexerKind::Prowlarr => bail!("Prowlarr n'est plus utilisé : les trackers sont intégrés à l'application"),
    }
}

// ─────────────────────────────── Sources publiques ───────────────────────────────

pub async fn yts(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let Some(imdb) = &q.imdb else { return Ok(vec![]) };
    if q.season.is_some() || q.anime {
        return Ok(vec![]);
    }
    // YTS change souvent de domaine : API officielle puis miroir.
    let mut body = String::new();
    for base in ["https://movies-api.accel.li/api/v2", "https://yts.gg/api/v2"] {
        if let Ok(b) = net::get_text(http, net, &format!("{base}/list_movies.json?query_term={imdb}")).await {
            body = b;
            break;
        }
    }
    if body.is_empty() {
        bail!("API injoignable");
    }
    let v: Value = serde_json::from_str(&body)?;
    let mut out = vec![];
    for m in v["data"]["movies"].as_array().cloned().unwrap_or_default() {
        let name = format!("{} {}", m["title"].as_str().unwrap_or_default(), m["year"].as_i64().unwrap_or(0));
        for t in m["torrents"].as_array().cloned().unwrap_or_default() {
            let title = format!(
                "{name} {} {} {} VO-YTS",
                t["quality"].as_str().unwrap_or_default(),
                t["type"].as_str().unwrap_or_default().replace("bluray", "BluRay").replace("web", "WEBRip"),
                t["video_codec"].as_str().unwrap_or_default()
            );
            let hash = t["hash"].as_str().map(String::from);
            out.push(candidate_from(title, "YTS", t["size_bytes"].as_u64().unwrap_or(0), t["seeds"].as_u64().map(|s| s as u32), hash, false));
        }
    }
    Ok(out)
}

pub async fn eztv(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let (Some(imdb), Some(s), Some(e)) = (&q.imdb, q.season, q.episode) else { return Ok(vec![]) };
    if q.anime {
        return Ok(vec![]);
    }
    let digits = imdb.trim_start_matches("tt");
    let body = net::get_text(http, net, &format!("https://eztvx.to/api/get-torrents?imdb_id={digits}&limit=100")).await?;
    let v: Value = serde_json::from_str(&body)?;
    Ok(v["torrents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|t| t["season"].as_str().and_then(|x| x.parse::<u32>().ok()) == Some(s) && t["episode"].as_str().and_then(|x| x.parse::<u32>().ok()) == Some(e))
        .map(|t| {
            let mut c = candidate_from(
                t["title"].as_str().unwrap_or_default().to_string(),
                "EZTV",
                t["size_bytes"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0),
                t["seeds"].as_u64().map(|x| x as u32),
                t["hash"].as_str().map(String::from),
                false,
            );
            c.magnet = t["magnet_url"].as_str().map(String::from);
            c
        })
        .collect())
}

pub async fn tpb(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let query = match (&q.imdb, q.season, q.episode) {
        (Some(i), None, None) if !q.anime => i.clone(),
        _ => q.text_query(),
    };
    let body = net::get_text(http, net, &format!("https://apibay.org/q.php?q={}&cat=200", urlencoding::encode(&query))).await?;
    let v: Value = serde_json::from_str(&body)?;
    let mut out: Vec<Candidate> = v
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|t| t["id"].as_str() != Some("0"))
        .map(|t| {
            candidate_from(
                t["name"].as_str().unwrap_or_default().to_string(),
                "TPB",
                t["size"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0),
                t["seeders"].as_str().and_then(|x| x.parse().ok()),
                t["info_hash"].as_str().map(String::from),
                false,
            )
        })
        .collect();
    if query != q.imdb.clone().unwrap_or_default() {
        out.retain(|c| title_ok(&c.title, &q.titles()));
    }
    Ok(out)
}

pub async fn nyaa(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let mut out = vec![];
    for title in q.anime_titles() {
        let query = match q.episode {
            Some(e) => format!("{title} {e:02}"),
            None => title.clone(),
        };
        let url = format!("https://nyaa.si/?page=rss&q={}&c=1_0&f=0", urlencoding::encode(&query));
        let Ok(xml) = net::get_text(http, net, &url).await else { continue };
        for it in rss_items(&xml) {
            let Some(t) = it.tags.get("title").cloned() else { continue };
            let size = it.tags.get("nyaa:size").map(|s| size_from_text(s)).unwrap_or(0);
            let seeders = it.tags.get("nyaa:seeders").and_then(|s| s.parse().ok());
            let c = candidate_from(t, "Nyaa", size, seeders, it.tags.get("nyaa:infohash").cloned(), false);
            out.push(c);
        }
        if out.len() > 20 {
            break;
        }
    }
    out.retain(|c| title_ok(&c.title, &q.anime_titles()));
    Ok(out)
}

pub async fn animetosho(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let mut out = vec![];
    for title in q.anime_titles().into_iter().take(2) {
        let query = match q.episode {
            Some(e) => format!("{title} {e:02}"),
            None => title.clone(),
        };
        let url = format!("https://feed.animetosho.org/json?q={}", urlencoding::encode(&query));
        let Ok(body) = net::get_text(http, net, &url).await else { continue };
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        for t in v.as_array().cloned().unwrap_or_default() {
            let Some(hash) = t["info_hash"].as_str() else { continue };
            let mut c = candidate_from(
                t["title"].as_str().unwrap_or_default().to_string(),
                "AnimeTosho",
                t["total_size"].as_u64().unwrap_or(0),
                t["seeders"].as_u64().map(|s| s as u32),
                Some(hash.to_string()),
                false,
            );
            c.magnet = t["magnet_uri"].as_str().map(String::from);
            out.push(c);
        }
        if out.len() > 20 {
            break;
        }
    }
    out.retain(|c| title_ok(&c.title, &q.anime_titles()));
    Ok(out)
}

/// Knaben : méta-moteur public (1337x, TorrentGalaxy, LimeTorrents, RuTracker, TPB…).
pub async fn knaben(http: &Client, q: &Query) -> Result<Vec<Candidate>> {
    let titles = if q.anime { q.anime_titles() } else { q.titles() };
    let query = match (q.season, q.episode, q.anime) {
        (_, Some(e), true) => format!("{} {e:02}", titles[0]),
        (Some(s), Some(e), false) => format!("{} S{s:02}E{e:02}", titles[0]),
        _ => match q.year {
            Some(y) => format!("{} {y}", titles[0]),
            None => titles[0].clone(),
        },
    };
    let body = serde_json::json!({
        "search_type": "100%", "search_field": "title", "query": query, "order_by": "seeders",
        "order_direction": "desc", "size": 100, "hide_unsafe": true, "hide_xxx": true,
    });
    let v: Value = http.post("https://api.knaben.org/v1").json(&body).timeout(Duration::from_secs(20)).send().await?.error_for_status()?.json().await?;
    let mut out: Vec<Candidate> = v["hits"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|h| {
            let title = h["title"].as_str()?.to_string();
            let magnet = h["magnetUrl"].as_str().filter(|m| m.starts_with("magnet:")).map(String::from);
            let hash = h["hash"].as_str().map(String::from).or_else(|| magnet.as_deref().and_then(hash_from_magnet))?;
            let origin = h["tracker"].as_str().or(h["cachedOrigin"].as_str()).unwrap_or("").split(" (").next().unwrap_or("").to_string();
            let source = if origin.is_empty() { "Knaben".to_string() } else { format!("Knaben/{origin}") };
            let mut c = candidate_from(title, &source, h["bytes"].as_u64().unwrap_or(0), h["seeders"].as_u64().or(h["liveSeeders"].as_u64()).map(|s| s as u32), Some(hash), false);
            c.magnet = magnet;
            Some(c)
        })
        .collect();
    out.retain(|c| title_ok(&c.title, &titles));
    Ok(out)
}

/// Torrents-CSV : index public de torrents (JSON).
pub async fn torrentscsv(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let titles = if q.anime { q.anime_titles() } else { q.titles() };
    let query = match (q.season, q.episode, q.anime) {
        (_, Some(e), true) => format!("{} {e:02}", titles[0]),
        (Some(s), Some(e), false) => format!("{} S{s:02}E{e:02}", titles[0]),
        _ => titles[0].clone(),
    };
    let body = net::get_text(http, net, &format!("https://torrents-csv.com/service/search?q={}&size=100", urlencoding::encode(&query))).await?;
    let v: Value = serde_json::from_str(&body)?;
    let mut out: Vec<Candidate> = v["torrents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            Some(candidate_from(
                t["name"].as_str()?.to_string(),
                "Torrents-CSV",
                t["size_bytes"].as_u64().unwrap_or(0),
                t["seeders"].as_u64().map(|s| s as u32),
                t["infohash"].as_str().map(String::from),
                false,
            ))
        })
        .collect();
    out.retain(|c| title_ok(&c.title, &titles));
    Ok(out)
}

/// SubsPlease : sorties anime (une par résolution), magnets directs.
pub async fn subsplease(http: &Client, net: &Network, q: &Query) -> Result<Vec<Candidate>> {
    let mut out = vec![];
    for title in q.anime_titles().into_iter().take(2) {
        let url = format!("https://subsplease.org/api/?f=search&tz=UTC&s={}", urlencoding::encode(&super::strip_season(&title)));
        let Ok(body) = net::get_text(http, net, &url).await else { continue };
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        for (_, e) in v.as_object().into_iter().flatten() {
            let show = e["show"].as_str().unwrap_or_default();
            let ep = e["episode"].as_str().unwrap_or_default();
            for d in e["downloads"].as_array().into_iter().flatten() {
                let Some(m) = d["magnet"].as_str() else { continue };
                let res = d["res"].as_str().unwrap_or("");
                let name = if ep.contains('-') {
                    format!("[SubsPlease] {show} ({ep}) ({res}p) [Batch]")
                } else {
                    format!("[SubsPlease] {show} - {ep} ({res}p)")
                };
                let size = m.split("xl=").nth(1).and_then(|x| x.split('&').next()).and_then(|x| x.parse().ok()).unwrap_or(0);
                let mut c = candidate_from(name, "SubsPlease", size, None, hash_from_magnet(m), false);
                c.magnet = Some(m.to_string());
                out.push(c);
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    Ok(out)
}
