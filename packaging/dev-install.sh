#!/usr/bin/bash
# Installation de Noosfin.
#   ./packaging/dev-install.sh              compile + installe pour l'utilisateur, puis (sudo) la session SDDM
#   ./packaging/dev-install.sh --user-only  sans la partie système
#   ./packaging/dev-install.sh --uninstall  retire la session SDDM (Bazzite reprend son démarrage habituel)
set -euo pipefail
cd "$(dirname "$0")/.."
USER_NAME=$(id -un)
SESSION_FILE=/usr/local/share/wayland-sessions/noosfin.desktop
# « zzs- » : après zz-holo-autologin (défaut Bazzite) mais avant zzt-holo-temp-login
# (sessions ponctuelles de steamos-manager), qui doivent toujours pouvoir passer devant.
SDDM_FILE=/etc/sddm.conf.d/zzs-noosfin-autologin.conf

if [ "${1:-}" = "--uninstall" ]; then
    sudo rm -f "$SESSION_FILE" "$SDDM_FILE"
    echo "Session Noosfin retirée de SDDM."
    exit 0
fi

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
install -Dm755 target/release/noosfin "$HOME/.local/bin/noosfin"
install -Dm755 session/noosfin-session "$HOME/.local/bin/noosfin-session"
install -Dm755 session/noosfin-mpv "$HOME/.local/bin/noosfin-mpv"
install -Dm644 assets/mpv/noosfin-osc.lua "$HOME/.local/share/noosfin/mpv/noosfin-osc.lua"
install -Dm644 assets/noos-logo.svg "$HOME/.local/share/icons/hicolor/scalable/apps/noosfin.svg"
install -Dm644 /dev/stdin "$HOME/.local/share/applications/noosfin.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=Noosfin
Comment=Médiathèque Jellyfin / AIOStreams / animes
Exec=$HOME/.local/bin/noosfin --media
Icon=noosfin
Categories=AudioVideo;Video;Player;
DESK
echo "Binaire installé : $HOME/.local/bin/noosfin"

[ "${1:-}" = "--user-only" ] && exit 0

echo "Installation de la session SDDM (sudo)…"
sudo install -Dm644 /dev/stdin "$SESSION_FILE" <<DESK
[Desktop Entry]
Name=Noosfin HTPC
Comment=Tableau de bord HTPC (KWin HDR) : médiathèque, Steam, bureau KDE
Exec=$HOME/.local/bin/noosfin-session
TryExec=$HOME/.local/bin/noosfin-session
Type=Application
DesktopNames=KDE
DESK
sudo install -Dm644 /dev/stdin "$SDDM_FILE" <<CONF
[Autologin]
User=$USER_NAME
Session=noosfin.desktop
Relogin=true

[Wayland]
SessionDir=/usr/local/share/wayland-sessions,/usr/share/wayland-sessions
CONF
echo "OK : la session « Noosfin HTPC » démarrera automatiquement au prochain démarrage."
echo "Pour revenir au comportement Bazzite : ./packaging/dev-install.sh --uninstall"
