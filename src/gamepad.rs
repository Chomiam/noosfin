//! Manettes (evdev) → actions de l'interface.
//! Valider / Retour, croix ou stick gauche = flèches, LB/RB = onglets, Start = menu,
//! bouton gauche / haut = actions secondaires (`Pad::X` / `Pad::Y`), Select = informations.
//!
//! Les pilotes ne codent pas les boutons de la même façon : `xpad` (Xbox, 8BitDo, Steam) envoie
//! BTN_X (= BTN_NORTH) pour le bouton X, situé à gauche ; `hid-playstation` et `hid-nintendo`
//! envoient des codes positionnels. On ramène tout à la position du bouton.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pad {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    PrevTab,
    NextTab,
    Menu,
    Select,
    X,
    Y,
}

/// Famille de manette : correspondance des boutons et symboles affichés.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Xbox,
    PlayStation,
    Nintendo,
}

impl Layout {
    pub fn name(self) -> &'static str {
        match self {
            Layout::Xbox => "xbox",
            Layout::PlayStation => "playstation",
            Layout::Nintendo => "nintendo",
        }
    }

    fn detect(dev: &Device) -> Layout {
        match dev.input_id().vendor() {
            0x054c => Layout::PlayStation,
            // Nintendo, y compris les 8BitDo en mode Switch (identifiant Nintendo).
            0x057e => Layout::Nintendo,
            _ => Layout::Xbox,
        }
    }
}

const REPEAT_DELAY: Duration = Duration::from_millis(380);
const REPEAT_RATE: Duration = Duration::from_millis(110);

/// Démarre la surveillance des manettes. `paused` coupe l'envoi (pendant la lecture mpv).
pub fn spawn(rt: &tokio::runtime::Handle, paused: Arc<AtomicBool>, sink: impl Fn(Pad, Layout) + Send + Sync + 'static) {
    let sink = Arc::new(sink);
    let (tx, mut rx) = mpsc::unbounded_channel::<(Pad, bool, Layout)>();

    // Découverte (et branchement à chaud) des manettes.
    rt.spawn(async move {
        let mut known: HashSet<PathBuf> = HashSet::new();
        loop {
            for (path, dev) in evdev::enumerate() {
                if known.contains(&path) || !is_gamepad(&dev) {
                    continue;
                }
                // Manette virtuelle de Steam (Steam Input) : doublerait chaque appui.
                if dev.input_id().vendor() == 0x28de && dev.input_id().product() == 0x11ff {
                    known.insert(path.clone());
                    continue;
                }
                log::info!("manette : {} ({}, {:?})", dev.name().unwrap_or("?"), path.display(), Layout::detect(&dev));
                known.insert(path.clone());
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = read_device(dev, tx).await {
                        log::info!("manette déconnectée {}: {e}", path.display());
                    }
                });
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            known.retain(|p| p.exists());
        }
    });

    // Répétition automatique tant qu'une direction est maintenue.
    rt.spawn(async move {
        let mut held: Option<(Pad, Layout)> = None;
        let mut next = tokio::time::Instant::now();
        loop {
            let wait = if held.is_some() { next } else { tokio::time::Instant::now() + Duration::from_secs(3600) };
            tokio::select! {
                ev = rx.recv() => {
                    let Some((pad, down, layout)) = ev else { break };
                    if down {
                        if !paused.load(Ordering::Relaxed) { sink(pad, layout); }
                        if matches!(pad, Pad::Up | Pad::Down | Pad::Left | Pad::Right) {
                            held = Some((pad, layout));
                            next = tokio::time::Instant::now() + REPEAT_DELAY;
                        }
                    } else if held.is_some_and(|(p, _)| p == pad) {
                        held = None;
                    }
                }
                _ = tokio::time::sleep_until(wait) => {
                    if let Some((p, layout)) = held {
                        if !paused.load(Ordering::Relaxed) { sink(p, layout); }
                        next = tokio::time::Instant::now() + REPEAT_RATE;
                    }
                }
            }
        }
    });
}

fn is_gamepad(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_SOUTH) && k.contains(KeyCode::BTN_EAST))
}

