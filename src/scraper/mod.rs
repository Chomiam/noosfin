//! Moteur de sources intégré (remplace Torrentio / Lumio / StreamFusion) :
//! recherche sur les trackers privés (Torznab, Prowlarr) et publics, tri selon les
//! préférences, vérification du cache et débridage (AllDebrid, TorBox, Real-Debrid).

pub mod cardigann;
pub mod config;
pub mod debrid;
pub mod net;
pub mod parse;
pub mod providers;
pub mod rank;
pub mod web;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::sync::Mutex;

use config::Sources;
use debrid::{DebridClient, Payload, Resolved, ResolveError};
use providers::Throttle;

/// Ce que l'on cherche.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub title: String,
    pub alt_titles: Vec<String>,
    pub year: Option<i32>,
    pub imdb: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub anime: bool,
}

impl Query {
    pub fn titles(&self) -> Vec<String> {
        let mut v = vec![self.title.clone()];
        v.extend(self.alt_titles.iter().cloned());
        v.retain(|t| !t.trim().is_empty());
        v.dedup();
        v
    }

    /// Titres d'anime, plus une variante sans « Season 2 » / « 2nd Season » / « Part 2 » et le titre
    /// court avant les deux-points (« Frieren: Beyond Journey's End » → « Frieren »).
    pub fn anime_titles(&self) -> Vec<String> {
        let mut v = vec![];
        for t in self.titles() {
            let short = strip_season(&t);
            let head = short.split(':').next().unwrap_or(&short).trim().to_string();
            for x in [t.clone(), short, head] {
                if !x.is_empty() && !v.contains(&x) {
                    v.push(x);
                }
            }
        }
        v
    }

    pub fn text_query(&self) -> String {
        match (self.season, self.episode, self.anime) {
            (_, Some(e), true) => format!("{} {e:02}", strip_season(&self.title)),
            (Some(s), Some(e), false) => format!("{} S{s:02}E{e:02}", self.title),
            _ => match self.year {
                Some(y) => format!("{} {y}", self.title),
                None => self.title.clone(),
            },
        }
    }

    fn cache_key(&self) -> String {
        format!("{}|{:?}|{:?}|{:?}|{}", self.title, self.imdb, self.season, self.episode, self.anime)
    }
}

fn test_query() -> Query {
    Query { title: "Interstellar".into(), year: Some(2014), imdb: Some("tt0816692".into()), ..Default::default() }
}

pub fn strip_season(t: &str) -> String {
    let lower = t.to_lowercase();
    let mut out = t.to_string();
    for pat in [" season", " part", " saison", " cour"] {
        if let Some(p) = lower.find(pat) {
            out = t[..p].to_string();
            break;
        }
    }
    // « Titre 2nd Season » → « Titre 2nd » → « Titre » ; « Titre S2 » → « Titre ».
    let mut words: Vec<&str> = out.split_whitespace().collect();
    while let Some(last) = words.last() {
        let l = last.to_lowercase();
        let ordinal = ["st", "nd", "rd", "th"].iter().any(|s| l.strip_suffix(s).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())));
        let season = l.strip_prefix('s').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if (ordinal || season) && words.len() > 1 {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ").trim().trim_end_matches([':', '-']).trim().to_string()
}

#[cfg(test)]
mod season_strip_tests {
    use super::*;

    #[test]
    fn strip() {
        assert_eq!(strip_season("Sousou no Frieren 2nd Season"), "Sousou no Frieren");
        assert_eq!(strip_season("Kusuriya no Hitorigoto 3rd Season"), "Kusuriya no Hitorigoto");
        assert_eq!(strip_season("Black Clover S2"), "Black Clover");
        assert_eq!(strip_season("Shingeki no Kyojin: The Final Season Part 2"), "Shingeki no Kyojin: The Final");
        assert_eq!(strip_season("Frieren"), "Frieren");
        let q = Query { title: "Sousou no Frieren 2nd Season".into(), alt_titles: vec!["Frieren: Beyond Journey's End Season 2".into()], anime: true, ..Default::default() };
        assert!(q.anime_titles().contains(&"Frieren".to_string()));
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub title: String,
    /// Indexeur(s) d'origine.
    pub source: String,
    pub size: u64,
    pub seeders: Option<u32>,
    pub hash: Option<String>,
    pub magnet: Option<String>,
    pub torrent_url: Option<String>,
    pub private: bool,
    pub info: parse::Release,
    pub cached: Option<bool>,
    pub score: i64,
    /// Tracker embarqué dont la page de détail donne le magnet (`torrent_url` = page).
    pub page_def: Option<String>,
}

pub struct Engine {
    pub cfg: Sources,
    http: Client,
    throttle: Throttle,
    debrid: Option<DebridClient>,
    cache: Mutex<HashMap<String, (Instant, Vec<Candidate>)>>,
}

pub struct SearchResult {
    pub candidates: Vec<Candidate>,
    pub errors: Vec<String>,
}

impl Engine {
    pub fn new(cfg: Sources) -> Arc<Self> {
        Arc::new(Self {
            http: net::client(&cfg.network),
            throttle: Throttle::default(),
            debrid: cfg.has_debrid().then(|| DebridClient::new(&cfg.debrid)),
            cache: Mutex::new(HashMap::new()),
            cfg,
        })
    }

