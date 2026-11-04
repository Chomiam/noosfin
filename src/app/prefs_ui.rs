//! Paramètres Audio et Sous-titres : langues par ordre de priorité (animes, films / séries),
//! sous-titres automatiques, taille et position. Même présentation que « Sources & débrideur ».

use super::*;
use crate::ScRow;
use crate::config::{LANGUAGES, PlayerConfig, language_label};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum PrefsPage {
    Audio,
    Subtitles,
}

/// Liste de langues éditée : audio ou sous-titres, animes ou films / séries.
#[derive(Clone, Copy, PartialEq)]
enum List {
    AudioAnime,
    AudioVideo,
    SubsAnime,
    SubsVideo,
}

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Output,
    UiSounds,
    Lang(List, &'static str),
    OffWithDub(bool),
    Size,
    Position,
}

const SIZES: [(f64, &str); 5] = [(0.8, "Petite"), (0.9, "Réduite"), (1.0, "Normale"), (1.2, "Grande"), (1.45, "Très grande")];

fn list_mut(p: &mut PlayerConfig, l: List) -> &mut Vec<String> {
    match l {
        List::AudioAnime => &mut p.audio_anime,
        List::AudioVideo => &mut p.audio_video,
        List::SubsAnime => &mut p.subs_anime,
        List::SubsVideo => &mut p.subs_video,
    }
}

fn list(p: &PlayerConfig, l: List) -> &Vec<String> {
    match l {
        List::AudioAnime => &p.audio_anime,
        List::AudioVideo => &p.audio_video,
        List::SubsAnime => &p.subs_anime,
        List::SubsVideo => &p.subs_video,
    }
}

impl Controller {
    pub(super) fn open_prefs(&mut self, page: PrefsPage) {
        self.prefs_page = page;
        self.sc_focus = 1;
        self.sc_msg = match page {
            PrefsPage::Audio => "Ⓐ active ou désactive une langue, ← → change son rang.\nLa première langue disponible dans le fichier est choisie.".into(),
            PrefsPage::Subtitles => "Ⓐ active ou désactive une langue, ← → change son rang.\nSans langue cochée : pas de sous-titres.".into(),
        };
        self.ui().set_sc_has_qr(false);
        self.go(SCR_PREFS);
    }

    /// Langues d'une liste : d'abord celles retenues (dans l'ordre), puis les autres.
    fn lang_rows(&self, l: List) -> Vec<Row> {
        let p = &self.svc.cfg.player;
        let chosen = list(p, l);
        let mut v: Vec<Row> = chosen.iter().filter_map(|c| LANGUAGES.iter().find(|x| x.0 == c).map(|x| Row::Lang(l, x.0))).collect();
        v.extend(LANGUAGES.iter().filter(|x| !chosen.iter().any(|c| c == x.0)).map(|x| Row::Lang(l, x.0)));
        v
    }

    fn prefs_rows(&self) -> Vec<Row> {
        let mut v = vec![];
        match self.prefs_page {
            PrefsPage::Audio => {
                v.extend([Row::Header("Sortie audio"), Row::Output, Row::UiSounds]);
                v.push(Row::Header("Animes · langue audio"));
                v.extend(self.lang_rows(List::AudioAnime));
                v.push(Row::Header("Films et séries · langue audio"));
                v.extend(self.lang_rows(List::AudioVideo));
            }
            PrefsPage::Subtitles => {
                v.push(Row::Header("Animes · sous-titres"));
                v.push(Row::OffWithDub(true));
                v.extend(self.lang_rows(List::SubsAnime));
                v.push(Row::Header("Films et séries · sous-titres"));
                v.push(Row::OffWithDub(false));
                v.extend(self.lang_rows(List::SubsVideo));
                v.extend([Row::Header("Affichage"), Row::Size, Row::Position]);
            }
        }
        v
    }

