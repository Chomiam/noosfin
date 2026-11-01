//! Installateur de Noosfin (Bazzite uniquement).
//!
//!   noosfin-installer              interface terminal : vérifications, options, installation
//!   noosfin-installer --update     mise à jour silencieuse des fichiers (lancée par l'application)
//!   noosfin-installer --uninstall  retire Noosfin (la configuration est conservée)
//!
//! Les fichiers à installer sont à côté du binaire (archive de release) :
//! bin/noosfin, bin/noosfin-installer, session/noosfin-session, session/noosfin-mpv,
//! share/noosfin-osc.lua, share/noos-logo.svg.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, List, ListItem, Padding, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SESSION_FILE: &str = "/usr/local/share/wayland-sessions/noosfin.desktop";
// « zzs- » : après zz-holo-autologin (Bazzite Deck) mais avant zzt-holo-temp-login
// (sessions ponctuelles de steamos-manager), qui doivent pouvoir passer devant.
const SDDM_FILE: &str = "/etc/sddm.conf.d/zzs-noosfin-autologin.conf";

// Catppuccin Mocha
const BASE: Color = Color::Rgb(30, 30, 46);
const SURFACE: Color = Color::Rgb(69, 71, 90);
const OVERLAY: Color = Color::Rgb(108, 112, 134);
const TEXT: Color = Color::Rgb(205, 214, 244);
const SUBTEXT: Color = Color::Rgb(166, 173, 200);
const MAUVE: Color = Color::Rgb(203, 166, 247);
const BLUE: Color = Color::Rgb(137, 180, 250);
const GREEN: Color = Color::Rgb(166, 227, 161);
const RED: Color = Color::Rgb(243, 139, 168);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const PEACH: Color = Color::Rgb(250, 179, 135);

const LOGO: [&str; 5] = [
    "███╗   ██╗ ██████╗  ██████╗ ███████╗███████╗██╗███╗   ██╗",
    "████╗  ██║██╔═══██╗██╔═══██╗██╔════╝██╔════╝██║████╗  ██║",
    "██╔██╗ ██║██║   ██║██║   ██║███████╗█████╗  ██║██╔██╗ ██║",
    "██║╚██╗██║╚██████╔╝╚██████╔╝╚════██║██╔══╝  ██║██║╚██╗██║",
    "╚═╝  ╚═══╝ ╚═════╝  ╚═════╝ ╚══════╝╚═╝     ╚═╝╚═╝  ╚═══╝",
];

// ─────────────────────────────── Système ───────────────────────────────

#[derive(Debug, Clone, Default)]
struct Edition {
    image: String,
    base: String,
    version: String,
}

impl Edition {
    fn detect() -> Option<Edition> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("/usr/share/ublue-os/image-info.json").ok()?).ok()?;
        let image = v["image-name"].as_str()?.to_string();
        image.starts_with("bazzite").then(|| Edition {
            image,
            base: v["base-image-name"].as_str().unwrap_or_default().to_string(),
            version: v["version-pretty"].as_str().unwrap_or_default().to_string(),
        })
    }

    fn kde(&self) -> bool {
        self.base == "kinoite"
    }

    fn deck(&self) -> bool {
        self.image.starts_with("bazzite-deck")
    }

    fn nvidia(&self) -> bool {
        self.image.contains("nvidia")
    }

    fn gamescope(&self) -> bool {
        self.deck() && !self.nvidia()
    }
}

fn gpus() -> Vec<String> {
    let mut out = vec![];
    for e in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let vendor = std::fs::read_to_string(e.path().join("device/vendor")).unwrap_or_default();
        let label = match vendor.trim() {
            "0x1002" => "AMD",
            "0x10de" => "NVIDIA",
            "0x8086" => "Intel",
            _ => continue,
        };
        if !out.contains(&label.to_string()) {
            out.push(label.to_string());
        }
    }
    out
}

