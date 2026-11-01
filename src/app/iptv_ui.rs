//! Application IPTV : profils enregistrés (connexion rapide), formulaire Xtream Codes / M3U,
//! navigation par catégories + grille, recherche, favoris et lecture mpv.

use super::*;
use crate::iptv::{Catalog, Entry, Kind, Profile, Source, Store, Xtream};
use crate::{FormField, IptvProfile};

const TABS: [&str; 4] = ["TV en direct", "Films", "Séries", "Favoris"];
const TAB_KINDS: [Kind; 3] = [Kind::Live, Kind::Movie, Kind::Series];
const FAV_TAB: i32 = 3;

pub(super) struct Active {
    profile_id: String,
    catalog: Arc<Catalog>,
    /// (kind, id) → index dans `catalog.entries[kind]`
    index: HashMap<(Kind, String), usize>,
    counts: HashMap<(Kind, String), usize>,
}

struct Field {
    label: &'static str,
    value: String,
    secret: bool,
    placeholder: &'static str,
}

struct Form {
    m3u: bool,
    fields: Vec<Field>,
    focus: i32,
    kb: bool,
    kb_focus: i32,
    shift: bool,
    error: String,
    busy: bool,
}

pub(super) struct IptvState {
    store: Store,
    sel: i32,
    form: Option<Form>,
    active: Option<Active>,
    loading: bool,
    load_stamp: u64,
    /// Octets reçus pendant le téléchargement du catalogue.
    progress: Arc<std::sync::atomic::AtomicU64>,
    pending_open: Option<(Kind, String)>,

    tab: i32,
    area: i32,
    cat: i32,
    searching: bool,
    query: String,
    kb_focus: i32,
    filtered: Vec<(Kind, usize)>,
    grid: i32,
    first_row: i32,

    profiles_model: Rc<VecModel<IptvProfile>>,
    fields_model: Rc<VecModel<FormField>>,
    form_keys: Vec<String>,
    form_keys_model: Rc<VecModel<SharedString>>,
    cats_model: Rc<VecModel<SharedString>>,
    grid_model: Rc<VecModel<Card>>,
}

fn form_keys(shift: bool) -> Vec<String> {
    let mut k: Vec<String> = ('a'..='z')
        .map(|c| if shift { c.to_ascii_uppercase() } else { c }.to_string())
        .chain(('0'..='9').map(String::from))
        .collect();
    k.extend([".", ":", "/", "-", "_", "@", "?", "=", "&", "~", "⇧", "␣", "⌫", "Effacer"].map(String::from));
    k
}
const KB_COLS: i32 = 7;

impl IptvState {
    pub(super) fn new() -> Self {
        let keys = form_keys(false);
        Self {
            store: Store::load(),
            sel: 0,
            form: None,
            active: None,
            loading: false,
            load_stamp: 0,
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_open: None,
            tab: 0,
            area: 0,
            cat: 1,
            searching: false,
            query: String::new(),
            kb_focus: 0,
            filtered: vec![],
            grid: 0,
            first_row: 0,
            profiles_model: Rc::new(VecModel::default()),
            fields_model: Rc::new(VecModel::default()),
            form_keys_model: Rc::new(VecModel::from(keys.iter().map(|k| SharedString::from(k.as_str())).collect::<Vec<_>>())),
            form_keys: keys,
            cats_model: Rc::new(VecModel::default()),
            grid_model: Rc::new(VecModel::default()),
        }
    }

    pub(super) fn searching(&self) -> bool {
        self.searching
    }

    fn profile(&self, id: &str) -> Option<&Profile> {
        self.store.profiles.iter().find(|p| p.id == id)
    }

    fn active_profile(&self) -> Option<&Profile> {
        self.active.as_ref().and_then(|a| self.profile(&a.profile_id))
    }

    /// Mise en page de la grille : (colonnes, rangées visibles, style « vignette »).
    fn layout(&self) -> (i32, i32, bool) {
        match self.tab {
            0 | FAV_TAB => (5, 4, true),
            _ => (7, 2, false),
        }
    }

    fn entry(&self, kind: Kind, idx: usize) -> Option<&Entry> {
        self.active.as_ref()?.catalog.entries.get(&kind)?.get(idx)
    }

    fn focused(&self) -> Option<(Kind, Entry)> {
        let (k, i) = *self.filtered.get(self.grid as usize)?;
        self.entry(k, i).map(|e| (k, e.clone()))
    }

