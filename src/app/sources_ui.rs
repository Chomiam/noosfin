//! Configuration des sources directement dans l'application (manette ou clavier) :
//! débrideur, clés, trackers, sources publiques, réseau et priorités. La page web
//! (QR code) reste disponible pour une saisie plus confortable sur le téléphone.

use super::*;
use crate::ScRow;
use scraper::config::{DebridKind, Dns, Indexer, IndexerKind, LOCAL_FLARESOLVERR, Sources};

/// Champ édité au clavier (formulaire partagé avec l'IPTV).
#[derive(Clone)]
pub(super) enum Edit {
    DebridKey,
    TmdbKey,
    FlareUrl,
    AddTracker { name: &'static str, url: &'static str, interval: u64 },
    TrackerKey(usize),
}

impl Edit {
    pub(super) fn title(&self) -> String {
        match self {
            Edit::DebridKey => "Clé API du débrideur".into(),
            Edit::TmdbKey => "Clé API TMDB (v3)".into(),
            Edit::FlareUrl => "Adresse de FlareSolverr".into(),
            Edit::AddTracker { name, .. } => format!("Ajouter {name}"),
            Edit::TrackerKey(_) => "Clé API du tracker".into(),
        }
    }
}

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Debrid,
    DebridKey,
    DebridTest,
    Uncached,
    Tmdb,
    TmdbLang,
    Tracker(usize),
    AddTracker,
    BuiltinGroup,
    Public(&'static str, &'static str),
    Builtin(String),
    Dns,
    Flare,
    NetTest,
    Language,
    AnimeLanguage,
    Resolution,
    CachedOnly,
    ExcludeCam,
    Phone,
}

/// Trackers privés proposés à l'ajout : (nom, URL Torznab, délai entre requêtes).
const PRESETS: [(&str, &str, u64); 3] =
    [("C411", "https://c411.org/api/torznab", 4100), ("Tr4ker", "https://tr4ker.net/api/torznab/all", 1000), ("Torznab", "", 1000)];

const PUBLIC: [(&str, &str); 8] = [
    ("knaben", "Knaben · 1337x, TorrentGalaxy, TPB…"),
    ("torrentscsv", "Torrents-CSV"),
    ("yts", "YTS · films VO"),
    ("eztv", "EZTV · séries VO"),
    ("tpb", "The Pirate Bay"),
    ("nyaa", "Nyaa · animes"),
    ("subsplease", "SubsPlease · animes"),
    ("aiostreams", "AIOStreams · en appoint"),
];
const LANGS: [&str; 7] = ["multi", "vff", "truefrench", "vfq", "vf2", "vostfr", "vo"];
const RESOLUTIONS: [&str; 4] = ["2160p", "1080p", "720p", "480p"];

fn lang_label(l: &str) -> &str {
    match l {
        "multi" => "MULTi",
        "vff" => "VFF",
        "truefrench" => "TRUEFRENCH",
        "vfq" => "VFQ",
        "vf2" => "VF2",
        "vostfr" => "VOSTFR",
        "vo" => "VO",
        other => other,
    }
}

fn masked(k: &str) -> String {
    let k = k.trim();
    if k.is_empty() {
        return "non renseignée".into();
    }
    let tail: String = k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("••••{tail}")
}

fn public_on(s: &Sources, key: &str) -> bool {
    let p = &s.public;
    match key {
        "knaben" => p.knaben,
        "torrentscsv" => p.torrentscsv,
        "yts" => p.yts,
        "eztv" => p.eztv,
        "tpb" => p.tpb,
        "nyaa" => p.nyaa,
        "subsplease" => p.subsplease,
        _ => p.aiostreams,
    }
}

fn public_flag<'a>(s: &'a mut Sources, key: &str) -> &'a mut bool {
    let p = &mut s.public;
    match key {
        "knaben" => &mut p.knaben,
        "torrentscsv" => &mut p.torrentscsv,
        "yts" => &mut p.yts,
        "eztv" => &mut p.eztv,
        "tpb" => &mut p.tpb,
        "nyaa" => &mut p.nyaa,
        "subsplease" => &mut p.subsplease,
        _ => &mut p.aiostreams,
    }
}

