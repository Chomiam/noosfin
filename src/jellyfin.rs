//! Client minimal de l'API Jellyfin (routes 10.9+ / 12.x).

use std::collections::HashMap;

use anyhow::{Result, bail};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const TICKS_PER_SEC: f64 = 10_000_000.0;

#[derive(Clone)]
pub struct Jellyfin {
    http: Client,
    pub base: String,
    pub token: String,
    pub user_id: String,
    device_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    pub playback_position_ticks: i64,
    pub played: bool,
    pub played_percentage: Option<f64>,
    pub unplayed_item_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub collection_type: Option<String>,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub index_number: Option<i64>,
    pub parent_index_number: Option<i64>,
    pub overview: Option<String>,
    pub production_year: Option<i64>,
    pub run_time_ticks: Option<i64>,
    pub community_rating: Option<f64>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub user_data: Option<UserData>,
    pub image_tags: HashMap<String, String>,
    pub backdrop_image_tags: Vec<String>,
    pub parent_backdrop_item_id: Option<String>,
    pub series_primary_image_tag: Option<String>,
    pub provider_ids: HashMap<String, String>,
    pub child_count: Option<i64>,
    pub media_streams: Vec<MediaStream>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResult {
    items: Vec<Item>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    #[serde(rename = "Type")]
    pub kind: String,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub display_title: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub video_range_type: Option<String>,
    pub channels: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    pub id: String,
    pub name: Option<String>,
    pub path: Option<String>,
    pub protocol: String,
    pub container: Option<String>,
    pub size: Option<i64>,
    pub bitrate: Option<i64>,
    pub media_streams: Vec<MediaStream>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlaybackInfo {
    pub media_sources: Vec<MediaSource>,
    pub play_session_id: Option<String>,
}




/// Champs demandés pour toutes les listes d'éléments.
const FIELDS: &str = "Overview,Genres,ProviderIds,PrimaryImageAspectRatio,ChildCount,MediaStreams";

impl Jellyfin {
    pub fn new(http: Client, base: &str, token: &str, user_id: &str, device_id: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            user_id: user_id.into(),
            device_id: device_id.into(),
        }
    }

    fn auth_header(&self) -> String {
        let mut h = format!(
            r#"MediaBrowser Client="Noosfin", Device="HTPC", DeviceId="{}", Version="{}""#,
            self.device_id,
            env!("CARGO_PKG_VERSION")
        );
        if !self.token.is_empty() {
            h.push_str(&format!(r#", Token="{}""#, self.token));
        }
        h
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Jellyfin {path}: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    async fn post(&self, path: &str, query: &[(&str, String)], body: serde_json::Value) -> Result<reqwest::Response> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .header("Authorization", self.auth_header())
            .query(query)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Jellyfin {path}: HTTP {}", resp.status());
        }
        Ok(resp)
    }

    fn uid(&self) -> (&'static str, String) {
        ("userId", self.user_id.clone())
    }

    // --- Authentification Quick Connect -------------------------------------------------




    // --- Navigation -----------------------------------------------------------------------

    pub async fn views(&self) -> Result<Vec<Item>> {
        Ok(self.get::<ItemsResult>("/UserViews", &[self.uid()]).await?.items)
    }

    pub async fn resume(&self) -> Result<Vec<Item>> {
        let q = [self.uid(), ("limit", "24".into()), ("fields", FIELDS.into()), ("mediaTypes", "Video".into())];
        Ok(self.get::<ItemsResult>("/UserItems/Resume", &q).await?.items)
    }

    pub async fn next_up(&self) -> Result<Vec<Item>> {
        let q = [self.uid(), ("limit", "24".into()), ("fields", FIELDS.into())];
        Ok(self.get::<ItemsResult>("/Shows/NextUp", &q).await?.items)
    }

    pub async fn latest(&self, parent_id: &str) -> Result<Vec<Item>> {
        let q = [self.uid(), ("parentId", parent_id.into()), ("limit", "30".into()), ("fields", FIELDS.into())];
        self.get::<Vec<Item>>("/Items/Latest", &q).await
    }

    pub async fn items(&self, parent_id: Option<&str>, types: &str, sort: &str, limit: u32, search: Option<&str>) -> Result<Vec<Item>> {
        let mut q = vec![
            self.uid(),
            ("recursive", "true".into()),
            ("includeItemTypes", types.into()),
            ("sortBy", sort.into()),
            ("sortOrder", if sort == "SortName" { "Ascending" } else { "Descending" }.into()),
            ("limit", limit.to_string()),
            ("fields", FIELDS.into()),
        ];
        if let Some(p) = parent_id {
            q.push(("parentId", p.into()));
        }
        if let Some(s) = search {
            q.push(("searchTerm", s.into()));
        }
        Ok(self.get::<ItemsResult>("/Items", &q).await?.items)
    }

    pub async fn item(&self, id: &str) -> Result<Item> {
        self.get(&format!("/Items/{id}"), &[self.uid()]).await
    }

    pub async fn seasons(&self, series_id: &str) -> Result<Vec<Item>> {
        let q = [self.uid(), ("fields", FIELDS.into())];
        Ok(self.get::<ItemsResult>(&format!("/Shows/{series_id}/Seasons"), &q).await?.items)
    }

    pub async fn episodes(&self, series_id: &str, season_id: Option<&str>) -> Result<Vec<Item>> {
        let mut q = vec![self.uid(), ("fields", FIELDS.into())];
        if let Some(s) = season_id {
            q.push(("seasonId", s.into()));
        }
        Ok(self.get::<ItemsResult>(&format!("/Shows/{series_id}/Episodes"), &q).await?.items)
    }

    pub async fn playback_info(&self, id: &str) -> Result<PlaybackInfo> {
        let body = json!({ "UserId": self.user_id, "EnableDirectPlay": true, "EnableDirectStream": true, "EnableTranscoding": false });
        Ok(self.post(&format!("/Items/{id}/PlaybackInfo"), &[self.uid()], body).await?.json().await?)
    }

    pub async fn set_played(&self, id: &str, played: bool) -> Result<()> {
        let url = format!("{}/UserPlayedItems/{id}", self.base);
        let req = if played { self.http.post(url) } else { self.http.delete(url) };
        let resp = req.header("Authorization", self.auth_header()).query(&[self.uid()]).send().await?;
        if !resp.status().is_success() {
            bail!("Jellyfin played: HTTP {}", resp.status());
        }
        Ok(())
    }

    // --- Rapport de lecture ---------------------------------------------------------------

    pub async fn report(&self, stage: ReportStage, r: &PlayReport, position_secs: f64, paused: bool) -> Result<()> {
        let path = match stage {
            ReportStage::Start => "/Sessions/Playing",
            ReportStage::Progress => "/Sessions/Playing/Progress",
            ReportStage::Stop => "/Sessions/Playing/Stopped",
        };
        let body = json!({
            "ItemId": r.item_id,
            "MediaSourceId": r.media_source_id,
            "PlaySessionId": r.play_session_id,
            "PositionTicks": (position_secs * TICKS_PER_SEC) as i64,
            "IsPaused": paused,
            "CanSeek": true,
            "PlayMethod": "DirectPlay",
        });
        self.post(path, &[], body).await?;
        Ok(())
    }

    // --- URLs -----------------------------------------------------------------------------

    pub fn image_url(&self, item_id: &str, kind: &str, tag: Option<&str>, max_height: u32) -> String {
        let mut u = format!("{}/Items/{item_id}/Images/{kind}?maxHeight={max_height}&quality=90", self.base);
        if let Some(t) = tag {
            u.push_str("&tag=");
            u.push_str(t);
        }
        u
    }

    pub fn stream_url(&self, item_id: &str, media_source_id: &str) -> String {
        format!(
            "{}/Videos/{item_id}/stream?static=true&mediaSourceId={media_source_id}&api_key={}",
            self.base, self.token
        )
    }

    pub fn auth_http_header(&self) -> String {
        format!(r#"Authorization: MediaBrowser Token="{}""#, self.token)
    }

    /// Affiche principale d'un élément (pour un épisode : vignette, sinon affiche de la série).
    pub fn poster_of(&self, it: &Item) -> Option<String> {
        if let Some(t) = it.image_tags.get("Primary") {
            return Some(self.image_url(&it.id, "Primary", Some(t), 480));
        }
        match (&it.series_id, &it.series_primary_image_tag) {
            (Some(sid), Some(t)) => Some(self.image_url(sid, "Primary", Some(t), 480)),
            _ => None,
        }
    }

    /// Affiche « portrait » : pour un épisode, celle de la série.
    pub fn series_poster_of(&self, it: &Item) -> Option<String> {
        if it.kind == "Episode" {
            if let (Some(sid), Some(t)) = (&it.series_id, &it.series_primary_image_tag) {
                return Some(self.image_url(sid, "Primary", Some(t), 480));
            }
        }
        self.poster_of(it)
    }

    pub fn backdrop_of(&self, it: &Item) -> Option<String> {
        if let Some(t) = it.backdrop_image_tags.first() {
            return Some(format!("{}/Items/{}/Images/Backdrop/0?maxWidth=1920&quality=85&tag={t}", self.base, it.id));
        }
        it.parent_backdrop_item_id
            .as_ref()
            .map(|p| format!("{}/Items/{p}/Images/Backdrop/0?maxWidth=1920&quality=85", self.base))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ReportStage {
    Start,
    Progress,
    Stop,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayReport {
    pub item_id: String,
    pub media_source_id: String,
    pub play_session_id: Option<String>,
}

impl Item {
    pub fn progress(&self) -> f32 {
        let Some(ud) = &self.user_data else { return -1.0 };
        if let Some(p) = ud.played_percentage {
            if p > 0.0 && !ud.played {
                return (p / 100.0) as f32;
            }
        }
        match self.run_time_ticks {
            Some(rt) if rt > 0 && ud.playback_position_ticks > 0 => ud.playback_position_ticks as f32 / rt as f32,
            _ => -1.0,
        }
    }

    pub fn played(&self) -> bool {
        self.user_data.as_ref().is_some_and(|u| u.played)
    }

    pub fn resume_secs(&self) -> f64 {
        self.user_data.as_ref().map_or(0.0, |u| u.playback_position_ticks as f64 / TICKS_PER_SEC)
    }

    pub fn imdb(&self) -> Option<&str> {
        self.provider_ids.get("Imdb").map(String::as_str).filter(|s| !s.is_empty())
    }

    pub fn tmdb(&self) -> Option<&str> {
        self.provider_ids.get("Tmdb").map(String::as_str).filter(|s| !s.is_empty())
    }

    pub fn episode_label(&self) -> String {
        match (self.parent_index_number, self.index_number) {
            (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
            (None, Some(e)) => format!("E{e:02}"),
            _ => String::new(),
        }
    }
}

impl MediaStream {
    /// Libellé de résolution d'après la largeur (les films en 2.39:1 font 1920x800).
    pub fn resolution_label(&self) -> Option<&'static str> {
        let w = self.width.unwrap_or(0);
        let h = self.height.unwrap_or(0);
        if w == 0 && h == 0 {
            return None;
        }
        Some(if w >= 3200 || h >= 1700 {
            "4K"
        } else if w >= 1800 || h >= 1000 {
            "1080p"
        } else if w >= 1200 || h >= 700 {
            "720p"
        } else {
            "SD"
        })
    }
}
