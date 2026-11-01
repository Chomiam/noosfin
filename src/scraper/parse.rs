//! Analyse des noms de release : résolution, source, codec, HDR, audio, langues, épisodes, groupe.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Release {
    pub resolution: Option<&'static str>,
    pub quality: Option<&'static str>,
    pub codec: Option<&'static str>,
    pub hdr: Vec<&'static str>,
    pub audio: Vec<&'static str>,
    /// vff, vfq, vf2, truefrench, multi, vostfr, vo
    pub languages: Vec<&'static str>,
    pub subtitles: Vec<&'static str>,
    pub cam: bool,
    pub seasons: Vec<u32>,
    pub episodes: Vec<u32>,
    /// Pack complet (saison / intégrale sans numéro d'épisode).
    pub pack: bool,
    pub group: Option<String>,
}

fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !(c.is_alphanumeric() || c == '+'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

fn push<T: PartialEq>(v: &mut Vec<T>, x: T) {
    if !v.contains(&x) {
        v.push(x);
    }
}

pub fn parse(name: &str) -> Release {
    let w = words(name);
    let lower = name.to_lowercase();
    let has = |k: &str| w.iter().any(|x| x == k);
    let mut r = Release::default();

    r.resolution = if has("2160p") || has("4k") || has("uhd") {
        Some("2160p")
    } else if has("1080p") || has("1080i") {
        Some("1080p")
    } else if has("720p") {
        Some("720p")
    } else if has("480p") || has("576p") || has("dvdrip") || has("sd") {
        Some("480p")
    } else {
        None
    };
    // Un « 1080p » explicite l'emporte sur « 4K » dans un titre (« 4K Remaster 1080p »).
    if r.resolution == Some("2160p") && has("1080p") && !has("2160p") {
        r.resolution = Some("1080p");
    }

    r.cam = ["cam", "camrip", "hdcam", "ts", "telesync", "hdts", "tc", "telecine", "hdtc"].iter().any(|k| has(k));
    r.quality = if has("remux") {
        Some("remux")
    } else if has("bluray") || has("blu") || has("bdrip") || has("brrip") || has("bdremux") {
        Some("bluray")
    } else if lower.contains("web-dl") || has("webdl") || (has("web") && has("dl")) {
        Some("web-dl")
    } else if has("webrip") || has("web") {
        Some("webrip")
    } else if has("hdtv") || has("tvrip") || has("hdtvrip") {
        Some("hdtv")
    } else if has("dvdrip") || has("dvd") {
        Some("dvd")
    } else {
        None
    };

    r.codec = if has("x265") || has("h265") || has("hevc") || lower.contains("h.265") {
        Some("hevc")
    } else if has("av1") {
        Some("av1")
    } else if has("x264") || has("h264") || has("avc") || lower.contains("h.264") {
        Some("avc")
    } else {
        None
    };

    if has("dv") || has("dovi") || lower.contains("dolby.vision") || lower.contains("dolby vision") {
        push(&mut r.hdr, "dv");
    }
    if lower.contains("hdr10+") || has("hdr10plus") {
        push(&mut r.hdr, "hdr10+");
    } else if has("hdr10") || has("hdr") {
        push(&mut r.hdr, "hdr10");
    }
    if has("hlg") {
        push(&mut r.hdr, "hlg");
    }

    for (k, v) in [("atmos", "Atmos"), ("truehd", "TrueHD"), ("dts", "DTS"), ("ddp", "DD+"), ("eac3", "DD+"), ("ac3", "AC3"), ("aac", "AAC"), ("flac", "FLAC"), ("opus", "Opus")] {
        if has(k) || (k == "ddp" && (lower.contains("dd+") || lower.contains("ddp5") || lower.contains("ddp2"))) {
            push(&mut r.audio, v);
        }
    }
    if lower.contains("dts-hd") || lower.contains("dts.hd") || has("dtshd") {
        r.audio.retain(|a| *a != "DTS");
        push(&mut r.audio, "DTS-HD MA");
    }

    // Langues (conventions des releases françaises)
    for (k, v) in [
        ("vff", "vff"),
        ("vfq", "vfq"),
        ("vf2", "vf2"),
        ("vfi", "vff"),
        ("truefrench", "truefrench"),
        ("french", "vff"),
        ("vf", "vff"),
        ("multi", "multi"),
        ("multilang", "multi"),
        ("vostfr", "vostfr"),
        ("subfrench", "vostfr"),
        ("vo", "vo"),
        ("eng", "vo"),
        ("english", "vo"),
        ("jap", "vo"),
        ("japanese", "vo"),
    ] {
        if has(k) {
            push(&mut r.languages, v);
        }
    }
    // « Multi Sub » / « MultiSubs » : sous-titres multiples, pas d'audio MULTi.
    let multi_sub = lower.contains("multi sub") || lower.contains("multi-sub") || lower.contains("multisub") || lower.contains("multiple subtitle");
    if multi_sub && !lower.contains("multi.") && !lower.replace("multi sub", "").replace("multi-sub", "").contains("multi") {
        r.languages.retain(|l| *l != "multi");
    }
    if r.languages.contains(&"multi") {
        // « MULTi » (souvent VFF + VO) : la VF exacte est indiquée en plus quand elle est connue.
        push(&mut r.languages, "vo");
    }
    if r.languages.contains(&"vostfr") || has("subfrench") || multi_sub {
        push(&mut r.subtitles, "fr");
    }
    if multi_sub || has("engsub") {
        push(&mut r.subtitles, "en");
    }

    parse_episodes(name, &mut r);
    r.group = group_of(name);
    r
}

fn num(s: &str) -> Option<u32> {
    let d: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if d.is_empty() || d.len() > 4 { None } else { d.parse().ok() }
}

/// S01E02, S01E02-E03, S01, Saison 1, E05, « - 05 », Episode 5.
fn parse_episodes(name: &str, r: &mut Release) {
    let up = name.to_uppercase();
    let b = up.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if boundary && b[i] == b'S' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            if let Some(s) = num(&up[i + 1..]) {
                let mut j = i + 1 + up[i + 1..].chars().take_while(|c| c.is_ascii_digit()).count();
                push(&mut r.seasons, s);
                // Épisodes collés : S01E02E03 / S01E02-E03 / S01E02-03
                while j < b.len() && (b[j] == b'E' || b[j] == b'-' || b[j] == b' ' || b[j] == b'.') {
                    let k = if b[j] == b'E' { j + 1 } else if j + 1 < b.len() && b[j + 1] == b'E' { j + 2 } else if b[j] == b'-' { j + 1 } else { break };
                    match num(&up[k..]) {
                        Some(e) if k < b.len() && b[k].is_ascii_digit() => {
                            push(&mut r.episodes, e);
                            j = k + up[k..].chars().take_while(|c| c.is_ascii_digit()).count();
                        }
                        _ => break,
                    }
                }
                i = j;
                continue;
            }
        }
        if boundary && b[i] == b'E' && i + 1 < b.len() && b[i + 1].is_ascii_digit() && r.episodes.is_empty() {
            if let Some(e) = num(&up[i + 1..]) {
                push(&mut r.episodes, e);
            }
        }
        i += 1;
    }
    // « 2nd Season », « 3rd Season »
    for (p, _) in up.match_indices(" SEASON") {
        let before = up[..p].trim_end();
        let tail: String = before.chars().rev().take_while(|c| c.is_ascii_alphanumeric()).collect::<Vec<_>>().into_iter().rev().collect();
        let d: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !d.is_empty() && ["ST", "ND", "RD", "TH"].contains(&&tail[d.len()..]) {
            if let Ok(n) = d.parse() {
                push(&mut r.seasons, n);
            }
        }
    }
    for key in ["SAISON ", "SEASON "] {
        if let Some(p) = up.find(key) {
            if let Some(s) = num(&up[p + key.len()..]) {
                push(&mut r.seasons, s);
            }
        }
    }
    for key in ["EPISODE ", "ÉPISODE ", "EP "] {
        if let Some(p) = up.find(key) {
            if let Some(e) = num(&up[p + key.len()..]) {
                push(&mut r.episodes, e);
            }
        }
    }
    // Anime : « [Groupe] Titre - 05 (1080p) » ou « Titre - 05v2 »
    if r.episodes.is_empty() {
        for (p, _) in name.match_indices(" - ") {
            let rest = &name[p + 3..];
            let d: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let next = rest[d.len()..].chars().next();
            if (1..=4).contains(&d.len()) && next.is_none_or(|c| matches!(c, ' ' | 'v' | 'V' | '[' | '(' | '.' | '-')) {
                if let Ok(e) = d.parse::<u32>() {
                    if !(1900..=2100).contains(&e) {
                        push(&mut r.episodes, e);
                    }
                }
            }
        }
    }
    let lower = name.to_lowercase();
    r.pack = r.episodes.is_empty()
        && (!r.seasons.is_empty() || ["integrale", "intégrale", "complete", "batch", "saison", "season"].iter().any(|k| lower.contains(k)));
}

