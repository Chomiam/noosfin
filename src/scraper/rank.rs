//! Filtres et tri des résultats selon les priorités choisies sur la page de configuration.

use super::config::{Criterion, Sources};
use super::{Candidate, Query, parse};

fn pos(list: &[String], v: &str) -> Option<usize> {
    list.iter().position(|x| x.eq_ignore_ascii_case(v))
}

/// Meilleur rang (plus grand = mieux) parmi les valeurs de la release.
fn best(list: &[String], values: &[&str]) -> i64 {
    values.iter().filter_map(|v| pos(list, v)).min().map_or(0, |i| (list.len() - i) as i64)
}

fn languages(c: &Candidate) -> Vec<&'static str> {
    if c.info.languages.is_empty() { vec!["vo"] } else { c.info.languages.clone() }
}

/// Garde-t-on ce résultat ?
fn keep(c: &Candidate, q: &Query, s: &Sources) -> bool {
    let r = &s.ranking;
    if r.exclude_cam && c.info.cam {
        return false;
    }
    if c.info.resolution.is_some_and(|res| pos(&r.excluded_resolutions, res).is_some()) {
        return false;
    }
    let langs = languages(c);
    if !r.excluded_languages.is_empty() && langs.iter().all(|l| pos(&r.excluded_languages, l).is_some()) {
        return false;
    }
    if r.max_size_gb > 0.0 && !c.info.pack && c.size as f64 > r.max_size_gb * 1e9 {
        return false;
    }
    if r.cached_only && c.cached == Some(false) {
        return false;
    }
    // Anime d'une saison ≥ 2 : la saison doit être explicite dans la release (sinon c'est
    // presque toujours la saison 1, numérotée « Titre - 05 »).
    if q.anime && q.season.is_some_and(|s| s >= 2) && !c.info.seasons.contains(&q.season.unwrap()) {
        return false;
    }
    if q.anime && q.season.is_none() && c.info.seasons.iter().any(|s| *s >= 2) {
        return false;
    }
    match q.episode {
        Some(e) => parse::matches_episode(&c.info, q.season, e, q.anime),
        // Film : on écarte les épisodes de séries.
        None => c.info.episodes.is_empty() || q.anime,
    }
}

fn key(c: &Candidate, q: &Query, s: &Sources) -> Vec<i64> {
    let r = &s.ranking;
    let lang_list = if q.anime { &s.anime.languages } else { &r.languages };
    // Toujours en premier : l'épisode exact passe devant un pack (plus lent, fichier incertain).
    let exact = match q.episode {
        Some(e) => i64::from(c.info.episodes.contains(&e)),
        None => 0,
    };
    std::iter::once(exact)
        .chain(r.order.iter()
        .map(|crit| match crit {
            Criterion::Cached => match c.cached {
                Some(true) => 2,
                None => 1,
                Some(false) => 0,
            },
            Criterion::Language => best(lang_list, &languages(c)),
            Criterion::Subtitles => best(&r.subtitles, &c.info.subtitles),
            Criterion::Resolution => c.info.resolution.map_or(0, |res| best(&r.resolutions, &[res])),
            Criterion::Quality => c.info.quality.map_or(0, |q| best(&r.qualities, &[q])),
            Criterion::Hdr => {
                let v: Vec<&str> = if c.info.hdr.is_empty() { vec!["sdr"] } else { c.info.hdr.clone() };
                best(&r.hdr, &v)
            }
            Criterion::Codec => c.info.codec.map_or(0, |x| best(&r.codecs, &[x])),
            Criterion::Size => (c.size / 50_000_000) as i64,
            Criterion::Seeders => c.seeders.map_or(0, |s| (s.max(1) as f64).log2() as i64),
            Criterion::Group => c
                .info
                .group
                .as_deref()
                .and_then(|g| s.anime.groups.iter().position(|x| x.eq_ignore_ascii_case(g)))
                .map_or(0, |i| (s.anime.groups.len() - i) as i64),
        }))
        .collect()
}

pub fn rank(mut list: Vec<Candidate>, q: &Query, s: &Sources) -> Vec<Candidate> {
    list.retain(|c| keep(c, q, s));
    // Groupe favori : toujours un léger bonus pour les animes, même hors critères choisis.
    let mut keyed: Vec<(Vec<i64>, Candidate)> = list.into_iter().map(|c| (key(&c, q, s), c)).collect();
    keyed.sort_by(|a, b| b.0.cmp(&a.0));
    let n = keyed.len() as i64;
    keyed
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut c))| {
            c.score = n - i as i64;
            c
        })
        .take(s.ranking.max_results.max(10))
        .collect()
}
