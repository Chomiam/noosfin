//! Sons de navigation de l'interface, synthétisés au premier usage (aucun fichier sous licence)
//! dans ~/.cache/noosfin/sounds, puis joués par PipeWire (`pw-play`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sound {
    /// Déplacement de la sélection.
    Move,
    /// Validation.
    Select,
    /// Retour.
    Back,
}

const RATE: u32 = 48_000;

impl Sound {
    fn name(self) -> &'static str {
        match self {
            Sound::Move => "move",
            Sound::Select => "select",
            Sound::Back => "back",
        }
    }

    /// Notes successives : (fréquence de départ, fréquence d'arrivée, durée en ms).
    fn notes(self) -> &'static [(f32, f32, u32)] {
        match self {
            Sound::Move => &[(1760.0, 1600.0, 28)],
            Sound::Select => &[(880.0, 880.0, 45), (1320.0, 1320.0, 70)],
            Sound::Back => &[(990.0, 990.0, 40), (660.0, 620.0, 70)],
        }
    }
}

/// Signal sinusoïdal doux (attaque rapide, décroissance exponentielle), PCM 16 bits mono.
fn synth(s: Sound) -> Vec<u8> {
    let mut samples: Vec<i16> = vec![];
    for &(f0, f1, ms) in s.notes() {
        let n = (RATE * ms / 1000) as usize;
        let mut phase = 0f32;
        for i in 0..n {
            let t = i as f32 / n as f32;
            let f = f0 + (f1 - f0) * t;
            phase += std::f32::consts::TAU * f / RATE as f32;
            let attack = (i as f32 / (RATE as f32 * 0.003)).min(1.0);
            let env = attack * (-4.5 * t).exp();
            // Une pointe d'harmonique 2 pour un timbre moins « bip ».
            let v = (phase.sin() + 0.18 * (2.0 * phase).sin()) * env * 0.42;
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn path(s: Sound) -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(format!("noosfin/sounds/{}-v1.wav", s.name()))
}

/// Joue un son sans bloquer ; les déplacements trop rapprochés (répétition) sont espacés.
pub fn play(s: Sound) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    if s == Sound::Move {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_millis(70)) {
            return;
        }
        *last = Some(Instant::now());
    }
    let p = path(s);
    if !p.exists() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if std::fs::write(&p, synth(s)).is_err() {
            return;
        }
    }
    std::thread::spawn(move || {
        let _ = std::process::Command::new("pw-play")
            .arg("--volume=0.5")
            .arg(&p)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header() {
        let w = synth(Sound::Select);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize, w.len() - 44);
    }
}