    /// Libellés de la colonne de gauche ; index 0 : recherche, 1 : tout.
    fn cat_labels(&self) -> Vec<String> {
        let Some(a) = &self.active else { return vec![] };
        let mut v = vec![
            if self.query.is_empty() { "Rechercher…".to_string() } else { format!("Recherche : {}", self.query) },
        ];
        if self.tab == FAV_TAB {
            let n = self.active_profile().map_or(0, |p| p.favorites.len());
            v.push(format!("Tous les favoris ({n})"));
            for (k, label) in [(Kind::Live, "Chaînes"), (Kind::Movie, "Films"), (Kind::Series, "Séries")] {
                let c = self.active_profile().map_or(0, |p| p.favorites.iter().filter(|f| f.starts_with(&format!("{}:", k.key()))).count());
                v.push(format!("{label} ({c})"));
            }
            return v;
        }
        let kind = TAB_KINDS[self.tab as usize];
        let total = a.catalog.entries.get(&kind).map_or(0, Vec::len);
        v.push(format!("Toutes ({total})"));
        for c in a.catalog.categories.get(&kind).map(Vec::as_slice).unwrap_or_default() {
            let n = a.counts.get(&(kind, c.id.clone())).copied().unwrap_or(0);
            if n > 0 {
                v.push(format!("{} ({n})", c.name));
            }
        }
        v
    }

    /// Catégories non vides, dans l'ordre de `cat_labels`.
    fn visible_categories(&self) -> Vec<String> {
        let Some(a) = &self.active else { return vec![] };
        if self.tab == FAV_TAB {
            return vec![];
        }
        let kind = TAB_KINDS[self.tab as usize];
        a.catalog
            .categories
            .get(&kind)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|c| a.counts.get(&(kind, c.id.clone())).copied().unwrap_or(0) > 0)
            .map(|c| c.id.clone())
            .collect()
    }

    fn refilter(&mut self) {
        let Some(a) = &self.active else {
            self.filtered.clear();
            return;
        };
        let q = self.query.to_lowercase();
        let matches = |e: &Entry| q.is_empty() || e.name.to_lowercase().contains(&q);
        self.filtered = if self.tab == FAV_TAB {
            let only = match self.cat {
                2 => Some(Kind::Live),
                3 => Some(Kind::Movie),
                4 => Some(Kind::Series),
                _ => None,
            };
            let favs = self.active_profile().map(|p| p.favorites.clone()).unwrap_or_default();
            favs.iter()
                .filter_map(|f| {
                    let (k, id) = f.split_once(':')?;
                    let kind = match k {
                        "live" => Kind::Live,
                        "movie" => Kind::Movie,
                        _ => Kind::Series,
                    };
                    if only.is_some_and(|o| o != kind) {
                        return None;
                    }
                    let idx = *a.index.get(&(kind, id.to_string()))?;
                    matches(&a.catalog.entries[&kind][idx]).then_some((kind, idx))
                })
                .collect()
        } else {
            let kind = TAB_KINDS[self.tab as usize];
            let cat = if self.cat >= 2 && q.is_empty() { self.visible_categories().get(self.cat as usize - 2).cloned() } else { None };
            a.catalog
                .entries
                .get(&kind)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .filter(|(_, e)| cat.as_ref().is_none_or(|c| &e.category == c) && matches(e))
                .map(|(i, _)| (kind, i))
                .collect()
        };
        self.grid = self.grid.min(self.filtered.len() as i32 - 1).max(0);
        self.first_row = 0;
        self.scroll_to_focus();
    }

    fn scroll_to_focus(&mut self) {
        let (cols, rows, _) = self.layout();
        let row = self.grid / cols;
        if row < self.first_row {
            self.first_row = row;
        } else if row >= self.first_row + rows {
            self.first_row = row - rows + 1;
        }
    }

    fn is_fav(&self, kind: Kind, id: &str) -> bool {
        self.active_profile().is_some_and(|p| p.favorites.iter().any(|f| *f == format!("{}:{id}", kind.key())))
    }
}

/// Branche les modèles IPTV sur la fenêtre.
pub(super) fn bind(ui: &AppWindow, c: &Controller) {
    let s = &c.iptv;
    ui.set_iptv_profiles(ModelRc::from(s.profiles_model.clone()));
    ui.set_form_fields(ModelRc::from(s.fields_model.clone()));
    ui.set_form_keys(ModelRc::from(s.form_keys_model.clone()));
    ui.set_iptv_cats(ModelRc::from(s.cats_model.clone()));
    ui.set_iptv_grid(ModelRc::from(s.grid_model.clone()));
    ui.set_iptv_tabs(ModelRc::from(Rc::new(VecModel::from(TABS.iter().map(|t| SharedString::from(*t)).collect::<Vec<_>>()))));
}