async fn read_device(dev: Device, tx: mpsc::UnboundedSender<(Pad, bool, Layout)>) -> std::io::Result<()> {
    let layout = Layout::detect(&dev);
    // Centre/plage des sticks pour la zone morte.
    let absinfo: Vec<_> = dev.get_absinfo()?.collect();
    let range = |axis: AbsoluteAxisCode| {
        absinfo.iter().find(|(a, _)| *a == axis).map(|(_, i)| (i.minimum(), i.maximum())).unwrap_or((-32768, 32767))
    };
    let (xmin, xmax) = range(AbsoluteAxisCode::ABS_X);
    let (ymin, ymax) = range(AbsoluteAxisCode::ABS_Y);

    let mut stream = dev.into_event_stream()?;
    let mut stick_x: Option<Pad> = None;
    let mut stick_y: Option<Pad> = None;
    let mut hat_x: Option<Pad> = None;
    let mut hat_y: Option<Pad> = None;

    let send = |p: Pad, d: bool| {
        let _ = tx.send((p, d, layout));
    };
    // Transition d'un axe à trois états (négatif / neutre / positif).
    let axis = |state: &mut Option<Pad>, new: Option<Pad>| {
        if *state != new {
            if let Some(old) = state.take() {
                send(old, false);
            }
            if let Some(n) = new {
                send(n, true);
            }
            *state = new;
        }
    };
    let tri = |v: i32, min: i32, max: i32, neg: Pad, pos: Pad| {
        let c = (min + max) / 2;
        let t = (max - min) / 4; // ~50 % de course
        if v < c - t {
            Some(neg)
        } else if v > c + t {
            Some(pos)
        } else {
            None
        }
    };

    loop {
        let ev = stream.next_event().await?;
        match ev.destructure() {
            EventSummary::Key(_, code, value) if value != 2 => {
                let down = value == 1;
                let pad = match (layout, code) {
                    // Switch : A (à droite) valide, B (en bas) annule, comme sur la console.
                    (Layout::Nintendo, KeyCode::BTN_EAST) => Pad::Accept,
                    (Layout::Nintendo, KeyCode::BTN_SOUTH) => Pad::Back,
                    (_, KeyCode::BTN_SOUTH) => Pad::Accept,
                    (_, KeyCode::BTN_EAST) => Pad::Back,
                    // xpad : BTN_NORTH (= BTN_X) est le bouton X, à gauche.
                    (Layout::Xbox, KeyCode::BTN_NORTH) => Pad::X,
                    (Layout::Xbox, KeyCode::BTN_WEST) => Pad::Y,
                    (_, KeyCode::BTN_NORTH) => Pad::Y,
                    (_, KeyCode::BTN_WEST) => Pad::X,
                    (_, KeyCode::BTN_TL | KeyCode::BTN_TL2) => Pad::PrevTab,
                    (_, KeyCode::BTN_TR | KeyCode::BTN_TR2) => Pad::NextTab,
                    (_, KeyCode::BTN_START | KeyCode::BTN_MODE) => Pad::Menu,
                    (_, KeyCode::BTN_SELECT) => Pad::Select,
                    (_, KeyCode::BTN_DPAD_UP) => Pad::Up,
                    (_, KeyCode::BTN_DPAD_DOWN) => Pad::Down,
                    (_, KeyCode::BTN_DPAD_LEFT) => Pad::Left,
                    (_, KeyCode::BTN_DPAD_RIGHT) => Pad::Right,
                    _ => continue,
                };
                send(pad, down);
            }
            EventSummary::AbsoluteAxis(_, code, v) => match code {
                AbsoluteAxisCode::ABS_HAT0X => axis(&mut hat_x, tri(v, -1, 1, Pad::Left, Pad::Right)),
                AbsoluteAxisCode::ABS_HAT0Y => axis(&mut hat_y, tri(v, -1, 1, Pad::Up, Pad::Down)),
                AbsoluteAxisCode::ABS_X => axis(&mut stick_x, tri(v, xmin, xmax, Pad::Left, Pad::Right)),
                AbsoluteAxisCode::ABS_Y => axis(&mut stick_y, tri(v, ymin, ymax, Pad::Up, Pad::Down)),
                _ => {}
            },
            _ => {}
        }
    }
}
