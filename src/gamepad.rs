//! Manettes (evdev) → touches clavier envoyées à l'interface.
//! A = Entrée, B = Échap, croix / stick gauche = flèches, LB/RB = onglets, Start = menu,
//! X / Y = actions secondaires, Select = informations (pendant la lecture).

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

const REPEAT_DELAY: Duration = Duration::from_millis(380);
const REPEAT_RATE: Duration = Duration::from_millis(110);

/// Démarre la surveillance des manettes. `paused` coupe l'envoi (pendant la lecture mpv).
pub fn spawn(rt: &tokio::runtime::Handle, paused: Arc<AtomicBool>, sink: impl Fn(Pad) + Send + Sync + 'static) {
    let sink = Arc::new(sink);
    let (tx, mut rx) = mpsc::unbounded_channel::<(Pad, bool)>();

    // Découverte (et branchement à chaud) des manettes.
    rt.spawn(async move {
        let mut known: HashSet<PathBuf> = HashSet::new();
        loop {
            for (path, dev) in evdev::enumerate() {
                if known.contains(&path) || !is_gamepad(&dev) {
                    continue;
                }
                log::info!("manette : {} ({})", dev.name().unwrap_or("?"), path.display());
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
        let mut held: Option<Pad> = None;
        let mut next = tokio::time::Instant::now();
        loop {
            let wait = if held.is_some() { next } else { tokio::time::Instant::now() + Duration::from_secs(3600) };
            tokio::select! {
                ev = rx.recv() => {
                    let Some((pad, down)) = ev else { break };
                    if down {
                        if !paused.load(Ordering::Relaxed) { sink(pad); }
                        if matches!(pad, Pad::Up | Pad::Down | Pad::Left | Pad::Right) {
                            held = Some(pad);
                            next = tokio::time::Instant::now() + REPEAT_DELAY;
                        }
                    } else if held == Some(pad) {
                        held = None;
                    }
                }
                _ = tokio::time::sleep_until(wait) => {
                    if let Some(p) = held {
                        if !paused.load(Ordering::Relaxed) { sink(p); }
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

async fn read_device(dev: Device, tx: mpsc::UnboundedSender<(Pad, bool)>) -> std::io::Result<()> {
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
        let _ = tx.send((p, d));
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
                let pad = match code {
                    KeyCode::BTN_SOUTH => Pad::Accept,
                    KeyCode::BTN_EAST => Pad::Back,
                    KeyCode::BTN_NORTH => Pad::Y,
                    KeyCode::BTN_WEST => Pad::X,
                    KeyCode::BTN_TL | KeyCode::BTN_TL2 => Pad::PrevTab,
                    KeyCode::BTN_TR | KeyCode::BTN_TR2 => Pad::NextTab,
                    KeyCode::BTN_START | KeyCode::BTN_MODE => Pad::Menu,
                    KeyCode::BTN_SELECT => Pad::Select,
                    KeyCode::BTN_DPAD_UP => Pad::Up,
                    KeyCode::BTN_DPAD_DOWN => Pad::Down,
                    KeyCode::BTN_DPAD_LEFT => Pad::Left,
                    KeyCode::BTN_DPAD_RIGHT => Pad::Right,
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
