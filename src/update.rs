//! Mises à jour via les releases GitHub : on compare la version courante à la dernière
//! release, puis l'installateur fourni dans l'archive remplace les fichiers de l'utilisateur.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::Value;

pub const REPO: &str = "Chomiam/noosfin";
pub const ASSET: &str = "noosfin-x86_64.tar.gz";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
}

fn parse(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn newer(candidate: &str, current: &str) -> bool {
    parse(candidate) > parse(current)
}

/// Dernière release si elle est plus récente que la version installée.
pub async fn check(http: &Client) -> Result<Option<Release>> {
    let v: Value = http
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "noosfin")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = v["tag_name"].as_str().context("release sans tag")?.to_string();
    // Pour les tests : NOOSFIN_VERSION=0.0.1 simule une installation ancienne.
    let current = std::env::var("NOOSFIN_VERSION").unwrap_or_else(|_| VERSION.to_string());
    if !newer(&tag, &current) {
        return Ok(None);
    }
    let url = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(ASSET))
        .and_then(|a| a["browser_download_url"].as_str())
        .context("archive absente de la release")?
        .to_string();
    Ok(Some(Release { version: tag.trim_start_matches('v').to_string(), url }))
}

/// Télécharge l'archive, puis lance son installateur en mode mise à jour (fichiers de
/// l'utilisateur uniquement, sans sudo).
pub async fn install(http: &Client, r: &Release) -> Result<()> {
    let dir: PathBuf = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("noosfin/update");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await?;
    let bytes = http.get(&r.url).header("User-Agent", "noosfin").send().await?.error_for_status()?.bytes().await?;
    let archive = dir.join(ASSET);
    tokio::fs::write(&archive, &bytes).await?;
    let ok = tokio::process::Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&dir).status().await?.success();
    if !ok {
        bail!("archive illisible");
    }
    let out = tokio::process::Command::new(dir.join("noosfin/bin/noosfin-installer")).arg("--update").output().await?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions() {
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }
}
