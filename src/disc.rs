//! Lecteur optique (interne ou USB) : détection du lecteur et du disque inséré via udev.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Empty,
    Dvd,
    Bluray,
    AudioCd,
    /// Disque de données (gravé) : pas de lecture directe.
    Data,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Disc {
    pub device: String,
    pub model: String,
    pub kind: Kind,
    pub label: String,
}

impl Disc {
    /// Titre lisible à partir du nom de volume (« MARVEL_STUDIOS_THUNDERBOLTS »).
    pub fn title(&self) -> String {
        let words: Vec<String> = self
            .label
            .split(['_', ' '])
            .filter(|w| !w.is_empty())
            .map(|w| {
                let lower = w.to_lowercase();
                let mut c = lower.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            })
            .collect();
        if words.is_empty() {
            match self.kind {
                Kind::Bluray => "Blu-ray".into(),
                Kind::Dvd => "DVD".into(),
                Kind::AudioCd => "CD audio".into(),
                _ => "Disque".into(),
            }
        } else {
            words.join(" ")
        }
    }

    /// Sous-titre de la tuile du tableau de bord.
    pub fn subtitle(&self) -> String {
        match self.kind {
            Kind::Empty => "Insérez un DVD ou un Blu-ray".into(),
            Kind::Dvd => format!("DVD · {}", self.title()),
            Kind::Bluray => format!("Blu-ray · {}", self.title()),
            Kind::AudioCd => "CD audio".into(),
            Kind::Data => "Disque de données".into(),
        }
    }

    /// URL et options mpv pour lire le disque.
    pub fn mpv(&self) -> Option<(String, Vec<String>)> {
        let dev = &self.device;
        match self.kind {
            Kind::Bluray => Some(("bd://".into(), vec![format!("--bluray-device={dev}")])),
            Kind::Dvd => Some(("dvd://".into(), vec![format!("--dvd-device={dev}")])),
            Kind::AudioCd => Some(("cdda://".into(), vec![format!("--cdrom-device={dev}")])),
            _ => None,
        }
    }
}

fn props(device: &str) -> Vec<(String, String)> {
    let out = std::process::Command::new("udevadm").args(["info", "--query=property", "--name", device]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Premier lecteur optique présent, avec l'état de son disque.
pub fn detect() -> Option<Disc> {
    // Pour les tests : NOOSFIN_DISC=bluray:TITRE simule un lecteur.
    if let Ok(sim) = std::env::var("NOOSFIN_DISC") {
        let (k, label) = sim.split_once(':').unwrap_or((&sim, ""));
        let kind = match k {
            "bluray" => Kind::Bluray,
            "dvd" => Kind::Dvd,
            "cd" => Kind::AudioCd,
            _ => Kind::Empty,
        };
        return Some(Disc { device: "/dev/sr0".into(), model: "simulé".into(), kind, label: label.into() });
    }
    let mut names: Vec<String> = std::fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("sr"))
        .collect();
    names.sort();
    let name = names.into_iter().next()?;
    let device = format!("/dev/{name}");
    let p = props(&device);
    let get = |k: &str| p.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let has = |k: &str| get(k).as_deref() == Some("1");
    let kind = if !has("ID_CDROM_MEDIA") {
        Kind::Empty
    } else if p.iter().any(|(k, v)| k.starts_with("ID_CDROM_MEDIA_BD") && v == "1") {
        Kind::Bluray
    } else if p.iter().any(|(k, v)| k.starts_with("ID_CDROM_MEDIA_DVD") && v == "1") {
        Kind::Dvd
    } else if get("ID_CDROM_MEDIA_TRACK_COUNT_AUDIO").is_some_and(|n| n != "0") {
        Kind::AudioCd
    } else {
        Kind::Data
    };
    Some(Disc {
        device,
        model: [get("ID_VENDOR"), get("ID_MODEL")].into_iter().flatten().collect::<Vec<_>>().join(" ").replace('_', " "),
        kind,
        label: get("ID_FS_LABEL").unwrap_or_default(),
    })
}

/// Blu-ray du commerce : chiffré AACS. libaacs a besoin d'un fichier de clés que Noosfin ne fournit pas.
pub fn aacs_hint() -> &'static str {
    "Blu-ray protégé (AACS) : mpv ne peut pas le déchiffrer sans configuration libaacs. Les Blu-ray non protégés et les DVD sont lisibles."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        let d = Disc { device: "/dev/sr0".into(), model: String::new(), kind: Kind::Bluray, label: "MARVEL_STUDIOS_THUNDERBOLTS".into() };
        assert_eq!(d.title(), "Marvel Studios Thunderbolts");
        assert_eq!(d.subtitle(), "Blu-ray · Marvel Studios Thunderbolts");
    }
}