fn order(list: &[String]) -> String {
    list.iter().map(|l| lang_label(l).to_string()).collect::<Vec<_>>().join(" › ")
}

/// Place `next` en tête de la liste (choix circulaire parmi `all`).
fn rotate_first(list: &mut Vec<String>, all: &[&str]) {
    let cur = list.first().cloned().unwrap_or_default();
    let i = all.iter().position(|x| *x == cur).map_or(0, |i| (i + 1) % all.len());
    let next = all[i].to_string();
    list.retain(|x| *x != next);
    list.insert(0, next);
}

impl Controller {
    fn sc_rows(&self) -> Vec<Row> {
        let s = &self.sc_cfg;
        let mut v = vec![Row::Header("Téléphone"), Row::Phone];
        v.extend([Row::Header("Débrideur"), Row::Debrid, Row::DebridKey, Row::DebridTest, Row::Uncached]);
        v.extend([Row::Header("Catalogues"), Row::Tmdb, Row::TmdbLang]);
        v.push(Row::Header("Trackers privés"));
        v.extend((0..s.indexers.len()).map(Row::Tracker));
        v.push(Row::AddTracker);
        v.push(Row::Header("Sources publiques"));
        v.extend(PUBLIC.iter().map(|(k, l)| Row::Public(k, l)));
        // 35 trackers intégrés : repliés derrière une seule ligne pour garder la page courte.
        v.push(Row::BuiltinGroup);
        if self.sc_builtin_open {
            v.extend(scraper::cardigann::all().iter().map(|d| Row::Builtin(d.id.clone())));
        }
        v.extend([Row::Header("Réseau"), Row::Dns, Row::Flare, Row::NetTest]);
        v.extend([Row::Header("Priorités"), Row::Language, Row::AnimeLanguage, Row::Resolution, Row::CachedOnly, Row::ExcludeCam]);
        v
    }

    fn sc_row_data(&self, r: &Row) -> ScRow {
        let s = &self.sc_cfg;
        let row = |label: String, value: String, state: i32| ScRow { header: false, label: label.into(), value: value.into(), state };
        let toggle = |label: &str, on: bool| row(label.into(), String::new(), on as i32);
        match r {
            Row::Header(t) => ScRow { header: true, label: (*t).into(), value: "".into(), state: -1 },
            Row::Debrid => row(
                "Service".into(),
                match s.debrid.service {
                    DebridKind::None => "aucun",
                    DebridKind::AllDebrid => "AllDebrid",
                    DebridKind::TorBox => "TorBox",
                    DebridKind::RealDebrid => "Real-Debrid",
                }
                .into(),
                -1,
            ),
            Row::DebridKey => row("Clé API".into(), masked(&s.debrid.api_key), -1),
            Row::DebridTest => row("Tester la clé".into(), "Ⓐ".into(), -1),
            Row::Uncached => toggle("Télécharger si pas en cache", s.debrid.download_uncached),
            Row::Tmdb => row("Clé TMDB".into(), masked(&s.metadata.tmdb_key), -1),
            Row::TmdbLang => row(
                "Langue des fiches".into(),
                match s.metadata.language.as_str() {
                    "fr-CA" => "Français (Québec)",
                    "en-US" => "Anglais",
                    _ => "Français (France)",
                }
                .into(),
                -1,
            ),
            Row::Tracker(i) => {
                let ix = &s.indexers[*i];
                row(format!("{} · {}", ix.name, masked(&ix.api_key)), String::new(), ix.enabled as i32)
            }
            Row::AddTracker => row("Ajouter un tracker".into(), format!("‹ {} ›", PRESETS[self.sc_preset].0), -1),
            Row::BuiltinGroup => row(
                format!("Trackers intégrés · {} actif(s) sur {}", s.public.trackers.len(), scraper::cardigann::all().len()),
                if self.sc_builtin_open { "▾ masquer".into() } else { "▸ afficher".into() },
                -1,
            ),
            Row::Public(k, l) => toggle(l, public_on(s, k)),
            Row::Builtin(id) => {
                let d = scraper::cardigann::get(id);
                let label = d.map(|d| {
                    let flag = if d.language.starts_with("fr") { "FR · " } else { "" };
                    let slow = if d.cloudflare() { " · lent" } else { "" };
                    let anime = if d.anime() { " · animes" } else { "" };
                    format!("{flag}{}{anime}{slow}", d.name)
                });
                toggle(&label.unwrap_or_else(|| id.clone()), s.public.trackers.contains(id))
            }
            Row::Dns => row(
                "DNS chiffré".into(),
                match s.network.dns {
                    Dns::Cloudflare => "Cloudflare",
                    Dns::Quad9 => "Quad9",
                    Dns::Google => "Google",
                    Dns::System => "DNS du système",
                }
                .into(),
                -1,
            ),
            Row::Flare => row(
                "FlareSolverr".into(),
                match s.network.flaresolverr.as_str() {
                    LOCAL_FLARESOLVERR => "intégré (podman)".to_string(),
                    "" => "désactivé".to_string(),
                    url => url.to_string(),
                },
                -1,
            ),
            Row::NetTest => row("Tester le réseau".into(), "Ⓐ".into(), -1),
            Row::Language => row("Langue audio".into(), order(&s.ranking.languages), -1),
            Row::AnimeLanguage => row("Langue des animes".into(), order(&s.anime.languages), -1),
            Row::Resolution => row("Résolution".into(), s.ranking.resolutions.join(" › "), -1),
            Row::CachedOnly => toggle("Uniquement les sources en cache", s.ranking.cached_only),
            Row::ExcludeCam => toggle("Exclure CAM / TS", s.ranking.exclude_cam),
            Row::Phone => row("Configurer depuis le téléphone".into(), "QR code".into(), -1),
        }
    }