fn group_of(name: &str) -> Option<String> {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let g = &rest[..end];
            if !g.is_empty() && g.len() < 40 {
                return Some(g.to_string());
            }
        }
    }
    let base = name.trim_end_matches(|c: char| c == ']' || c == ')').rsplit_once('.').map_or(name, |(b, ext)| {
        if ["mkv", "mp4", "avi", "ts"].contains(&ext.to_lowercase().as_str()) { b } else { name }
    });
    let g = base.rsplit_once('-')?.1.trim();
    (!g.is_empty() && g.len() < 30 && !g.contains(' ')).then(|| g.to_string())
}

/// Ce fichier / cette release correspond-elle à l'épisode demandé ?
pub fn matches_episode(r: &Release, season: Option<u32>, episode: u32, anime: bool) -> bool {
    if r.episodes.contains(&episode) {
        // Saison explicite différente : refus (sauf anime, numérotation souvent absolue).
        return anime || season.is_none() || r.seasons.is_empty() || r.seasons.contains(&season.unwrap());
    }
    // Pack de saison : le bon fichier sera choisi chez le débrideur.
    r.pack && (r.seasons.is_empty() || season.is_none_or(|s| r.seasons.contains(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let r = parse("Interstellar.2014.MULTi.VFF.2160p.BluRay.REMUX.DV.HDR10+.HEVC.TrueHD.7.1.Atmos-FGT");
        assert_eq!(r.resolution, Some("2160p"));
        assert_eq!(r.quality, Some("remux"));
        assert_eq!(r.codec, Some("hevc"));
        assert!(r.hdr.contains(&"dv") && r.hdr.contains(&"hdr10+"));
        assert!(r.languages.contains(&"multi") && r.languages.contains(&"vff"));
        assert_eq!(r.group.as_deref(), Some("FGT"));
    }

    #[test]
    fn episodes() {
        let r = parse("Show.S02E05.FRENCH.1080p.WEB.H264-GRP");
        assert_eq!(r.seasons, vec![2]);
        assert_eq!(r.episodes, vec![5]);
        assert!(matches_episode(&r, Some(2), 5, false));
        assert!(!matches_episode(&r, Some(1), 5, false));
        let a = parse("[Tsundere-Raws] Frieren - 05 VOSTFR (CR) [WEB 1080p x264 AAC].mkv");
        assert_eq!(a.episodes, vec![5]);
        assert_eq!(a.group.as_deref(), Some("Tsundere-Raws"));
        assert!(a.languages.contains(&"vostfr") && a.subtitles.contains(&"fr"));
        let ms = parse("[Anime Time] Black Clover - 01 [1080p][HEVC 10bit x265][Multi Sub][AAC].mkv");
        assert!(!ms.languages.contains(&"multi") && ms.subtitles.contains(&"fr"));
        let p = parse("Show.S01.MULTi.1080p.BluRay.x265-GRP");
        assert!(p.pack && matches_episode(&p, Some(1), 3, false));
        let m = parse("Show.S01E01-E03.1080p");
        assert_eq!(m.episodes, vec![1, 3]);
    }

    #[test]
    fn cam() {
        assert!(parse("Movie.2026.HDCAM.x264").cam);
        assert!(!parse("Movie.2026.1080p.WEB").cam);
    }
}

/// Saison indiquée dans un titre d'anime AniList (« 2nd Season », « Season 3 », « S2 »).
pub fn anime_season(title: &str) -> Option<u32> {
    let r = parse(title);
    r.seasons.into_iter().find(|s| *s >= 2)
}

#[cfg(test)]
mod season_tests {
    use super::*;

    #[test]
    fn anime_seasons() {
        assert_eq!(anime_season("Sousou no Frieren 2nd Season"), Some(2));
        assert_eq!(anime_season("Kaguya-sama Season 3"), Some(3));
        assert_eq!(anime_season("Sousou no Frieren"), None);
        let r = parse("[SubsPlease] Sousou no Frieren S2 - 05 (1080p).mkv");
        assert_eq!((r.seasons.clone(), r.episodes.clone()), (vec![2], vec![5]));
        assert_eq!(parse("[Erai-raws] Sousou no Frieren 2nd Season - 05 [1080p]").seasons, vec![2]);
    }
}