fn entry_card(kind: Kind, e: &Entry, fav: bool, cat_name: Option<&str>) -> CardInfo {
    let subtitle = match kind {
        Kind::Live => cat_name.unwrap_or_default().to_string(),
        _ => [e.year.clone().unwrap_or_default(), e.rating.clone().map(|r| format!("★ {r}")).unwrap_or_default()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  ·  "),
    };
    CardInfo {
        title: e.name.clone(),
        subtitle,
        image: e.logo.clone(),
        progress: -1.0,
        badge: if fav { "★".into() } else { String::new() },
        ..Default::default()
    }
}

/// « Groapa - S01E01 - Family » → « Family »
fn short_episode_title(t: &str, season: i64, number: i64) -> String {
    let tag = format!("S{season:02}E{number:02}");
    match t.find(&tag) {
        Some(i) => {
            let rest = t[i + tag.len()..].trim_start_matches([' ', '-', '·', ':']).trim();
            if rest.is_empty() { format!("Épisode {number}") } else { rest.to_string() }
        }
        None => t.to_string(),
    }
}

pub(super) fn episodes_of(info: &iptv::SeriesInfo, season: i64) -> Vec<(EpRef, EpInfo)> {
    info.episodes
        .iter()
        .filter(|e| e.season == season)
        .map(|e| {
            (
                EpRef::Iptv(e.clone()),
                EpInfo {
                    label: format!("S{:02}E{:02}", e.season, e.number),
                    title: short_episode_title(&e.title, e.season, e.number),
                    overview: e.plot.clone(),
                    image: e.image.clone().or_else(|| info.backdrop.clone()),
                    progress: -1.0,
                    watched: false,
                    local: false,
                },
            )
        })
        .collect()
}

/// Fiche d'un film ou d'une série IPTV.
pub(super) async fn load_detail(svc: &Services, r: IptvRef) -> anyhow::Result<DetailLoad> {
    let e = &r.entry;
    let mut meta = vec![];
    if let Some(y) = &e.year {
        meta.push(y.clone());
    }
    if let Some(rt) = &e.rating {
        meta.push(format!("★ {rt}"));
    }
    let mut d = DetailLoad {
        title: e.name.clone(),
        overview: e.plot.clone().unwrap_or_default(),
        badges: vec![if r.kind == Kind::Movie { "Film".into() } else { "Série".into() }, r.profile.name.clone()],
        poster: e.logo.clone(),
        backdrop: e.logo.clone(),
        ..Default::default()
    };
    if r.kind == Kind::Series && !r.episodes.is_empty() {
        // Série issue d'une playlist : épisodes déjà connus.
        let info = iptv::SeriesInfo { cover: e.logo.clone(), episodes: r.episodes.clone(), ..Default::default() };
        let mut seasons: Vec<i64> = info.episodes.iter().map(|x| x.season).collect();
        seasons.sort();
        seasons.dedup();
        d.badges.push(format!("{} épisodes", info.episodes.len()));
        d.seasons = seasons.iter().map(|s| (SeasonRef::Iptv(*s), format!("Saison {s}"))).collect();
        d.episodes = episodes_of(&info, seasons.first().copied().unwrap_or(1));
        d.iptv_info = Some(info);
    } else if let Source::Xtream { server, user, pass } = &r.profile.source {
        let x = Xtream::new(&svc.http, server, user, pass);
        match r.kind {
            Kind::Movie => {
                if let Ok((plot, backdrop, genre, duration)) = x.vod_info(&e.id).await {
                    if !plot.is_empty() {
                        d.overview = plot;
                    }
                    if backdrop.is_some() {
                        d.backdrop = backdrop;
                    }
                    if !genre.is_empty() {
                        meta.push(genre);
                    }
                    if !duration.is_empty() {
                        meta.push(duration);
                    }
                }
            }
            Kind::Series if e.url.is_none() => {
                let info = x.series_info(&e.id).await?;
                if !info.plot.is_empty() {
                    d.overview = info.plot.clone();
                }
                if info.backdrop.is_some() {
                    d.backdrop = info.backdrop.clone();
                }
                if info.cover.is_some() {
                    d.poster = info.cover.clone();
                }
                if !info.year.is_empty() && !meta.contains(&info.year) {
                    meta.insert(0, info.year.clone());
                }
                if !info.genre.is_empty() {
                    meta.push(info.genre.clone());
                }
                let mut seasons: Vec<i64> = info.episodes.iter().map(|e| e.season).collect();
                seasons.dedup();
                seasons.sort();
                seasons.dedup();
                d.badges.push(format!("{} épisodes", info.episodes.len()));
                d.seasons = seasons.iter().map(|s| (SeasonRef::Iptv(*s), format!("Saison {s}"))).collect();
                d.episodes = episodes_of(&info, seasons.first().copied().unwrap_or(1));
                d.iptv_info = Some(info);
            }
            _ => {}
        }
    }
    d.meta = meta.join("  ·  ");
    d.iptv = Some(r);
    Ok(d)
}

impl Controller {
    pub(super) fn open_iptv(&mut self) {
        self.iptv.sel = 0;
        self.go(SCR_IPTV_PROFILES);
    }

    pub(super) fn iptv_is_fav(&self, profile: &str, key: &str) -> bool {
        self.iptv.profile(profile).is_some_and(|p| p.favorites.iter().any(|f| f == key))
    }

    fn set_fav(&mut self, profile: &str, key: String) -> bool {
        let Some(p) = self.iptv.store.profiles.iter_mut().find(|p| p.id == profile) else { return false };
        let on = if let Some(i) = p.favorites.iter().position(|f| *f == key) {
            p.favorites.remove(i);
            false
        } else {
            p.favorites.insert(0, key);
            true
        };
        if let Err(e) = self.iptv.store.save() {
            self.toast(format!("{e:#}"));
        }
        self.toast(if on { "Ajouté aux favoris" } else { "Retiré des favoris" });
        on
    }

    pub(super) fn toggle_iptv_fav_detail(&mut self) {
        let Some(r) = self.detail.as_ref().and_then(|d| d.data.iptv.clone()) else { return };
        self.set_fav(&r.profile.id, r.key());
        self.rebuild_buttons();
    }

    // ─────────────── Profils ───────────────

    fn profile_rows(&self) -> Vec<IptvProfile> {
        let now = chrono::Local::now().timestamp();
        let mut v: Vec<IptvProfile> = self
            .iptv
            .store
            .profiles
            .iter()
            .map(|p| {
                let ago = match now - p.last_used {
                    _ if p.last_used == 0 => "jamais utilisé".to_string(),
                    d if d < 3600 => "utilisé il y a quelques minutes".into(),
                    d if d < 86_400 => format!("utilisé il y a {} h", d / 3600),
                    d => format!("utilisé il y a {} j", d / 86_400),
                };
                let host = match &p.source {
                    Source::Xtream { server, user, .. } => format!("{user} @ {}", server.trim_start_matches("http://").trim_start_matches("https://")),
                    Source::M3u { url } => url.split('?').next().unwrap_or_default().chars().take(60).collect(),
                };
                IptvProfile {
                    name: p.name.clone().into(),
                    kind: p.kind_label().into(),
                    detail: format!("{host}  ·  {} favoris  ·  {ago}", p.favorites.len()).into(),
                    action: false,
                }
            })
            .collect();
        v.push(IptvProfile { name: "Ajouter un compte Xtream Codes".into(), kind: "＋".into(), detail: "Serveur, identifiant et mot de passe".into(), action: true });
        v.push(IptvProfile { name: "Ajouter une playlist M3U".into(), kind: "＋".into(), detail: "URL http(s) ou fichier local".into(), action: true });
        v
    }

    pub(super) fn nav_iptv_profiles(&mut self, n: Nav) {
        let np = self.iptv.store.profiles.len() as i32;
        let len = np + 2;
        match n {
            Nav::Up => self.iptv.sel = (self.iptv.sel - 1).max(0),
            Nav::Down => self.iptv.sel = (self.iptv.sel + 1).min(len - 1),
            Nav::Back => self.go(SCR_DASH),
            Nav::Accept => {
                let s = self.iptv.sel;
                if s < np {
                    let id = self.iptv.store.profiles[s as usize].id.clone();
                    self.connect_profile(&id, None);
                } else {
                    self.open_form(s == np + 1);
                }
            }
            Nav::Fav if self.iptv.sel < np => {
                let id = self.iptv.store.profiles[self.iptv.sel as usize].id.clone();
                iptv::drop_cache(&id);
                if self.iptv.active.as_ref().is_some_and(|a| a.profile_id == id) {
                    self.iptv.active = None;
                }
                self.toast("Catalogue rechargé depuis le serveur");
                self.connect_profile(&id, None);
            }
            Nav::Alt if self.iptv.sel < np => {
                if self.confirmed(30 + self.iptv.sel) {
                    let p = self.iptv.store.profiles.remove(self.iptv.sel as usize);
                    let _ = self.iptv.store.save();
                    iptv::drop_cache(&p.id);
                    if self.iptv.active.as_ref().is_some_and(|a| a.profile_id == p.id) {
                        self.iptv.active = None;
                    }
                    self.toast(format!("Profil « {} » supprimé", p.name));
                    self.iptv.sel = self.iptv.sel.min(self.iptv.store.profiles.len() as i32 + 1);
                }
            }
            _ => {}
        }
    }

    /// Connexion à un profil (catalogue rechargé), puis ouverture éventuelle d'un élément.
    fn connect_profile(&mut self, id: &str, preloaded: Option<Catalog>) {
        let Some(p) = self.iptv.profile(id).cloned() else { return };
        if let Some(pp) = self.iptv.store.profiles.iter_mut().find(|x| x.id == p.id) {
            pp.last_used = chrono::Local::now().timestamp();
        }
        let _ = self.iptv.store.save();
        self.iptv.store.profiles.sort_by(|a, b| b.last_used.cmp(&a.last_used));
        self.iptv.sel = 0;

        let s = &mut self.iptv;
        let already = s.active.as_ref().is_some_and(|a| a.profile_id == p.id) && preloaded.is_none();
        s.tab = 0;
        s.area = 1;
        s.cat = 1;
        s.searching = false;
        s.query.clear();
        s.grid = 0;
        if self.screen != SCR_IPTV {
            if self.screen == SCR_IPTV_FORM {
                self.screen = SCR_IPTV_PROFILES;
            }
            self.go(SCR_IPTV);
        }
        if already {
            self.iptv.refilter();
            self.after_catalog();
            return;
        }
        self.iptv.active = None;
        self.iptv.loading = true;
        self.iptv.load_stamp += 1;
        let stamp = self.iptv.load_stamp;
        if let Some(cat) = preloaded {
            iptv::save_cache(&p.id, &cat);
            self.catalog_ready(stamp, p.id.clone(), Ok(cat));
            return;
        }
        let http = self.svc.http.clone();
        let progress = self.iptv.progress.clone();
        progress.store(0, std::sync::atomic::Ordering::Relaxed);
        self.start_progress_timer(stamp);
        self.spawn(async move {
            let id = p.id.clone();
            let cached = tokio::task::spawn_blocking(move || iptv::load_cache(&id)).await.ok().flatten();
            let res = match cached {
                Some(c) => Ok(c),
                None => {
                    let r = match &p.source {
                        Source::Xtream { server, user, pass } => Xtream::new(&http, server, user, pass).catalog(&progress).await,
                        Source::M3u { url } => iptv::load_m3u(url, &progress).await,
                    };
                    match r {
                        Ok(cat) => {
                            let id = p.id.clone();
                            tokio::task::spawn_blocking(move || {
                                iptv::save_cache(&id, &cat);
                                cat
                            })
                            .await
                            .map_err(anyhow::Error::from)
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            post(move |c| c.catalog_ready(stamp, p.id.clone(), res));
        });
    }

    /// Rafraîchit l'écran pendant le téléchargement (progression en Mo).
    fn start_progress_timer(&self, stamp: u64) {
        fn tick(stamp: u64) {
            slint::Timer::single_shot(Duration::from_millis(500), move || {
                with_ctrl(|c| {
                    if c.iptv.loading && c.iptv.load_stamp == stamp {
                        if c.screen == SCR_IPTV {
                            c.iptv_sync();
                        }
                        tick(stamp);
                    }
                })
            });
        }
        tick(stamp);
    }

    fn catalog_ready(&mut self, stamp: u64, profile_id: String, res: anyhow::Result<Catalog>) {
        if stamp != self.iptv.load_stamp {
            return;
        }
        self.iptv.loading = false;
        match res {
            Ok(cat) => {
                let mut index = HashMap::new();
                let mut counts: HashMap<(Kind, String), usize> = HashMap::new();
                for (k, list) in &cat.entries {
                    for (i, e) in list.iter().enumerate() {
                        index.insert((*k, e.id.clone()), i);
                        *counts.entry((*k, e.category.clone())).or_default() += 1;
                    }
                }
                // Onglet de départ : le premier type non vide.
                let first = TAB_KINDS.iter().position(|k| cat.entries.get(k).is_some_and(|v| !v.is_empty())).unwrap_or(0);
                self.iptv.tab = first as i32;
                self.iptv.active = Some(Active { profile_id, catalog: Arc::new(cat), index, counts });
                self.iptv.refilter();
                self.after_catalog();
            }
            Err(e) => {
                self.toast(format!("IPTV : {e:#}"));
                self.screen = SCR_IPTV_PROFILES;
            }
        }
        self.sync();
    }

    /// Élément demandé depuis l'historique avant que le catalogue soit chargé.
    fn after_catalog(&mut self) {
        let Some((kind, id)) = self.iptv.pending_open.take() else { return };
        let Some(a) = &self.iptv.active else { return };
        let Some(&idx) = a.index.get(&(kind, id)) else {
            self.toast("Élément introuvable dans ce profil");
            return;
        };
        self.iptv_open(kind, idx);
    }

    pub(super) fn open_iptv_history(&mut self, profile: &str, kind: &str, id: &str, h: &HistoryEntry) {
        let kind = match kind {
            "live" => Kind::Live,
            "movie" => Kind::Movie,
            _ => Kind::Series,
        };
        // Épisode de série : la clé d'historique vise l'épisode, `id` la série.
        let _ = h;
        if self.iptv.profile(profile).is_none() {
            self.toast("Profil IPTV supprimé");
            return;
        }
        self.iptv.pending_open = Some((kind, id.to_string()));
        self.connect_profile(profile, None);
    }

    // ─────────────── Formulaire ───────────────

    fn open_form(&mut self, m3u: bool) {
        let f = |label, secret, placeholder| Field { label, value: String::new(), secret, placeholder };
        let fields = if m3u {
            vec![f("Nom du profil", false, "Ma playlist"), f("URL de la playlist", false, "http://exemple.com/playlist.m3u ou /chemin/fichier.m3u")]
        } else {
            vec![
                f("Nom du profil", false, "Mon IPTV"),
                f("Serveur", false, "http://exemple.com:8080"),
                f("Utilisateur", false, "identifiant"),
                f("Mot de passe", true, "mot de passe"),
            ]
        };
        self.iptv.form = Some(Form { m3u, fields, focus: 0, kb: false, kb_focus: 0, shift: false, error: String::new(), busy: false });
        self.go(SCR_IPTV_FORM);
    }

    pub(super) fn nav_iptv_form(&mut self, n: Nav) {
        let Some(f) = self.iptv.form.as_mut() else { return self.back() };
        if f.busy {
            return;
        }
        let nf = f.fields.len() as i32;
        let nkeys = self.iptv.form_keys.len() as i32;
        let field = (f.focus < nf).then_some(f.focus as usize);
        match n {
            Nav::Char(ch) => {
                if let Some(i) = field {
                    f.fields[i].value.push(ch);
                }
            }
            Nav::Backspace => {
                if let Some(i) = field {
                    f.fields[i].value.pop();
                }
            }
            _ if f.kb => match n {
                Nav::Up if f.kb_focus >= KB_COLS => f.kb_focus -= KB_COLS,
                Nav::Down if f.kb_focus + KB_COLS < nkeys => f.kb_focus += KB_COLS,
                Nav::Down => f.kb_focus = nkeys - 1,
                Nav::Left if f.kb_focus % KB_COLS == 0 => f.kb = false,
                Nav::Left => f.kb_focus -= 1,
                Nav::Right if f.kb_focus % KB_COLS < KB_COLS - 1 && f.kb_focus < nkeys - 1 => f.kb_focus += 1,
                Nav::Back => f.kb = false,
                Nav::Accept => {
                    let k = self.iptv.form_keys[f.kb_focus as usize].clone();
                    let Some(i) = field else { return };
                    let v = &mut f.fields[i].value;
                    match k.as_str() {
                        "⇧" => {
                            f.shift = !f.shift;
                            self.iptv.form_keys = form_keys(f.shift);
                            self.iptv.form_keys_model.set_vec(self.iptv.form_keys.iter().map(|k| SharedString::from(k.as_str())).collect::<Vec<_>>());
                        }
                        "␣" => v.push(' '),
                        "⌫" => {
                            v.pop();
                        }
                        "Effacer" => v.clear(),
                        s => v.push_str(s),
                    }
                }
                _ => {}
            },
            Nav::Up => f.focus = (f.focus - 1).max(0),
            Nav::Down => f.focus = (f.focus + 1).min(nf + 1),
            Nav::Right if field.is_some() => f.kb = true,
            Nav::Back => {
                self.iptv.form = None;
                self.ui().set_form_has_qr(false);
                self.back();
            }
            Nav::Accept => match f.focus {
                x if x < nf => f.kb = true,
                x if x == nf => self.submit_form(),
                _ => {
                    self.iptv.form = None;
                    self.ui().set_form_has_qr(false);
                    self.back();
                }
            },
            _ => {}
        }
    }

    fn submit_form(&mut self) {
        let Some(f) = self.iptv.form.as_mut() else { return };
        let vals: Vec<String> = f.fields.iter().map(|x| x.value.trim().to_string()).collect();
        if vals.iter().skip(1).any(|v| v.is_empty()) {
            f.error = "Tous les champs sont requis".into();
            return;
        }
        let name = if vals[0].is_empty() { if f.m3u { "Playlist M3U".into() } else { "IPTV".into() } } else { vals[0].clone() };
        let source = if f.m3u {
            Source::M3u { url: vals[1].clone() }
        } else {
            let server = if vals[1].starts_with("http") { vals[1].clone() } else { format!("http://{}", vals[1]) };
            Source::Xtream { server, user: vals[2].clone(), pass: vals[3].clone() }
        };
        f.busy = true;
        f.error = "Connexion…".into();
        let http = self.svc.http.clone();
        self.spawn(async move {
            // Vérifie la connexion avant d'enregistrer ; la playlist chargée est réutilisée.
            let res: anyhow::Result<(Option<Catalog>, String)> = match &source {
                Source::Xtream { server, user, pass } => Xtream::new(&http, server, user, pass).account().await.map(|a| {
                    let exp = a
                        .expires
                        .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
                        .map(|d| format!(", expire le {}", d.format("%d/%m/%Y")))
                        .unwrap_or_default();
                    (None, format!("Compte {}{exp} · {} connexion(s)", a.status, a.max_connections.unwrap_or_else(|| "?".into())))
                }),
                Source::M3u { url } => iptv::load_m3u(url, &Default::default()).await.map(|c| {
                    let n: usize = c.entries.values().map(Vec::len).sum();
                    (Some(c), format!("{n} entrées"))
                }),
            };
            post(move |c| {
                let Some(f) = c.iptv.form.as_mut() else { return };
                f.busy = false;
                match res {
                    Ok((cat, info)) => {
                        let id = uuid::Uuid::new_v4().simple().to_string();
                        c.iptv.store.profiles.insert(0, Profile { id: id.clone(), name: name.clone(), source, last_used: 0, favorites: vec![] });
                        if let Err(e) = c.iptv.store.save() {
                            c.toast(format!("{e:#}"));
                        }
                        c.iptv.form = None;
                        c.toast(format!("« {name} » enregistré — {info}"));
                        c.connect_profile(&id, cat);
                    }
                    Err(e) => f.error = format!("Échec : {e:#}"),
                }
                c.sync();
            });
        });
    }

    // ─────────────── Navigation dans le catalogue ───────────────

    pub(super) fn nav_iptv(&mut self, n: Nav) {
        if self.iptv.loading {
            if n == Nav::Back {
                self.iptv.load_stamp += 1;
                self.iptv.loading = false;
                self.back();
            }
            return;
        }
        let (cols, _, _) = self.iptv.layout();
        let ncat = self.iptv.cat_labels().len() as i32;
        let nkeys = self.keys.len() as i32;
        let s = &mut self.iptv;
        let len = s.filtered.len() as i32;
        match n {
            Nav::PrevTab | Nav::NextTab => {
                s.tab = (s.tab + if n == Nav::PrevTab { -1 } else { 1 }).rem_euclid(TABS.len() as i32);
                s.cat = 1;
                s.grid = 0;
                s.refilter();
            }
            Nav::Char(ch) => {
                s.searching = true;
                s.area = 0;
                s.query.push(ch);
                s.cat = 0;
                s.refilter();
            }
            Nav::Backspace => {
                s.query.pop();
                s.refilter();
            }
            Nav::Back if s.searching => {
                s.searching = false;
                s.query.clear();
                s.cat = 1;
                s.refilter();
            }
            Nav::Back if s.area == 1 => s.area = 0,
            Nav::Back => self.back(),
            // Clavier de recherche (colonne de gauche)
            _ if s.area == 0 && s.searching => match n {
                Nav::Up if s.kb_focus >= 7 => s.kb_focus -= 7,
                Nav::Down if s.kb_focus + 7 < nkeys => s.kb_focus += 7,
                Nav::Down => s.kb_focus = nkeys - 1,
                Nav::Left if s.kb_focus % 7 > 0 => s.kb_focus -= 1,
                Nav::Right if s.kb_focus % 7 < 6 && s.kb_focus < nkeys - 1 => s.kb_focus += 1,
                Nav::Right if len > 0 => s.area = 1,
                Nav::Accept => {
                    let k = self.keys[s.kb_focus as usize].clone();
                    match k.as_str() {
                        "␣" => s.query.push(' '),
                        "⌫" => {
                            s.query.pop();
                        }
                        "Effacer" => s.query.clear(),
                        x => s.query.push_str(x),
                    }
                    s.refilter();
                }
                _ => {}
            },
            // Catégories
            _ if s.area == 0 => match n {
                Nav::Up if s.cat > 0 => {
                    s.cat -= 1;
                    if s.cat >= 1 {
                        s.grid = 0;
                        s.refilter();
                    }
                }
                Nav::Down if s.cat < ncat - 1 => {
                    s.cat += 1;
                    s.grid = 0;
                    s.refilter();
                }
                Nav::Right | Nav::Accept if s.cat == 0 => {
                    s.searching = true;
                    s.kb_focus = 0;
                }
                Nav::Right | Nav::Accept if len > 0 => s.area = 1,
                _ => {}
            },
            // Grille
            _ => match n {
                Nav::Left if s.grid % cols == 0 => s.area = 0,
                Nav::Left => s.grid -= 1,
                Nav::Right if s.grid + 1 < len && s.grid % cols < cols - 1 => s.grid += 1,
                Nav::Up if s.grid >= cols => s.grid -= cols,
                Nav::Down if s.grid + cols < len => s.grid += cols,
                Nav::Down if s.grid / cols < (len - 1) / cols => s.grid = len - 1,
                Nav::Accept | Nav::Alt => {
                    if let Some(&(k, i)) = s.filtered.get(s.grid as usize) {
                        self.iptv_open(k, i);
                    }
                    return;
                }
                Nav::Fav => {
                    if let (Some((k, e)), Some(pid)) = (s.focused(), s.active.as_ref().map(|a| a.profile_id.clone())) {
                        self.set_fav(&pid, format!("{}:{}", k.key(), e.id));
                        if self.iptv.tab == FAV_TAB {
                            self.iptv.refilter();
                        }
                    }
                    return;
                }
                _ => {}
            },
        }
        self.iptv.scroll_to_focus();
    }

    fn iptv_open(&mut self, kind: Kind, idx: usize) {
        let Some(e) = self.iptv.entry(kind, idx).cloned() else { return };
        let Some(p) = self.iptv.active_profile().cloned() else { return };
        if kind == Kind::Live {
            let key = format!("iptv:{}:live:{}", p.id, e.id);
            let t = PlayTarget::Direct { url: e.url.clone().unwrap_or_default(), title: e.name.clone(), key: key.clone(), live: true };
            let hist = HistoryEntry {
                key,
                title: e.name.clone(),
                subtitle: format!("En direct · {}", p.name),
                image: e.logo.clone(),
                backdrop: e.logo.clone(),
                open: OpenRef::Iptv { profile: p.id.clone(), kind: "live".into(), id: e.id.clone() },
                position: 0.0,
                duration: 0.0,
                finished: false,
                watched_at: 0,
            };
            let req = PlayRequest { url: e.url.clone().unwrap_or_default(), title: e.name.clone(), start_secs: 0.0, http_headers: vec![], report: None, live: true };
            self.played_ep = None;
            self.launch(t, req, None, Some(hist));
            return;
        }
        let episodes = match (kind, &self.iptv.active) {
            (Kind::Series, Some(a)) => a.catalog.series_episodes(&e.id),
            _ => vec![],
        };
        self.open_detail(MediaRef::Iptv(Box::new(IptvRef { profile: p, kind, entry: e, episodes })));
    }

    pub(super) fn iptv_click(&mut self, screen: i32, a: i32, b: i32) {
        match screen {
            SCR_IPTV_PROFILES => {
                self.iptv.sel = b;
                self.nav_iptv_profiles(Nav::Accept);
            }
            SCR_IPTV_FORM => {
                if let Some(f) = self.iptv.form.as_mut() {
                    if a < 0 {
                        f.kb = true;
                        f.kb_focus = b;
                    } else {
                        f.kb = false;
                        f.focus = b;
                    }
                }
                self.nav_iptv_form(Nav::Accept);
            }
            _ => match a {
                -2 => {
                    self.iptv.tab = b;
                    self.iptv.cat = 1;
                    self.iptv.refilter();
                }
                -1 => {
                    self.iptv.area = 0;
                    if self.iptv.searching {
                        self.iptv.kb_focus = b;
                    } else {
                        self.iptv.cat = b;
                        self.iptv.refilter();
                    }
                    self.nav_iptv(Nav::Accept);
                }
                _ => {
                    let (cols, _, _) = self.iptv.layout();
                    self.iptv.area = 1;
                    self.iptv.grid = self.iptv.first_row * cols + b;
                    self.nav_iptv(Nav::Accept);
                }
            },
        }
    }

    // ─────────────── Interface ───────────────

    pub(super) fn iptv_sync(&mut self) {
        let ui = self.ui();
        let rows = self.profile_rows();
        self.iptv.profiles_model.set_vec(rows);
        ui.set_iptv_sel(self.iptv.sel);

        if let Some(f) = &self.iptv.form {
            ui.set_form_submit("Se connecter et enregistrer".into());
            ui.set_form_title(
                if f.m3u {
                    "Nouvelle playlist M3U"
                } else {
                    "Nouveau compte Xtream Codes"
                }
                .into(),
            );
            self.iptv.fields_model.set_vec(
                f.fields
                    .iter()
                    .map(|x| FormField {
                        label: x.label.into(),
                        value: if x.secret { "•".repeat(x.value.chars().count()) } else { x.value.clone() }.into(),
                        placeholder: x.placeholder.into(),
                    })
                    .collect::<Vec<_>>(),
            );
            ui.set_form_focus(f.focus);
            ui.set_form_kb(f.kb);
            ui.set_form_key(f.kb_focus);
            ui.set_form_error(f.error.clone().into());
            ui.set_form_busy(f.busy);
        }

        let s = &self.iptv;
        let (cols, rows, tile) = s.layout();
        ui.set_iptv_loading(s.loading);
        ui.set_iptv_tab(s.tab);
        ui.set_iptv_area(s.area);
        ui.set_iptv_cat(s.cat);
        ui.set_iptv_searching(s.searching);
        ui.set_iptv_query(s.query.clone().into());
        ui.set_iptv_key(s.kb_focus);
        ui.set_iptv_cols(cols);
        ui.set_iptv_tile(tile);
        ui.set_iptv_profile_name(s.active_profile().map(|p| p.name.clone()).unwrap_or_default().into());
        let labels = s.cat_labels();
        let cur: Vec<SharedString> = (0..s.cats_model.row_count()).filter_map(|i| s.cats_model.row_data(i)).collect();
        if cur.len() != labels.len() || cur.iter().zip(&labels).any(|(a, b)| a.as_str() != b) {
            s.cats_model.set_vec(labels.into_iter().map(SharedString::from).collect::<Vec<_>>());
        }
        let n = s.filtered.len();
        ui.set_iptv_status(
            if s.loading {
                let mb = s.progress.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6;
                if mb > 0.5 { format!("Téléchargement du catalogue… {mb:.0} Mo") } else { "Chargement du catalogue…".to_string() }
            } else if n == 0 {
                if s.tab == FAV_TAB { "Aucun favori : Ⓧ sur une chaîne, un film ou une série".into() } else { "Aucun résultat".into() }
            } else {
                format!("{} / {n}", s.grid + 1)
            }
            .into(),
        );
        let start = (s.first_row * cols) as usize;
        let end = (start + (rows * cols) as usize).min(n);
        let cat_names: HashMap<String, String> = s
            .active
            .as_ref()
            .map(|a| a.catalog.categories.values().flatten().map(|c| (c.id.clone(), c.name.clone())).collect())
            .unwrap_or_default();
        let window: Vec<(Kind, Entry)> = s.filtered[start.min(n)..end].iter().filter_map(|&(k, i)| s.entry(k, i).map(|e| (k, e.clone()))).collect();
        let cards: Vec<Card> = window
            .iter()
            .map(|(k, e)| {
                let fav = self.iptv.is_fav(*k, &e.id);
                make_card(&entry_card(*k, e, fav, cat_names.get(&e.category).map(String::as_str)), &mut self.images)
            })
            .collect();
        self.iptv.grid_model.set_vec(cards);
        ui.set_iptv_grid_focus(if self.iptv.area == 1 { self.iptv.grid - self.iptv.first_row * cols } else { -1 });
    }

    pub(super) fn iptv_images(&self, url: &str, img: &Image) {
        update_card_model(&self.iptv.grid_model, url, img);
    }
}