    /// Le moteur est utilisable dès qu'un débrideur est configuré.
    pub fn enabled(&self) -> bool {
        self.debrid.is_some()
    }

    pub fn debrid_name(&self) -> &'static str {
        self.debrid.as_ref().map_or("—", |d| d.name())
    }

    pub async fn search(self: &Arc<Self>, q: &Query) -> SearchResult {
        let key = q.cache_key();
        if let Some((t, list)) = self.cache.lock().await.get(&key) {
            if t.elapsed() < Duration::from_secs(20 * 60) {
                return SearchResult { candidates: list.clone(), errors: vec![] };
            }
        }
        log::info!("recherche : {q:?}");
        let mut set = tokio::task::JoinSet::new();
        let limit = Duration::from_secs(self.cfg.network.timeout_secs.max(5) + 25);
        macro_rules! job {
            ($name:expr, $fut:expr) => {{
                let name: String = $name;
                let fut = $fut;
                set.spawn(async move { (name, tokio::time::timeout(limit, fut).await) });
            }};
        }
        for ix in self.cfg.indexers.iter().filter(|i| i.enabled && (!q.anime || i.anime)) {
            let (me, ix, q) = (self.clone(), ix.clone(), q.clone());
            job!(ix.name.clone(), async move { providers::indexer(&me.http, &me.cfg.network, &ix, &me.throttle, &q).await });
        }
        let p = &self.cfg.public;
        let public: [(bool, &str, u8); 8] = [
            (p.yts, "YTS", 0),
            (p.eztv, "EZTV", 1),
            (p.tpb, "TPB", 2),
            (p.nyaa && q.anime, "Nyaa", 3),
            (p.knaben, "Knaben", 5),
            (p.torrentscsv, "Torrents-CSV", 6),
            (p.subsplease && q.anime, "SubsPlease", 7),
            (false, "AnimeTosho", 4),
        ];
        for (on, name, which) in public {
            if !on {
                continue;
            }
            let (me, q) = (self.clone(), q.clone());
            job!(name.to_string(), async move {
                let (h, n) = (&me.http, &me.cfg.network);
                match which {
                    0 => providers::yts(h, n, &q).await,
                    1 => providers::eztv(h, n, &q).await,
                    2 => providers::tpb(h, n, &q).await,
                    3 => providers::nyaa(h, n, &q).await,
                    5 => providers::knaben(h, &q).await,
                    6 => providers::torrentscsv(h, n, &q).await,
                    7 => providers::subsplease(h, n, &q).await,
                    _ => providers::animetosho(h, n, &q).await,
                }
            });
        }
        // Trackers intégrés (définitions Cardigann embarquées).
        for id in &p.trackers {
            let Some(def) = cardigann::get(id) else { continue };
            if q.anime != def.anime() && def.anime() {
                continue;
            }
            let (me, q) = (self.clone(), q.clone());
            // Sites publics souvent lents : on ne les attend pas au-delà du délai réglé.
            let soft = Duration::from_secs(me.cfg.network.timeout_secs.max(5));
            job!(def.name.clone(), async move {
                tokio::time::timeout(soft, cardigann::search(&me.http, &me.cfg.network, def, &me.throttle, &q))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("trop lent, ignoré")))
            });
        }

        let mut all: Vec<Candidate> = vec![];
        let mut errors = vec![];
        while let Some(r) = set.join_next().await {
            match r {
                Ok((name, Ok(Ok(list)))) => {
                    log::info!("source {name} : {} résultat(s)", list.len());
                    all.extend(list)
                }
                Ok((name, Ok(Err(e)))) => {
                    log::warn!("source {name} : {e:#}");
                    errors.push(format!("{name} : {e:#}"));
                }
                Ok((name, Err(_))) => errors.push(format!("{name} : délai dépassé")),
                Err(e) => errors.push(format!("tâche : {e}")),
            }
        }

        // Déduplication par infohash (on garde la version d'un tracker privé, qui a le .torrent).
        let mut by_hash: HashMap<String, usize> = HashMap::new();
        let mut list: Vec<Candidate> = vec![];
        for c in all {
            match c.hash.clone().and_then(|h| by_hash.get(&h).copied()) {
                Some(i) => {
                    let existing = &mut list[i];
                    if !existing.source.contains(&c.source) {
                        existing.source = format!("{} + {}", existing.source, c.source);
                    }
                    if c.private && !existing.private {
                        existing.private = true;
                        existing.torrent_url = c.torrent_url;
                    }
                    existing.seeders = existing.seeders.max(c.seeders);
                }
                None => {
                    if let Some(h) = &c.hash {
                        by_hash.insert(h.clone(), list.len());
                    }
                    list.push(c);
                }
            }
        }

        // Premier tri, puis vérification du cache sur les meilleurs résultats, puis tri final.
        let mut ranked = rank::rank(list, q, &self.cfg);
        let top = ranked.len().min(25);
        if ranked[..top].iter().any(|c| c.hash.is_none()) {
            self.hydrate(&mut ranked[..top]).await;
            let mut seen = std::collections::HashSet::new();
            ranked.retain(|c| c.hash.as_ref().is_none_or(|h| seen.insert(h.clone())));
        }
        if let Some(d) = &self.debrid {
            let hashes: Vec<String> = ranked.iter().filter_map(|c| c.hash.clone()).take(40).collect();
            match d.cached(&hashes).await {
                Ok(map) => {
                    for c in ranked.iter_mut() {
                        if let Some(h) = &c.hash {
                            if let Some(v) = map.get(h) {
                                c.cached = Some(*v);
                            }
                        }
                    }
                    ranked = rank::rank(ranked, q, &self.cfg);
                }
                Err(e) => errors.push(format!("{} : {e:#}", d.name())),
            }
        }
        self.cache.lock().await.insert(key, (Instant::now(), ranked.clone()));
        SearchResult { candidates: ranked, errors }
    }

    /// Résultats sans infohash (pages de détail, liens .torrent) : on récupère le magnet ou le
    /// .torrent pour connaître le hash, indispensable à la vérification du cache.
    async fn hydrate(&self, list: &mut [Candidate]) {
        let mut set = tokio::task::JoinSet::new();
        for (i, c) in list.iter().enumerate() {
            if c.hash.is_some() || c.private {
                continue;
            }
            let Some(url) = c.torrent_url.clone() else { continue };
            let (http, net, def) = (self.http.clone(), self.cfg.network.clone(), c.page_def.clone());
            set.spawn(async move {
                let link = match def {
                    Some(d) => cardigann::resolve_page(&http, &net, &d, &url).await.ok()?,
                    None => url,
                };
                if link.starts_with("magnet:") {
                    let h = providers::hash_from_magnet(&link)?;
                    return Some((i, h, Some(link), None));
                }
                let r = tokio::time::timeout(Duration::from_secs(15), http.get(&link).send()).await.ok()?.ok()?;
                // Certains sites redirigent le lien .torrent vers un magnet.
                if let Some(loc) = r.url().as_str().strip_prefix("magnet:") {
                    let m = format!("magnet:{loc}");
                    return Some((i, providers::hash_from_magnet(&m)?, Some(m), None));
                }
                let b = r.bytes().await.ok()?;
                (b.first() == Some(&b'd')).then_some(())?;
                Some((i, debrid::infohash(&b)?, None, Some(link)))
            });
        }
        while let Some(r) = set.join_next().await {
            if let Ok(Some((i, h, magnet, torrent))) = r {
                let c = &mut list[i];
                c.hash = Some(h);
                if magnet.is_some() {
                    c.magnet = magnet;
                }
                c.torrent_url = torrent.or(c.torrent_url.take().filter(|_| c.page_def.is_none()));
                c.page_def = None;
            }
        }
    }

    /// Lien direct lisible par mpv.
    pub async fn resolve(&self, c: &Candidate, q: &Query) -> Result<Resolved, ResolveError> {
        let d = self.debrid.as_ref().context("aucun débrideur configuré")?;
        let mut hash = c.hash.clone();
        let mut c = c.clone();
        if let (Some(def), Some(url)) = (c.page_def.take(), c.torrent_url.take()) {
            let link = cardigann::resolve_page(&self.http, &self.cfg.network, &def, &url).await?;
            if link.starts_with("magnet:") {
                c.magnet = Some(link);
            } else {
                c.torrent_url = Some(link);
            }
        }
        let c = &c;
        let payload = match (&c.torrent_url, c.private, &c.magnet, &c.hash) {
            // Tracker privé : le .torrent contient la passkey, indispensable au débrideur.
            (Some(url), true, _, _) => Payload::Torrent(self.fetch_torrent(url).await?),
            (_, _, Some(m), _) => Payload::Magnet(m.clone()),
            (_, _, None, Some(h)) => Payload::Magnet(providers::magnet(h, &c.title)),
            (Some(url), false, None, None) => Payload::Torrent(self.fetch_torrent(url).await?),
            _ => return Err(anyhow::anyhow!("source sans magnet ni .torrent").into()),
        };
        if let (None, Payload::Torrent(bytes)) = (&hash, &payload) {
            hash = debrid::infohash(bytes);
        }
        let r = d.resolve(payload, q.season, q.episode, q.anime).await;
        if r.is_ok() {
            // Le résultat devient « en cache » pour les prochaines recherches.
            let key = q.cache_key();
            if let Some((_, list)) = self.cache.lock().await.get_mut(&key) {
                for x in list.iter_mut().filter(|x| (x.hash.is_some() && x.hash == hash) || (x.title == c.title && x.source == c.source)) {
                    x.cached = Some(true);
                    x.hash = x.hash.clone().or(hash.clone());
                }
            }
        }
        r
    }

    async fn fetch_torrent(&self, url: &str) -> Result<Vec<u8>> {
        // Le lien .torrent d'un tracker privé contient la passkey : jamais dans les erreurs.
        let r = self.http.get(url).send().await.and_then(|r| r.error_for_status()).map_err(|e| e.without_url())?;
        let b = r.bytes().await.map_err(|e| e.without_url())?.to_vec();
        if b.first() != Some(&b'd') {
            anyhow::bail!("le tracker n'a pas renvoyé de fichier .torrent");
        }
        Ok(b)
    }

    // ─────────────── Tests (page de configuration) ───────────────

    pub async fn test_debrid(&self) -> Result<String> {
        self.debrid.as_ref().context("aucun débrideur configuré")?.account().await
    }

    pub async fn test_indexer(&self, i: usize) -> Result<String> {
        let ix = self.cfg.indexers.get(i).context("indexeur inconnu")?;
        providers::test_indexer(&self.http, &self.cfg.network, ix).await
    }

    pub async fn test_tracker(&self, id: &str) -> Result<String> {
        match id {
            "knaben" => providers::knaben(&self.http, &test_query()).await.map(|v| format!("{} résultat(s)", v.len())),
            "torrentscsv" => providers::torrentscsv(&self.http, &self.cfg.network, &test_query()).await.map(|v| format!("{} résultat(s)", v.len())),
            "subsplease" => {
                let q = Query { title: "Frieren".into(), anime: true, ..Default::default() };
                providers::subsplease(&self.http, &self.cfg.network, &q).await.map(|v| format!("{} résultat(s)", v.len()))
            }
            "yts" => providers::yts(&self.http, &self.cfg.network, &test_query()).await.map(|v| format!("{} résultat(s)", v.len())),
            "tpb" => providers::tpb(&self.http, &self.cfg.network, &test_query()).await.map(|v| format!("{} résultat(s)", v.len())),
            _ => cardigann::test(&self.http, &self.cfg.network, cardigann::get(id).context("tracker inconnu")?).await,
        }
    }

    pub async fn test_network(&self) -> Result<String> {
        let mut parts = vec![];
        // Domaine souvent bloqué par les DNS des FAI : sert de test au DoH.
        match self.http.get("https://eztvx.to/api/get-torrents?limit=1").send().await {
            Ok(r) => parts.push(format!("DNS : eztvx.to joignable (HTTP {})", r.status().as_u16())),
            Err(e) => parts.push(format!("DNS : eztvx.to injoignable ({e})")),
        }
        if !self.cfg.network.flaresolverr.is_empty() {
            match net::check_flaresolverr(&self.http, &self.cfg.network.flaresolverr).await {
                Ok(v) => parts.push(format!("FlareSolverr {v} OK")),
                Err(e) => parts.push(format!("FlareSolverr injoignable ({e})")),
            }
        }
        Ok(parts.join(" · "))
    }
}