fn has(cmd: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {cmd}")).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn flatpak_installed(app: &str) -> bool {
    Command::new("flatpak").args(["info", app]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Dossier de l'archive (parent de bin/).
fn payload() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let root = exe.parent().and_then(Path::parent).context("emplacement de l'installateur")?.to_path_buf();
    if !root.join("bin/noosfin").exists() {
        bail!("fichiers de Noosfin introuvables à côté de l'installateur ({})", root.display());
    }
    Ok(root)
}

/// Copie atomique (fichier temporaire puis renommage) : sûr même si le binaire tourne.
fn put(src: &Path, dst: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let tmp = dst.with_extension("noosfin-new");
    std::fs::copy(src, &tmp).with_context(|| format!("copie de {}", src.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn install_user_files(root: &Path) -> Result<()> {
    let h = home();
    let bin = h.join(".local/bin");
    put(&root.join("bin/noosfin"), &bin.join("noosfin"), 0o755)?;
    put(&root.join("bin/noosfin-installer"), &bin.join("noosfin-installer"), 0o755)?;
    put(&root.join("session/noosfin-session"), &bin.join("noosfin-session"), 0o755)?;
    put(&root.join("session/noosfin-mpv"), &bin.join("noosfin-mpv"), 0o755)?;
    put(&root.join("share/noosfin-osc.lua"), &h.join(".local/share/noosfin/mpv/noosfin-osc.lua"), 0o644)?;
    put(&root.join("share/noos-logo.svg"), &h.join(".local/share/icons/hicolor/scalable/apps/noosfin.svg"), 0o644)?;
    // Filtres d'image du lecteur (menu Start dans mpv).
    for e in std::fs::read_dir(root.join("share/shaders")).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "glsl") {
            put(&p, &h.join(".local/share/noosfin/shaders").join(e.file_name()), 0o644)?;
        }
    }
    // mpv (Flatpak) doit lire l'overlay et les shaders, et mémoriser le filtre choisi.
    if has("flatpak") {
        let _ = Command::new("flatpak").args(["override", "--user", "--filesystem=xdg-data/noosfin", "io.mpv.Mpv"]).status();
    }
    Ok(())
}

fn install_desktop_entry() -> Result<()> {
    let h = home();
    let p = h.join(".local/share/applications/noosfin.desktop");
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        format!(
            "[Desktop Entry]\nType=Application\nName=Noosfin\nComment=Médiathèque, animes et IPTV pour la TV\nExec={}/.local/bin/noosfin --media\nIcon=noosfin\nCategories=AudioVideo;Video;Player;\n",
            h.display()
        ),
    )?;
    Ok(())
}

fn sudo_write(path: &str, content: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .args(["-n", "install", "-Dm644", "/dev/stdin", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(content.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("sudo : {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn install_session() -> Result<()> {
    let h = home();
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).context("utilisateur inconnu")?;
    sudo_write(
        SESSION_FILE,
        &format!(
            "[Desktop Entry]\nName=Noosfin HTPC\nComment=Tableau de bord HTPC (KWin HDR)\nExec={0}/.local/bin/noosfin-session\nTryExec={0}/.local/bin/noosfin-session\nType=Application\nDesktopNames=KDE\n",
            h.display()
        ),
    )?;
    sudo_write(
        SDDM_FILE,
        &format!("[Autologin]\nUser={user}\nSession=noosfin.desktop\nRelogin=true\n\n[Wayland]\nSessionDir=/usr/local/share/wayland-sessions,/usr/share/wayland-sessions\n"),
    )
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.lines().last().unwrap_or("échec").trim());
    }
    Ok(())
}

fn install_flatpak(app: &str) -> Result<()> {
    if flatpak_installed(app) {
        return Ok(());
    }
    run(Command::new("flatpak").args(["remote-add", "--user", "--if-not-exists", "flathub", "https://dl.flathub.org/repo/flathub.flatpakrepo"]))?;
    run(Command::new("flatpak").args(["install", "--user", "-y", "--noninteractive", "flathub", app]))
}

// ─────────────────────────────── Modes sans interface ───────────────────────────────

fn update_mode() -> Result<()> {
    let root = payload()?;
    install_user_files(&root)?;
    println!("Noosfin {VERSION} installé.");
    Ok(())
}

fn uninstall_mode() -> Result<()> {
    print!("Retirer Noosfin de ce PC ? (la configuration ~/.config/noosfin est conservée) [o/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("o") {
        println!("Annulé.");
        return Ok(());
    }
    if Path::new(SESSION_FILE).exists() || Path::new(SDDM_FILE).exists() {
        println!("Suppression de la session SDDM (sudo)…");
        run(Command::new("sudo").args(["rm", "-f", SESSION_FILE, SDDM_FILE]).stdin(Stdio::inherit()))?;
    }
    let h = home();
    for f in [
        ".local/bin/noosfin",
        ".local/bin/noosfin-session",
        ".local/bin/noosfin-mpv",
        ".local/share/noosfin/mpv/noosfin-osc.lua",
        ".local/share/noosfin/mpv/image-preset",
        ".local/share/applications/noosfin.desktop",
        ".local/share/icons/hicolor/scalable/apps/noosfin.svg",
        ".local/bin/noosfin-installer",
    ] {
        let _ = std::fs::remove_file(h.join(f));
    }
    let _ = std::fs::remove_dir_all(h.join(".local/share/noosfin/shaders"));
    let _ = Command::new("podman").args(["rm", "-f", "noosfin-flaresolverr"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    println!("Noosfin a été retiré. Bazzite reprendra son démarrage habituel.");
    Ok(())
}

// ─────────────────────────────── Interface ───────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Checks,
    Options,
    Installing,
    Done,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pending,
    Running,
    Ok,
    Failed,
}

struct Check {
    label: String,
    detail: String,
    ok: Option<bool>,
}

struct Opt {
    label: &'static str,
    detail: &'static str,
    on: bool,
}

struct Step {
    label: String,
    state: State,
    note: String,
}

enum Msg {
    Step(usize, State, String),
    Finished,
}

struct App {
    stage: Stage,
    edition: Option<Edition>,
    checks: Vec<Check>,
    blocking: Option<String>,
    opts: Vec<Opt>,
    sel: usize,
    steps: Vec<Step>,
    rx: Option<mpsc::Receiver<Msg>>,
    quit: bool,
    reboot: bool,
}

const OPT_SESSION: usize = 0;
const OPT_MPV: usize = 1;
const OPT_MENU: usize = 2;
const OPT_YOUTUBE: usize = 3;

impl App {
    fn new() -> App {
        let edition = Edition::detect();
        let mut checks = vec![];
        let mut blocking = None;
        match &edition {
            Some(e) => {
                checks.push(Check { label: "Bazzite".into(), detail: format!("{} · {}", e.image, e.version), ok: Some(true) });
                checks.push(Check {
                    label: "Bureau KDE Plasma".into(),
                    detail: if e.kde() { "KWin gère le HDR de la session".into() } else { "édition GNOME : non prise en charge".into() },
                    ok: Some(e.kde()),
                });
                if !e.kde() {
                    blocking = Some("Noosfin a besoin de KWin : installez une édition KDE de Bazzite.".into());
                }
                checks.push(Check {
                    label: "Session Steam Gamescope".into(),
                    detail: if e.gamescope() {
                        "disponible : tuile Steam sur le tableau de bord".into()
                    } else if e.nvidia() {
                        "édition NVIDIA : pas de tuile Steam".into()
                    } else {
                        "édition non Deck : pas de tuile Steam".into()
                    },
                    ok: if e.gamescope() { Some(true) } else { None },
                });
            }
            None => {
                checks.push(Check { label: "Bazzite".into(), detail: "système non reconnu".into(), ok: Some(false) });
                blocking = Some("Noosfin ne fonctionne que sur Bazzite (https://bazzite.gg).".into());
            }
        }
        let gpu = gpus();
        checks.push(Check { label: "Carte graphique".into(), detail: if gpu.is_empty() { "inconnue".into() } else { gpu.join(" + ") }, ok: Some(!gpu.is_empty()) });
        checks.push(Check {
            label: "Flatpak".into(),
            detail: if has("flatpak") { "présent (lecteur mpv)".into() } else { "absent".into() },
            ok: Some(has("flatpak")),
        });
        checks.push(Check {
            label: "Podman".into(),
            detail: if has("podman") { "présent (FlareSolverr local)".into() } else { "absent : sites Cloudflare indisponibles".into() },
            ok: if has("podman") { Some(true) } else { None },
        });
        if payload().is_err() && blocking.is_none() {
            blocking = Some("Fichiers de Noosfin introuvables : lancez l'installation depuis l'archive de release.".into());
        }
        let mpv = flatpak_installed("io.mpv.Mpv");
        App {
            stage: Stage::Checks,
            edition,
            checks,
            blocking,
            opts: vec![
                Opt { label: "Démarrer sur Noosfin", detail: "session par défaut au démarrage du PC (SDDM, mot de passe administrateur)", on: true },
                Opt { label: "Lecteur mpv", detail: if mpv { "io.mpv.Mpv déjà installé" } else { "Flatpak io.mpv.Mpv (HDR, gpu-next)" }, on: !mpv },
                Opt { label: "Raccourci dans le menu", detail: "lancer la médiathèque depuis le bureau KDE", on: true },
                Opt { label: "YouTube (VacuumTube)", detail: "Flatpak rocks.shy.VacuumTube, interface YouTube TV", on: false },
            ],
            sel: 0,
            steps: vec![],
            rx: None,
            quit: false,
            reboot: false,
        }
    }

    fn start(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // Le mot de passe sudo se tape dans le terminal normal, avant l'installation.
        if self.opts[OPT_SESSION].on {
            ratatui::restore();
            println!("\n  Noosfin · le démarrage automatique demande le mot de passe administrateur.\n");
            let ok = Command::new("sudo").arg("-v").status().is_ok_and(|s| s.success());
            *terminal = ratatui::init();
            if !ok {
                self.opts[OPT_SESSION].on = false;
            }
        }
        let mut plan: Vec<(String, usize)> = vec![("Fichiers de Noosfin (~/.local)".into(), 99)];
        if self.opts[OPT_MPV].on {
            plan.push(("Lecteur mpv (Flatpak)".into(), OPT_MPV));
        }
        if self.opts[OPT_YOUTUBE].on {
            plan.push(("VacuumTube (Flatpak)".into(), OPT_YOUTUBE));
        }
        if self.opts[OPT_MENU].on {
            plan.push(("Raccourci dans le menu".into(), OPT_MENU));
        }
        if self.opts[OPT_SESSION].on {
            plan.push(("Session de démarrage (SDDM)".into(), OPT_SESSION));
        }
        self.steps = plan.iter().map(|(l, _)| Step { label: l.clone(), state: State::Pending, note: String::new() }).collect();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            for (i, (_, what)) in plan.iter().enumerate() {
                let _ = tx.send(Msg::Step(i, State::Running, String::new()));
                let r = match *what {
                    OPT_MPV => install_flatpak("io.mpv.Mpv"),
                    OPT_YOUTUBE => install_flatpak("rocks.shy.VacuumTube"),
                    OPT_MENU => install_desktop_entry(),
                    OPT_SESSION => install_session(),
                    _ => payload().and_then(|root| install_user_files(&root)),
                };
                let _ = tx.send(match r {
                    Ok(()) => Msg::Step(i, State::Ok, String::new()),
                    Err(e) => Msg::Step(i, State::Failed, format!("{e:#}")),
                });
            }
            let _ = tx.send(Msg::Finished);
        });
        self.stage = Stage::Installing;
        Ok(())
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Step(i, st, note) => {
                    if let Some(s) = self.steps.get_mut(i) {
                        s.state = st;
                        s.note = note;
                    }
                }
                Msg::Finished => self.stage = Stage::Done,
            }
        }
    }

    fn key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        match (self.stage, code) {
            (_, KeyCode::Char('q')) | (_, KeyCode::Esc) if self.stage != Stage::Installing => self.quit = true,
            (Stage::Checks, KeyCode::Enter) if self.blocking.is_none() => self.stage = Stage::Options,
            (Stage::Options, KeyCode::Up | KeyCode::Char('k')) => self.sel = self.sel.saturating_sub(1),
            (Stage::Options, KeyCode::Down | KeyCode::Char('j')) => self.sel = (self.sel + 1).min(self.opts.len() - 1),
            (Stage::Options, KeyCode::Char(' ')) => self.opts[self.sel].on = !self.opts[self.sel].on,
            (Stage::Options, KeyCode::Enter) => self.start(terminal)?,
            (Stage::Done, KeyCode::Char('r')) if self.opts[OPT_SESSION].on => {
                self.reboot = true;
                self.quit = true;
            }
            (Stage::Done, KeyCode::Enter) => self.quit = true,
            _ => {}
        }
        Ok(())
    }
}

