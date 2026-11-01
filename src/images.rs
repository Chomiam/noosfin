//! Téléchargement + décodage des affiches hors du fil d'interface.

use anyhow::Result;
use image::imageops::FilterType;
use reqwest::Client;
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Limite la taille décodée (les backdrops sont plus larges que les affiches).
pub async fn fetch(http: &Client, url: &str) -> Result<SharedPixelBuffer<Rgba8Pixel>> {
    let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
    let wide = url.contains("Backdrop") || url.contains("banner") || url.contains("background");
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes)?;
        let (max_w, max_h) = if wide { (1920, 1080) } else { (640, 640) };
        let img = if img.width() > max_w || img.height() > max_h {
            img.resize(max_w, max_h, FilterType::Triangle)
        } else {
            img
        };
        let rgba = img.to_rgba8();
        Ok(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
    })
    .await?
}
