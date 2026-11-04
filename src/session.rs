//! Bascule de session (Plasma / Steam Gamescope), alimentation et HDR (kscreen-doctor).
//!
//! Bazzite (comme SteamOS) gère les sessions SDDM avec steamos-manager :
//!   - `zz-holo-autologin.conf`    : session par défaut (remplacée par `zzs-noosfin-autologin.conf`)
//!   - `zzt-holo-temp-login.conf`  : session ponctuelle, effacée dès que la suivante atteint
//!                                   `graphical-session.target` (steamos-manager-session-cleanup)
//! Pour aller vers Plasma ou Steam, Noosfin pose une session ponctuelle via D-Bus puis ferme
//! sa propre session : SDDM se reconnecte (Relogin=true) sur la session demandée, et à la
//! déconnexion de celle-ci on revient à Noosfin, la session par défaut.
//! Si steamos-manager ne répond pas, `noosfin-session` lance la session cible lui-même.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Plasma,
    Steam,
    /// Relance KWin + le tableau de bord.
    Restart,
}

pub fn in_noosfin_session() -> bool {
    std::env::var_os("NOOSFIN_SESSION").is_some()
}

fn next_file() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("noosfin-next")
}

/// Pose la session SDDM ponctuelle via steamos-manager (bus système, sans sudo).
pub fn set_temporary_session(desktop: &str) -> Result<()> {
    let st = Command::new("busctl")
        .args([
            "call",
            "com.steampowered.SteamOSManager1",
            "/com/steampowered/SteamOSManager1",
            "com.steampowered.SteamOSManager1.RootManager",
            "SetTemporarySession",
            "s",
            desktop,
        ])
        .status()?;
    if !st.success() {
        bail!("steamos-manager a refusé la session {desktop}");
    }
    Ok(())
}

/// Retourne `true` si l'application doit se fermer pour laisser la place à la session choisie.
pub fn switch_to(target: Target, steam_client: &str) -> Result<bool> {
    if in_noosfin_session() {
        let desktop = match target {
            Target::Plasma => Some("plasma.desktop".to_string()),
            Target::Steam => Some(format!("gamescope-session-{steam_client}.desktop")),
            Target::Restart => None,
        };
        let choice = match (target, desktop) {
            (_, Some(d)) => match set_temporary_session(&d) {
                // La session Noosfin se termine, SDDM ouvre la session ponctuelle.
                Ok(()) => "relogin",
                Err(e) => {
                    log::warn!("{e:#} : lancement direct depuis la session Noosfin");
                    if target == Target::Plasma { "plasma" } else { "steam" }
                }
            },
            _ => "restart",
        };
        std::fs::write(next_file(), choice)?;
        return Ok(true);
    }
    // Hors session HTPC (test depuis Plasma) : comportement de repli.
    match target {
        Target::Steam => {
            Command::new("steam").arg("steam://open/bigpicture").spawn()?;
            Ok(false)
        }
        Target::Plasma | Target::Restart => Ok(true),
    }
}