    fn prefs_row_data(&self, r: &Row) -> ScRow {
        let p = &self.svc.cfg.player;
        let row = |label: String, value: String, state: i32| ScRow { header: false, label: label.into(), value: value.into(), state };
        match r {
            Row::Header(t) => ScRow { header: true, label: (*t).into(), value: "".into(), state: -1 },
            Row::Output => row(
                "Mode".into(),
                if p.audio == "bitperfect" { "Bit-perfect · bitstream HDMI".into() } else { "Standard · mixage PipeWire".into() },
                -1,
            ),
            Row::UiSounds => row("Sons de navigation de l'interface".into(), String::new(), p.ui_sounds as i32),
            Row::Lang(l, code) => {
                let rank = list(p, *l).iter().position(|c| c == code);
                let label = match rank {
                    Some(i) => format!("{} · {}", i + 1, language_label(code)),
                    None => format!("    {}", language_label(code)),
                };
                row(label, if rank.is_some() { "‹ ›".into() } else { String::new() }, rank.is_some() as i32)
            }
            Row::OffWithDub(anime) => {
                let on = if *anime { p.subs_off_with_dub_anime } else { p.subs_off_with_dub_video };
                row("Pas de sous-titres si l'audio est déjà en français (VF)".into(), String::new(), on as i32)
            }
            Row::Size => {
                let label = SIZES.iter().min_by(|a, b| (a.0 - p.sub_scale).abs().total_cmp(&(b.0 - p.sub_scale).abs())).map_or("Normale", |s| s.1);
                row("Taille du texte".into(), format!("‹ {label} ›"), -1)
            }
            Row::Position => row("Position".into(), if p.sub_top { "‹ En haut ›".into() } else { "‹ En bas ›".into() }, -1),
        }
    }

    pub(super) fn prefs_sync(&mut self) {
        let rows: Vec<ScRow> = self.prefs_rows().iter().map(|r| self.prefs_row_data(r)).collect();
        let ui = self.ui();
        self.sc_model.set_vec(rows);
        ui.set_sc_focus(self.sc_focus);
        ui.set_sc_msg(self.glyphs(&self.sc_msg).into());
        ui.set_sc_title(if self.prefs_page == PrefsPage::Audio { "Audio" } else { "Sous-titres" }.into());
    }

    pub(super) fn nav_prefs(&mut self, n: Nav) {
        let rows = self.prefs_rows();
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
        let Some(r) = rows.get(self.sc_focus as usize).cloned() else { return };
        match n {
            Nav::Back => return self.back(),
            Nav::Up => self.sc_focus = step(self.sc_focus, -1),
            Nav::Down => self.sc_focus = step(self.sc_focus, 1),
            Nav::PrevTab => self.sc_focus = (0..6).fold(self.sc_focus, |f, _| step(f, -1)),
            Nav::NextTab => self.sc_focus = (0..6).fold(self.sc_focus, |f, _| step(f, 1)),
            Nav::Accept | Nav::Left | Nav::Right => self.prefs_activate(r, n),
            _ => {}
        }
    }

    fn prefs_activate(&mut self, r: Row, n: Nav) {
        let d: i32 = if n == Nav::Left { -1 } else { 1 };
        match r {
            Row::Header(_) => {}
            Row::UiSounds => self.update_config(|c| c.player.ui_sounds = !c.player.ui_sounds),
            Row::Output => self.update_config(|c| c.player.audio = if c.player.audio == "bitperfect" { "standard".into() } else { "bitperfect".into() }),
            Row::Lang(l, code) => {
                let mut focus_code = None;
                self.update_config(|c| {
                    let v = list_mut(&mut c.player, l);
                    match (n, v.iter().position(|x| x == code)) {
                        // Ⓐ : retenir ou retirer la langue (retenue : ajoutée en dernière position).
                        (Nav::Accept, Some(i)) => {
                            v.remove(i);
                        }
                        (Nav::Accept, None) => v.push(code.to_string()),
                        // ← → : monter ou descendre d'un rang.
                        (_, Some(i)) => {
                            let j = i as i32 + d;
                            if j >= 0 && (j as usize) < v.len() {
                                v.swap(i, j as usize);
                                focus_code = Some(code);
                            }
                        }
                        _ => {}
                    }
                });
                // La sélection suit la langue déplacée.
                if let Some(code) = focus_code.or(Some(code)) {
                    if let Some(i) = self.prefs_rows().iter().position(|x| matches!(x, Row::Lang(ll, c) if *ll == l && *c == code)) {
                        self.sc_focus = i as i32;
                    }
                }
            }
            Row::OffWithDub(anime) => self.update_config(|c| {
                let f = if anime { &mut c.player.subs_off_with_dub_anime } else { &mut c.player.subs_off_with_dub_video };
                *f = !*f;
            }),
            Row::Size => self.update_config(|c| {
                let i = SIZES.iter().enumerate().min_by(|a, b| (a.1.0 - c.player.sub_scale).abs().total_cmp(&(b.1.0 - c.player.sub_scale).abs())).map_or(2, |x| x.0);
                let j = (i as i32 + d).clamp(0, SIZES.len() as i32 - 1) as usize;
                c.player.sub_scale = SIZES[j].0;
            }),
            Row::Position => self.update_config(|c| c.player.sub_top = !c.player.sub_top),
        }
    }
}