    pub(super) fn open_sc(&mut self) {
        self.sc_cfg = Sources::load();
        self.sc_focus = 3;
        self.sc_msg.clear();
        self.sc_builtin_open = false;
        self.go(SCR_SRC_CFG);
        // QR code affiché d'emblée, en haut à droite.
        self.sc_phone();
    }

    pub(super) fn sc_sync(&mut self) {
        let rows: Vec<ScRow> = self
            .sc_rows()
            .iter()
            .map(|r| {
                let mut d = self.sc_row_data(r);
                d.value = self.glyphs(&d.value).into();
                d
            })
            .collect();
        let ui = self.ui();
        self.sc_model.set_vec(rows);
        ui.set_sc_focus(self.sc_focus);
        ui.set_sc_msg(self.sc_msg.clone().into());
    }

    /// Enregistre et recharge le moteur de sources.
    fn sc_save(&mut self) {
        match self.sc_cfg.save() {
            Ok(()) => self.reload_sources(),
            Err(e) => self.toast(format!("Enregistrement impossible : {e:#}")),
        }
    }

    fn sc_test(&mut self, what: &'static str, index: usize, id: String) {
        self.sc_msg = "Test en cours…".into();
        let engine = Engine::new(self.sc_cfg.clone());
        self.spawn(async move {
            let r = match what {
                "debrid" => engine.test_debrid().await,
                "indexer" => engine.test_indexer(index).await,
                "tracker" => engine.test_tracker(&id).await,
                _ => engine.test_network().await,
            };
            post(move |c| {
                c.sc_msg = match r {
                    Ok(m) => format!("✓ {m}"),
                    Err(e) => format!("✕ {e:#}"),
                };
                c.sync();
            });
        });
    }