fn card(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(SURFACE))
        .title(Line::from(format!(" {title} ")).fg(MAUVE).bold())
        .padding(Padding::new(2, 2, 1, 1))
        .bg(BASE)
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(Block::new().bg(Color::Rgb(17, 17, 27)), area);
    let width = area.width.min(96);
    let inner = Rect { x: area.x + (area.width - width) / 2, y: area.y, width, height: area.height };
    let [logo, sub, body, help] = Layout::vertical([Constraint::Length(7), Constraint::Length(2), Constraint::Min(8), Constraint::Length(2)]).areas(inner);

    let colors = [BLUE, BLUE, MAUVE, MAUVE, RED];
    let lines: Vec<Line> = LOGO.iter().zip(colors).map(|(l, c)| Line::from(*l).fg(c)).collect();
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center).block(Block::new().padding(Padding::top(1))), logo);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("Tableau de bord HTPC pour Bazzite  ·  ").fg(SUBTEXT),
            Span::raw(format!("v{VERSION}")).fg(PEACH),
        ]))
        .alignment(Alignment::Center),
        sub,
    );

    let hint = match app.stage {
        Stage::Checks if app.blocking.is_some() => "q  quitter",
        Stage::Checks => "Entrée  continuer    q  quitter",
        Stage::Options => "↑ ↓  choisir    Espace  cocher    Entrée  installer    q  quitter",
        Stage::Installing => "Installation en cours…",
        Stage::Done if app.opts[OPT_SESSION].on => "r  redémarrer maintenant    Entrée  quitter",
        Stage::Done => "Entrée  quitter",
    };
    f.render_widget(Paragraph::new(hint).fg(OVERLAY).alignment(Alignment::Center), help);

    match app.stage {
        Stage::Checks => {
            let items: Vec<ListItem> = app
                .checks
                .iter()
                .map(|c| {
                    let (mark, color) = match c.ok {
                        Some(true) => ("✔", GREEN),
                        Some(false) => ("✘", RED),
                        None => ("•", YELLOW),
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(format!(" {mark}  ")).fg(color).bold(),
                        Span::raw(format!("{:<26}", c.label)).fg(TEXT).bold(),
                        Span::raw(c.detail.clone()).fg(SUBTEXT),
                    ]))
                })
                .collect();
            let [l, msg] = Layout::vertical([Constraint::Length(app.checks.len() as u16 + 4), Constraint::Min(3)]).areas(body);
            f.render_widget(List::new(items).block(card("Vérification du système")), l);
            let text = match &app.blocking {
                Some(b) => Line::from(format!("✘  {b}")).fg(RED).bold(),
                None => Line::from("Tout est prêt. Appuyez sur Entrée pour choisir les options.").fg(GREEN),
            };
            f.render_widget(Paragraph::new(text).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(Block::new().padding(Padding::top(1))), msg);
        }
        Stage::Options => {
            let items: Vec<ListItem> = app
                .opts
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let focused = i == app.sel;
                    let check = if o.on { "[✔]" } else { "[ ]" };
                    let style = if focused { Style::new().fg(BASE).bg(MAUVE).add_modifier(Modifier::BOLD) } else { Style::new().fg(TEXT) };
                    ListItem::new(vec![
                        Line::from(vec![Span::styled(format!(" {check} {} ", o.label), style)]),
                        Line::from(format!("      {}", o.detail)).fg(if focused { SUBTEXT } else { OVERLAY }),
                        Line::from(""),
                    ])
                })
                .collect();
            let mut title = String::from("Options");
            if let Some(e) = &app.edition {
                if !e.gamescope() {
                    title.push_str(" · sans tuile Steam sur cette édition");
                }
            }
            let [l, _] = Layout::vertical([Constraint::Length(app.opts.len() as u16 * 3 + 3), Constraint::Min(0)]).areas(body);
            f.render_widget(List::new(items).block(card(&title)), l);
        }
        Stage::Installing | Stage::Done => {
            let done = app.steps.iter().filter(|s| matches!(s.state, State::Ok | State::Failed)).count();
            let [g, l, msg] = Layout::vertical([Constraint::Length(3), Constraint::Length(app.steps.len() as u16 * 2 + 4), Constraint::Min(2)]).areas(body);
            let ratio = if app.steps.is_empty() { 0.0 } else { done as f64 / app.steps.len() as f64 };
            f.render_widget(
                Gauge::default()
                    .ratio(ratio)
                    .label(format!("{done} / {}", app.steps.len()))
                    .gauge_style(Style::new().fg(MAUVE).bg(SURFACE))
                    .block(Block::new().padding(Padding::new(2, 2, 1, 1))),
                g,
            );
            let items: Vec<ListItem> = app
                .steps
                .iter()
                .map(|s| {
                    let (mark, color) = match s.state {
                        State::Pending => ("·", OVERLAY),
                        State::Running => ("➜", BLUE),
                        State::Ok => ("✔", GREEN),
                        State::Failed => ("✘", RED),
                    };
                    let mut lines = vec![Line::from(vec![Span::raw(format!(" {mark}  ")).fg(color).bold(), Span::raw(s.label.clone()).fg(TEXT)])];
                    lines.push(Line::from(format!("     {}", s.note)).fg(RED));
                    ListItem::new(lines)
                })
                .collect();
            f.render_widget(List::new(items).block(card(if app.stage == Stage::Done { "Terminé" } else { "Installation" })), l);
            if app.stage == Stage::Done {
                let failed = app.steps.iter().any(|s| s.state == State::Failed);
                let text = if failed {
                    Line::from("Certaines étapes ont échoué : corrigez puis relancez l'installation.").fg(YELLOW)
                } else if app.opts[OPT_SESSION].on {
                    Line::from("Noosfin démarrera au prochain démarrage du PC. Mises à jour : Paramètres → Mise à jour.").fg(GREEN)
                } else {
                    Line::from("Noosfin est installé : lancez « Noosfin » depuis le menu des applications.").fg(GREEN)
                };
                f.render_widget(Paragraph::new(text).alignment(Alignment::Center).wrap(Wrap { trim: true }), msg);
            }
        }
    }
}