pub fn power(action: &str) -> Result<()> {
    let st = Command::new("systemctl").arg(action).status()?;
    if !st.success() {
        bail!("systemctl {action} a échoué");
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct HdrStatus {
    pub output: String,
    pub available: bool,
    pub enabled: bool,
}

/// Premier écran actif capable de HDR (ou le premier actif).
pub fn hdr_status() -> HdrStatus {
    let Ok(out) = Command::new("kscreen-doctor").arg("-j").output() else { return HdrStatus::default() };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let outputs = v["outputs"].as_array().cloned().unwrap_or_default();
    let enabled: Vec<&Value> = outputs.iter().filter(|o| o["enabled"].as_bool() == Some(true)).collect();
    let pick = enabled
        .iter()
        .find(|o| o["capabilities"].as_u64().is_some_and(|c| c & 0x4000 != 0) || o["hdr"].as_bool() == Some(true))
        .or(enabled.first());
    match pick {
        Some(o) => HdrStatus {
            output: o["name"].as_str().unwrap_or_default().to_string(),
            available: o.get("hdr").is_some(),
            enabled: o["hdr"].as_bool().unwrap_or(false),
        },
        None => HdrStatus::default(),
    }
}

pub fn set_hdr(output: &str, on: bool) -> Result<()> {
    let s = if on { "enable" } else { "disable" };
    let st = Command::new("kscreen-doctor")
        .arg(format!("output.{output}.hdr.{s}"))
        .arg(format!("output.{output}.wcg.{s}"))
        .status()?;
    if !st.success() {
        bail!("kscreen-doctor a échoué");
    }
    Ok(())
}

// ─────────────────────────────── Applications Flatpak ───────────────────────────────

pub const VACUUMTUBE: &str = "rocks.shy.VacuumTube";

pub fn flatpak_installed(app: &str) -> bool {
    Command::new("flatpak").args(["info", app]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Installation utilisateur depuis Flathub (pas d'authentification polkit dans la session TV).
pub async fn flatpak_install(app: &str) -> Result<()> {
    let remote = tokio::process::Command::new("flatpak")
        .args(["remote-add", "--user", "--if-not-exists", "flathub", "https://dl.flathub.org/repo/flathub.flatpakrepo"])
        .status()
        .await?;
    if !remote.success() {
        bail!("impossible d'ajouter le dépôt Flathub");
    }
    let out = tokio::process::Command::new("flatpak")
        .args(["install", "--user", "-y", "--noninteractive", "flathub", app])
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.lines().last().unwrap_or("échec de l'installation"));
    }
    Ok(())
}

/// Lance une application Flatpak et attend sa fermeture.
pub async fn flatpak_run(app: &str, args: &[String]) -> Result<()> {
    let st = tokio::process::Command::new("flatpak").args(["run", app]).args(args).kill_on_drop(true).status().await?;
    if !st.success() {
        bail!("{app} s'est arrêté avec le code {:?}", st.code());
    }
    Ok(())
}

// ─────────────── Clavier physique ───────────────

/// Dispositions proposées : (code XKB « disposition(variante) », libellé).
pub const KEYBOARDS: &[(&str, &str)] = &[
    ("fr", "AZERTY · France"),
    ("fr(oss)", "AZERTY · France (variante)"),
    ("be", "AZERTY · Belgique"),
    ("ca(fr)", "QWERTY · Canada français"),
    ("ch(fr)", "QWERTZ · Suisse romande"),
    ("us", "QWERTY · États-Unis"),
    ("gb", "QWERTY · Royaume-Uni"),
    ("de", "QWERTZ · Allemagne"),
];

pub fn keyboard_label(code: &str) -> String {
    KEYBOARDS.iter().find(|k| k.0 == code).map_or_else(|| code.to_string(), |k| k.1.to_string())
}

/// Le KWin de la session lit `kxkbrc` aussi dans ce dossier (XDG_CONFIG_DIRS, voir
/// noosfin-session) : la disposition choisie ici ne touche pas celle de Plasma.
fn kxkbrc_path() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_default().join("noosfin/xdg/kxkbrc")
}

/// Écrit la disposition ; KWin la lit à son lancement (« Relancer l'interface »).
/// Pas de rechargement à chaud : le signal reloadConfig peut faire planter KWin.
pub fn apply_keyboard(code: &str) -> anyhow::Result<()> {
    let (layout, variant) = match code.split_once('(') {
        Some((l, v)) => (l, v.trim_end_matches(')')),
        None => (code, ""),
    };
    let p = kxkbrc_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, format!("[Layout]\nUse=true\nLayoutList={layout}\nVariantList={variant}\n"))?;
    Ok(())
}

// ─────────────── Édition de Bazzite ───────────────

/// Image Bazzite installée (/usr/share/ublue-os/image-info.json).
#[derive(Debug, Clone, Default)]
pub struct Edition {
    /// ex. « bazzite-deck », « bazzite-nvidia », « bazzite-deck-gnome »
    pub image: String,
    /// « kinoite » (KDE) ou « silverblue » (GNOME)
    pub base: String,
    pub version: String,
}

impl Edition {
    pub fn detect() -> Option<Edition> {
        // Pour les tests : NOOSFIN_IMAGE=bazzite-nvidia simule une autre édition.
        if let Ok(image) = std::env::var("NOOSFIN_IMAGE") {
            return Some(Edition { image, base: "kinoite".into(), version: "test".into() });
        }
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("/usr/share/ublue-os/image-info.json").ok()?).ok()?;
        let image = v["image-name"].as_str()?.to_string();
        image.starts_with("bazzite").then(|| Edition {
            image,
            base: v["base-image-name"].as_str().unwrap_or_default().to_string(),
            version: v["version-pretty"].as_str().unwrap_or_default().to_string(),
        })
    }

    pub fn summary(&self) -> String {
        format!("Bazzite {} · {} · {}", self.version, self.image, if self.base == "kinoite" { "KDE" } else { &self.base })
    }

    pub fn deck(&self) -> bool {
        self.image.starts_with("bazzite-deck")
    }

    pub fn nvidia(&self) -> bool {
        self.image.contains("nvidia")
    }

    /// La session Steam Gamescope n'existe que sur les éditions Deck (hors NVIDIA).
    pub fn gamescope(&self) -> bool {
        self.deck() && !self.nvidia()
    }
}

/// Tuile « Steam » proposée sur le tableau de bord.
pub fn gamescope_available() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| Edition::detect().is_some_and(|e| e.gamescope()))
}
