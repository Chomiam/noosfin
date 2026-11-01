//! Page de configuration servie sur le réseau local, ouverte depuis le téléphone (QR code).
//! Protégée par un jeton aléatoire dans l'URL ; les clés ne sont renvoyées que masquées.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Query as Q, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Engine;
use super::config::Sources;

const PAGE: &str = include_str!("../../assets/web/sources.html");
const MASK: &str = "••••";

pub type OnSave = Arc<dyn Fn(Sources) + Send + Sync>;

#[derive(Clone)]
struct Ctx {
    token: String,
    on_save: OnSave,
}

#[derive(Deserialize)]
struct Token {
    t: Option<String>,
}

pub struct WebServer {
    pub url: String,
}

/// Adresse IP du PC sur le réseau local (route par défaut, sans envoyer de paquet).
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.1.1:9").or_else(|_| s.connect("8.8.8.8:80")).ok()?;
    Some(s.local_addr().ok()?.ip())
}

fn mask(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{MASK}{tail}")
}

fn masked(s: &Sources) -> Sources {
    let mut s = s.clone();
    s.debrid.api_key = mask(&s.debrid.api_key);
    s.metadata.tmdb_key = mask(&s.metadata.tmdb_key);
    for i in s.indexers.iter_mut() {
        i.api_key = mask(&i.api_key);
    }
    s
}

/// Remet les vraies clés à la place des valeurs masquées non modifiées.
fn unmask(mut new: Sources, old: &Sources) -> Sources {
    if new.debrid.api_key.starts_with(MASK) {
        new.debrid.api_key = old.debrid.api_key.clone();
    }
    if new.metadata.tmdb_key.starts_with(MASK) {
        new.metadata.tmdb_key = old.metadata.tmdb_key.clone();
    }
    for ix in new.indexers.iter_mut() {
        if ix.api_key.starts_with(MASK) {
            if let Some(o) = old.indexers.iter().find(|o| o.url == ix.url || o.name == ix.name) {
                ix.api_key = o.api_key.clone();
            }
        }
    }
    new
}

fn authorized(ctx: &Ctx, t: &Token) -> bool {
    t.t.as_deref() == Some(ctx.token.as_str())
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Html("<h1>Lien expiré</h1><p>Scannez à nouveau le QR code affiché sur la TV.</p>")).into_response()
}

async fn page(State(ctx): State<Ctx>, Q(t): Q<Token>) -> Response {
    if !authorized(&ctx, &t) {
        return forbidden();
    }
    Html(PAGE).into_response()
}

async fn get_config(State(ctx): State<Ctx>, Q(t): Q<Token>) -> Response {
    if !authorized(&ctx, &t) {
        return forbidden();
    }
    Json(masked(&Sources::load())).into_response()
}

async fn save_config(State(ctx): State<Ctx>, Q(t): Q<Token>, Json(new): Json<Sources>) -> Response {
    if !authorized(&ctx, &t) {
        return forbidden();
    }
    let full = unmask(new, &Sources::load());
    match full.save() {
        Ok(()) => {
            (ctx.on_save)(full.clone());
            Json(json!({ "ok": true, "message": format!("Enregistré · {}", full.summary()) })).into_response()
        }
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

#[derive(Deserialize)]
struct TestReq {
    what: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: String,
    config: Sources,
}

async fn test(State(ctx): State<Ctx>, Q(t): Q<Token>, Json(req): Json<TestReq>) -> Response {
    if !authorized(&ctx, &t) {
        return forbidden();
    }
    let cfg = unmask(req.config, &Sources::load());
    let engine = Engine::new(cfg);
    let r = match req.what.as_str() {
        "debrid" => engine.test_debrid().await,
        "indexer" => engine.test_indexer(req.index).await,
        "tracker" => engine.test_tracker(&req.id).await,
        "tmdb" => match crate::tmdb::Tmdb::new(reqwest::Client::new(), &engine.cfg.metadata.tmdb_key, &engine.cfg.metadata.language) {
            Some(t) => t.check().await,
            None => Err(anyhow::anyhow!("aucune clé TMDB")),
        },
        _ => engine.test_network().await,
    };
    let v: Value = match r {
        Ok(m) => json!({ "ok": true, "message": m }),
        Err(e) => json!({ "ok": false, "message": format!("{e:#}") }),
    };
    Json(v).into_response()
}

/// Trackers publics intégrés (définitions embarquées).
async fn trackers(State(ctx): State<Ctx>, Q(t): Q<Token>) -> Response {
    if !authorized(&ctx, &t) {
        return forbidden();
    }
    let list: Vec<Value> = super::cardigann::all()
        .iter()
        .map(|d| {
            json!({
                "id": d.id, "name": d.name, "language": d.language, "description": d.description,
                "anime": d.anime(), "cloudflare": d.cloudflare(),
            })
        })
        .collect();
    Json(list).into_response()
}

/// Démarre le serveur (premier port libre à partir de 8787).
pub async fn start(on_save: OnSave) -> Result<WebServer> {
    let token: String = rand::rng().sample_iter(&rand::distr::Alphanumeric).take(24).map(char::from).collect();
    let ctx = Ctx { token: token.clone(), on_save };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/test", post(test))
        .route("/api/trackers", get(trackers))
        .with_state(ctx);
    let mut port = 8787;
    let listener = loop {
        match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(l) => break l,
            Err(e) if port < 8797 => {
                log::debug!("port {port} : {e}");
                port += 1;
            }
            Err(e) => return Err(e.into()),
        }
    };
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("serveur de configuration : {e}");
        }
    });
    let ip = lan_ip().map(|i| i.to_string()).unwrap_or_else(|| "127.0.0.1".into());
    Ok(WebServer { url: format!("http://{ip}:{port}/?t={token}") })
}
