//! Contrôleur : état de navigation, chargements asynchrones et liaison avec l'interface Slint.
//!
//! Toute la logique vit sur le fil d'interface (`CTRL`, thread-local) ; les tâches tokio
//! renvoient leurs résultats avec `post`, qui s'exécute dans la boucle d'événements Slint.

mod iptv_ui;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Datelike, Timelike};
use slint::platform::Key;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak};
use tokio::runtime::Handle;

use crate::aio::{self, Aio, CatalogDef, Meta, StreamInfo, Video};
use crate::anilist::{self, AniList, Anime};
use crate::config::Config;
use crate::gamepad::{self, Pad};
use crate::jellyfin::{Item, Jellyfin, MediaSource, PlayReport};
use crate::player::{self, PlayOutcome, PlayRequest};
use crate::scraper::{self, Candidate, Engine, Query as SQuery};
use crate::session::{self, HdrStatus, Target};
use crate::iptv;
use crate::state::{AnimeProgress, Favorite, HistoryEntry, OpenRef, State};
use crate::{AppWindow, Card, EpisodeData, MediaRow, SourceData};

// ═══════════════════════════════ Services ═══════════════════════════════

pub struct Services {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub jf: Jellyfin,
    pub aio: Aio,
    pub ani: AniList,
    /// Moteur de sources intégré (trackers + débrideur).
    pub engine: Arc<Engine>,
    /// TMDB (si une clé est configurée) : textes français des animes.
    pub tmdb: Option<crate::tmdb::Tmdb>,
}

impl Services {
    fn new(cfg: Config, http: reqwest::Client) -> Arc<Self> {
        let j = &cfg.jellyfin;
        let sources = scraper::config::Sources::load();
        let tmdb = crate::tmdb::Tmdb::new(http.clone(), &sources.metadata.tmdb_key, &sources.metadata.language);
        Arc::new(Self {
            jf: Jellyfin::new(http.clone(), &j.url, &j.token, &j.user_id, &j.device_id),
            // Catalogues et fiches : TMDB si une clé est configurée, sinon AIOStreams, sinon Cinemeta.
            aio: Aio::new(
                http.clone(),
                if cfg.aiostreams.manifest_url.trim().is_empty() { "https://v3-cinemeta.strem.io/manifest.json" } else { &cfg.aiostreams.manifest_url },
            )
            .with_tmdb(tmdb.clone()),
            tmdb,
            engine: Engine::new(sources),
            ani: AniList::new(http.clone()),
            http,
            cfg,
        })
    }

    fn jf_ready(&self) -> bool {
        self.cfg.jellyfin_ready()
    }
}

// ═══════════════════════════════ Fil d'interface ═══════════════════════════════