    pub(super) fn nav_sc(&mut self, n: Nav) {
        let rows = self.sc_rows();
        let len = rows.len() as i32;
        let step = |from: i32, d: i32| {
            let mut i = from;
            loop {
                i += d;
                if i < 0 || i >= len {
                    return from;
                }
                if !matches!(rows[i as usize], Row::Header(_)) {
                    return i;
                }
            }
        };
        let row = rows.get(self.sc_focus as usize).cloned();
        match n {
            Nav::Back => {
                self.ui().set_sc_has_qr(false);
                return self.back();
            }
            Nav::Up => self.sc_focus = step(self.sc_focus, -1),
            Nav::Down => self.sc_focus = step(self.sc_focus, 1),
            Nav::PrevTab => self.sc_focus = (0..6).fold(self.sc_focus, |f, _| step(f, -1)),
            Nav::NextTab => self.sc_focus = (0..6).fold(self.sc_focus, |f, _| step(f, 1)),
            Nav::Accept | Nav::Left | Nav::Right => {
                if let Some(r) = row {
                    self.sc_activate(r, n);
                }
            }
            // Ⓨ : tester le tracker sélectionné ; Ⓧ : retirer un tracker privé.
            Nav::Alt => match row {
                Some(Row::Tracker(i)) => self.sc_test("indexer", i, String::new()),
                Some(Row::Builtin(id)) => self.sc_test("tracker", 0, id),
                Some(Row::Public(k, _)) if k != "aiostreams" => self.sc_test("tracker", 0, k.to_string()),
                _ => {}
            },
            Nav::Fav => {
                if let Some(Row::Tracker(i)) = row {
                    if self.confirmed(30) {
                        let name = self.sc_cfg.indexers.remove(i).name;
                        self.sc_save();
                        self.sc_focus = self.sc_focus.min(self.sc_rows().len() as i32 - 1);
                        self.toast(format!("{name} retiré"));
                    }
                }
            }
            _ => {}
        }
    }

    fn sc_activate(&mut self, r: Row, n: Nav) {
        let back = n == Nav::Left;
        let d: i32 = if back { -1 } else { 1 };
        let cycle = |i: usize, n: usize| ((i as i32 + d).rem_euclid(n as i32)) as usize;
        let s = &mut self.sc_cfg;
        match r {
            Row::Header(_) => return,
            Row::Debrid => {
                const ALL: [DebridKind; 4] = [DebridKind::AllDebrid, DebridKind::TorBox, DebridKind::RealDebrid, DebridKind::None];
                let i = ALL.iter().position(|k| *k == s.debrid.service).unwrap_or(3);
                s.debrid.service = ALL[cycle(i, 4)].clone();
            }
            Row::DebridKey => return self.sc_edit(Edit::DebridKey),
            Row::DebridTest => return self.sc_test("debrid", 0, String::new()),
            Row::Uncached => s.debrid.download_uncached = !s.debrid.download_uncached,
            Row::Tmdb => return self.sc_edit(Edit::TmdbKey),
            Row::TmdbLang => {
                const ALL: [&str; 3] = ["fr-FR", "fr-CA", "en-US"];
                let i = ALL.iter().position(|l| *l == s.metadata.language).unwrap_or(0);
                s.metadata.language = ALL[cycle(i, 3)].into();
            }
            Row::Tracker(i) => {
                if back {
                    return self.sc_edit(Edit::TrackerKey(i));
                }
                s.indexers[i].enabled = !s.indexers[i].enabled;
            }
            Row::AddTracker => {
                if n != Nav::Accept {
                    self.sc_preset = cycle(self.sc_preset, PRESETS.len());
                    return;
                }
                let (name, url, interval) = PRESETS[self.sc_preset];
                return self.sc_edit(Edit::AddTracker { name, url, interval });
            }
            Row::BuiltinGroup => {
                self.sc_builtin_open = !self.sc_builtin_open;
                return;
            }
            Row::Public(k, _) => {
                let f = public_flag(s, k);
                *f = !*f;
            }
            Row::Builtin(id) => {
                if s.public.trackers.contains(&id) {
                    s.public.trackers.retain(|x| *x != id);
                } else {
                    s.public.trackers.push(id);
                }
            }
            Row::Dns => {
                const ALL: [Dns; 4] = [Dns::Cloudflare, Dns::Quad9, Dns::Google, Dns::System];
                let i = ALL.iter().position(|k| *k == s.network.dns).unwrap_or(0);
                s.network.dns = ALL[cycle(i, 4)].clone();
            }
            Row::Flare => {
                if back {
                    return self.sc_edit(Edit::FlareUrl);
                }
                s.network.flaresolverr = if s.network.flaresolverr == LOCAL_FLARESOLVERR { String::new() } else { LOCAL_FLARESOLVERR.into() };
            }
            Row::NetTest => return self.sc_test("network", 0, String::new()),
            Row::Language => rotate_first(&mut s.ranking.languages, &LANGS),
            Row::AnimeLanguage => rotate_first(&mut s.anime.languages, &LANGS),
            Row::Resolution => rotate_first(&mut s.ranking.resolutions, &RESOLUTIONS),
            Row::CachedOnly => s.ranking.cached_only = !s.ranking.cached_only,
            Row::ExcludeCam => s.ranking.exclude_cam = !s.ranking.exclude_cam,
            Row::Phone => return self.sc_phone(),
        }
        self.sc_save();
    }

