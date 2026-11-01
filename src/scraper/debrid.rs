//! Débrideurs : AllDebrid, TorBox, Real-Debrid.
//! Vérification du cache, ajout (magnet ou .torrent des trackers privés), choix du fichier, lien direct.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use sha1::{Digest, Sha1};

use super::config::{Debrid, DebridKind};
use super::parse;

const AD: &str = "https://api.alldebrid.com";
const TB: &str = "https://api.torbox.app/v1/api";
const RD: &str = "https://api.real-debrid.com/rest/1.0";

const VIDEO_EXT: &[&str] = &["mkv", "mp4", "avi", "m4v", "ts", "webm", "mov", "wmv", "m2ts"];

/// Ce qu'on envoie au débrideur.
pub enum Payload {
    Magnet(String),
    Torrent(Vec<u8>),
}

/// Fichier d'un torrent chez le débrideur.
#[derive(Debug, Clone)]
pub struct DFile {
    pub name: String,
    pub size: u64,
    /// Lien (AllDebrid), identifiant de fichier (TorBox / Real-Debrid).
    pub link: String,
}

pub struct Resolved {
    pub url: String,
    pub filename: String,
}

#[derive(Debug)]
pub enum ResolveError {
    /// Pas en cache : téléchargement lancé (ou refusé) chez le débrideur.
    NotCached { started: bool },
    Other(anyhow::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotCached { started: true } => write!(f, "pas en cache : téléchargement lancé chez le débrideur"),
            ResolveError::NotCached { started: false } => write!(f, "pas en cache chez le débrideur"),
            ResolveError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for ResolveError {
    fn from(e: anyhow::Error) -> Self {
        ResolveError::Other(e)
    }
}
impl From<reqwest::Error> for ResolveError {
    fn from(e: reqwest::Error) -> Self {
        ResolveError::Other(e.into())
    }
}

/// Infohash (SHA-1 du dictionnaire `info`) d'un fichier .torrent.
pub fn infohash(torrent: &[u8]) -> Option<String> {
    let start = torrent.windows(6).position(|w| w == b"4:info")? + 6;
    let end = start + bencode_len(&torrent[start..])?;
    let digest = Sha1::digest(&torrent[start..end]);
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Longueur d'une valeur bencodée.
fn bencode_len(b: &[u8]) -> Option<usize> {
    match *b.first()? {
        b'i' => Some(b.iter().position(|&c| c == b'e')? + 1),
        b'l' | b'd' => {
            let mut i = 1;
            while *b.get(i)? != b'e' {
                i += bencode_len(&b[i..])?;
            }
            Some(i + 1)
        }
        b'0'..=b'9' => {
            let colon = b.iter().position(|&c| c == b':')?;
            let n: usize = std::str::from_utf8(&b[..colon]).ok()?.parse().ok()?;
            Some(colon + 1 + n)
        }
        _ => None,
    }
}

/// Choisit le fichier vidéo : l'épisode demandé dans un pack, sinon le plus gros.
pub fn pick_file(files: &[DFile], season: Option<u32>, episode: Option<u32>, anime: bool) -> Option<DFile> {
    let videos: Vec<&DFile> = files
        .iter()
        .filter(|f| VIDEO_EXT.iter().any(|e| f.name.to_lowercase().ends_with(&format!(".{e}"))))
        .filter(|f| !f.name.to_lowercase().contains("sample"))
        .collect();
    let pool = if videos.is_empty() { files.iter().collect() } else { videos };
    if let Some(e) = episode {
        let base = |f: &DFile| f.name.rsplit('/').next().unwrap_or(&f.name).to_string();
        if let Some(f) = pool.iter().filter(|f| {
            let r = parse::parse(&base(f));
            r.episodes.contains(&e) && (anime || season.is_none() || r.seasons.is_empty() || r.seasons.contains(&season.unwrap()))
        }).max_by_key(|f| f.size)
        {
            return Some((*f).clone());
        }
        if pool.len() > 1 {
            // Pack sans correspondance : mieux vaut échouer que lire le mauvais épisode.
            return None;
        }
    }
    pool.into_iter().max_by_key(|f| f.size).cloned()
}

pub struct DebridClient {
    http: Client,
    cfg: Debrid,
}

impl DebridClient {
    pub fn new(cfg: &Debrid) -> Self {
        let http = Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { http, cfg: cfg.clone() }
    }

    pub fn name(&self) -> &'static str {
        match self.cfg.service {
            DebridKind::AllDebrid => "AllDebrid",
            DebridKind::TorBox => "TorBox",
            DebridKind::RealDebrid => "Real-Debrid",
            DebridKind::None => "aucun",
        }
    }

    fn key(&self) -> &str {
        self.cfg.api_key.trim()
    }

    async fn ad(&self, path: &str, form: &[(&str, String)]) -> Result<Value> {
        let v: Value = self.http.post(format!("{AD}{path}")).bearer_auth(self.key()).form(form).send().await?.json().await?;
        if v["status"] != "success" {
            bail!("AllDebrid : {}", v["error"]["message"].as_str().or(v["error"]["code"].as_str()).unwrap_or("erreur"));
        }
        Ok(v["data"].clone())
    }

    /// Compte : (nom, premium jusqu'au…)
    pub async fn account(&self) -> Result<String> {
        match self.cfg.service {
            DebridKind::AllDebrid => {
                let v: Value = self.http.get(format!("{AD}/v4/user")).bearer_auth(self.key()).send().await?.json().await?;
                if v["status"] != "success" {
                    bail!("{}", v["error"]["message"].as_str().unwrap_or("clé refusée"));
                }
                let u = &v["data"]["user"];
                let until = u["premiumUntil"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.format("%d/%m/%Y").to_string());
                Ok(format!("{} · premium {}", u["username"].as_str().unwrap_or("?"), until.unwrap_or_else(|| "non".into())))
            }
            DebridKind::TorBox => {
                let v: Value = self.http.get(format!("{TB}/user/me")).bearer_auth(self.key()).send().await?.json().await?;
                if v["success"] != true {
                    bail!("{}", v["detail"].as_str().unwrap_or("clé refusée"));
                }
                Ok(format!("{} · plan {}", v["data"]["email"].as_str().unwrap_or("?"), v["data"]["plan"]))
            }
            DebridKind::RealDebrid => {
                let r = self.http.get(format!("{RD}/user")).bearer_auth(self.key()).send().await?;
                if !r.status().is_success() {
                    bail!("clé refusée (HTTP {})", r.status());
                }
                let v: Value = r.json().await?;
                Ok(format!("{} · {}", v["username"].as_str().unwrap_or("?"), v["type"].as_str().unwrap_or("?")))
            }
            DebridKind::None => bail!("aucun débrideur choisi"),
        }
    }

    /// Hashs déjà en cache. Real-Debrid ne le permet plus (aucun résultat).
    pub async fn cached(&self, hashes: &[String]) -> Result<HashMap<String, bool>> {
        let mut out = HashMap::new();
        if hashes.is_empty() {
            return Ok(out);
        }
        match self.cfg.service {
            DebridKind::AllDebrid => {
                // L'ajout renvoie `ready` pour les torrents déjà en cache ; les autres sont retirés
                // aussitôt (limite de 30 magnets actifs).
                let form: Vec<(&str, String)> = hashes.iter().map(|h| ("magnets[]", h.clone())).collect();
                let d = self.ad("/v4/magnet/upload", &form).await?;
                let mut drop = vec![];
                for m in d["magnets"].as_array().cloned().unwrap_or_default() {
                    let Some(h) = m["hash"].as_str() else { continue };
                    let ready = m["ready"].as_bool().unwrap_or(false);
                    out.insert(h.to_lowercase(), ready);
                    if !ready {
                        if let Some(id) = m["id"].as_i64() {
                            drop.push(id);
                        }
                    }
                }
                for id in drop {
                    let _ = self.ad("/v4/magnet/delete", &[("id", id.to_string())]).await;
                }
            }
            DebridKind::TorBox => {
                for chunk in hashes.chunks(50) {
                    let v: Value = self
                        .http
                        .get(format!("{TB}/torrents/checkcached"))
                        .bearer_auth(self.key())
                        .query(&[("hash", chunk.join(",").as_str()), ("format", "object")])
                        .send()
                        .await?
                        .json()
                        .await?;
                    for h in chunk {
                        out.insert(h.clone(), v["data"].get(h.as_str()).is_some_and(|x| !x.is_null()));
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// Ajoute le torrent, attend qu'il soit prêt (cache) et renvoie le lien direct du bon fichier.
    pub async fn resolve(&self, payload: Payload, season: Option<u32>, episode: Option<u32>, anime: bool) -> Result<Resolved, ResolveError> {
        match self.cfg.service {
            DebridKind::AllDebrid => self.resolve_ad(payload, season, episode, anime).await,
            DebridKind::TorBox => self.resolve_tb(payload, season, episode, anime).await,
            DebridKind::RealDebrid => self.resolve_rd(payload, season, episode, anime).await,
            DebridKind::None => Err(anyhow::anyhow!("aucun débrideur configuré").into()),
        }
    }

    async fn resolve_ad(&self, payload: Payload, season: Option<u32>, episode: Option<u32>, anime: bool) -> Result<Resolved, ResolveError> {
        let (id, ready) = match payload {
            Payload::Magnet(m) => {
                let d = self.ad("/v4/magnet/upload", &[("magnets[]", m)]).await?;
                let m = &d["magnets"][0];
                if !m["error"].is_null() {
                    return Err(anyhow::anyhow!("AllDebrid : {}", m["error"]["message"].as_str().unwrap_or("magnet refusé")).into());
                }
                (m["id"].as_i64().context("AllDebrid : id manquant")?, m["ready"].as_bool().unwrap_or(false))
            }
            Payload::Torrent(bytes) => {
                let part = Part::bytes(bytes).file_name("source.torrent").mime_str("application/x-bittorrent").map_err(anyhow::Error::from)?;
                let v: Value = self
                    .http
                    .post(format!("{AD}/v4/magnet/upload/file"))
                    .bearer_auth(self.key())
                    .multipart(Form::new().part("files[]", part))
                    .send()
                    .await?
                    .json()
                    .await?;
                if v["status"] != "success" {
                    return Err(anyhow::anyhow!("AllDebrid : {}", v["error"]["message"].as_str().unwrap_or("torrent refusé")).into());
                }
                let f = &v["data"]["files"][0];
                if !f["error"].is_null() {
                    return Err(anyhow::anyhow!("AllDebrid : {}", f["error"]["message"].as_str().unwrap_or("torrent refusé")).into());
                }
                (f["id"].as_i64().context("AllDebrid : id manquant")?, f["ready"].as_bool().unwrap_or(false))
            }
        };
        if !ready {
            if !self.cfg.download_uncached {
                let _ = self.ad("/v4/magnet/delete", &[("id", id.to_string())]).await;
            }
            return Err(ResolveError::NotCached { started: self.cfg.download_uncached });
        }
        let d = self.ad("/v4/magnet/files", &[("id[]", id.to_string())]).await?;
        let mut files = vec![];
        fn walk(v: &Value, prefix: &str, out: &mut Vec<DFile>) {
            for e in v.as_array().cloned().unwrap_or_default() {
                let name = format!("{prefix}{}", e["n"].as_str().unwrap_or_default());
                if let Some(children) = e.get("e") {
                    walk(children, &format!("{name}/"), out);
                } else if let Some(l) = e["l"].as_str() {
                    out.push(DFile { name, size: e["s"].as_u64().unwrap_or(0), link: l.to_string() });
                }
            }
        }
        walk(&d["magnets"][0]["files"], "", &mut files);
        let f = pick_file(&files, season, episode, anime).context("fichier de l'épisode introuvable dans le torrent")?;
        let u = self.ad("/v4/link/unlock", &[("link", f.link.clone())]).await?;
        if let Some(link) = u["link"].as_str().filter(|l| !l.is_empty()) {
            return Ok(Resolved { url: link.to_string(), filename: u["filename"].as_str().unwrap_or(&f.name).to_string() });
        }
        // Lien différé : attente de la génération.
        let gen_id = u["delayed"].as_i64().context("AllDebrid : lien indisponible")?;
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let d = self.ad("/v4/link/delayed", &[("id", gen_id.to_string())]).await?;
            if d["status"].as_i64() == Some(2) {
                return Ok(Resolved { url: d["link"].as_str().unwrap_or_default().to_string(), filename: f.name });
            }
        }
        Err(anyhow::anyhow!("AllDebrid : lien toujours en préparation").into())
    }

    async fn resolve_tb(&self, payload: Payload, season: Option<u32>, episode: Option<u32>, anime: bool) -> Result<Resolved, ResolveError> {
        let mut form = Form::new().text("allow_zip", "false");
        form = match payload {
            Payload::Magnet(m) => form.text("magnet", m),
            Payload::Torrent(b) => form.part("file", Part::bytes(b).file_name("source.torrent").mime_str("application/x-bittorrent").map_err(anyhow::Error::from)?),
        };
        if !self.cfg.download_uncached {
            form = form.text("add_only_if_cached", "true");
        }
        let v: Value = self.http.post(format!("{TB}/torrents/createtorrent")).bearer_auth(self.key()).multipart(form).send().await?.json().await?;
        if v["success"] != true {
            let detail = v["detail"].as_str().unwrap_or("refusé").to_string();
            if detail.to_lowercase().contains("cache") {
                return Err(ResolveError::NotCached { started: false });
            }
            return Err(anyhow::anyhow!("TorBox : {detail}").into());
        }
        let tid = v["data"]["torrent_id"].as_i64().context("TorBox : id manquant")?;
        for _ in 0..10 {
            let v: Value = self.http.get(format!("{TB}/torrents/mylist")).bearer_auth(self.key()).query(&[("id", tid.to_string())]).send().await?.json().await?;
            let t = &v["data"];
            if t["download_finished"] == true || t["download_present"] == true {
                let files: Vec<DFile> = t["files"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|f| DFile { name: f["name"].as_str().unwrap_or_default().to_string(), size: f["size"].as_u64().unwrap_or(0), link: f["id"].to_string() })
                    .collect();
                let f = pick_file(&files, season, episode, anime).context("fichier de l'épisode introuvable dans le torrent")?;
                let d: Value = self
                    .http
                    .get(format!("{TB}/torrents/requestdl"))
                    .query(&[("token", self.key()), ("torrent_id", &tid.to_string()), ("file_id", &f.link), ("redirect", "false")])
                    .send()
                    .await?
                    .json()
                    .await?;
                let url = d["data"].as_str().context("TorBox : lien indisponible")?;
                return Ok(Resolved { url: url.to_string(), filename: f.name });
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        Err(ResolveError::NotCached { started: true })
    }

    async fn resolve_rd(&self, payload: Payload, season: Option<u32>, episode: Option<u32>, anime: bool) -> Result<Resolved, ResolveError> {
        let v: Value = match payload {
            Payload::Magnet(m) => self.http.post(format!("{RD}/torrents/addMagnet")).bearer_auth(self.key()).form(&[("magnet", m)]).send().await?.json().await?,
            Payload::Torrent(b) => self.http.put(format!("{RD}/torrents/addTorrent")).bearer_auth(self.key()).body(b).send().await?.json().await?,
        };
        let id = v["id"].as_str().context("Real-Debrid : torrent refusé")?.to_string();
        let info = |id: String| {
            let http = self.http.clone();
            let key = self.key().to_string();
            async move { http.get(format!("{RD}/torrents/info/{id}")).bearer_auth(key).send().await?.json::<Value>().await }
        };
        let mut t = info(id.clone()).await?;
        if t["status"] == "waiting_files_selection" {
            let files: Vec<DFile> = t["files"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|f| DFile { name: f["path"].as_str().unwrap_or_default().to_string(), size: f["bytes"].as_u64().unwrap_or(0), link: f["id"].to_string() })
                .collect();
            let f = pick_file(&files, season, episode, anime).context("fichier de l'épisode introuvable dans le torrent")?;
            self.http.post(format!("{RD}/torrents/selectFiles/{id}")).bearer_auth(self.key()).form(&[("files", f.link.clone())]).send().await?;
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(1200)).await;
                t = info(id.clone()).await?;
                if t["status"] == "downloaded" {
                    break;
                }
            }
        }
        if t["status"] != "downloaded" {
            if !self.cfg.download_uncached {
                let _ = self.http.delete(format!("{RD}/torrents/delete/{id}")).bearer_auth(self.key()).send().await;
            }
            return Err(ResolveError::NotCached { started: self.cfg.download_uncached });
        }
        let link = t["links"][0].as_str().context("Real-Debrid : lien manquant")?;
        let u: Value = self.http.post(format!("{RD}/unrestrict/link")).bearer_auth(self.key()).form(&[("link", link)]).send().await?.json().await?;
        Ok(Resolved {
            url: u["download"].as_str().context("Real-Debrid : débridage refusé")?.to_string(),
            filename: u["filename"].as_str().unwrap_or_default().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_torrent() {
        // d8:announce3:url4:infod4:name1:aee
        let t = b"d8:announce3:url4:infod4:name1:aee";
        let h = infohash(t).unwrap();
        let expect: String = Sha1::digest(b"d4:name1:ae").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(h, expect);
    }

    #[test]
    fn pick() {
        let f = |n: &str, s| DFile { name: n.into(), size: s, link: n.into() };
        let files = vec![f("Show.S01E01.mkv", 10), f("Show.S01E02.mkv", 11), f("sample.mkv", 99), f("info.nfo", 1)];
        assert_eq!(pick_file(&files, Some(1), Some(2), false).unwrap().name, "Show.S01E02.mkv");
        assert!(pick_file(&files, Some(1), Some(5), false).is_none());
        assert_eq!(pick_file(&[f("Movie.mkv", 5)], None, None, false).unwrap().name, "Movie.mkv");
    }
}