thread_local! {
    static CTRL: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

fn with_ctrl(f: impl FnOnce(&mut Controller)) {
    CTRL.with(|c| {
        if let Ok(mut g) = c.try_borrow_mut() {
            if let Some(ctrl) = g.as_mut() {
                f(ctrl);
            }
        }
    });
}

/// Exécute `f` sur le contrôleur depuis n'importe quel fil.
fn post(f: impl FnOnce(&mut Controller) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_ctrl(f));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nav {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    PrevTab,
    NextTab,
    Menu,
    Alt,
    /// Bouton X : favori.
    Fav,
    Backspace,
    Char(char),
}

const SCR_DASH: i32 = 0;
const SCR_MEDIA: i32 = 1;
const SCR_DETAIL: i32 = 2;
const SCR_SOURCES: i32 = 3;
const SCR_SEARCH: i32 = 4;
const SCR_SETTINGS: i32 = 5;
const SCR_IPTV_PROFILES: i32 = 6;
const SCR_IPTV_FORM: i32 = 7;
const SCR_IPTV: i32 = 8;

const TAB_HOME: i32 = 0;
const TAB_ANIME: i32 = 1;
const TAB_MOVIES: i32 = 2;
const TAB_SHOWS: i32 = 3;
const TAB_HISTORY: i32 = 4;
const TAB_SEARCH: i32 = 5;
const TAB_COUNT: i32 = 6;

// ═══════════════════════════════ Images ═══════════════════════════════

struct Images {
    cache: HashMap<String, Image>,
    pending: HashSet<String>,
    rt: Handle,
    http: reqwest::Client,
}

impl Images {
    fn get(&mut self, url: Option<&str>) -> Image {
        let Some(url) = url.filter(|u| !u.is_empty()) else { return Image::default() };
        if let Some(i) = self.cache.get(url) {
            return i.clone();
        }
        if self.pending.insert(url.to_string()) {
            let http = self.http.clone();
            let url = url.to_string();
            self.rt.spawn(async move {
                match crate::images::fetch(&http, &url).await {
                    Ok(buf) => post(move |c| c.image_ready(url, buf)),
                    Err(e) => log::debug!("image {url}: {e:#}"),
                }
            });
        }
        Image::default()
    }
}

// ═══════════════════════════════ Rangées ═══════════════════════════════

#[derive(Clone)]
enum MediaRef {
    Jf(Item),
    Meta(Meta),
    Anime(Anime),
    AnimeId(i64),
    JfId(String),
    /// Entrée d'historique (clé).
    Hist(String),
    /// Carte d'action (1 : effacer l'historique).
    Action(u8),
    Iptv(Box<IptvRef>),
}

/// Film / série / chaîne d'un profil IPTV.
#[derive(Clone)]
pub(crate) struct IptvRef {
    profile: iptv::Profile,
    kind: iptv::Kind,
    entry: iptv::Entry,
    /// Épisodes connus localement (séries issues d'une playlist M3U).
    episodes: Vec<iptv::SeriesEpisode>,
}

impl IptvRef {
    fn key(&self) -> String {
        format!("{}:{}", self.kind.key(), self.entry.id)
    }
}

#[derive(Clone, Default)]
struct CardInfo {
    title: String,
    subtitle: String,
    image: Option<String>,
    backdrop: Option<String>,
    overview: String,
    meta: String,
    progress: f32,
    badge: String,
    watched: bool,
}

struct RowSlot {
    title: String,
    wide: bool,
    items: Vec<(MediaRef, CardInfo)>,
}

struct Rows {
    slots: Vec<RowSlot>,
    visible: Vec<usize>,
    cards: Vec<Rc<VecModel<Card>>>,
    model: Rc<VecModel<MediaRow>>,
    scrolls: Rc<VecModel<i32>>,
    row: i32,
    cols: Vec<i32>,
    stamp: u64,
    /// L'utilisateur a bougé : on cesse de recaler le focus sur la première rangée.
    touched: bool,
}

fn make_card(ci: &CardInfo, images: &mut Images) -> Card {
    Card {
        key: ci.image.clone().unwrap_or_default().into(),
        title: ci.title.clone().into(),
        subtitle: ci.subtitle.clone().into(),
        image: images.get(ci.image.as_deref()),
        progress: ci.progress,
        badge: ci.badge.clone().into(),
        watched: ci.watched,
    }
}

impl Rows {
    fn new(row: i32) -> Self {
        Self {
            slots: vec![],
            visible: vec![],
            cards: vec![],
            model: Rc::new(VecModel::default()),
            scrolls: Rc::new(VecModel::default()),
            row,
            cols: vec![],
            stamp: 0,
            touched: false,
        }
    }

    fn reset(&mut self, titles: Vec<(String, bool)>, images: &mut Images) -> u64 {
        self.stamp += 1;
        self.slots = titles.into_iter().map(|(title, wide)| RowSlot { title, wide, items: vec![] }).collect();
        self.cols.clear();
        self.visible.clear();
        self.row = self.row.min(0);
        self.touched = false;
        self.rebuild(images);
        self.stamp
    }

    fn fill(&mut self, stamp: u64, slot: usize, items: Vec<(MediaRef, CardInfo)>, images: &mut Images) -> bool {
        if stamp != self.stamp || slot >= self.slots.len() {
            return false;
        }
        self.slots[slot].items = items;
        self.rebuild(images);
        true
    }

    fn rebuild(&mut self, images: &mut Images) {
        let focused_slot = usize::try_from(self.row).ok().and_then(|r| self.visible.get(r).copied());
        let old_cols: HashMap<usize, i32> = self.visible.iter().copied().zip(self.cols.iter().copied()).collect();
        self.visible = (0..self.slots.len()).filter(|&i| !self.slots[i].items.is_empty()).collect();
        self.cards = self
            .visible
            .iter()
            .map(|&s| Rc::new(VecModel::from(self.slots[s].items.iter().map(|(_, ci)| make_card(ci, images)).collect::<Vec<_>>())))
            .collect();
        let rows: Vec<MediaRow> = self
            .visible
            .iter()
            .zip(&self.cards)
            .map(|(&s, m)| MediaRow { title: self.slots[s].title.clone().into(), wide: self.slots[s].wide, items: ModelRc::from(m.clone()) })
            .collect();
        self.model.set_vec(rows);
        self.cols = self
            .visible
            .iter()
            .map(|s| old_cols.get(s).copied().unwrap_or(0).min(self.slots[*s].items.len() as i32 - 1).max(0))
            .collect();
        self.scrolls.set_vec(self.cols.iter().map(|&c| scroll_for(c)).collect::<Vec<_>>());
        if self.row >= 0 && !self.touched {
            self.row = 0;
        } else if self.row >= 0 {
            self.row = match focused_slot.and_then(|f| self.visible.iter().position(|&s| s == f)) {
                Some(p) => p as i32,
                None => self.row.min(self.visible.len() as i32 - 1).max(0),
            };
        }
    }

    fn focused(&self) -> Option<&(MediaRef, CardInfo)> {
        let r = usize::try_from(self.row).ok()?;
        let s = *self.visible.get(r)?;
        self.slots[s].items.get(*self.cols.get(r)? as usize)
    }

    fn get(&self, r: i32, c: i32) -> Option<&(MediaRef, CardInfo)> {
        let s = *self.visible.get(usize::try_from(r).ok()?)?;
        self.slots[s].items.get(usize::try_from(c).ok()?)
    }

    fn col(&self) -> i32 {
        usize::try_from(self.row).ok().and_then(|r| self.cols.get(r).copied()).unwrap_or(0)
    }

    fn set_focus(&mut self, r: i32, c: i32) {
        self.row = r;
        if let Some(col) = usize::try_from(r).ok().and_then(|r| self.cols.get_mut(r)) {
            *col = c;
            self.scrolls.set_row_data(r as usize, scroll_for(c));
        }
    }

    /// Déplacement ; `allow_top` autorise la sortie vers le haut (rangée -1).
    fn nav(&mut self, n: Nav, allow_top: bool) -> bool {
        self.touched = true;
        let rows = self.visible.len() as i32;
        match n {
            Nav::Up if self.row > 0 => self.row -= 1,
            Nav::Up if self.row == 0 && allow_top => self.row = -1,
            Nav::Down if self.row < rows - 1 => self.row += 1,
            Nav::Left | Nav::Right if self.row >= 0 => {
                let r = self.row as usize;
                let len = self.slots[self.visible[r]].items.len() as i32;
                let c = self.cols[r] + if n == Nav::Left { -1 } else { 1 };
                if c < 0 || c >= len {
                    return false;
                }
                self.set_focus(self.row, c);
            }
            _ => return false,
        }
        true
    }

    fn update_images(&self, url: &str, img: &Image) {
        for m in &self.cards {
            update_card_model(m, url, img);
        }
    }
}

fn scroll_for(c: i32) -> i32 {
    (c - 1).max(0)
}

fn update_card_model(m: &VecModel<Card>, url: &str, img: &Image) {
    for i in 0..m.row_count() {
        if let Some(mut c) = m.row_data(i) {
            if c.key == url {
                c.image = img.clone();
                m.set_row_data(i, c);
            }
        }
    }
}

// ═══════════════════════════════ Fiche détaillée ═══════════════════════════════

#[derive(Clone)]
struct AnimeMark {
    id: i64,
    ep: i64,
    title: String,
    cover: Option<String>,
    total: Option<i64>,
}

#[derive(Clone)]
enum PlayTarget {
    Jf { item: Item, aio: Option<(String, String)>, anime: Option<AnimeMark>, label: String },
    Aio { kind: String, id: String, title: String, anime: Option<AnimeMark> },
    /// URL lue telle quelle (IPTV).
    Direct { url: String, title: String, key: String, live: bool },
}

impl PlayTarget {
    fn title(&self) -> String {
        match self {
            PlayTarget::Jf { label, .. } => label.clone(),
            PlayTarget::Aio { title, .. } | PlayTarget::Direct { title, .. } => title.clone(),
        }
    }
    fn is_anime(&self) -> bool {
        self.anime().is_some()
    }
    fn anime(&self) -> Option<&AnimeMark> {
        match self {
            PlayTarget::Jf { anime, .. } | PlayTarget::Aio { anime, .. } => anime.as_ref(),
            PlayTarget::Direct { .. } => None,
        }
    }
    /// Clé de reprise locale (positions / historique).
    fn stream_key(&self) -> String {
        match self {
            PlayTarget::Jf { item, .. } => format!("jf:{}", item.id),
            PlayTarget::Aio { id, .. } => format!("aio:{id}"),
            PlayTarget::Direct { key, .. } => key.clone(),
        }
    }
}

#[derive(Clone)]
enum SeasonRef {
    Jf(Item),
    /// Autre saison d'un anime (fiche AniList).
    Anime(i64),
    Stremio(i64),
    Iptv(i64),
}

#[derive(Clone)]
enum EpRef {
    Jf(Item),
    Anime { n: i64, jf: Option<Item> },
    Stremio(Video),
    Iptv(iptv::SeriesEpisode),
}

#[derive(Clone, Default)]
struct EpInfo {
    label: String,
    title: String,
    overview: String,
    image: Option<String>,
    progress: f32,
    watched: bool,
    local: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Play,
    Resume,
    Restart,
    Sources,
    ToggleWatched,
    Fav,
    IptvFav,
}

#[derive(Clone)]
struct AnimeCtx {
    anime: Anime,
    kitsu: Option<String>,
}

#[derive(Clone, Default)]
struct DetailLoad {
    title: String,
    meta: String,
    overview: String,
    badges: Vec<String>,
    backdrop: Option<String>,
    poster: Option<String>,
    seasons: Vec<(SeasonRef, String)>,
    season: usize,
    episodes: Vec<(EpRef, EpInfo)>,
    focus_ep: Option<usize>,
    item: Option<Item>,
    series: Option<Item>,
    anime: Option<AnimeCtx>,
    meta_obj: Option<Meta>,
    iptv: Option<IptvRef>,
    iptv_info: Option<iptv::SeriesInfo>,
}

struct Detail {
    root: MediaRef,
    data: DetailLoad,
    buttons: Vec<(String, Action)>,
    ep_model: Rc<VecModel<EpisodeData>>,
    area: i32,
    button: i32,
    ep: i32,
    season: i32,
    stamp: u64,
}

enum SourceEntry {
    Jf { item: Item, ms: MediaSource, session: Option<String> },
    Aio(StreamInfo),
    /// Résultat du moteur intégré (débridé au moment de la lecture).
    Torrent(Candidate),
}

// ═══════════════════════════════ Contrôleur ═══════════════════════════════

pub struct Controller {
    ui: Weak<AppWindow>,
    rt: Handle,
    svc: Arc<Services>,
    state: State,
    images: Images,
    playing: Arc<AtomicBool>,
    screen: i32,
    stack: Vec<i32>,
    toast_gen: u64,
    confirm: Option<(i32, std::time::Instant)>,

    // Tableau de bord
    dash_area: i32,
    dash_index: i32,
    dash_items: Vec<(MediaRef, CardInfo)>,
    dash_model: Rc<VecModel<Card>>,
    hdr: HdrStatus,

    // Médiathèque
    tab: i32,
    home: Rows,
    hero_url: Option<String>,
    /// Animes dont le résumé français a déjà été demandé.
    fr_requested: HashSet<i64>,
    /// Lecteur optique (interne ou USB) et disque inséré.
    disc: Option<crate::disc::Disc>,
    /// Lecture de disque en cours (pour expliquer un échec AACS).
    disc_playing: bool,
    /// Release GitHub plus récente que la version installée.
    update: Option<crate::update::Release>,
    update_busy: bool,
    catalogs: Vec<CatalogDef>,
    aio_name: String,

    // Fiche / sources
    detail: Option<Detail>,
    detail_gen: u64,
    sources: Vec<SourceEntry>,
    src_model: Rc<VecModel<SourceData>>,
    src_focus: i32,
    src_target: Option<PlayTarget>,
    src_gen: u64,

    // Recherche
    query: String,
    keys: Vec<String>,
    kb_focus: i32,
    kb_active: bool,
    search: Rows,
    search_gen: u64,

    // Paramètres
    set_focus: i32,
    qc_gen: u64,
    views: Vec<Item>,


    // Lecture : épisode lancé depuis la fiche et proposition de l'épisode suivant
    played_ep: Option<usize>,
    next_prompt: Option<(u64, usize)>,
    next_gen: u64,
    next_count: i32,

    iptv: iptv_ui::IptvState,

    // Moteur de sources : requête de l'écran Sources, page de configuration (téléphone)
    src_query: Option<SQuery>,
    web_url: Option<String>,

    // Application externe (VacuumTube) au premier plan, et fenêtre de confirmation
    external: bool,
    dialog: Option<Dialog>,
}

#[derive(Clone, Copy, PartialEq)]
enum Dialog {
    InstallVacuumTube { focus: i32, busy: bool },
}

pub fn install(ui: &AppWindow, rt: Handle, start_media: bool) {
    let cfg = Config::load();
    let http = reqwest::Client::builder()
        .user_agent(concat!("noosfin/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(45))
        .build()
        .expect("client HTTP");
    let svc = Services::new(cfg, http.clone());

    let mut keys: Vec<String> = ('a'..='z').chain('0'..='9').map(String::from).collect();
    keys.extend(["␣".into(), "⌫".into(), "Effacer".into()]);

    let playing = Arc::new(AtomicBool::new(false));
    let ctrl = Controller {
        ui: ui.as_weak(),
        rt: rt.clone(),
        svc,
        state: State::load(),
        images: Images { cache: HashMap::new(), pending: HashSet::new(), rt: rt.clone(), http },
        playing: playing.clone(),
        screen: SCR_DASH,
        stack: vec![],
        toast_gen: 0,
        confirm: None,
        dash_area: 0,
        dash_index: 0,
        dash_items: vec![],
        dash_model: Rc::new(VecModel::default()),
        hdr: HdrStatus::default(),
        tab: TAB_HOME,
        home: Rows::new(0),
        hero_url: None,
        fr_requested: HashSet::new(),
        update: None,
        update_busy: false,
        disc: None,
        disc_playing: false,
        catalogs: vec![],
        aio_name: String::new(),
        detail: None,
        detail_gen: 0,
        sources: vec![],
        src_model: Rc::new(VecModel::default()),
        src_focus: 0,
        src_target: None,
        src_gen: 0,
        query: String::new(),
        keys,
        kb_focus: 0,
        kb_active: true,
        search: Rows::new(0),
        search_gen: 0,
        set_focus: 0,
        qc_gen: 0,
        views: vec![],
        played_ep: None,
        next_prompt: None,
        next_gen: 0,
        next_count: 0,
        iptv: iptv_ui::IptvState::new(),
        external: false,
        dialog: None,
        src_query: None,
        web_url: None,
    };

    ui.set_dash_resume(ModelRc::from(ctrl.dash_model.clone()));
    ui.set_rows(ModelRc::from(ctrl.home.model.clone()));
    ui.set_scrolls(ModelRc::from(ctrl.home.scrolls.clone()));
    ui.set_q_rows(ModelRc::from(ctrl.search.model.clone()));
    ui.set_q_scrolls(ModelRc::from(ctrl.search.scrolls.clone()));
    ui.set_s_items(ModelRc::from(ctrl.src_model.clone()));
    ui.set_q_keys(ModelRc::from(Rc::new(VecModel::from(
        ctrl.keys.iter().map(|k| SharedString::from(k.as_str())).collect::<Vec<_>>(),
    ))));

    iptv_ui::bind(ui, &ctrl);
    CTRL.with(|c| *c.borrow_mut() = Some(ctrl));

    ui.on_key(|text| {
        let mut handled = false;
        with_ctrl(|c| handled = c.on_text_key(&text));
        handled
    });
    ui.on_click(|screen, a, b| with_ctrl(|c| c.on_click(screen, a, b)));

    // Pendant la lecture, les boutons sont relayés à l'overlay mpv (voir on_pad).
    let _ = playing;
    gamepad::spawn(&rt, Arc::new(AtomicBool::new(false)), |pad| post(move |c| c.on_pad(pad)));

    // Horloge
    let clock = slint::Timer::default();
    let weak = ui.as_weak();
    let tick = move || {
        if let Some(ui) = weak.upgrade() {
            let now = chrono::Local::now();
            ui.set_clock(format!("{:02}:{:02}", now.hour(), now.minute()).into());
            ui.set_date(french_date(&now).into());
        }
    };
    tick();
    clock.start(slint::TimerMode::Repeated, Duration::from_secs(1), tick);
    std::mem::forget(clock);

    // Lecteur optique : branchement USB et insertion de disque.
    let disc_timer = slint::Timer::default();
    disc_timer.start(slint::TimerMode::Repeated, Duration::from_secs(3), || {
        with_ctrl(|c| {
            c.spawn(async {
                let d = tokio::task::spawn_blocking(crate::disc::detect).await.ok().flatten();
                post(move |c| c.disc_changed(d));
            });
        });
    });
    std::mem::forget(disc_timer);

    with_ctrl(|c| {
        c.refresh_hdr();
        c.check_update(true);
        c.disc = crate::disc::detect();
        c.load_manifest();
        c.load_dashboard();
        c.load_tab(TAB_HOME);
        if start_media {
            c.go(SCR_MEDIA);
        }
        c.sync();
    });
}

/// Injection de touches pour le mode `--script`.
pub fn script_key(name: &str) {
    let nav = match name {
        "up" => Nav::Up,
        "down" => Nav::Down,
        "left" => Nav::Left,
        "right" => Nav::Right,
        "a" | "enter" => Nav::Accept,
        "b" | "back" => Nav::Back,
        "lb" => Nav::PrevTab,
        "rb" => Nav::NextTab,
        "y" => Nav::Alt,
        "x" => Nav::Fav,
        "menu" => Nav::Menu,
        s => match s.strip_prefix("type=") {
            Some(t) => {
                for ch in t.chars() {
                    with_ctrl(|c| c.on_nav(Nav::Char(ch)));
                }
                return;
            }
            None => return,
        },
    };
    with_ctrl(|c| c.on_nav(nav));
}

pub fn shutdown() {
    CTRL.with(|c| {
        if let Some(ctrl) = c.borrow().as_ref() {
            ctrl.state.save();
        }
    });
}

fn french_date(d: &chrono::DateTime<chrono::Local>) -> String {
    const DAYS: [&str; 7] = ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"];
    const MONTHS: [&str; 12] =
        ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre"];
    format!("{} {} {}", DAYS[d.weekday().num_days_from_monday() as usize], d.day(), MONTHS[d.month0() as usize])
}

// ─────────────────────────────── Fabrication des cartes ───────────────────────────────

fn runtime_label(ticks: Option<i64>) -> Option<String> {
    let mins = ticks? / 600_000_000;
    (mins > 0).then(|| if mins >= 60 { format!("{}h{:02}", mins / 60, mins % 60) } else { format!("{mins} min") })
}

fn jf_meta_line(it: &Item) -> String {
    let mut p = vec![];
    if let Some(y) = it.production_year {
        p.push(y.to_string());
    }
    if let Some(r) = runtime_label(it.run_time_ticks) {
        p.push(r);
    }
    if let Some(r) = it.community_rating {
        p.push(format!("★ {r:.1}"));
    }
    if !it.genres.is_empty() {
        p.push(it.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
    }
    p.join("  ·  ")
}

fn jf_card(jf: &Jellyfin, it: &Item, wide: bool) -> CardInfo {
    let (title, subtitle) = if it.kind == "Episode" {
        (it.series_name.clone().unwrap_or_default(), format!("{}  {}", it.episode_label(), it.name))
    } else {
        (it.name.clone(), it.production_year.map(|y| y.to_string()).unwrap_or_default())
    };
    let image = if wide {
        it.image_tags
            .get("Thumb")
            .map(|t| jf.image_url(&it.id, "Thumb", Some(t), 360))
            .or_else(|| jf.backdrop_of(it).map(|u| u.replace("maxWidth=1920", "maxWidth=720")))
            .or_else(|| jf.poster_of(it))
    } else {
        jf.series_poster_of(it)
    };
    let unplayed = it.user_data.as_ref().and_then(|u| u.unplayed_item_count).unwrap_or(0);
    CardInfo {
        title,
        subtitle,
        image,
        backdrop: jf.backdrop_of(it),
        overview: it.overview.clone().unwrap_or_default(),
        meta: jf_meta_line(it),
        progress: it.progress(),
        badge: if it.kind == "Series" && unplayed > 0 { unplayed.to_string() } else { String::new() },
        watched: it.played() && it.kind != "Series",
    }
}

fn meta_card(m: &Meta) -> CardInfo {
    let mut meta = vec![];
    if let Some(r) = &m.release_info {
        meta.push(r.clone());
    }
    if let Some(r) = &m.imdb_rating {
        meta.push(format!("★ {r}"));
    }
    if !m.genres.is_empty() {
        meta.push(m.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
    }
    CardInfo {
        title: m.name.clone(),
        subtitle: m.release_info.clone().unwrap_or_default(),
        image: m.poster.clone(),
        backdrop: m.background.clone(),
        overview: m.description.clone().unwrap_or_default(),
        meta: meta.join("  ·  "),
        progress: -1.0,
        badge: String::new(),
        watched: false,
    }
}

fn anime_card(a: &Anime, state: &State) -> CardInfo {
    let prog = state.anime.get(&a.id);
    let aired = a.aired_episodes();
    let subtitle = match (prog, a.next_airing.as_ref()) {
        (Some(p), _) => format!("Vu jusqu'à l'ép. {}", p.last_watched),
        (None, Some(n)) => format!("Ép. {} bientôt", n.episode),
        (None, None) => a.season_year.map(|y| y.to_string()).unwrap_or_default(),
    };
    let new_eps = prog.map_or(0, |p| aired - p.last_watched);
    CardInfo {
        title: a.display_title(),
        subtitle,
        image: a.cover_url(),
        backdrop: a.banner.clone().or_else(|| a.cover_url()),
        overview: fr_overview(a.id).unwrap_or_else(|| a.clean_description()),
        meta: a.meta_line(),
        progress: match (prog, a.episodes) {
            (Some(p), Some(t)) if t > 0 => p.last_watched as f32 / t as f32,
            _ => -1.0,
        },
        badge: if new_eps > 0 { format!("+{new_eps}") } else { String::new() },
        watched: false,
    }
}

fn progress_card(id: i64, p: &AnimeProgress) -> (MediaRef, CardInfo) {
    (
        MediaRef::AnimeId(id),
        CardInfo {
            title: p.title.clone(),
            subtitle: format!("Prochain : épisode {}", p.last_watched + 1),
            image: p.cover.clone(),
            backdrop: p.cover.clone(),
            overview: String::new(),
            meta: format!("Vu jusqu'à l'épisode {}{}", p.last_watched, p.total.map(|t| format!(" / {t}")).unwrap_or_default()),
            progress: p.total.map_or(-1.0, |t| p.last_watched as f32 / t.max(1) as f32),
            badge: String::new(),
            watched: false,
        },
    )
}

/// Carte d'un favori local.
fn fav_card(f: &Favorite) -> Option<(MediaRef, CardInfo)> {
    let r = match &f.open {
        OpenRef::Anime { id } => MediaRef::AnimeId(*id),
        OpenRef::Jf { id } => MediaRef::JfId(id.clone()),
        OpenRef::Meta { kind, id } => MediaRef::Meta(Meta {
            id: id.clone(),
            kind: kind.clone(),
            name: f.title.clone(),
            poster: f.image.clone(),
            background: f.backdrop.clone(),
            ..Default::default()
        }),
        OpenRef::Iptv { .. } => return None,
    };
    Some((
        r,
        CardInfo {
            title: f.title.clone(),
            subtitle: f.subtitle.clone(),
            image: f.image.clone(),
            backdrop: f.backdrop.clone(),
            overview: String::new(),
            meta: f.subtitle.clone(),
            progress: -1.0,
            badge: String::new(),
            watched: false,
        },
    ))
}

fn hist_card(h: &HistoryEntry) -> (MediaRef, CardInfo) {
    let remaining = if h.finished {
        "Terminé".to_string()
    } else if h.duration > 0.0 {
        format!("{} min restantes", ((h.duration - h.position) / 60.0).ceil() as i64)
    } else {
        "En direct".to_string()
    };
    let when = chrono::DateTime::from_timestamp(h.watched_at, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default();
    (
        MediaRef::Hist(h.key.clone()),
        CardInfo {
            title: h.title.clone(),
            subtitle: [h.subtitle.as_str(), remaining.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("  ·  "),
            image: h.backdrop.clone().or(h.image.clone()),
            backdrop: h.backdrop.clone().or(h.image.clone()),
            overview: h.subtitle.clone(),
            meta: format!("Regardé à {when}  ·  {remaining}"),
            progress: if h.duration > 0.0 && !h.finished { (h.position / h.duration) as f32 } else { -1.0 },
            badge: String::new(),
            watched: h.finished,
        },
    )
}

fn video_badges(it: &Item) -> Vec<String> {
    let mut b = vec![];
    if let Some(v) = it.media_streams.iter().find(|s| s.kind == "Video") {
        if let Some(l) = v.resolution_label() {
            b.push(l.to_string());
        }
        if let Some(r) = &v.video_range_type {
            let l = if r.contains("DOVI") {
                "Dolby Vision"
            } else if r.contains("HDR10Plus") {
                "HDR10+"
            } else if r.contains("HDR10") {
                "HDR10"
            } else if r.contains("HLG") {
                "HLG"
            } else {
                ""
            };
            if !l.is_empty() {
                b.push(l.into());
            }
        }
    }
    if let Some(a) = it.media_streams.iter().find(|s| s.kind == "Audio") {
        let mut s = a.codec.clone().unwrap_or_default().to_uppercase();
        if let Some(c) = a.channels {
            s.push_str(&format!(" {}", if c >= 8 { "7.1" } else if c >= 6 { "5.1" } else { "2.0" }));
        }
        if !s.trim().is_empty() {
            b.push(s);
        }
    }
    if let Some(r) = &it.official_rating {
        b.push(r.clone());
    }
    b
}

fn jf_ep_info(jf: &Jellyfin, e: &Item) -> EpInfo {
    let image = e
        .image_tags
        .get("Primary")
        .map(|t| jf.image_url(&e.id, "Primary", Some(t), 300))
        .or_else(|| jf.backdrop_of(e).map(|u| u.replace("maxWidth=1920", "maxWidth=480")));
    EpInfo {
        label: e.episode_label(),
        title: e.name.clone(),
        overview: e.overview.clone().unwrap_or_default(),
        image,
        progress: e.progress(),
        watched: e.played(),
        local: true,
    }
}

fn source_data(e: &SourceEntry) -> SourceData {
    match e {
        SourceEntry::Torrent(c) => {
            let i = &c.info;
            let hdr = i
                .hdr
                .iter()
                .map(|h| match *h {
                    "dv" => "DV",
                    "hdr10+" => "HDR10+",
                    "hdr10" => "HDR10",
                    _ => "HLG",
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let mut tags: Vec<String> = i.languages.iter().filter(|l| **l != "vo" || i.languages.len() == 1).map(|l| l.to_uppercase()).collect();
            if i.subtitles.contains(&"fr") && !i.languages.contains(&"vostfr") {
                tags.push("ST FR".into());
            }
            if i.pack {
                tags.push("Pack".into());
            }
            SourceData {
                quality: match i.resolution {
                    Some("2160p") => "4K",
                    Some("1080p") => "1080p",
                    Some("720p") => "720p",
                    Some(_) => "SD",
                    None => "?",
                }
                .into(),
                hdr: hdr.into(),
                provider: c.source.clone().into(),
                filename: c.title.clone().into(),
                details: [i.quality.unwrap_or_default().to_uppercase(), i.codec.unwrap_or_default().to_uppercase(), i.audio.join(" ")]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join("  ·  ")
                    .into(),
                size: if c.size > 0 { format!("{:.1} Go", c.size as f64 / 1e9) } else { String::new() }.into(),
                langs: c.seeders.map(|s| format!("{s} seeders")).unwrap_or_default().into(),
                tags: tags.join(" · ").into(),
                origin: match c.cached {
                    Some(true) => "⚡ En cache".into(),
                    Some(false) => "Pas en cache".into(),
                    None => "Torrent".into(),
                },
            }
        }
        SourceEntry::Aio(s) => SourceData {
            quality: s.quality.clone().into(),
            hdr: s.hdr.clone().into(),
            provider: s.provider.clone().into(),
            filename: s.filename.clone().into(),
            details: [s.source.as_str(), s.codec.as_str(), s.audio.as_str()]
                .iter()
                .filter(|x| !x.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("  ·  ")
                .into(),
            size: s.size.clone().into(),
            langs: s.langs.clone().into(),
            tags: s.tags.join(" · ").into(),
            origin: "AIOStreams".into(),
        },
        SourceEntry::Jf { ms, .. } => {
            let v = ms.media_streams.iter().find(|s| s.kind == "Video");

            let range = v.and_then(|v| v.video_range_type.clone()).unwrap_or_default();
            let hdr = if range.contains("DOVI") {
                "DV"
            } else if range.contains("HDR10Plus") {
                "HDR10+"
            } else if range.contains("HDR") {
                "HDR10"
            } else if range.contains("HLG") {
                "HLG"
            } else {
                ""
            };
            let audio: Vec<String> = ms
                .media_streams
                .iter()
                .filter(|s| s.kind == "Audio")
                .filter_map(|s| s.language.clone())
                .collect::<Vec<_>>();
            let details = ms
                .media_streams
                .iter()
                .filter(|s| s.kind == "Video" || s.kind == "Audio")
                .filter_map(|s| s.display_title.clone())
                .take(2)
                .collect::<Vec<_>>()
                .join("  ·  ");
            let path = ms.path.clone().unwrap_or_default();
            let file = if path.starts_with("http") { ms.name.clone().unwrap_or_default() } else { path.rsplit('/').next().unwrap_or_default().to_string() };
            SourceData {
                quality: v.and_then(|v| v.resolution_label()).or_else(|| {
                    crate::aio::resolution_of(ms.name.as_deref().unwrap_or_default()).map(|h| match h {
                        2160.. => "4K",
                        1000.. => "1080p",
                        700.. => "720p",
                        _ => "SD",
                    })
                }).unwrap_or("?").into(),
                hdr: hdr.into(),
                provider: ms.name.clone().unwrap_or_else(|| "Jellyfin".into()).into(),
                filename: file.into(),
                details: details.into(),
                size: ms.size.map(|b| format!("{:.1} Go", b as f64 / 1e9)).unwrap_or_default().into(),
                langs: audio.join(", ").into(),
                tags: ms.container.clone().unwrap_or_default().to_uppercase().into(),
                origin: if ms.protocol == "Http" { "Gelato".into() } else { "Jellyfin".into() },
            }
        }
    }
}

// ─────────────────────────────── Implémentation ───────────────────────────────

impl Controller {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("fenêtre")
    }

    fn spawn<F>(&self, f: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(f);
    }

    fn toast(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.toast_gen += 1;
        let stamp = self.toast_gen;
        self.ui().set_toast(msg.into());
        slint::Timer::single_shot(Duration::from_millis(3800), move || {
            with_ctrl(|c| {
                if c.toast_gen == stamp {
                    c.ui().set_toast("".into());
                }
            })
        });
    }

    fn go(&mut self, screen: i32) {
        if screen != self.screen {
            if screen == SCR_DASH {
                self.stack.clear();
            } else {
                self.stack.push(self.screen);
            }
            self.screen = screen;
        }
        self.sync();
    }

    fn back(&mut self) {
        self.screen = self.stack.pop().unwrap_or(SCR_DASH);
        if self.screen == SCR_SEARCH && self.tab != TAB_SEARCH {
            self.screen = SCR_MEDIA;
        }
        self.sync();
    }

    /// Pousse l'état courant vers l'interface.
    fn sync(&mut self) {
        let ui = self.ui();
        ui.set_screen(self.screen);
        ui.set_dash_area(self.dash_area);
        ui.set_dash_tiles(ModelRc::from(Rc::new(VecModel::from(self.dash_tiles()))));
        ui.set_dash_disc(self.disc.as_ref().map(|d| d.subtitle()).unwrap_or_default().into());
        ui.set_dash_index(self.dash_index);
        ui.set_hdr_on(self.hdr.enabled);
        ui.set_hdr_available(self.hdr.available);
        ui.set_hdr_output(self.hdr.output.clone().into());
        ui.set_media_status("Rien en cours de lecture".into());
        ui.set_tab(self.tab);
        ui.set_focus_row(self.home.row);
        ui.set_focus_col(self.home.col());
        ui.set_q(self.query.clone().into());
        ui.set_q_key(self.kb_focus);
        ui.set_q_keyboard(self.kb_active);
        ui.set_q_row(self.search.row);
        ui.set_q_col(self.search.col());
        ui.set_s_focus(self.src_focus);
        ui.set_set_focus(self.set_focus);
        if let Some(d) = &self.detail {
            ui.set_d_area(d.area);
            ui.set_d_button(d.button);
            ui.set_d_episode(d.ep);
            ui.set_d_season(d.season);
        }
        ui.set_hints(
            match self.screen {
                SCR_MEDIA if self.tab == TAB_HISTORY => "Ⓐ Ouvrir    Ⓨ Retirer    Ⓑ Accueil    LB / RB Onglets",
                SCR_MEDIA => "Ⓐ Ouvrir    Ⓑ Accueil    LB / RB Onglets",
                SCR_DETAIL => "Ⓐ Lire    Ⓨ Sources    Ⓧ Favori    Ⓑ Retour",
                SCR_SOURCES => "Ⓐ Lire cette source    Ⓑ Retour",
                SCR_SEARCH => "Ⓐ Saisir / ouvrir    Ⓑ Retour",
                SCR_IPTV_PROFILES => "Ⓐ Se connecter    Ⓧ Recharger le catalogue    Ⓨ Supprimer    Ⓑ Accueil",
                SCR_IPTV_FORM => "Ⓐ Saisir    → Clavier    Ⓑ Annuler",
                SCR_IPTV => "Ⓐ Lire / ouvrir    Ⓧ Favori    LB / RB Onglets    Ⓑ Profils",
                _ => "",
            }
            .into(),
        );
        ui.set_next_visible(self.next_prompt.is_some());
        match self.dialog {
            Some(Dialog::InstallVacuumTube { focus, busy }) => {
                ui.set_dlg_visible(true);
                ui.set_dlg_title("VacuumTube n'est pas installé".into());
                ui.set_dlg_text(
                    if busy {
                        "Téléchargement et installation depuis Flathub… (quelques minutes)"
                    } else {
                        "VacuumTube est l'interface YouTube pour TV, pilotable à la manette.\nLe télécharger et l'installer depuis Flathub (rocks.shy.VacuumTube) ?"
                    }
                    .into(),
                );
                ui.set_dlg_focus(focus);
                ui.set_dlg_busy(busy);
            }
            None => ui.set_dlg_visible(false),
        }
        ui.set_next_count(self.next_count);
        if self.screen == SCR_MEDIA {
            self.update_hero();
        }
        if self.screen == SCR_SETTINGS {
            self.sync_settings();
        }
        if matches!(self.screen, SCR_IPTV_PROFILES | SCR_IPTV_FORM | SCR_IPTV) {
            self.iptv_sync();
        }
    }

    /// Les écrans à saisie reçoivent les caractères du clavier physique.
    fn accepts_text(&self) -> bool {
        self.screen == SCR_SEARCH || self.screen == SCR_IPTV_FORM || (self.screen == SCR_IPTV && self.iptv.searching())
    }

    // ─────────────── Entrées ───────────────

    fn on_text_key(&mut self, text: &str) -> bool {
        let Some(c) = text.chars().next() else { return false };
        let k = |key: Key| char::from(key);
        let nav = match c {
            _ if c == k(Key::UpArrow) => Nav::Up,
            _ if c == k(Key::DownArrow) => Nav::Down,
            _ if c == k(Key::LeftArrow) => Nav::Left,
            _ if c == k(Key::RightArrow) => Nav::Right,
            _ if c == k(Key::Return) || c == '\r' => Nav::Accept,
            _ if c == k(Key::Escape) => Nav::Back,
            _ if c == k(Key::Backspace) => {
                if self.accepts_text() {
                    Nav::Backspace
                } else {
                    Nav::Back
                }
            }
            _ if c == k(Key::PageUp) => Nav::PrevTab,
            _ if c == k(Key::PageDown) => Nav::NextTab,
            _ if c == k(Key::Tab) => Nav::Alt,
            _ if c == k(Key::Home) || c == k(Key::Menu) => Nav::Menu,
            ' ' if !self.accepts_text() => Nav::Accept,
            'f' | 'F' if !self.accepts_text() => Nav::Fav,
            c if self.accepts_text() && !c.is_control() && (c as u32) < 0xF700 => Nav::Char(c),
            _ => return false,
        };
        if self.playing.load(Ordering::Relaxed) {
            return true;
        }
        self.on_nav(nav);
        true
    }

    /// Bouton de manette : relayé à l'overlay mpv pendant la lecture, sinon navigation.
    fn on_pad(&mut self, pad: Pad) {
        if self.external {
            // VacuumTube gère lui-même la manette.
            return;
        }
        if self.playing.load(Ordering::Relaxed) {
            let name = match pad {
                Pad::Up => "up",
                Pad::Down => "down",
                Pad::Left => "left",
                Pad::Right => "right",
                Pad::Accept => "accept",
                Pad::Back => "back",
                Pad::PrevTab => "lb",
                Pad::NextTab => "rb",
                Pad::Menu => "menu",
                Pad::Select => "select",
                Pad::X => "x",
                Pad::Y => "y",
            };
            let sock = self.svc.cfg.player.ipc_socket.clone();
            self.spawn(async move { player::send_pad(&sock, name).await });
            return;
        }
        self.on_nav(match pad {
            Pad::Up => Nav::Up,
            Pad::Down => Nav::Down,
            Pad::Left => Nav::Left,
            Pad::Right => Nav::Right,
            Pad::Accept => Nav::Accept,
            Pad::Back => Nav::Back,
            Pad::PrevTab => Nav::PrevTab,
            Pad::NextTab => Nav::NextTab,
            Pad::Menu | Pad::Select => Nav::Menu,
            Pad::Y => Nav::Alt,
            Pad::X => Nav::Fav,
        });
    }

    fn on_nav(&mut self, n: Nav) {
        if self.playing.load(Ordering::Relaxed) {
            return;
        }
        if self.external {
            return;
        }
        if let Some(d) = self.dialog {
            self.nav_dialog(d, n);
            self.sync();
            return;
        }
        if self.next_prompt.is_some() {
            match n {
                Nav::Accept => self.play_next_now(),
                Nav::Back | Nav::Menu => self.cancel_next(),
                _ => {}
            }
            self.sync();
            return;
        }
        if n == Nav::Menu {
            self.go(SCR_DASH);
            return;
        }
        match self.screen {
            SCR_DASH => self.nav_dash(n),
            SCR_MEDIA => self.nav_media(n),
            SCR_DETAIL => self.nav_detail(n),
            SCR_SOURCES => self.nav_sources(n),
            SCR_SEARCH => self.nav_search(n),
            SCR_SETTINGS => self.nav_settings(n),
            SCR_IPTV_PROFILES => self.nav_iptv_profiles(n),
            SCR_IPTV_FORM => self.nav_iptv_form(n),
            SCR_IPTV => self.nav_iptv(n),
            _ => {}
        }
        self.sync();
    }

    fn on_click(&mut self, screen: i32, a: i32, b: i32) {
        match screen {
            0 => {
                self.dash_area = a;
                self.dash_index = b;
                self.nav_dash(Nav::Accept);
            }
            1 if a < 0 => self.switch_tab(b),
            1 => {
                self.home.set_focus(a, b);
                self.nav_media(Nav::Accept);
            }
            2 => {
                if let Some(d) = self.detail.as_mut() {
                    d.area = a;
                    match a {
                        0 => d.button = b,
                        1 => d.season = b,
                        _ => d.ep = b,
                    }
                }
                self.nav_detail(Nav::Accept);
            }
            3 => {
                self.src_focus = b;
                self.nav_sources(Nav::Accept);
            }
            4 if a < 0 => {
                self.kb_active = true;
                self.kb_focus = b;
                self.nav_search(Nav::Accept);
            }
            4 => {
                self.kb_active = false;
                self.search.set_focus(a, b);
                self.nav_search(Nav::Accept);
            }
            5 => {
                self.set_focus = b;
                self.nav_settings(Nav::Accept);
            }
            6..=8 => self.iptv_click(screen, a, b),
            9 => self.play_next_now(),
            10 => {
                if let Some(Dialog::InstallVacuumTube { busy, .. }) = self.dialog {
                    self.nav_dialog(Dialog::InstallVacuumTube { focus: b, busy }, Nav::Accept);
                }
            }
            _ => {}
        }
        self.sync();
    }

    // ─────────────── Tableau de bord ───────────────

    fn nav_dash(&mut self, n: Nav) {
        let resume_len = self.dash_items.len() as i32;
        let tiles = self.dash_tiles();
        let area_len = |a: i32| match a {
            0 => tiles.len() as i32,
            1 => resume_len,
            _ => 5,
        };
        match n {
            Nav::Left if self.dash_index > 0 => self.dash_index -= 1,
            Nav::Right if self.dash_index < area_len(self.dash_area) - 1 => self.dash_index += 1,
            Nav::Up | Nav::Down => {
                // Ordre à l'écran, de haut en bas : barre système (2), tuiles (0), reprise (1).
                const ROWS: [i32; 3] = [2, 0, 1];
                let rows: Vec<i32> = ROWS.into_iter().filter(|a| *a != 1 || resume_len > 0).collect();
                let pos = rows.iter().position(|a| *a == self.dash_area).unwrap_or(1) as i32;
                let next = pos + if n == Nav::Up { -1 } else { 1 };
                if let Some(&a) = rows.get(next.max(0) as usize).filter(|_| next >= 0) {
                    self.dash_area = a;
                    self.dash_index = self.dash_index.min(area_len(a) - 1).max(0);
                }
            }
            // Tuiles variables (disque, Steam) : on raisonne par identifiant de tuile.
            Nav::Accept => match (self.dash_area, if self.dash_area == 0 { tiles.get(self.dash_index as usize).copied().unwrap_or(-1) } else { self.dash_index }) {
                (0, 0) => {
                    self.switch_tab(TAB_HOME);
                    self.go(SCR_MEDIA);
                }
                (0, 1) => self.open_iptv(),
                (0, 2) => self.open_vacuumtube(),
                (0, 3) => self.play_disc(),
                (0, 4) => self.switch_session(Target::Steam),
                (0, 5) => self.switch_session(Target::Plasma),
                (1, i) => {
                    if let Some((r, _)) = self.dash_items.get(i as usize).cloned() {
                        self.open_detail(r);
                    }
                }
                (2, 0) => self.toggle_hdr(),
                (2, 1) => {
                    self.set_focus = 0;
                    self.go(SCR_SETTINGS);
                }
                (2, 2) => self.switch_session(Target::Restart),
                (2, 3) => {
                    if self.confirmed(3) {
                        self.power("reboot");
                    }
                }
                (2, 4) => {
                    if self.confirmed(4) {
                        self.power("poweroff");
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // ─────────────── VacuumTube ───────────────

    fn open_vacuumtube(&mut self) {
        if session::flatpak_installed(session::VACUUMTUBE) {
            self.run_vacuumtube();
        } else {
            self.dialog = Some(Dialog::InstallVacuumTube { focus: 0, busy: false });
        }
    }

    fn run_vacuumtube(&mut self) {
        self.external = true;
        let ui = self.ui();
        ui.set_playing(true);
        ui.set_playing_title("YouTube · VacuumTube".into());
        self.spawn(async {
            let r = session::flatpak_run(session::VACUUMTUBE).await;
            post(move |c| {
                c.external = false;
                c.ui().set_playing(false);
                if let Err(e) = r {
                    c.toast(format!("{e:#}"));
                }
                c.sync();
            });
        });
    }

    fn nav_dialog(&mut self, d: Dialog, n: Nav) {
        let Dialog::InstallVacuumTube { focus, busy } = d;
        if busy {
            return;
        }
        match n {
            Nav::Left | Nav::Right => self.dialog = Some(Dialog::InstallVacuumTube { focus: 1 - focus, busy }),
            Nav::Back => self.dialog = None,
            Nav::Accept if focus == 1 => self.dialog = None,
            Nav::Accept => {
                self.dialog = Some(Dialog::InstallVacuumTube { focus, busy: true });
                self.spawn(async {
                    let r = session::flatpak_install(session::VACUUMTUBE).await;
                    post(move |c| {
                        c.dialog = None;
                        match r {
                            Ok(()) => {
                                c.toast("VacuumTube installé");
                                c.run_vacuumtube();
                            }
                            Err(e) => c.toast(format!("Installation impossible : {e:#}")),
                        }
                        c.sync();
                    });
                });
            }
            _ => {}
        }
    }

    /// Double appui requis pour les actions destructrices.
    fn confirmed(&mut self, what: i32) -> bool {
        let now = std::time::Instant::now();
        if let Some((w, t)) = self.confirm {
            if w == what && now.duration_since(t) < Duration::from_secs(4) {
                self.confirm = None;
                return true;
            }
        }
        self.confirm = Some((what, now));
        self.toast("Appuyez à nouveau pour confirmer");
        false
    }

    fn switch_session(&mut self, t: Target) {
        self.state.save();
        match session::switch_to(t, &self.svc.cfg.session.steam_client) {
            Ok(true) => {
                let _ = slint::quit_event_loop();
            }
            Ok(false) => self.toast("Steam Big Picture lancé"),
            Err(e) => self.toast(format!("Échec du changement de session : {e:#}")),
        }
    }

    fn power(&mut self, action: &'static str) {
        self.state.save();
        if let Err(e) = session::power(action) {
            self.toast(format!("{e:#}"));
        }
    }

    fn refresh_hdr(&mut self) {
        self.spawn(async {
            let st = tokio::task::spawn_blocking(session::hdr_status).await.unwrap_or_default();
            post(move |c| {
                c.hdr = st;
                c.sync();
            });
        });
    }

    fn toggle_hdr(&mut self) {
        if !self.hdr.available {
            self.toast("Aucun écran HDR détecté");
            return;
        }
        let (out, on) = (self.hdr.output.clone(), !self.hdr.enabled);
        self.spawn(async move {
            let r = tokio::task::spawn_blocking(move || session::set_hdr(&out, on)).await;
            tokio::time::sleep(Duration::from_millis(600)).await;
            let st = tokio::task::spawn_blocking(session::hdr_status).await.unwrap_or_default();
            post(move |c| {
                if let Ok(Err(e)) = r {
                    c.toast(format!("HDR : {e:#}"));
                } else {
                    c.toast(if st.enabled { "HDR activé" } else { "HDR désactivé" });
                }
                c.hdr = st;
                c.sync();
            });
        });
    }

    fn load_dashboard(&mut self) {
        let svc = self.svc.clone();
        let watching = self.state.watching();
        self.spawn(async move {
            let mut items: Vec<(MediaRef, CardInfo)> = vec![];
            if svc.jf_ready() {
                if let Ok(r) = svc.jf.resume().await {
                    items.extend(r.into_iter().map(|i| {
                        let ci = jf_card(&svc.jf, &i, true);
                        (MediaRef::Jf(i), ci)
                    }));
                }
            }
            for (id, p) in watching.iter().take(8) {
                let (r, mut ci) = progress_card(*id, p);
                ci.title = format!("{}", p.title);
                items.push((r, ci));
            }
            post(move |c| {
                c.dash_items = items;
                let cards: Vec<Card> = c.dash_items.iter().map(|(_, ci)| make_card(ci, &mut c.images)).collect();
                c.dash_model.set_vec(cards);
                if c.dash_area == 1 && c.dash_items.is_empty() {
                    c.dash_area = 0;
                }
                c.sync();
            });
        });
    }

    // ─────────────── Médiathèque ───────────────

    fn switch_tab(&mut self, t: i32) {
        if t == TAB_SEARCH {
            self.tab = TAB_SEARCH;
            self.kb_active = true;
            self.screen = SCR_SEARCH;
            if self.stack.last() != Some(&SCR_MEDIA) {
                self.stack.push(SCR_MEDIA);
            }
            self.sync();
            return;
        }
        if t != self.tab || self.home.slots.is_empty() {
            self.tab = t;
            self.load_tab(t);
        }
        self.sync();
    }

    fn load_manifest(&mut self) {
        let svc = self.svc.clone();
        self.spawn(async move {
            match svc.aio.manifest().await {
                Ok(m) => post(move |c| {
                    c.aio_name = format!("{} {}", m.name, m.version);
                    let had = !c.catalogs.is_empty();
                    c.catalogs = m.catalogs;
                    if !had && c.tab != TAB_SEARCH {
                        c.load_tab(c.tab);
                        c.sync();
                    }
                }),
                Err(e) => log::warn!("manifest AIOStreams : {e:#}"),
            }
        });
    }

    fn browse_catalogs(&self, kind: &str) -> Vec<CatalogDef> {
        self.catalogs
            .iter()
            .filter(|c| c.kind == kind && !c.is_search() && !c.id.ends_with(".year") && !c.id.ends_with(".language"))
            .cloned()
            .collect()
    }

    fn anime_library(&self) -> Option<String> {
        let want = self.svc.cfg.jellyfin.anime_library.to_lowercase();
        self.views.iter().find(|v| v.name.to_lowercase().contains(&want)).map(|v| v.id.clone())
    }

    fn load_tab(&mut self, tab: i32) {
        // Chaque rangée : titre, format large, chargeur asynchrone.
        type Loader = std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(MediaRef, CardInfo)>> + Send>>;
        let svc = self.svc.clone();
        let jf_ok = svc.jf_ready();
        let mut rows: Vec<(String, bool, Loader)> = vec![];
        let state_rows: Vec<(MediaRef, CardInfo)> = self.state.watching().iter().map(|(id, p)| progress_card(*id, p)).collect();
        // Lectures en streaming non terminées (Jellyfin garde déjà sa propre reprise).
        let continue_rows: Vec<(MediaRef, CardInfo)> = self
            .state
            .history
            .iter()
            .filter(|h| !h.finished && !h.key.starts_with("jf:") && !h.key.contains(":live:"))
            .take(20)
            .map(hist_card)
            .collect();
        let anime_continue: Vec<(MediaRef, CardInfo)> = self
            .state
            .history
            .iter()
            .filter(|h| !h.finished && matches!(h.open, OpenRef::Anime { .. }))
            .take(20)
            .map(hist_card)
            .collect();
        let favs: Vec<(MediaRef, CardInfo)> = self.state.favorites.iter().filter_map(fav_card).collect();
        let anime_favs: Vec<(MediaRef, CardInfo)> = self.state.favorites.iter().filter(|f| matches!(f.open, OpenRef::Anime { .. })).filter_map(fav_card).collect();

        let jf_list = |fut: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Item>>> + Send>>, svc: Arc<Services>, wide: bool| -> Loader {
            Box::pin(async move {
                match fut.await {
                    Ok(v) => v
                        .into_iter()
                        .map(|i| {
                            let ci = jf_card(&svc.jf, &i, wide);
                            (MediaRef::Jf(i), ci)
                        })
                        .collect(),
                    Err(e) => {
                        log::warn!("{e:#}");
                        vec![]
                    }
                }
            })
        };
        let ani_list = |fut: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Anime>>> + Send>>, state: Arc<HashMap<i64, AnimeProgress>>| -> Loader {
            Box::pin(async move {
                let st = State { anime: (*state).clone(), ..Default::default() };
                match fut.await {
                    Ok(v) => v
                        .into_iter()
                        .map(|a| {
                            let ci = anime_card(&a, &st);
                            (MediaRef::Anime(a), ci)
                        })
                        .collect(),
                    Err(e) => {
                        log::warn!("{e:#}");
                        vec![]
                    }
                }
            })
        };
        let cat_list = |svc: Arc<Services>, c: CatalogDef| -> Loader {
            Box::pin(async move {
                match svc.aio.catalog(&c.kind, &c.id, None).await {
                    Ok(v) => v
                        .into_iter()
                        .map(|m| {
                            let ci = meta_card(&m);
                            (MediaRef::Meta(m), ci)
                        })
                        .collect(),
                    Err(e) => {
                        log::warn!("{e:#}");
                        vec![]
                    }
                }
            })
        };
        let anime_state = Arc::new(self.state.anime.clone());
        let static_rows = |v: Vec<(MediaRef, CardInfo)>| -> Loader { Box::pin(async move { v }) };
        let anime_lib = self.anime_library();
        let (season, year) = anilist::current_season();
        let season_title = format!("Saison {} {year}", anilist::season_fr(season));

        match tab {
            TAB_HOME => {
                rows.push(("Continuer la lecture".into(), true, static_rows(continue_rows)));
                if jf_ok {
                    let s = svc.clone();
                    rows.push(("Reprendre · Jellyfin".into(), true, jf_list(Box::pin(async move { s.jf.resume().await }), svc.clone(), true)));
                }
                rows.push(("Animes en cours".into(), false, static_rows(state_rows)));
                if jf_ok {
                    let s = svc.clone();
                    rows.push(("À suivre".into(), true, jf_list(Box::pin(async move { s.jf.next_up().await }), svc.clone(), true)));
                }
                rows.push(("Mes favoris".into(), false, static_rows(favs)));
                if jf_ok {
                    for v in self.views.iter().filter(|v| matches!(v.collection_type.as_deref(), Some("movies") | Some("tvshows"))) {
                        let (s, id) = (svc.clone(), v.id.clone());
                        rows.push((
                            format!("Ajouts récents · {}", v.name),
                            false,
                            jf_list(Box::pin(async move { s.jf.latest(&id).await }), svc.clone(), false),
                        ));
                    }
                }
                let s = svc.clone();
                rows.push((season_title.clone(), false, ani_list(Box::pin(async move { s.ani.seasonal().await }), anime_state.clone())));
                for c in self.browse_catalogs("movie").into_iter().take(1) {
                    rows.push((format!("Films · {}", c.name), false, cat_list(svc.clone(), c)));
                }
                for c in self.browse_catalogs("series").into_iter().take(1) {
                    rows.push((format!("Séries · {}", c.name), false, cat_list(svc.clone(), c)));
                }
            }
            TAB_ANIME => {
                rows.push(("Reprendre".into(), true, static_rows(anime_continue)));
                rows.push(("Animes en cours".into(), false, static_rows(state_rows)));
                rows.push(("Mes favoris".into(), false, static_rows(anime_favs)));
                if jf_ok {
                    if let Some(lib) = anime_lib.clone() {
                        let s = svc.clone();
                        let l = lib.clone();
                        rows.push(("Ma bibliothèque · récents".into(), false, jf_list(Box::pin(async move { s.jf.latest(&l).await }), svc.clone(), false)));
                        let s = svc.clone();
                        rows.push((
                            "Ma bibliothèque · A-Z".into(),
                            false,
                            jf_list(Box::pin(async move { s.jf.items(Some(&lib), "Series,Movie", "SortName", 300, None).await }), svc.clone(), false),
                        ));
                    }
                }
                let s = svc.clone();
                rows.push((season_title, false, ani_list(Box::pin(async move { s.ani.seasonal().await }), anime_state.clone())));
                let s = svc.clone();
                rows.push(("Tendances".into(), false, ani_list(Box::pin(async move { s.ani.trending().await }), anime_state.clone())));
                let s = svc.clone();
                rows.push(("Les plus populaires".into(), false, ani_list(Box::pin(async move { s.ani.popular().await }), anime_state.clone())));
            }
            TAB_MOVIES | TAB_SHOWS => {
                let (kind, jf_type) = if tab == TAB_MOVIES { ("movie", "Movie") } else { ("series", "Series") };
                if jf_ok {
                    for v in self.views.iter().filter(|v| {
                        v.collection_type.as_deref() == Some(if tab == TAB_MOVIES { "movies" } else { "tvshows" }) && Some(&v.id) != anime_lib.as_ref()
                    }) {
                        let (s, id) = (svc.clone(), v.id.clone());
                        rows.push((
                            format!("{} · récents", v.name),
                            false,
                            jf_list(Box::pin(async move { s.jf.items(Some(&id), jf_type, "DateCreated", 40, None).await }), svc.clone(), false),
                        ));
                    }
                }
                for c in self.browse_catalogs(kind) {
                    rows.push((c.name.clone(), false, cat_list(svc.clone(), c)));
                }
                if jf_ok {
                    for v in self.views.iter().filter(|v| v.collection_type.as_deref() == Some(if tab == TAB_MOVIES { "movies" } else { "tvshows" }) && Some(&v.id) != anime_lib.as_ref()) {
                        let (s, id) = (svc.clone(), v.id.clone());
                        rows.push((format!("{} · A-Z", v.name), false, jf_list(Box::pin(async move { s.jf.items(Some(&id), jf_type, "SortName", 500, None).await }), svc.clone(), false)));
                    }
                }
            }
            TAB_HISTORY => {
                // Regroupement par jour, du plus récent au plus ancien.
                let mut days: Vec<(String, Vec<(MediaRef, CardInfo)>)> = vec![];
                let today = chrono::Local::now().date_naive();
                for h in &self.state.history {
                    let d = chrono::DateTime::from_timestamp(h.watched_at, 0)
                        .map(|d| d.with_timezone(&chrono::Local))
                        .unwrap_or_else(chrono::Local::now);
                    let label = match (today - d.date_naive()).num_days() {
                        0 => "Aujourd'hui".to_string(),
                        1 => "Hier".to_string(),
                        _ => {
                            let s = french_date(&d);
                            let mut c = s.chars();
                            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or(s)
                        }
                    };
                    match days.last_mut() {
                        Some((l, v)) if *l == label => v.push(hist_card(h)),
                        _ => days.push((label, vec![hist_card(h)])),
                    }
                }
                for (label, v) in days {
                    rows.push((label, true, static_rows(v)));
                }
                let manage = if self.state.history.is_empty() {
                    vec![(MediaRef::Action(0), CardInfo {
                        title: "Historique vide".into(),
                        subtitle: format!("Conservé {} jours", crate::state::HISTORY_DAYS),
                        overview: "Les films, épisodes et chaînes regardés apparaîtront ici.".into(),
                        progress: -1.0,
                        ..Default::default()
                    })]
                } else {
                    vec![(MediaRef::Action(1), CardInfo {
                        title: "Effacer l'historique".into(),
                        subtitle: format!("{} lectures · {} jours", self.state.history.len(), crate::state::HISTORY_DAYS),
                        overview: "Supprime tout l'historique de lecture (double appui pour confirmer). Les positions de reprise sont conservées.".into(),
                        meta: "Gestion".into(),
                        progress: -1.0,
                        ..Default::default()
                    })]
                };
                rows.push(("Gérer l'historique".into(), true, static_rows(manage)));
            }
            _ => {}
        }

        let titles = rows.iter().map(|(t, w, _)| (t.clone(), *w)).collect();
        let stamp = self.home.reset(titles, &mut self.images);
        self.hero_url = None;
        for (slot, (_, _, fut)) in rows.into_iter().enumerate() {
            self.spawn(async move {
                let items = fut.await;
                post(move |c| {
                    if c.home.fill(stamp, slot, items, &mut c.images) {
                        c.sync();
                    }
                });
            });
        }

        // Les bibliothèques Jellyfin sont nécessaires aux rangées : chargement puis relance.
        if jf_ok && self.views.is_empty() {
            let svc = self.svc.clone();
            self.spawn(async move {
                match svc.jf.views().await {
                    Ok(v) if !v.is_empty() => post(move |c| {
                        c.views = v;
                        c.load_tab(c.tab);
                        c.sync();
                    }),
                    Ok(_) => {}
                    Err(e) => post(move |c| c.toast(format!("Jellyfin : {e:#}"))),
                }
            });
        }
    }

    fn nav_media(&mut self, n: Nav) {
        match n {
            Nav::PrevTab => self.switch_tab((self.tab - 1).rem_euclid(TAB_COUNT)),
            Nav::NextTab => self.switch_tab((self.tab + 1).rem_euclid(TAB_COUNT)),
            Nav::Back => self.go(SCR_DASH),
            _ if self.home.row < 0 => match n {
                Nav::Left => self.switch_tab((self.tab - 1).max(0)),
                Nav::Right => self.switch_tab((self.tab + 1).min(TAB_COUNT - 1)),
                Nav::Down | Nav::Accept => {
                    if !self.home.visible.is_empty() {
                        self.home.row = 0;
                    }
                }
                _ => {}
            },
            Nav::Alt if self.tab == TAB_HISTORY => {
                if let Some((MediaRef::Hist(key), _)) = self.home.focused().cloned() {
                    self.state.remove_history(&key);
                    self.state.save();
                    self.toast("Retiré de l'historique");
                    self.reload_tab_keep_focus();
                }
            }
            Nav::Accept | Nav::Alt => {
                if let Some((r, _)) = self.home.focused().cloned() {
                    self.open_detail(r);
                }
            }
            _ => {
                self.home.nav(n, true);
            }
        }
    }

    fn reload_tab_keep_focus(&mut self) {
        if self.tab == TAB_SEARCH {
            return;
        }
        let (row, cols, touched) = (self.home.row, self.home.cols.clone(), self.home.touched);
        self.load_tab(self.tab);
        self.home.row = row.min(self.home.visible.len() as i32 - 1);
        self.home.cols = cols;
        self.home.touched = touched;
    }

    fn update_hero(&mut self) {
        let ui = self.ui();
        let Some((r, ci)) = self.home.focused().cloned().or_else(|| self.home.get(0, 0).cloned()) else {
            ui.set_hero_title(if self.home.slots.is_empty() { "" } else { "Chargement…" }.into());
            ui.set_hero_meta("".into());
            ui.set_hero_overview("".into());
            ui.set_hero_image(Image::default());
            return;
        };
        ui.set_hero_title(ci.title.clone().into());
        ui.set_hero_meta(ci.meta.clone().into());
        let anime_id = match &r {
            MediaRef::Anime(a) => Some(a.id),
            MediaRef::AnimeId(id) => Some(*id),
            _ => None,
        };
        match anime_id.and_then(fr_overview) {
            Some(fr) => ui.set_hero_overview(fr.into()),
            None => {
                ui.set_hero_overview(ci.overview.clone().into());
                if let MediaRef::Anime(a) = &r {
                    self.fetch_fr_overview(a.clone());
                }
            }
        }
        let url = ci.backdrop.clone().or(ci.image.clone());
        if url != self.hero_url {
            self.hero_url = url.clone();
            ui.set_hero_image(self.images.get(url.as_deref()));
        }
    }

    /// Résumé français d'un anime (TMDB) pour le bandeau, chargé une seule fois par anime.
    fn fetch_fr_overview(&mut self, a: Anime) {
        let Some(tmdb) = self.svc.tmdb.clone() else { return };
        if !self.fr_requested.insert(a.id) {
            return;
        }
        let ani = self.svc.ani.clone();
        self.spawn(async move {
            let mut map = ani.mappings(a.id).await.unwrap_or_default();
            map.episodes.clear();
            let titles: Vec<String> = [a.title.english.clone(), a.title.romaji.clone()].into_iter().flatten().collect();
            let movie = a.format.as_deref() == Some("MOVIE");
            if let Ok(Some(fr)) = tmdb.anime_fr(&map, &titles, a.season_year, movie).await {
                fr_overview_store(a.id, &fr.overview);
                post(|c| c.update_hero());
            }
        });
    }

    fn image_ready(&mut self, url: String, buf: SharedPixelBuffer<Rgba8Pixel>) {
        let img = Image::from_rgba8(buf);
        self.images.pending.remove(&url);
        if self.images.cache.len() > 600 {
            self.images.cache.clear();
        }
        self.images.cache.insert(url.clone(), img.clone());
        self.home.update_images(&url, &img);
        self.search.update_images(&url, &img);
        update_card_model(&self.dash_model, &url, &img);
        self.iptv_images(&url, &img);
        let ui = self.ui();
        if self.hero_url.as_deref() == Some(&url) {
            ui.set_hero_image(img.clone());
        }
        if let Some(d) = &self.detail {
            if d.data.backdrop.as_deref() == Some(&url) {
                ui.set_d_backdrop(img.clone());
            }
            if d.data.poster.as_deref() == Some(&url) {
                ui.set_d_poster(img.clone());
            }
            for i in 0..d.ep_model.row_count() {
                if let Some(mut e) = d.ep_model.row_data(i) {
                    if e.key == url {
                        e.image = img.clone();
                        d.ep_model.set_row_data(i, e);
                    }
                }
            }
        }
    }

    // ─────────────── Fiche ───────────────

    fn open_detail(&mut self, r: MediaRef) {
        let r = match r {
            MediaRef::Action(1) => {
                if self.confirmed(10) {
                    self.state.clear_history();
                    self.state.save();
                    self.toast("Historique effacé");
                    self.reload_tab_keep_focus();
                }
                return;
            }
            MediaRef::Action(_) => return,
            MediaRef::Hist(key) => {
                let Some(h) = self.state.history.iter().find(|h| h.key == key).cloned() else { return };
                match h.open.clone() {
                    OpenRef::Anime { id } => MediaRef::AnimeId(id),
                    OpenRef::Jf { id } => MediaRef::JfId(id),
                    OpenRef::Meta { kind, id } => MediaRef::Meta(Meta {
                        id,
                        kind,
                        name: h.title.clone(),
                        poster: h.image.clone(),
                        background: h.backdrop.clone(),
                        ..Default::default()
                    }),
                    OpenRef::Iptv { profile, kind, id } => {
                        self.open_iptv_history(&profile, &kind, &id, &h);
                        return;
                    }
                }
            }
            r => r,
        };
        // Les dossiers / collections Jellyfin s'ouvrent comme une recherche de contenu.
        if let MediaRef::Jf(it) = &r {
            if !matches!(it.kind.as_str(), "Movie" | "Series" | "Episode" | "Season" | "Video") {
                self.toast(format!("Type non pris en charge : {}", it.kind));
                return;
            }
        }
        self.detail_gen += 1;
        let stamp = self.detail_gen;
        let preview = match &r {
            MediaRef::Jf(it) => {
                let ci = jf_card(&self.svc.jf, it, false);
                DetailLoad { title: it.series_name.clone().unwrap_or(ci.title), backdrop: ci.backdrop, poster: ci.image, overview: ci.overview, meta: ci.meta, ..Default::default() }
            }
            MediaRef::Meta(m) => {
                let ci = meta_card(m);
                DetailLoad { title: ci.title, backdrop: ci.backdrop, poster: ci.image, overview: ci.overview, meta: ci.meta, ..Default::default() }
            }
            MediaRef::Anime(a) => {
                let ci = anime_card(a, &self.state);
                DetailLoad { title: ci.title, backdrop: ci.backdrop, poster: ci.image, overview: ci.overview, meta: ci.meta, ..Default::default() }
            }
            MediaRef::AnimeId(id) => {
                let p = self.state.anime.get(id).cloned().unwrap_or_default();
                DetailLoad { title: p.title, poster: p.cover.clone(), backdrop: p.cover, ..Default::default() }
            }
            MediaRef::Iptv(r) => DetailLoad {
                title: r.entry.name.clone(),
                poster: r.entry.logo.clone(),
                backdrop: r.entry.logo.clone(),
                overview: r.entry.plot.clone().unwrap_or_default(),
                ..Default::default()
            },
            MediaRef::JfId(_) | MediaRef::Hist(_) | MediaRef::Action(_) => DetailLoad { title: "Chargement…".into(), ..Default::default() },
        };
        self.detail = Some(Detail {
            root: r.clone(),
            data: DetailLoad::default(),
            buttons: vec![],
            ep_model: Rc::new(VecModel::default()),
            area: 0,
            button: 0,
            ep: 0,
            season: 0,
            stamp,
        });
        self.apply_detail(preview, true);
        if self.screen != SCR_DETAIL {
            self.go(SCR_DETAIL);
        }

        let svc = self.svc.clone();
        let anime_lib = self.anime_library();
        self.spawn(async move {
            let res = load_detail(&svc, r, anime_lib).await;
            post(move |c| {
                if c.detail.as_ref().map(|d| d.stamp) != Some(stamp) {
                    return;
                }
                match res {
                    Ok(d) => c.apply_detail(d, false),
                    Err(e) => {
                        c.ui().set_d_loading(false);
                        c.toast(format!("Erreur : {e:#}"));
                    }
                }
                c.sync();
            });
        });
    }

    fn apply_detail(&mut self, mut data: DetailLoad, loading: bool) {
        decorate_eps(&self.state, &mut data);
        let ui = self.ui();
        let poster = self.images.get(data.poster.as_deref());
        let backdrop = self.images.get(data.backdrop.as_deref());
        ui.set_d_title(data.title.clone().into());
        ui.set_d_meta(data.meta.clone().into());
        ui.set_d_overview(data.overview.clone().into());
        ui.set_d_poster(poster);
        ui.set_d_backdrop(backdrop);
        ui.set_d_loading(loading);
        ui.set_d_badges(ModelRc::from(Rc::new(VecModel::from(data.badges.iter().map(|b| SharedString::from(b.as_str())).collect::<Vec<_>>()))));
        ui.set_d_seasons(ModelRc::from(Rc::new(VecModel::from(
            data.seasons.iter().map(|(_, n)| SharedString::from(n.as_str())).collect::<Vec<_>>(),
        ))));
        let eps: Vec<EpisodeData> = data.episodes.iter().map(|(_, e)| self.ep_data(e)).collect();
        let Some(d) = self.detail.as_mut() else { return };
        d.ep_model.set_vec(eps);
        ui.set_d_episodes(ModelRc::from(d.ep_model.clone()));
        d.season = data.season as i32;
        if let Some(f) = data.focus_ep {
            d.ep = f as i32;
        } else {
            d.ep = d.ep.min(data.episodes.len() as i32 - 1).max(0);
        }
        d.data = data;
        self.rebuild_buttons();
    }

    fn ep_data(&mut self, e: &EpInfo) -> EpisodeData {
        EpisodeData {
            key: e.image.clone().unwrap_or_default().into(),
            label: e.label.clone().into(),
            title: e.title.clone().into(),
            overview: e.overview.clone().into(),
            image: self.images.get(e.image.as_deref()),
            progress: e.progress,
            watched: e.watched,
            local: e.local,
        }
    }

    /// Cible de lecture « principale » : le film, ou l'épisode focalisé / suivant.
    fn current_target(&self, use_focus: bool) -> Option<PlayTarget> {
        let d = self.detail.as_ref()?;
        let data = &d.data;
        if data.episodes.is_empty() {
            if let Some(r) = &data.iptv {
                return r.entry.url.clone().map(|url| PlayTarget::Direct {
                    url,
                    title: r.entry.name.clone(),
                    key: format!("iptv:{}:{}", r.profile.id, r.key()),
                    live: r.kind == iptv::Kind::Live,
                });
            }
            return match (&data.item, &d.root, &data.meta_obj) {
                (Some(it), _, _) => Some(PlayTarget::Jf {
                    item: it.clone(),
                    aio: it.imdb().map(|i| ("movie".into(), i.to_string())).or_else(|| it.tmdb().map(|t| ("movie".into(), format!("tmdb:{t}")))),
                    anime: None,
                    label: it.name.clone(),
                }),
                (None, _, Some(m)) if m.kind == "movie" => Some(PlayTarget::Aio { kind: "movie".into(), id: m.id.clone(), title: m.name.clone(), anime: None }),
                _ => None,
            };
        }
        let idx = if use_focus { d.ep as usize } else { self.next_episode_index()? };
        let (ep, _) = data.episodes.get(idx)?;
        Some(self.target_for_episode(ep))
    }

    fn next_episode_index(&self) -> Option<usize> {
        let d = self.detail.as_ref()?;
        let eps = &d.data.episodes;
        if let Some(a) = &d.data.anime {
            let last = self.state.anime.get(&a.anime.id).map_or(0, |p| p.last_watched);
            if let Some(i) = eps.iter().position(|(e, _)| matches!(e, EpRef::Anime { n, .. } if *n == last + 1)) {
                return Some(i);
            }
        }
        eps.iter()
            .position(|(_, i)| i.progress > 0.0 && !i.watched)
            .or_else(|| eps.iter().position(|(_, i)| !i.watched))
            .or(Some(0))
    }

    fn target_for_episode(&self, ep: &EpRef) -> PlayTarget {
        let d = self.detail.as_ref().unwrap();
        let data = &d.data;
        let series_title = data.title.clone();
        match ep {
            EpRef::Jf(e) => {
                let s = e.parent_index_number.unwrap_or(1);
                let n = e.index_number.unwrap_or(1);
                let series = data.series.as_ref();
                let aio = series
                    .and_then(|s| s.imdb())
                    .map(|i| ("series".into(), format!("{i}:{s}:{n}")))
                    .or_else(|| series.and_then(|x| x.provider_ids.get("Tvdb")).map(|t| ("series".into(), format!("tvdb:{t}:{s}:{n}"))));
                PlayTarget::Jf { item: e.clone(), aio, anime: None, label: format!("{series_title} — {} · {}", e.episode_label(), e.name) }
            }
            EpRef::Anime { n, jf } => {
                let a = data.anime.as_ref().unwrap();
                let mark = AnimeMark { id: a.anime.id, ep: *n, title: a.anime.display_title(), cover: a.anime.cover_url(), total: a.anime.episodes };
                let (kind, id) = anime_stream_id(a, *n);
                let title = format!("{} — Épisode {n}", a.anime.display_title());
                match jf {
                    Some(item) => PlayTarget::Jf { item: item.clone(), aio: Some((kind, id)), anime: Some(mark), label: title },
                    None => PlayTarget::Aio { kind, id, title, anime: Some(mark) },
                }
            }
            EpRef::Stremio(v) => {
                let label = match (v.season, v.episode) {
                    (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
                    _ => String::new(),
                };
                let t = v.name.clone().or(v.title.clone()).unwrap_or_default();
                PlayTarget::Aio { kind: "series".into(), id: v.id.clone(), title: format!("{series_title} — {label} · {t}"), anime: None }
            }
            EpRef::Iptv(e) => {
                let profile = data.iptv.as_ref().map(|r| r.profile.id.clone()).unwrap_or_default();
                PlayTarget::Direct {
                    url: e.url.clone(),
                    title: format!("{series_title} — S{:02}E{:02} · {}", e.season, e.number, e.title),
                    key: format!("iptv:{profile}:ep:{}", e.id),
                    live: false,
                }
            }
        }
    }

    /// Référence de la fiche courante, pour les favoris (hors IPTV, qui a les siens).
    fn detail_open_ref(&self) -> Option<OpenRef> {
        let d = self.detail.as_ref()?;
        Some(match &d.root {
            MediaRef::Jf(it) => OpenRef::Jf { id: it.series_id.clone().unwrap_or_else(|| it.id.clone()) },
            MediaRef::JfId(id) => OpenRef::Jf { id: id.clone() },
            MediaRef::Meta(m) => OpenRef::Meta { kind: m.kind.clone(), id: m.id.clone() },
            MediaRef::Anime(a) => OpenRef::Anime { id: a.id },
            MediaRef::AnimeId(id) => OpenRef::Anime { id: *id },
            MediaRef::Iptv(_) | MediaRef::Hist(_) | MediaRef::Action(_) => return None,
        })
    }

    fn toggle_fav(&mut self) {
        let Some(open) = self.detail_open_ref() else { return };
        let Some(d) = self.detail.as_ref() else { return };
        let fav = Favorite {
            open,
            title: d.data.title.clone(),
            subtitle: d.data.meta.split("  ·  ").take(2).collect::<Vec<_>>().join(" · "),
            image: d.data.poster.clone(),
            backdrop: d.data.backdrop.clone(),
            added: chrono::Local::now().timestamp(),
        };
        let on = self.state.toggle_favorite(fav);
        self.state.save();
        self.toast(if on { "Ajouté aux favoris" } else { "Retiré des favoris" });
        self.rebuild_buttons();
        self.sync();
    }

    fn rebuild_buttons(&mut self) {
        let target = self.current_target(false);
        let next_idx = self.next_episode_index();
        let fav_btn = self.detail_open_ref().map(|o| self.state.is_favorite(&o));
        let iptv_fav = self.detail.as_ref().and_then(|d| d.data.iptv.as_ref()).map(|r| self.iptv_is_fav(&r.profile.id, &r.key()));
        let positions = &self.state.positions;
        let Some(d) = self.detail.as_mut() else { return };
        let follows_watched = next_idx.is_some_and(|i| i > 0 && d.data.episodes.get(i - 1).is_some_and(|(_, e)| e.watched));
        let mut b: Vec<(String, Action)> = vec![];
        let ep_label = |t: &PlayTarget| match t {
            PlayTarget::Jf { item, anime: None, .. } if item.kind == "Episode" => format!(" {}", item.episode_label()),
            PlayTarget::Jf { anime: Some(m), .. } | PlayTarget::Aio { anime: Some(m), .. } => format!(" ép. {}", m.ep),
            PlayTarget::Aio { id, kind, .. } if kind == "series" => {
                let p: Vec<&str> = id.split(':').collect();
                if p.len() >= 3 { format!(" S{:0>2}E{:0>2}", p[p.len() - 2], p[p.len() - 1]) } else { String::new() }
            }
            _ => String::new(),
        };
        if let Some(t) = &target {
            let resume = match t {
                PlayTarget::Jf { item, .. } => item.resume_secs() > 30.0,
                PlayTarget::Aio { .. } | PlayTarget::Direct { .. } => positions.get(&t.stream_key()).is_some_and(|p| *p > 30.0),
            };
            if resume {
                let at = positions.get(&t.stream_key()).copied().or(match t {
                    PlayTarget::Jf { item, .. } => Some(item.resume_secs()),
                    _ => None,
                });
                let at = at.map(|s| format!(" ({:02}:{:02})", (s / 60.0) as i64, (s % 60.0) as i64)).unwrap_or_default();
                b.push((format!("Reprendre{}{at}", ep_label(t)), Action::Resume));
                b.push(("Depuis le début".into(), Action::Restart));
            } else if follows_watched {
                b.push((format!("Épisode suivant ·{}", ep_label(t)), Action::Play));
            } else {
                b.push((format!("Lecture{}", ep_label(t)), Action::Play));
            }
            if !matches!(t, PlayTarget::Direct { .. }) {
                b.push(("Sources".into(), Action::Sources));
            }
            if let PlayTarget::Jf { item, .. } = t {
                if item.kind == "Movie" {
                    b.push((if item.played() { "Marquer non vu" } else { "Marquer vu" }.into(), Action::ToggleWatched));
                }
            }
        }
        if let Some(fav) = fav_btn {
            b.push((if fav { "★ Favori" } else { "☆ Ajouter aux favoris" }.into(), Action::Fav));
        }
        if let Some(fav) = iptv_fav {
            b.push((if fav { "★ Favori" } else { "☆ Ajouter aux favoris" }.into(), Action::IptvFav));
        }
        d.button = d.button.min(b.len() as i32 - 1).max(0);
        d.buttons = b;
        let ui = self.ui.upgrade().unwrap();
        ui.set_d_buttons(ModelRc::from(Rc::new(VecModel::from(
            d.buttons.iter().map(|(s, _)| SharedString::from(s.as_str())).collect::<Vec<_>>(),
        ))));
    }

    fn nav_detail(&mut self, n: Nav) {
        let Some(d) = self.detail.as_mut() else { return self.back() };
        let has_seasons = !d.data.seasons.is_empty();
        let has_eps = !d.data.episodes.is_empty();
        match n {
            Nav::Back => return self.back(),
            Nav::Up => {
                d.area = match d.area {
                    2 if has_seasons => 1,
                    _ => 0,
                }
            }
            Nav::Down => {
                d.area = match d.area {
                    0 if has_seasons => 1,
                    0 | 1 if has_eps => 2,
                    a => a,
                }
            }
            Nav::Left | Nav::Right => {
                let dir = if n == Nav::Left { -1 } else { 1 };
                match d.area {
                    0 => d.button = (d.button + dir).clamp(0, d.buttons.len() as i32 - 1),
                    1 => {
                        let s = (d.season + dir).clamp(0, d.data.seasons.len() as i32 - 1);
                        if s != d.season {
                            d.season = s;
                            self.load_season(s as usize);
                        }
                    }
                    _ => d.ep = (d.ep + dir).clamp(0, d.data.episodes.len() as i32 - 1),
                }
            }
            Nav::PrevTab | Nav::NextTab if d.area == 2 => {
                let dir = if n == Nav::PrevTab { -10 } else { 10 };
                d.ep = (d.ep + dir).clamp(0, d.data.episodes.len() as i32 - 1);
            }
            Nav::Accept => match d.area {
                0 => {
                    if let Some(a) = d.buttons.get(d.button as usize).map(|(_, a)| *a) {
                        self.do_action(a);
                    }
                }
                1 => d.area = 2,
                _ => {
                    self.played_ep = Some(d.ep as usize);
                    if let Some(t) = self.current_target(true) {
                        let resume = match &t {
                            PlayTarget::Jf { item, .. } => item.resume_secs() > 30.0,
                            t => self.state.positions.get(&t.stream_key()).is_some_and(|p| *p > 30.0),
                        };
                        self.play_target(t, resume);
                    }
                }
            },
            Nav::Alt => {
                let use_focus = d.area == 2;
                self.played_ep = if use_focus { Some(d.ep as usize) } else { self.next_episode_index() };
                match self.current_target(use_focus) {
                    Some(PlayTarget::Direct { .. }) => self.toast("Source unique pour ce contenu IPTV"),
                    Some(t) => self.open_sources(t),
                    None => {}
                }
            }
            Nav::Fav => {
                if d.data.iptv.is_some() {
                    self.do_action(Action::IptvFav);
                } else {
                    self.do_action(Action::Fav);
                }
            }
            _ => {}
        }
    }

    fn do_action(&mut self, a: Action) {
        match a {
            Action::Fav => return self.toggle_fav(),
            Action::IptvFav => return self.toggle_iptv_fav_detail(),
            _ => {}
        }
        let Some(t) = self.current_target(false) else { return };
        let has_eps = self.detail.as_ref().is_some_and(|d| !d.data.episodes.is_empty());
        self.played_ep = if has_eps { self.next_episode_index() } else { None };
        match a {
            Action::Play => self.play_target(t, false),
            Action::Resume => self.play_target(t, true),
            Action::Restart => {
                self.state.positions.remove(&t.stream_key());
                self.play_target(t, false)
            }
            Action::Fav | Action::IptvFav => {}
            Action::Sources => self.open_sources(t),
            Action::ToggleWatched => {
                if let PlayTarget::Jf { item, .. } = t {
                    let svc = self.svc.clone();
                    let played = !item.played();
                    self.spawn(async move {
                        let r = svc.jf.set_played(&item.id, played).await;
                        post(move |c| {
                            match r {
                                Ok(()) => c.toast(if played { "Marqué comme vu" } else { "Marqué comme non vu" }),
                                Err(e) => c.toast(format!("{e:#}")),
                            }
                            c.reload_detail();
                        });
                    });
                }
            }
        }
    }

    fn reload_detail(&mut self) {
        let Some(d) = &self.detail else { return };
        let (root, season, ep, area, button) = (d.root.clone(), d.season, d.ep, d.area, d.button);
        let seasons_root = d.data.seasons.get(season as usize).map(|(s, _)| s.clone());
        self.detail_gen += 1;
        let stamp = self.detail_gen;
        if let Some(d) = self.detail.as_mut() {
            d.stamp = stamp;
        }
        let svc = self.svc.clone();
        let anime_lib = self.anime_library();
        self.spawn(async move {
            let mut res = load_detail(&svc, root, anime_lib).await;
            // Conserve la saison affichée.
            if let (Ok(d), Some(SeasonRef::Jf(s))) = (&mut res, &seasons_root) {
                if let Some(series) = &d.series {
                    if let Ok(eps) = svc.jf.episodes(&series.id, Some(&s.id)).await {
                        d.episodes = eps.iter().map(|e| (EpRef::Jf(e.clone()), jf_ep_info(&svc.jf, e))).collect();
                        d.season = season as usize;
                        d.focus_ep = None;
                    }
                }
            }
            post(move |c| {
                if c.detail.as_ref().map(|d| d.stamp) != Some(stamp) {
                    return;
                }
                if let Ok(d) = res {
                    c.apply_detail(d, false);
                    if let Some(d) = c.detail.as_mut() {
                        d.ep = ep.min(d.data.episodes.len() as i32 - 1).max(0);
                        d.area = area;
                        d.button = button.min(d.buttons.len() as i32 - 1).max(0);
                    }
                }
                c.sync();
            });
        });
    }

    fn load_season(&mut self, idx: usize) {
        let Some(d) = &self.detail else { return };
        let Some((sref, _)) = d.data.seasons.get(idx).cloned() else { return };
        match sref {
            SeasonRef::Jf(season) => {
                let stamp = d.stamp;
                let svc = self.svc.clone();
                let series_id = season.series_id.clone().unwrap_or_default();
                self.spawn(async move {
                    let eps = svc.jf.episodes(&series_id, Some(&season.id)).await;
                    post(move |c| {
                        let Ok(eps) = eps else { return };
                        let infos: Vec<(EpRef, EpInfo)> = eps.iter().map(|e| (EpRef::Jf(e.clone()), jf_ep_info(&c.svc.jf, e))).collect();
                        c.set_episodes(stamp, infos);
                    });
                });
            }
            SeasonRef::Anime(id) => {
                let (stamp, svc, lib) = (d.stamp, self.svc.clone(), self.anime_library());
                self.ui().set_d_loading(true);
                self.spawn(async move {
                    let res = load_anime(&svc, id, lib).await;
                    post(move |c| {
                        if c.detail.as_ref().map(|d| d.stamp) != Some(stamp) {
                            return;
                        }
                        match res {
                            Ok(data) => {
                                if let Some(d) = c.detail.as_mut() {
                                    d.root = MediaRef::AnimeId(id);
                                    d.ep = 0;
                                }
                                c.apply_detail(data, false);
                            }
                            Err(e) => {
                                c.ui().set_d_loading(false);
                                c.toast(format!("Erreur : {e:#}"));
                            }
                        }
                        c.sync();
                    });
                });
            }
            SeasonRef::Iptv(season) => {
                let infos = d.data.iptv_info.as_ref().map(|i| iptv_ui::episodes_of(i, season)).unwrap_or_default();
                let stamp = d.stamp;
                self.set_episodes(stamp, infos);
            }
            SeasonRef::Stremio(s) => {
                let infos = d
                    .data
                    .meta_obj
                    .as_ref()
                    .map(|m| stremio_episodes(m, s))
                    .unwrap_or_default();
                let stamp = d.stamp;
                self.set_episodes(stamp, infos);
            }
        }
    }

    fn set_episodes(&mut self, stamp: u64, infos: Vec<(EpRef, EpInfo)>) {
        if self.detail.as_ref().map(|d| d.stamp) != Some(stamp) {
            return;
        }
        let infos = {
            let d = self.detail.as_ref().unwrap();
            let mut tmp = DetailLoad { episodes: infos, anime: d.data.anime.clone(), iptv: d.data.iptv.clone(), ..Default::default() };
            decorate_eps(&self.state, &mut tmp);
            tmp.episodes
        };
        let eps: Vec<EpisodeData> = infos.iter().map(|(_, e)| self.ep_data(e)).collect();
        let d = self.detail.as_mut().unwrap();
        d.ep_model.set_vec(eps);
        d.data.episodes = infos;
        d.ep = 0;
        self.rebuild_buttons();
        self.sync();
    }

    // ─────────────── Sources ───────────────

    fn open_sources(&mut self, t: PlayTarget) {
        self.src_gen += 1;
        let stamp = self.src_gen;
        self.sources.clear();
        self.src_model.set_vec(vec![]);
        self.src_focus = 0;
        self.src_target = Some(t.clone());
        let ui = self.ui();
        ui.set_s_title(t.title().into());
        ui.set_s_loading(true);
        ui.set_s_status("".into());
        self.go(SCR_SOURCES);

        let svc = self.svc.clone();
        let query = self.engine_query(&t);
        self.src_query = query.clone();
        self.spawn(async move {
            let mut out: Vec<SourceEntry> = vec![];
            let mut errors = vec![];
            let anime = t.is_anime();
            let engine = svc.engine.clone();
            let use_engine = engine.enabled() && query.is_some();
            let engine_task = query.filter(|_| use_engine).map(|q| {
                let e = engine.clone();
                tokio::spawn(async move { e.search(&q).await })
            });
            let aio_id = match &t {
                PlayTarget::Jf { item, aio, .. } => {
                    match svc.jf.playback_info(&item.id).await {
                        Ok(pi) => out.extend(pi.media_sources.into_iter().map(|ms| SourceEntry::Jf { item: item.clone(), ms, session: pi.play_session_id.clone() })),
                        Err(e) => errors.push(format!("Jellyfin : {e:#}")),
                    }
                    aio.clone()
                }
                PlayTarget::Aio { kind, id, .. } => Some((kind.clone(), id.clone())),
                PlayTarget::Direct { .. } => None,
            };
            if let Some(task) = engine_task {
                if let Ok(r) = task.await {
                    out.extend(r.candidates.into_iter().map(SourceEntry::Torrent));
                    errors.extend(r.errors);
                }
            }
            // AIOStreams : seulement sans moteur intégré, ou en appoint si coché.
            let aio_wanted = !use_engine || engine.cfg.public.aiostreams;
            if let (Some((kind, id)), true) = (aio_id, aio_wanted && !svc.cfg.aiostreams.manifest_url.trim().is_empty()) {
                match svc.aio.streams(&kind, &id, &svc.cfg.prefs, anime).await {
                    Ok(s) => out.extend(s.into_iter().map(SourceEntry::Aio)),
                    Err(e) => errors.push(format!("AIOStreams : {e:#}")),
                }
            }
            post(move |c| {
                if c.src_gen != stamp {
                    return;
                }
                let ui = c.ui();
                ui.set_s_loading(false);
                let n = out.len();
                ui.set_s_status(
                    if n == 0 {
                        format!("Aucune source trouvée. {}", errors.join(" · "))
                    } else {
                        format!(
                            "{n} sources — {} · triées selon vos priorités{}",
                            c.svc.engine.debrid_name(),
                            if errors.is_empty() { String::new() } else { format!(" · {}", errors.join(" · ").chars().take(160).collect::<String>()) }
                        )
                    }
                    .into(),
                );
                c.src_model.set_vec(out.iter().map(source_data).collect::<Vec<_>>());
                c.sources = out;
                c.sync();
            });
        });
    }

    /// Requête du moteur intégré pour une cible de lecture (titres, IMDb, saison / épisode).
    fn engine_query(&self, t: &PlayTarget) -> Option<SQuery> {
        let d = self.detail.as_ref()?;
        let data = &d.data;
        if let (Some(m), Some(a)) = (t.anime(), &data.anime) {
            let an = &a.anime;
            let title = an.title.romaji.clone().unwrap_or_else(|| an.display_title());
            let mut alt = vec![];
            if let Some(e) = &an.title.english {
                alt.push(e.clone());
            }
            let season = scraper::parse::anime_season(&title).or_else(|| an.title.english.as_deref().and_then(scraper::parse::anime_season));
            return Some(SQuery { title, alt_titles: alt, year: an.season_year.map(|y| y as i32), imdb: None, season, episode: Some(m.ep as u32), anime: true });
        }
        match t {
            PlayTarget::Jf { item, .. } => {
                let (name, year, imdb) = match (&data.series, item.kind.as_str()) {
                    (Some(se), "Episode") => (se.name.clone(), se.production_year, se.imdb().map(String::from)),
                    _ => (item.name.clone(), item.production_year, item.imdb().map(String::from)),
                };
                let ep = item.kind == "Episode";
                Some(SQuery {
                    title: name,
                    year: year.map(|y| y as i32),
                    imdb,
                    season: ep.then(|| item.parent_index_number.unwrap_or(1) as u32),
                    episode: if ep { item.index_number.map(|e| e as u32) } else { None },
                    ..Default::default()
                })
            }
            PlayTarget::Aio { id, kind, .. } => {
                let parts: Vec<&str> = id.split(':').collect();
                let imdb = parts.first().filter(|p| p.starts_with("tt")).map(|p| p.to_string());
                let (season, episode) = if kind == "series" && parts.len() >= 3 {
                    (parts[parts.len() - 2].parse().ok(), parts[parts.len() - 1].parse().ok())
                } else {
                    (None, None)
                };
                let meta = data.meta_obj.as_ref();
                let title = meta.map(|m| m.name.clone()).unwrap_or_else(|| data.title.clone());
                let year = meta.and_then(|m| m.release_info.as_deref()).and_then(|r| r.get(..4)).and_then(|y| y.parse().ok());
                let imdb = imdb.or_else(|| meta.and_then(|m| m.imdb_id.clone()));
                let alt_titles = meta.map(|m| m.alt_names.clone()).unwrap_or_default();
                Some(SQuery { title, alt_titles, year, imdb, season, episode, ..Default::default() })
            }
            PlayTarget::Direct { .. } => None,
        }
    }

    /// Débride les candidats dans l'ordre jusqu'au premier lien lisible, puis lance mpv.
    fn play_candidates(&mut self, t: PlayTarget, list: Vec<Candidate>, q: SQuery, resume: bool) {
        let engine = self.svc.engine.clone();
        self.toast(format!("Préparation via {}…", engine.debrid_name()));
        self.spawn(async move {
            let mut last = String::new();
            for cand in &list {
                match engine.resolve(cand, &q).await {
                    Ok(res) => {
                        let label = [cand.info.resolution.unwrap_or_default(), &cand.info.languages.join("/").to_uppercase(), &cand.source]
                            .iter()
                            .filter(|x| !x.is_empty())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" · ");
                        let info = StreamInfo { url: res.url, filename: res.filename, provider: cand.source.clone(), ..Default::default() };
                        return post(move |c| {
                            c.toast(label);
                            c.play_entry(t, SourceEntry::Aio(info), resume);
                        });
                    }
                    Err(e) => {
                        log::info!("source écartée ({}) : {e}", cand.title);
                        last = e.to_string();
                    }
                }
            }
            post(move |c| c.toast(format!("Lecture impossible : {last}")));
        });
    }

    /// Démarre (une fois) la page de configuration et affiche son QR code.
    fn open_sources_config(&mut self) {
        if let Some(url) = self.web_url.clone() {
            return self.show_web_qr(&url);
        }
        let on_save: scraper::web::OnSave = Arc::new(|_cfg| post(|c| c.reload_sources()));
        self.spawn(async move {
            match scraper::web::start(on_save).await {
                Ok(w) => post(move |c| {
                    c.web_url = Some(w.url.clone());
                    c.show_web_qr(&w.url);
                }),
                Err(e) => post(move |c| c.toast(format!("Serveur de configuration : {e:#}"))),
            }
        });
    }

    fn show_web_qr(&mut self, url: &str) {
        let ui = self.ui();
        ui.set_set_code_qr(crate::qr::image(url));
        ui.set_set_has_qr(true);
        let host = url.split('?').next().unwrap_or(url).trim_start_matches("http://").trim_end_matches('/');
        ui.set_set_code("".into());
        ui.set_set_code_title("Sources & débrideur".into());
        ui.set_set_code_hint(
            format!("Scannez avec votre téléphone (même réseau Wi-Fi)\npour choisir le débrideur, les trackers et les priorités.\n{host}").into(),
        );
    }

    fn reload_sources(&mut self) {
        let had_tmdb = self.aio_name.starts_with("TMDB");
        self.svc = Services::new(self.svc.cfg.clone(), self.svc.http.clone());
        // Clé TMDB ajoutée ou retirée : on recharge les catalogues.
        if had_tmdb != !scraper::config::Sources::load().metadata.tmdb_key.trim().is_empty() {
            self.catalogs.clear();
            self.home.slots.clear();
            self.load_manifest();
        }
        self.toast(format!("Sources mises à jour : {}", self.svc.engine.cfg.summary()));
        self.sync();
    }

    fn nav_sources(&mut self, n: Nav) {
        let len = self.sources.len() as i32;
        match n {
            Nav::Up => self.src_focus = (self.src_focus - 1).max(0),
            Nav::Down => self.src_focus = (self.src_focus + 1).min(len - 1).max(0),
            Nav::PrevTab => self.src_focus = (self.src_focus - 5).max(0),
            Nav::NextTab => self.src_focus = (self.src_focus + 5).min(len - 1).max(0),
            Nav::Back => self.back(),
            Nav::Accept => {
                let Some(t) = self.src_target.clone() else { return };
                let Some(entry) = self.sources.get(self.src_focus as usize) else { return };
                let entry = match entry {
                    SourceEntry::Jf { item, ms, session } => SourceEntry::Jf { item: item.clone(), ms: ms.clone(), session: session.clone() },
                    SourceEntry::Aio(s) => SourceEntry::Aio(s.clone()),
                    SourceEntry::Torrent(c) => {
                        let (c, q) = (c.clone(), self.src_query.clone().unwrap_or_default());
                        return self.play_candidates(t, vec![c], q, true);
                    }
                };
                self.play_entry(t, entry, true);
            }
            _ => {}
        }
    }

    // ─────────────── Lecture ───────────────

    /// Lecture directe : source Jellyfin par défaut, sinon meilleure source AIOStreams.
    fn play_target(&mut self, t: PlayTarget, resume: bool) {
        match &t {
            PlayTarget::Jf { item, .. } => {
                let svc = self.svc.clone();
                let item = item.clone();
                self.toast("Préparation de la lecture…");
                self.spawn(async move {
                    let pi = svc.jf.playback_info(&item.id).await;
                    post(move |c| match pi {
                        Ok(pi) if !pi.media_sources.is_empty() => {
                            let ms = pi.media_sources[0].clone();
                            c.play_entry(t, SourceEntry::Jf { item, ms, session: pi.play_session_id }, resume);
                        }
                        Ok(_) => c.open_sources(t),
                        Err(e) => {
                            c.toast(format!("Jellyfin : {e:#} — recherche d'autres sources"));
                            c.open_sources(t);
                        }
                    });
                });
            }
            PlayTarget::Aio { .. } if self.svc.engine.enabled() && self.engine_query(&t).is_some() => {
                let q = self.engine_query(&t).unwrap();
                let engine = self.svc.engine.clone();
                self.toast("Recherche de la meilleure source…");
                self.spawn(async move {
                    let r = engine.search(&q).await;
                    post(move |c| {
                        if r.candidates.is_empty() {
                            c.toast(format!("Aucune source trouvée{}", r.errors.first().map(|e| format!(" ({e})")).unwrap_or_default()));
                            return;
                        }
                        // Les meilleures sources en cache d'abord ; repli automatique en cas d'échec.
                        let mut list: Vec<Candidate> = r.candidates.iter().filter(|x| x.cached != Some(false)).take(4).cloned().collect();
                        if list.is_empty() {
                            list = r.candidates.into_iter().take(2).collect();
                        }
                        c.play_candidates(t, list, q, resume);
                    });
                });
            }
            PlayTarget::Aio { kind, id, .. } => {
                let svc = self.svc.clone();
                let (kind, id) = (kind.clone(), id.clone());
                let anime = t.is_anime();
                self.toast("Recherche de la meilleure source…");
                self.spawn(async move {
                    let r = svc.aio.streams(&kind, &id, &svc.cfg.prefs, anime).await;
                    post(move |c| match r {
                        Ok(mut s) if !s.is_empty() => {
                            let best = s.remove(0);
                            c.toast([best.quality.as_str(), best.hdr.as_str(), best.provider.as_str()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" · "));
                            c.play_entry(t, SourceEntry::Aio(best), resume);
                        }
                        Ok(_) => c.toast("Aucune source disponible"),
                        Err(e) => c.toast(format!("AIOStreams : {e:#}")),
                    });
                });
            }
            PlayTarget::Direct { url, title, live, .. } => {
                let key = t.stream_key();
                let live = *live;
                let start = if resume && !live { self.state.positions.get(&key).copied().unwrap_or(0.0) } else { 0.0 };
                let req = PlayRequest { url: url.clone(), title: title.clone(), start_secs: start, http_headers: vec![], report: None, live, mpv_args: vec![] };
                let hist = self.history_template(&t);
                self.launch(t, req, (!live).then_some(key), hist);
            }
        }
    }

    fn play_entry(&mut self, t: PlayTarget, entry: SourceEntry, resume: bool) {
        if self.playing.swap(true, Ordering::SeqCst) {
            return;
        }
        let svc = self.svc.clone();
        let title = t.title();
        let (req, state_key) = match (&t, entry) {
            (PlayTarget::Jf { item, .. }, SourceEntry::Jf { ms, session, .. }) => {
                let direct = ms.protocol == "Http" && ms.path.as_deref().is_some_and(|p| p.starts_with("http"));
                let (url, headers) = if direct {
                    (ms.path.clone().unwrap(), vec![])
                } else {
                    (svc.jf.stream_url(&item.id, &ms.id), vec![svc.jf.auth_http_header()])
                };
                (
                    PlayRequest {
                        url,
                        title: title.clone(),
                        start_secs: if resume { item.resume_secs() } else { 0.0 },
                        http_headers: headers,
                        report: Some(PlayReport { item_id: item.id.clone(), media_source_id: ms.id.clone(), play_session_id: session }),
                        live: false,
                        mpv_args: vec![],
                    },
                    None,
                )
            }
            (_, SourceEntry::Aio(s)) => {
                let key = match &t {
                    PlayTarget::Aio { id, .. } => Some(format!("aio:{id}")),
                    PlayTarget::Jf { aio: Some((_, id)), .. } => Some(format!("aio:{id}")),
                    _ => None,
                };
                let start = match (&t, resume) {
                    (PlayTarget::Jf { item, .. }, true) => item.resume_secs(),
                    (_, true) => key.as_ref().and_then(|k| self.state.positions.get(k).copied()).unwrap_or(0.0),
                    _ => 0.0,
                };
                // Un élément Jellyfin lu via AIOStreams remonte quand même sa progression.
                let report = match &t {
                    PlayTarget::Jf { item, .. } => Some(PlayReport { item_id: item.id.clone(), media_source_id: item.id.clone(), play_session_id: None }),
                    _ => None,
                };
                (PlayRequest { url: s.url, title: title.clone(), start_secs: start, http_headers: vec![], report, live: false, mpv_args: vec![] }, key)
            }
            (PlayTarget::Aio { .. } | PlayTarget::Direct { .. }, SourceEntry::Jf { .. }) | (_, SourceEntry::Torrent(_)) => unreachable!(),
        };
        let hist = self.history_template(&t);
        self.launch(t, req, state_key, hist);
    }

    /// Lance mpv ; `key` : clé de reprise locale, `hist` : entrée d'historique à compléter.
    fn launch(&mut self, t: PlayTarget, req: PlayRequest, key: Option<String>, hist: Option<HistoryEntry>) {
        self.playing.store(true, Ordering::SeqCst);
        let svc = self.svc.clone();
        let ui = self.ui();
        ui.set_playing(true);
        ui.set_playing_title(req.title.clone().into());
        let jf = svc.jf_ready().then(|| svc.jf.clone());
        let anime = t.anime().cloned();
        self.spawn(async move {
            let res = player::play(&svc.cfg.player, req, jf).await;
            post(move |c| c.playback_done(res, key, anime, hist));
        });
    }

    /// Modèle d'entrée d'historique pour une lecture lancée depuis la fiche courante.
    fn history_template(&self, t: &PlayTarget) -> Option<HistoryEntry> {
        let full = t.title();
        let (title, subtitle) = match full.split_once(" — ") {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (full.clone(), String::new()),
        };
        let d = self.detail.as_ref()?;
        let open = match &d.root {
            MediaRef::Jf(it) => OpenRef::Jf { id: it.series_id.clone().unwrap_or_else(|| it.id.clone()) },
            MediaRef::JfId(id) => OpenRef::Jf { id: id.clone() },
            MediaRef::Meta(m) => OpenRef::Meta { kind: m.kind.clone(), id: m.id.clone() },
            MediaRef::Anime(a) => OpenRef::Anime { id: a.id },
            MediaRef::AnimeId(id) => OpenRef::Anime { id: *id },
            MediaRef::Iptv(r) => OpenRef::Iptv { profile: r.profile.id.clone(), kind: r.kind.key().into(), id: r.entry.id.clone() },
            MediaRef::Hist(_) | MediaRef::Action(_) => return None,
        };
        Some(HistoryEntry {
            key: t.stream_key(),
            title,
            subtitle,
            image: d.data.poster.clone(),
            backdrop: d.data.backdrop.clone(),
            open,
            position: 0.0,
            duration: 0.0,
            finished: false,
            watched_at: 0,
        })
    }

    fn playback_done(&mut self, res: anyhow::Result<PlayOutcome>, key: Option<String>, anime: Option<AnimeMark>, hist: Option<HistoryEntry>) {
        self.playing.store(false, Ordering::SeqCst);
        let ui = self.ui();
        ui.set_playing(false);
        if std::mem::take(&mut self.disc_playing) {
            // Échec immédiat d'un Blu-ray : presque toujours le chiffrement AACS.
            let failed = res.as_ref().map_or(true, |o| o.duration <= 0.0);
            if failed && self.disc.as_ref().is_some_and(|d| d.kind == crate::disc::Kind::Bluray) {
                self.toast(crate::disc::aacs_hint());
            }
        }
        let mut finished = false;
        match res {
            Ok(o) => {
                finished = o.finished();
                if let Some(k) = &key {
                    if o.finished() || o.position < 30.0 {
                        self.state.positions.remove(k);
                    } else {
                        self.state.positions.insert(k.clone(), o.position);
                    }
                    if o.duration > 0.0 {
                        self.state.durations.insert(k.clone(), o.duration);
                    }
                }
                if let Some(mut h) = hist {
                    if o.position > 15.0 || o.duration == 0.0 {
                        h.position = o.position;
                        h.duration = o.duration;
                        h.finished = o.finished();
                        h.watched_at = chrono::Local::now().timestamp();
                        self.state.record(h);
                    }
                }
                if let Some(m) = anime {
                    if o.finished() {
                        let e = self.state.anime.entry(m.id).or_default();
                        e.title = m.title;
                        e.cover = m.cover;
                        e.total = m.total;
                        e.last_watched = e.last_watched.max(m.ep);
                        e.updated = chrono::Local::now().timestamp();
                        self.toast(format!("Épisode {} marqué comme vu", m.ep));
                    } else if o.position > 60.0 && !self.state.anime.contains_key(&m.id) {
                        // Commencé : apparaît dans « Animes en cours ».
                        self.state.anime.insert(m.id, AnimeProgress { title: m.title, cover: m.cover, last_watched: m.ep - 1, total: m.total, updated: chrono::Local::now().timestamp() });
                    }
                }
                self.state.save();
            }
            Err(e) => self.toast(format!("Lecture impossible : {e:#}")),
        }
        self.reload_detail();
        self.load_dashboard();
        self.reload_tab_keep_focus();
        // Proposer l'épisode suivant.
        if let (true, Some(i)) = (finished, self.played_ep.take()) {
            let has_next = self.screen == SCR_DETAIL && self.detail.as_ref().is_some_and(|d| i + 1 < d.data.episodes.len());
            if has_next {
                self.start_next_prompt(i + 1);
            }
        }
        self.sync();
    }

    fn start_next_prompt(&mut self, idx: usize) {
        let Some(d) = &self.detail else { return };
        let Some((_, info)) = d.data.episodes.get(idx) else { return };
        let label = [info.label.as_str(), info.title.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("  ·  ");
        let ui = self.ui();
        ui.set_next_title(label.into());
        ui.set_next_image(self.images.get(info.image.as_deref()).clone());
        self.next_gen += 1;
        let g = self.next_gen;
        self.next_prompt = Some((g, idx));
        self.next_count = 10;
        fn tick(g: u64) {
            slint::Timer::single_shot(Duration::from_secs(1), move || {
                with_ctrl(|c| {
                    if c.next_prompt.map(|p| p.0) != Some(g) {
                        return;
                    }
                    c.next_count -= 1;
                    if c.next_count <= 0 {
                        c.play_next_now();
                    } else {
                        tick(g);
                    }
                    c.sync();
                })
            });
        }
        tick(g);
    }

    fn play_next_now(&mut self) {
        let Some((_, idx)) = self.next_prompt.take() else { return };
        if let Some(d) = self.detail.as_mut() {
            d.ep = idx as i32;
            d.area = 2;
        }
        self.played_ep = Some(idx);
        if let Some(t) = self.current_target(true) {
            let resume = self.state.positions.get(&t.stream_key()).is_some_and(|p| *p > 30.0);
            self.play_target(t, resume);
        }
    }

    fn cancel_next(&mut self) {
        self.next_prompt = None;
    }

    // ─────────────── Recherche ───────────────

    fn nav_search(&mut self, n: Nav) {
        let cols = 7;
        let nkeys = self.keys.len() as i32;
        match n {
            Nav::Back => {
                self.tab = TAB_HOME;
                self.screen = SCR_MEDIA;
                self.stack.retain(|s| *s != SCR_MEDIA);
                self.load_tab(TAB_HOME);
            }
            Nav::PrevTab => {
                self.screen = SCR_MEDIA;
                self.switch_tab(TAB_HISTORY);
            }
            Nav::NextTab => {}
            Nav::Char(ch) => {
                self.query.push(ch);
                self.schedule_search();
            }
            Nav::Backspace => {
                self.query.pop();
                self.schedule_search();
            }
            _ if self.kb_active => match n {
                Nav::Up if self.kb_focus >= cols => self.kb_focus -= cols,
                Nav::Down if self.kb_focus + cols < nkeys => self.kb_focus += cols,
                Nav::Down => self.kb_focus = nkeys - 1,
                Nav::Left if self.kb_focus % cols > 0 => self.kb_focus -= 1,
                Nav::Right if self.kb_focus % cols < cols - 1 && self.kb_focus < nkeys - 1 => self.kb_focus += 1,
                Nav::Right if !self.search.visible.is_empty() => {
                    self.kb_active = false;
                    self.search.row = self.search.row.max(0);
                }
                Nav::Accept => {
                    let k = self.keys[self.kb_focus as usize].clone();
                    match k.as_str() {
                        "␣" => self.query.push(' '),
                        "⌫" => {
                            self.query.pop();
                        }
                        "Effacer" => self.query.clear(),
                        s => self.query.push_str(s),
                    }
                    self.schedule_search();
                }
                _ => {}
            },
            Nav::Left if self.search.col() == 0 => self.kb_active = true,
            Nav::Accept | Nav::Alt => {
                if let Some((r, _)) = self.search.focused().cloned() {
                    self.open_detail(r);
                }
            }
            _ => {
                self.search.nav(n, false);
            }
        }
    }

    fn schedule_search(&mut self) {
        self.search_gen += 1;
        let stamp = self.search_gen;
        let q = self.query.trim().to_string();
        if q.len() < 2 {
            self.search.reset(vec![], &mut self.images);
            self.ui().set_q_loading(false);
            return;
        }
        self.ui().set_q_loading(true);
        let svc = self.svc.clone();
        let search_cats: Vec<CatalogDef> = self.catalogs.iter().filter(|c| c.is_search()).cloned().collect();
        let anime_state = self.state.anime.clone();
        slint::Timer::single_shot(Duration::from_millis(650), move || {
            with_ctrl(|c| {
                if c.search_gen != stamp {
                    return;
                }
                let mut titles = vec![("Animes".to_string(), false)];
                if svc.jf_ready() {
                    titles.insert(0, ("Ma médiathèque".into(), false));
                }
                for cat in &search_cats {
                    titles.push((if cat.kind == "movie" { "Films".into() } else { "Séries".into() }, false));
                }
                let rgen = c.search.reset(titles, &mut c.images);
                let mut slot = 0;
                if svc.jf_ready() {
                    let (s, q) = (svc.clone(), q.clone());
                    c.spawn(async move {
                        let items = s.jf.items(None, "Movie,Series", "SortName", 40, Some(&q)).await.unwrap_or_default();
                        let v: Vec<_> = items.into_iter().map(|i| { let ci = jf_card(&s.jf, &i, false); (MediaRef::Jf(i), ci) }).collect();
                        post(move |c| { c.search.fill(rgen, 0, v, &mut c.images); c.ui().set_q_loading(false); c.sync(); });
                    });
                    slot = 1;
                }
                {
                    let (s, q, st) = (svc.clone(), q.clone(), anime_state.clone());
                    let my = slot;
                    c.spawn(async move {
                        let st = State { anime: st, ..Default::default() };
                        let v: Vec<_> = s.ani.search(&q).await.unwrap_or_default().into_iter().map(|a| { let ci = anime_card(&a, &st); (MediaRef::Anime(a), ci) }).collect();
                        post(move |c| { c.search.fill(rgen, my, v, &mut c.images); c.ui().set_q_loading(false); c.sync(); });
                    });
                    slot += 1;
                }
                for cat in search_cats {
                    let (s, q) = (svc.clone(), q.clone());
                    let my = slot;
                    c.spawn(async move {
                        let v: Vec<_> = s.aio.catalog(&cat.kind, &cat.id, Some(&q)).await.unwrap_or_default().into_iter().map(|m| { let ci = meta_card(&m); (MediaRef::Meta(m), ci) }).collect();
                        post(move |c| { c.search.fill(rgen, my, v, &mut c.images); c.ui().set_q_loading(false); c.sync(); });
                    });
                    slot += 1;
                }
                c.sync();
            });
        });
    }

    fn clear_code_card(&self) {
        let ui = self.ui();
        ui.set_set_code("".into());
        ui.set_set_has_qr(false);
        ui.set_set_code_qr(Image::default());
    }

    // ─────────────── Paramètres ───────────────

    fn settings_actions(&self) -> Vec<String> {
        vec![
            "Sources & débrideur (configurer sur le téléphone)".into(),
            format!("Clavier physique : {}", session::keyboard_label(&self.svc.cfg.session.keyboard)),
            format!("Audio : {}", if self.svc.cfg.player.audio == "bitperfect" { "bit-perfect (fréquence d'origine, bitstream HDMI)" } else { "standard (mixage PipeWire)" }),
            match (&self.update, self.update_busy) {
                (_, true) => "Mise à jour en cours…".to_string(),
                (Some(r), false) => format!("Installer la mise à jour {} (actuelle : {})", r.version, crate::update::VERSION),
                (None, false) => format!("Rechercher une mise à jour (version {})", crate::update::VERSION),
            },
            if self.svc.jf_ready() { "Déconnecter Jellyfin".into() } else { "Connecter Jellyfin (Quick Connect)".into() },
            format!("Bibliothèque anime : {}", self.anime_library().and_then(|id| self.views.iter().find(|v| v.id == id).map(|v| v.name.clone())).unwrap_or_else(|| "aucune".into())),
            "Importer AIOStreams depuis Gelato (SSH)".into(),
            "Recharger la médiathèque".into(),
            "Retour".into(),
        ]
    }

    fn sync_settings(&mut self) {
        let cfg = &self.svc.cfg;
        let lines = vec![
            format!(
                "Jellyfin : {} — {}",
                cfg.jellyfin.url,
                if cfg.jellyfin_ready() { format!("connecté ({})", cfg.jellyfin.user_name) } else { "non connecté".into() }
            ),
            format!(
                "Catalogues : {}",
                if self.aio_name.is_empty() { if self.svc.aio.enabled() { "injoignable".to_string() } else { "non configuré".into() } } else { format!("{} · {} catalogues", self.aio_name, self.catalogs.len()) }
            ),
            format!(
                "Écran : {} — {}",
                if self.hdr.output.is_empty() { "?" } else { &self.hdr.output },
                if self.hdr.enabled { "HDR activé (lecture mpv gpu-next + Vulkan Wayland)" } else if self.hdr.available { "HDR désactivé" } else { "HDR non disponible" }
            ),
            format!("Sources : {}", self.svc.engine.cfg.summary()),
            format!("Favoris : {} (enregistrés sur ce PC)", self.state.favorites.len()),
            format!("Lecteur : {} · décodage {}", cfg.player.command, if cfg.player.hwdec == "auto" { crate::iptv::hwdec_for_gpu() } else { &cfg.player.hwdec }),
            format!(
                "Système : {} — Steam Gamescope {}",
                session::Edition::detect().map(|e| e.summary()).unwrap_or_else(|| "non Bazzite".into()),
                if session::gamescope_available() { "disponible" } else { "indisponible (édition non Deck ou NVIDIA)" }
            ),
            format!("Session HTPC : {}", if session::in_noosfin_session() { "active" } else { "non (lancé hors session noosfin)" }),
            format!("Configuration : {}", crate::config::config_path().display()),
        ];
        let ui = self.ui();
        ui.set_set_lines(ModelRc::from(Rc::new(VecModel::from(lines.into_iter().map(SharedString::from).collect::<Vec<_>>()))));
        ui.set_set_actions(ModelRc::from(Rc::new(VecModel::from(
            self.settings_actions().into_iter().map(SharedString::from).collect::<Vec<_>>(),
        ))));
    }

    fn nav_settings(&mut self, n: Nav) {
        let len = self.settings_actions().len() as i32;
        match n {
            Nav::Up => self.set_focus = (self.set_focus - 1).max(0),
            Nav::Down => self.set_focus = (self.set_focus + 1).min(len - 1),
            Nav::Back => {
                self.qc_gen += 1;
                self.clear_code_card();
                self.back();
            }
            Nav::Accept => match self.set_focus {
                0 => self.open_sources_config(),
                1 => self.cycle_keyboard(),
                2 => self.toggle_audio(),
                3 => self.update_action(),
                4 if self.svc.jf_ready() => {
                    if self.confirmed(21) {
                        self.update_config(|c| {
                            c.jellyfin.token.clear();
                            c.jellyfin.user_id.clear();
                            c.jellyfin.user_name.clear();
                        });
                        self.views.clear();
                        self.load_tab(TAB_HOME);
                        self.load_dashboard();
                        self.toast("Jellyfin déconnecté");
                    }
                }
                4 => self.quick_connect(),
                5 => self.cycle_anime_library(),
                6 => self.import_gelato(),
                7 => {
                    self.views.clear();
                    self.load_manifest();
                    self.load_tab(self.tab.min(TAB_SHOWS));
                    self.load_dashboard();
                    self.refresh_hdr();
                    self.toast("Rechargement…");
                }
                _ => {
                    self.qc_gen += 1;
                    self.clear_code_card();
                    self.back();
                }
            },
            _ => {}
        }
    }

    fn update_config(&mut self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.svc.cfg.clone();
        f(&mut cfg);
        if let Err(e) = cfg.save() {
            self.toast(format!("Sauvegarde impossible : {e:#}"));
        }
        self.svc = Services::new(cfg, self.svc.http.clone());
    }

    // ─────────────── Disque optique ───────────────

    /// Tuiles du tableau de bord (voir `Dashboard.tiles` dans app.slint).
    fn dash_tiles(&self) -> Vec<i32> {
        let mut v = vec![0, 1, 2];
        if self.disc.is_some() {
            v.push(3);
        }
        if session::gamescope_available() {
            v.push(4);
        }
        v.push(5);
        v
    }

    fn disc_changed(&mut self, disc: Option<crate::disc::Disc>) {
        if disc == self.disc {
            return;
        }
        use crate::disc::Kind;
        let inserted = disc.as_ref().filter(|d| !matches!(d.kind, Kind::Empty)).map(|d| d.subtitle());
        let was = self.disc.as_ref().map(|d| d.kind);
        let tiles_before = self.dash_tiles().len();
        self.disc = disc;
        if let Some(s) = inserted.filter(|_| !matches!(was, Some(k) if k != Kind::Empty)) {
            self.toast(format!("Disque inséré : {s}"));
        }
        if self.dash_tiles().len() != tiles_before {
            self.dash_index = self.dash_index.min(self.dash_tiles().len() as i32 - 1);
        }
        self.sync();
    }

    fn play_disc(&mut self) {
        use crate::disc::Kind;
        let Some(d) = self.disc.clone() else { return };
        let Some((url, args)) = d.mpv() else {
            return self.toast(match d.kind {
                Kind::Empty => "Aucun disque dans le lecteur".to_string(),
                _ => "Disque de données : rien à lire directement".to_string(),
            });
        };
        let title = d.title();
        let key = format!("disc:{}", d.label);
        let req = PlayRequest { url: url.clone(), title: title.clone(), start_secs: 0.0, http_headers: vec![], report: None, live: false, mpv_args: args };
        self.disc_playing = true;
        self.launch(PlayTarget::Direct { url, title, key, live: false }, req, None, None);
    }

    // ─────────────── Mises à jour ───────────────

    fn check_update(&mut self, quiet: bool) {
        let http = self.svc.http.clone();
        self.spawn(async move {
            let r = crate::update::check(&http).await;
            post(move |c| {
                match r {
                    Ok(Some(rel)) => {
                        c.toast(format!("Noosfin {} est disponible · Paramètres → Mise à jour", rel.version));
                        c.update = Some(rel);
                    }
                    Ok(None) if !quiet => c.toast(format!("Noosfin est à jour ({})", crate::update::VERSION)),
                    Err(e) if !quiet => c.toast(format!("Vérification impossible : {e:#}")),
                    _ => {}
                }
                c.sync();
            });
        });
    }

    fn update_action(&mut self) {
        if self.update_busy {
            return;
        }
        let Some(rel) = self.update.clone() else { return self.check_update(false) };
        self.update_busy = true;
        self.toast(format!("Téléchargement de Noosfin {}…", rel.version));
        let http = self.svc.http.clone();
        self.spawn(async move {
            let r = crate::update::install(&http, &rel).await;
            post(move |c| {
                c.update_busy = false;
                match r {
                    Ok(()) => {
                        c.update = None;
                        if session::in_noosfin_session() {
                            c.toast(format!("Noosfin {} installé · relance de l'interface", rel.version));
                            c.switch_session(Target::Restart);
                        } else {
                            c.toast(format!("Noosfin {} installé · relancez l'application", rel.version));
                        }
                    }
                    Err(e) => c.toast(format!("Mise à jour impossible : {e:#}")),
                }
                c.sync();
            });
        });
        self.sync();
    }

    fn toggle_audio(&mut self) {
        let bitperfect = self.svc.cfg.player.audio != "bitperfect";
        self.update_config(|c| c.player.audio = if bitperfect { "bitperfect" } else { "standard" }.into());
        self.toast(if bitperfect {
            "Audio bit-perfect : fréquence d'origine, Dolby / DTS envoyés tels quels à l'ampli (HDMI)"
        } else {
            "Audio standard : mixage PipeWire"
        });
        self.sync();
    }

    fn cycle_keyboard(&mut self) {
        let cur = &self.svc.cfg.session.keyboard;
        let i = session::KEYBOARDS.iter().position(|k| k.0 == cur).map_or(0, |i| (i + 1) % session::KEYBOARDS.len());
        let code = session::KEYBOARDS[i].0.to_string();
        self.update_config(|c| c.session.keyboard = code.clone());
        match session::apply_keyboard(&code) {
            Ok(()) => self.toast(format!("Clavier : {} · appliqué à la relance de l'interface", session::keyboard_label(&code))),
            Err(e) => self.toast(format!("Clavier : {e:#}")),
        }
        self.sync();
    }

    fn cycle_anime_library(&mut self) {
        let shows: Vec<Item> = self.views.iter().filter(|v| v.collection_type.as_deref() == Some("tvshows")).cloned().collect();
        if shows.is_empty() {
            self.toast("Aucune bibliothèque de séries");
            return;
        }
        let cur = self.anime_library();
        let idx = cur.and_then(|c| shows.iter().position(|v| v.id == c)).map_or(0, |i| (i + 1) % shows.len());
        let name = shows[idx].name.clone();
        self.update_config(|c| c.jellyfin.anime_library = name.clone());
        self.toast(format!("Bibliothèque anime : {name}"));
    }

    fn quick_connect(&mut self) {
        self.qc_gen += 1;
        let stamp = self.qc_gen;
        // Client sans jeton : on repart de zéro.
        let mut cfg = self.svc.cfg.clone();
        cfg.jellyfin.token.clear();
        let jf = Jellyfin::new(self.svc.http.clone(), &cfg.jellyfin.url, "", "", &cfg.jellyfin.device_id);
        self.spawn(async move {
            let st = match jf.quick_connect_initiate().await {
                Ok(s) => s,
                Err(e) => return post(move |c| c.toast(format!("Quick Connect indisponible : {e:#}"))),
            };
            let (code, secret) = (st.code.clone(), st.secret.clone());
            post(move |c| {
                let ui = c.ui();
                ui.set_set_has_qr(false);
                ui.set_set_code(code.into());
                ui.set_set_code_title("Quick Connect".into());
                ui.set_set_code_hint("Dans Jellyfin (téléphone ou navigateur) :\nProfil → Quick Connect → saisir ce code".into());
            });
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let mut alive = true;
                let (tx, rx) = tokio::sync::oneshot::channel();
                post(move |c| {
                    let _ = tx.send(c.qc_gen == stamp);
                });
                if let Ok(v) = rx.await {
                    alive = v;
                }
                if !alive {
                    return;
                }
                match jf.quick_connect_state(&secret).await {
                    Ok(s) if s.authenticated => {
                        let auth = jf.quick_connect_auth(&secret).await;
                        return post(move |c| {
                            c.clear_code_card();
                            match auth {
                                Ok(a) => {
                                    c.update_config(|cfg| {
                                        cfg.jellyfin.token = a.access_token;
                                        cfg.jellyfin.user_id = a.user.id;
                                        cfg.jellyfin.user_name = a.user.name.clone();
                                    });
                                    c.toast(format!("Connecté à Jellyfin : {}", a.user.name));
                                    c.views.clear();
                                    c.load_tab(TAB_HOME);
                                    c.load_dashboard();
                                }
                                Err(e) => c.toast(format!("Échec de l'authentification : {e:#}")),
                            }
                            c.sync();
                        });
                    }
                    Ok(_) => {}
                    Err(e) => log::debug!("quick connect : {e:#}"),
                }
            }
            post(move |c| {
                if c.qc_gen == stamp {
                    c.clear_code_card();
                    c.toast("Code Quick Connect expiré");
                }
            });
        });
    }

    fn import_gelato(&mut self) {
        let host = self.svc.cfg.aiostreams.gelato_ssh.clone();
        if host.is_empty() {
            return self.toast("Renseignez [aiostreams] gelato_ssh = \"utilisateur@serveur\" dans config.toml");
        }
        self.toast(format!("Lecture de la configuration Gelato sur {host}…"));
        self.spawn(async move {
            let out = tokio::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=6", &host, "docker exec jellyfin cat /config/plugins/configurations/Gelato.xml"])
                .output()
                .await;
            let url = out.ok().and_then(|o| aio::url_from_gelato_xml(&String::from_utf8_lossy(&o.stdout)));
            post(move |c| match url {
                Some(u) => {
                    c.update_config(|cfg| cfg.aiostreams.manifest_url = u);
                    c.catalogs.clear();
                    c.load_manifest();
                    c.toast("AIOStreams importé depuis Gelato");
                    c.sync();
                }
                None => c.toast("URL introuvable (SSH ou Gelato.xml)"),
            });
        });
    }
}

// ═══════════════════════════════ Chargement d'une fiche ═══════════════════════════════

/// Identifiant AIOStreams d'un épisode d'anime (Kitsu en priorité : fichiers par épisode).
fn anime_stream_id(a: &AnimeCtx, n: i64) -> (String, String) {
    match (&a.kitsu, a.anime.id_mal) {
        (Some(k), _) => ("series".to_string(), format!("kitsu:{k}:{n}")),
        (None, Some(m)) => ("anime".to_string(), format!("mal:{m}:{n}")),
        (None, None) => ("anime".to_string(), format!("anilist:{}:{n}", a.anime.id)),
    }
}

/// Progression et « vu » des épisodes lus en streaming (état local).
fn decorate_eps(state: &State, data: &mut DetailLoad) {
    let last = data.anime.as_ref().and_then(|a| state.anime.get(&a.anime.id)).map_or(0, |p| p.last_watched);
    let profile = data.iptv.as_ref().map(|r| r.profile.id.clone()).unwrap_or_default();
    for (ep, info) in data.episodes.iter_mut() {
        let key = match ep {
            EpRef::Anime { n, jf } => {
                info.watched |= *n <= last;
                match (jf, &data.anime) {
                    (None, Some(a)) => Some(format!("aio:{}", anime_stream_id(a, *n).1)),
                    _ => None,
                }
            }
            EpRef::Stremio(v) => Some(format!("aio:{}", v.id)),
            EpRef::Iptv(e) => Some(format!("iptv:{profile}:ep:{}", e.id)),
            EpRef::Jf(_) => None,
        };
        if let Some(k) = key {
            let p = state.progress_of(&k);
            if p > 0.0 && !info.watched {
                info.progress = p;
            }
            if state.history.iter().any(|h| h.key == k && h.finished) {
                info.watched = true;
            }
        }
    }
}

fn stremio_episodes(m: &Meta, season: i64) -> Vec<(EpRef, EpInfo)> {
    m.videos
        .iter()
        .filter(|v| v.season.unwrap_or(1) == season)
        .map(|v| {
            let label = match (v.season, v.episode) {
                (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
                (_, Some(e)) => format!("E{e:02}"),
                _ => String::new(),
            };
            (
                EpRef::Stremio(v.clone()),
                EpInfo {
                    label,
                    title: v.name.clone().or(v.title.clone()).unwrap_or_default(),
                    overview: v.overview.clone().or(v.description.clone()).unwrap_or_default(),
                    image: v.thumbnail.clone(),
                    progress: -1.0,
                    watched: false,
                    local: false,
                },
            )
        })
        .collect()
}

async fn load_detail(svc: &Services, r: MediaRef, anime_lib: Option<String>) -> anyhow::Result<DetailLoad> {
    match r {
        MediaRef::Jf(it) => load_jf(svc, it).await,
        MediaRef::Meta(m) => load_meta(svc, m).await,
        MediaRef::Anime(a) => load_anime(svc, a.id, anime_lib).await,
        MediaRef::AnimeId(id) => load_anime(svc, id, anime_lib).await,
        MediaRef::JfId(id) => load_jf(svc, svc.jf.item(&id).await?).await,
        MediaRef::Iptv(r) => iptv_ui::load_detail(svc, *r).await,
        MediaRef::Hist(_) | MediaRef::Action(_) => anyhow::bail!("élément non ouvrable"),
    }
}

async fn load_jf(svc: &Services, it: Item) -> anyhow::Result<DetailLoad> {
    let jf = &svc.jf;
    let (series_id, focus_season, focus_ep) = match it.kind.as_str() {
        "Episode" => (it.series_id.clone(), it.season_id.clone(), Some(it.id.clone())),
        "Season" => (it.series_id.clone(), Some(it.id.clone()), None),
        "Series" => (Some(it.id.clone()), None, None),
        _ => (None, None, None),
    };
    let Some(series_id) = series_id else {
        // Film
        let full = jf.item(&it.id).await.unwrap_or(it);
        let ci = jf_card(jf, &full, false);
        return Ok(DetailLoad {
            title: full.name.clone(),
            meta: ci.meta,
            overview: ci.overview,
            badges: video_badges(&full),
            backdrop: ci.backdrop,
            poster: ci.image,
            item: Some(full),
            ..Default::default()
        });
    };
    let series = jf.item(&series_id).await?;
    let seasons = jf.seasons(&series_id).await.unwrap_or_default();
    let mut season_idx = 0;
    if let Some(fs) = &focus_season {
        season_idx = seasons.iter().position(|s| &s.id == fs).unwrap_or(0);
    } else if let Ok(next) = jf.next_up().await {
        // Saison de l'épisode « à suivre » de cette série.
        if let Some(n) = next.iter().find(|n| n.series_id.as_deref() == Some(&series_id)) {
            season_idx = seasons.iter().position(|s| Some(&s.id) == n.season_id.as_ref()).unwrap_or(0);
        }
    } else if let Some(i) = seasons.iter().position(|s| s.index_number.unwrap_or(0) >= 1) {
        season_idx = i;
    }
    let eps = match seasons.get(season_idx) {
        Some(s) => jf.episodes(&series_id, Some(&s.id)).await.unwrap_or_default(),
        None => jf.episodes(&series_id, None).await.unwrap_or_default(),
    };
    let focus = focus_ep.and_then(|f| eps.iter().position(|e| e.id == f));
    let ci = jf_card(jf, &series, false);
    let mut badges = vec![];
    if let Some(r) = &series.official_rating {
        badges.push(r.clone());
    }
    badges.push(format!("{} saison{}", seasons.len(), if seasons.len() > 1 { "s" } else { "" }));
    Ok(DetailLoad {
        title: series.name.clone(),
        meta: ci.meta,
        overview: ci.overview,
        badges,
        backdrop: ci.backdrop,
        poster: ci.image,
        seasons: seasons.iter().map(|s| (SeasonRef::Jf(s.clone()), s.name.clone())).collect(),
        season: season_idx,
        episodes: eps.iter().map(|e| (EpRef::Jf(e.clone()), jf_ep_info(jf, e))).collect(),
        focus_ep: focus,
        series: Some(series),
        ..Default::default()
    })
}

async fn load_meta(svc: &Services, m: Meta) -> anyhow::Result<DetailLoad> {
    let full = svc.aio.meta(&m.kind, &m.id).await.unwrap_or_else(|_| m.clone());
    let full = Meta { poster: full.poster.or(m.poster), background: full.background.or(m.background), ..full };
    let ci = meta_card(&full);
    let mut seasons: Vec<i64> = full.videos.iter().filter_map(|v| v.season).filter(|s| *s > 0).collect();
    seasons.sort();
    seasons.dedup();
    let season = seasons.first().copied().unwrap_or(1);
    let episodes = if full.kind == "series" { stremio_episodes(&full, season) } else { vec![] };
    Ok(DetailLoad {
        title: full.name.clone(),
        meta: ci.meta,
        overview: ci.overview,
        badges: vec![if full.kind == "movie" { "Film" } else { "Série" }.into(), if full.id.starts_with("tmdb:") || svc.aio.tmdb_active() { "TMDB".into() } else { "AIOStreams".into() }],
        backdrop: ci.backdrop,
        poster: ci.image,
        seasons: if full.kind == "series" { seasons.iter().map(|s| (SeasonRef::Stremio(*s), format!("Saison {s}"))).collect() } else { vec![] },
        season: 0,
        episodes,
        meta_obj: Some(full),
        ..Default::default()
    })
}

async fn load_anime(svc: &Services, id: i64, anime_lib: Option<String>) -> anyhow::Result<DetailLoad> {
    let a = svc.ani.by_id(id).await?;
    // Autres saisons et textes français, en parallèle du reste.
    let franchise = {
        let ani = svc.ani.clone();
        tokio::spawn(async move { ani.franchise(id).await.unwrap_or_default() })
    };
    let french = {
        let (ani, tmdb) = (svc.ani.clone(), svc.tmdb.clone());
        let titles: Vec<String> = [a.title.english.clone(), a.title.romaji.clone()].into_iter().flatten().collect();
        let (year, movie) = (a.season_year, a.format.as_deref() == Some("MOVIE"));
        tokio::spawn(async move {
            let tmdb = tmdb?;
            let map = ani.mappings(id).await.unwrap_or_default();
            tmdb.anime_fr(&map, &titles, year, movie).await.map_err(|e| log::warn!("TMDB anime {id} : {e:#}")).ok().flatten()
        })
    };
    let kitsu = match a.id_mal {
        Some(m) => svc.ani.kitsu_id(m).await.unwrap_or(None),
        None => None,
    };
    let aired = a.aired_episodes().max(if a.status.as_deref() == Some("NOT_YET_RELEASED") { 0 } else { 1 });
    let kitsu_eps = match &kitsu {
        Some(k) => svc.ani.kitsu_episodes(k, aired).await.unwrap_or_default(),
        None => vec![],
    };

    // Correspondance avec la bibliothèque Jellyfin (série à saison unique).
    let mut jf_eps: HashMap<i64, Item> = HashMap::new();
    if svc.jf_ready() {
        let titles = [a.title.romaji.clone(), a.title.english.clone()];
        for t in titles.into_iter().flatten() {
            let Ok(found) = svc.jf.items(anime_lib.as_deref(), "Series", "SortName", 5, Some(&t)).await else { continue };
            if let Some(series) = found.first() {
                let eps = svc.jf.episodes(&series.id, None).await.unwrap_or_default();
                let regular: Vec<&Item> = eps.iter().filter(|e| e.parent_index_number.unwrap_or(1) >= 1).collect();
                let seasons: HashSet<i64> = regular.iter().filter_map(|e| e.parent_index_number).collect();
                if seasons.len() <= 1 {
                    for e in regular {
                        if let Some(n) = e.index_number {
                            jf_eps.insert(n, e.clone());
                        }
                    }
                }
                break;
            }
        }
    }

    let french = french.await.ok().flatten();
    if let Some(fr) = &french {
        fr_overview_store(id, &fr.overview);
    }
    let streaming = &a.streaming_episodes;
    let mut episodes = vec![];
    for n in 1..=aired {
        let k = kitsu_eps.iter().find(|e| e.number == n);
        let jf = jf_eps.get(&n).cloned();
        let se = streaming.iter().find(|s| s.title.as_deref().is_some_and(|t| t.starts_with(&format!("Episode {n} "))));
        let title = k
            .map(|k| k.title.clone())
            .filter(|t| !t.is_empty())
            .or_else(|| se.and_then(|s| s.title.clone()).map(|t| t.splitn(2, " - ").nth(1).unwrap_or(&t).to_string()))
            .or_else(|| jf.as_ref().map(|j| j.name.clone()))
            .unwrap_or_default();
        let fr = french.as_ref().and_then(|f| f.episodes.get(&n));
        let image = k
            .and_then(|k| k.thumbnail.clone())
            .or_else(|| fr.and_then(|f| f.2.clone()))
            .or_else(|| se.and_then(|s| s.thumbnail.clone()))
            .or_else(|| jf.as_ref().and_then(|j| j.image_tags.get("Primary").map(|t| svc.jf.image_url(&j.id, "Primary", Some(t), 300))));
        let title = fr.map(|f| f.0.clone()).filter(|t| !t.is_empty()).unwrap_or(title);
        let info = EpInfo {
            label: format!("Épisode {n}"),
            title,
            overview: fr.map(|f| f.1.clone()).filter(|o| !o.is_empty()).or_else(|| k.map(|k| k.overview.clone())).unwrap_or_default(),
            image,
            progress: jf.as_ref().map_or(-1.0, |j| j.progress()),
            watched: jf.as_ref().is_some_and(|j| j.played()),
            local: jf.is_some(),
        };
        episodes.push((EpRef::Anime { n, jf }, info));
    }
    let mut badges = vec![];
    if let Some(f) = &a.format {
        badges.push(f.replace('_', " "));
    }
    badges.push(if kitsu.is_some() { "Kitsu ✓".into() } else { "MAL".into() });
    if !jf_eps.is_empty() {
        badges.push(format!("{} ép. dans Jellyfin", jf_eps.len()));
    }
    if french.is_some() {
        badges.push("FR".into());
    }
    let franchise = franchise.await.unwrap_or_default();
    let season = franchise.iter().position(|e| e.id == id).unwrap_or(0);
    let seasons: Vec<(SeasonRef, String)> = franchise.into_iter().map(|e| (SeasonRef::Anime(e.id), e.label)).collect();
    if let Some(n) = &a.next_airing {
        let when = chrono::DateTime::from_timestamp(n.airing_at, 0).map(|d| d.with_timezone(&chrono::Local));
        if let Some(w) = when {
            badges.push(format!("Ép. {} le {:02}/{:02} à {:02}h{:02}", n.episode, w.day(), w.month(), w.hour(), w.minute()));
        }
    }
    let mut overview = french.as_ref().map(|f| f.overview.clone()).filter(|o| !o.is_empty()).unwrap_or_else(|| a.clean_description());
    if !a.genres.is_empty() {
        overview = format!("{}\n\n{}", a.genres.join(" · "), overview);
    }
    Ok(DetailLoad {
        title: a.display_title(),
        meta: a.meta_line(),
        overview,
        badges,
        backdrop: a.banner.clone().or_else(|| a.cover_url()),
        poster: a.cover_url(),
        episodes,
        seasons,
        season,
        anime: Some(AnimeCtx { anime: a, kitsu }),
        ..Default::default()
    })
}

// Résumés français des animes déjà ouverts, pour les cartes (~/.cache/noosfin/anime-fr.json).
fn fr_overviews() -> &'static std::sync::Mutex<HashMap<i64, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<HashMap<i64, String>>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let p = dirs::cache_dir().unwrap_or_default().join("noosfin/anime-fr.json");
        std::sync::Mutex::new(std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    })
}

fn fr_overview(id: i64) -> Option<String> {
    fr_overviews().lock().ok()?.get(&id).cloned()
}

fn fr_overview_store(id: i64, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut m) = fr_overviews().lock() {
        if m.get(&id).map(String::as_str) == Some(text) {
            return;
        }
        m.insert(id, text.to_string());
        let p = dirs::cache_dir().unwrap_or_default().join("noosfin/anime-fr.json");
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, serde_json::to_string(&*m).unwrap_or_default());
    }
}