    fn sc_phone(&mut self) {
        if let Some(url) = self.web_url.clone() {
            return self.sc_show_qr(&url);
        }
        let on_save: scraper::web::OnSave = Arc::new(|_cfg| {
            post(|c| {
                c.sc_cfg = Sources::load();
                c.reload_sources();
            })
        });
        self.spawn(async move {
            match scraper::web::start(on_save).await {
                Ok(w) => post(move |c| {
                    c.web_url = Some(w.url.clone());
                    c.sc_show_qr(&w.url);
                    c.sync();
                }),
                Err(e) => post(move |c| c.toast(format!("Serveur de configuration : {e:#}"))),
            }
        });
    }

    fn sc_show_qr(&mut self, url: &str) {
        let ui = self.ui();
        ui.set_sc_qr(crate::qr::image(url));
        ui.set_sc_has_qr(true);
        let host = url.split('?').next().unwrap_or(url).trim_end_matches('/');
        self.sc_msg = format!("Téléphone sur le même Wi-Fi que le PC, données mobiles coupées.\nSi la page ne s'ouvre pas, saisissez l'adresse en commençant par http:// :\n{host}");
    }

    fn sc_edit(&mut self, e: Edit) {
        let fields = match &e {
            Edit::DebridKey | Edit::TmdbKey | Edit::TrackerKey(_) => vec![("Clé API", true, "collez ou tapez la clé", String::new())],
            Edit::FlareUrl => vec![("Adresse", false, "http://hôte:8191", self.sc_cfg.network.flaresolverr.replace(LOCAL_FLARESOLVERR, ""))],
            Edit::AddTracker { url, .. } => vec![("URL Torznab", false, "https://…/api/torznab", url.to_string()), ("Clé API", true, "clé de votre compte", String::new())],
        };
        self.open_text_form(e, fields);
    }

    /// Valeurs saisies dans le formulaire (voir `submit_form`).
    pub(super) fn apply_edit(&mut self, e: Edit, vals: Vec<String>) {
        let v0 = vals.first().cloned().unwrap_or_default();
        let s = &mut self.sc_cfg;
        match e {
            Edit::DebridKey => {
                s.debrid.api_key = v0;
                if s.debrid.service == DebridKind::None {
                    s.debrid.service = DebridKind::AllDebrid;
                }
            }
            Edit::TmdbKey => s.metadata.tmdb_key = v0,
            Edit::FlareUrl => s.network.flaresolverr = if v0.is_empty() { LOCAL_FLARESOLVERR.into() } else { v0 },
            Edit::TrackerKey(i) => {
                if let Some(ix) = s.indexers.get_mut(i) {
                    ix.api_key = v0;
                }
            }
            Edit::AddTracker { name, interval, .. } => {
                let key = vals.get(1).cloned().unwrap_or_default();
                if v0.is_empty() || key.is_empty() {
                    return self.toast("URL et clé API requises");
                }
                s.indexers.push(Indexer {
                    name: name.into(),
                    kind: IndexerKind::Torznab,
                    url: v0,
                    api_key: key,
                    enabled: true,
                    private: true,
                    min_interval_ms: interval,
                    anime: true,
                });
            }
        }
        self.sc_save();
        self.sc_msg = "✓ Enregistré".into();
    }
}
