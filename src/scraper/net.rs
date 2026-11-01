//! Réseau des sources : DNS chiffré (DoH) contre les blocages des FAI, et FlareSolverr
//! pour les pages protégées par Cloudflare.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use reqwest::Client;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{Value, json};

use super::config::{Dns, Network};

pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

/// Résolveur DoH (format JSON de Cloudflare / Google / Quad9), avec cache.
struct DohResolver {
    endpoint: &'static str,
    http: Client,
    cache: Arc<Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>>,
}

impl DohResolver {
    async fn lookup(endpoint: &str, http: &Client, host: &str) -> Result<(Vec<IpAddr>, u64)> {
        let mut ips = vec![];
        let mut ttl = 300;
        for t in ["A", "AAAA"] {
            let v: Value = http
                .get(endpoint)
                .query(&[("name", host), ("type", t)])
                .header("accept", "application/dns-json")
                .send()
                .await?
                .json()
                .await?;
            for a in v["Answer"].as_array().cloned().unwrap_or_default() {
                // type 1 = A, 28 = AAAA (les CNAME sont suivis par le serveur)
                if matches!(a["type"].as_u64(), Some(1) | Some(28)) {
                    if let Some(ip) = a["data"].as_str().and_then(|d| d.parse().ok()) {
                        ips.push(ip);
                        ttl = ttl.min(a["TTL"].as_u64().unwrap_or(300));
                    }
                }
            }
            if !ips.is_empty() {
                break;
            }
        }
        if ips.is_empty() {
            bail!("DoH : aucune adresse pour {host}");
        }
        Ok((ips, ttl.max(60)))
    }
}

impl Resolve for DohResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        if let Some((ips, until)) = self.cache.lock().unwrap().get(&host).cloned() {
            if until > Instant::now() {
                return Box::pin(async move { Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs) });
            }
        }
        let (endpoint, http, cache) = (self.endpoint, self.http.clone(), self.cache.clone());
        Box::pin(async move {
            let res = match DohResolver::lookup(endpoint, &http, &host).await {
                Ok(r) => r,
                // Repli sur le DNS système (réseau local, DoH injoignable).
                Err(e) => {
                    log::debug!("{e:#}");
                    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.map(|a| a.ip()).collect();
                    (addrs, 60)
                }
            };
            cache.lock().unwrap().insert(host, (res.0.clone(), Instant::now() + Duration::from_secs(res.1)));
            Ok(Box::new(res.0.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

/// Client HTTP des sources (DoH selon la configuration).
pub fn client(net: &Network) -> Client {
    let endpoint = match net.dns {
        Dns::System => None,
        // Adresses IP directes : pas de résolution DNS nécessaire pour joindre le service DoH.
        Dns::Cloudflare => Some("https://1.1.1.1/dns-query"),
        Dns::Quad9 => Some("https://9.9.9.9:5053/dns-query"),
        Dns::Google => Some("https://8.8.8.8/resolve"),
    };
    let mut b = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(net.timeout_secs.max(5)));
    if let Some(endpoint) = endpoint {
        let http = Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default();
        b = b.dns_resolver(Arc::new(DohResolver { endpoint, http, cache: Arc::new(Mutex::new(HashMap::new())) }));
    }
    b.build().unwrap_or_default()
}

/// Page protégée par Cloudflare ?
pub fn is_challenge(status: u16, body: &str) -> bool {
    (status == 403 || status == 503 || status == 429)
        && (body.contains("cf-chl") || body.contains("Just a moment") || body.contains("challenge-platform") || body.contains("cloudflare"))
}

/// GET texte, avec passage par FlareSolverr si Cloudflare bloque.
pub async fn get_text(http: &Client, net: &Network, url: &str) -> Result<String> {
    let r = http.get(url).send().await;
    match r {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            if status < 400 {
                return Ok(body);
            }
            if is_challenge(status, &body) && !net.flaresolverr.is_empty() {
                return flaresolverr(http, &net.flaresolverr, url).await;
            }
            bail!("HTTP {status}");
        }
        Err(e) if !net.flaresolverr.is_empty() && (e.is_connect() || e.is_timeout()) => {
            // Blocage réseau côté PC : FlareSolverr (avec son propre DNS) fait la requête à notre place.
            log::debug!("{url} : {e} → FlareSolverr");
            flaresolverr(http, &net.flaresolverr, url).await
        }
        Err(e) => Err(e.into()),
    }
}

const LOCAL_URL: &str = "http://127.0.0.1:8191";
const CONTAINER: &str = "noosfin-flaresolverr";

/// URL de FlareSolverr ; en mode « local », démarre le conteneur podman au premier besoin.
pub async fn flaresolverr_base(base: &str) -> Result<String> {
    if base != super::config::LOCAL_FLARESOLVERR {
        return Ok(base.trim_end_matches('/').to_string());
    }
    static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _g = START.lock().await;
    let probe = Client::builder().timeout(Duration::from_secs(2)).build()?;
    if probe.get(LOCAL_URL).send().await.is_ok() {
        return Ok(LOCAL_URL.into());
    }
    let podman = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        async move { tokio::process::Command::new("podman").args(&args).output().await.map(|o| o.status.success()).unwrap_or(false) }
    };
    if !podman(&["start", CONTAINER]).await {
        log::info!("création du conteneur {CONTAINER}");
        // DNS public : le DNS du FAI bloque certains trackers.
        let ok = podman(&[
            "run", "-d", "--name", CONTAINER, "-p", "127.0.0.1:8191:8191", "--dns", "1.1.1.1", "-e", "LOG_LEVEL=warn",
            "ghcr.io/flaresolverr/flaresolverr:latest",
        ])
        .await;
        if !ok {
            bail!("impossible de lancer FlareSolverr (podman)");
        }
    }
    for _ in 0..60 {
        if probe.get(LOCAL_URL).send().await.is_ok() {
            return Ok(LOCAL_URL.into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("FlareSolverr local ne répond pas")
}

pub async fn flaresolverr(http: &Client, base: &str, url: &str) -> Result<String> {
    let base = flaresolverr_base(base).await?;
    let v: Value = http
        .post(format!("{base}/v1"))
        .timeout(Duration::from_secs(70))
        .json(&json!({ "cmd": "request.get", "url": url, "maxTimeout": 60000 }))
        .send()
        .await?
        .json()
        .await?;
    if v["status"] != "ok" {
        bail!("FlareSolverr : {}", v["message"].as_str().unwrap_or("échec"));
    }
    let html = v["solution"]["response"].as_str().unwrap_or_default().to_string();
    // Les API JSON sont renvoyées dans un <pre> par le navigateur de FlareSolverr.
    if let (Some(s), Some(e)) = (html.find("<pre"), html.rfind("</pre>")) {
        if let Some(gt) = html[s..e].find('>') {
            return Ok(unescape(&html[s + gt + 1..e]));
        }
    }
    Ok(html)
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

pub async fn check_flaresolverr(http: &Client, base: &str) -> Result<String> {
    let base = flaresolverr_base(base).await?;
    let v: Value = http.get(&base).timeout(Duration::from_secs(6)).send().await?.json().await?;
    Ok(v["version"].as_str().unwrap_or("?").to_string())
}
