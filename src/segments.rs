//! Segments à passer (intro, récapitulatif, générique de fin), affichés sur la timeline du
//! lecteur et proposés en popup. Animes : AniSkip (par identifiant MyAnimeList) ; séries et
//! films : IntroDB (par identifiant IMDb). Les chapitres du fichier complètent côté mpv.

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct Segments {
    pub intro: Option<(f64, f64)>,
    pub recap: Option<(f64, f64)>,
    /// Début du générique de fin.
    pub outro: Option<f64>,
}

impl Segments {
    /// Options passées à l'overlay (`--script-opts`).
    pub fn script_opts(&self) -> Vec<String> {
        let mut v = vec![];
        if let Some((a, b)) = self.intro {
            v.push(format!("noosfin-intro={a:.1}-{b:.1}"));
        }
        if let Some((a, b)) = self.recap {
            v.push(format!("noosfin-recap={a:.1}-{b:.1}"));
        }
        if let Some(o) = self.outro {
            v.push(format!("noosfin-outro={o:.1}"));
        }
        v
    }
}

/// Ce qu'on sait de l'épisode lu.
pub enum Lookup {
    Anime { mal: i64, episode: i64 },
    Video { imdb: String, season: Option<u32>, episode: Option<u32> },
}

async fn json(http: &Client, url: &str) -> Option<Value> {
    let r = tokio::time::timeout(Duration::from_secs(3), http.get(url).header("User-Agent", "noosfin").send()).await.ok()?.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

fn range(v: &Value) -> Option<(f64, f64)> {
    let (a, b) = (v["start_sec"].as_f64()?, v["end_sec"].as_f64()?);
    (b > a).then_some((a, b))
}

pub async fn fetch(http: &Client, what: Lookup) -> Segments {
    let mut s = Segments::default();
    match what {
        Lookup::Anime { mal, episode } => {
            let url = format!("https://api.aniskip.com/v2/skip-times/{mal}/{episode}?types=op&types=ed&types=recap&episodeLength=0");
            let Some(v) = json(http, &url).await else { return s };
            for r in v["results"].as_array().into_iter().flatten() {
                let (a, b) = (r["interval"]["startTime"].as_f64().unwrap_or(0.0), r["interval"]["endTime"].as_f64().unwrap_or(0.0));
                match r["skipType"].as_str() {
                    Some("op") if b > a => s.intro = Some((a, b)),
                    Some("recap") if b > a => s.recap = Some((a, b)),
                    Some("ed") => s.outro = Some(a),
                    _ => {}
                }
            }
        }
        Lookup::Video { imdb, season, episode } => {
            let mut url = format!("https://api.introdb.app/segments?imdb_id={imdb}");
            if let (Some(se), Some(ep)) = (season, episode) {
                url.push_str(&format!("&season={se}&episode={ep}"));
            }
            let Some(v) = json(http, &url).await else { return s };
            s.intro = range(&v["intro"]);
            s.recap = range(&v["recap"]);
            s.outro = v["outro"]["start_sec"].as_f64();
        }
    }
    s
}
