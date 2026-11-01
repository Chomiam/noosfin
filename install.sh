#!/usr/bin/env bash
# Installation de Noosfin en une ligne :
#   curl -fsSL https://raw.githubusercontent.com/Chomiam/noosfin/main/install.sh | bash
# Télécharge la dernière release puis lance l'installateur (interface terminal).
set -euo pipefail
REPO="Chomiam/noosfin"

if [ ! -r /usr/share/ublue-os/image-info.json ] || ! grep -q '"image-name": *"bazzite' /usr/share/ublue-os/image-info.json; then
    echo "✘ Noosfin ne fonctionne que sur Bazzite (https://bazzite.gg)." >&2
    exit 1
fi
if [ "$(uname -m)" != "x86_64" ]; then
    echo "✘ Seule l'architecture x86_64 est prise en charge." >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "➜ Téléchargement de Noosfin…"
curl -fL --progress-bar "https://github.com/$REPO/releases/latest/download/noosfin-x86_64.tar.gz" -o "$tmp/noosfin.tar.gz"
tar -xzf "$tmp/noosfin.tar.gz" -C "$tmp"
# Le script arrive par un tube : l'installateur lit le clavier sur le terminal.
"$tmp/noosfin/bin/noosfin-installer" "$@" </dev/tty