fn tui() -> Result<()> {
    let mut app = App::new();
    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        while !app.quit {
            app.poll();
            terminal.draw(|f| draw(f, &app))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        app.key(k.code, &mut terminal)?;
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res?;
    if app.reboot {
        Command::new("systemctl").arg("reboot").status()?;
    }
    Ok(())
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let r = match arg.as_str() {
        "--update" => update_mode(),
        "--uninstall" => uninstall_mode(),
        // Aperçu texte de chaque écran (développement).
        "--snapshot" => {
            let mut app = App::new();
            app.blocking = None;
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 34)).unwrap();
            for stage in [Stage::Checks, Stage::Options, Stage::Done] {
                app.stage = stage;
                if stage == Stage::Done {
                    app.steps = ["Fichiers de Noosfin (~/.local)", "Lecteur mpv (Flatpak)", "Session de démarrage (SDDM)"]
                        .iter()
                        .map(|l| Step { label: l.to_string(), state: State::Ok, note: String::new() })
                        .collect();
                }
                t.draw(|f| draw(f, &app)).unwrap();
                let buf = t.backend().buffer().clone();
                for y in 0..buf.area.height {
                    let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
                    println!("{}", line.trim_end());
                }
            }
            Ok(())
        }
        "--version" => {
            println!("noosfin-installer {VERSION}");
            Ok(())
        }
        _ => tui(),
    };
    if let Err(e) = r {
        eprintln!("Erreur : {e:#}");
        std::process::exit(1);
    }
}
