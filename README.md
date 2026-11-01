# Noosfin

Tableau de bord HTPC pour Bazzite : backend Rust, interface Slint, session KWin (HDR).
Au démarrage, SDDM ouvre la session **Noosfin HTPC** (session par défaut), depuis laquelle on choisit :

- **Médiathèque** : catalogues AIOStreams (ou Cinemeta) + AniList, sources du moteur intégré, onglets Accueil / Animes / Films / Séries / Historique / Recherche (Jellyfin optionnel)
- **IPTV** : comptes Xtream Codes et playlists M3U enregistrés, catégories, recherche, favoris
- **YouTube** : VacuumTube (Flatpak `rocks.shy.VacuumTube`, installation proposée s'il manque)
- **Steam** : `gamescope-session-plus ogui-steam`
- **Bureau KDE** : Plasma Wayland

### Changement de session (mécanisme Bazzite / SteamOS)

| Fichier `/etc/sddm.conf.d/` | Rôle |
|---|---|
| `zz-holo-autologin.conf` | défaut Bazzite (écrit par steamos-manager) |
| `zzs-noosfin-autologin.conf` | **Noosfin par défaut** (installé par `install.sh`, prioritaire) |
| `zzt-holo-temp-login.conf` | session ponctuelle, prioritaire, effacée au démarrage de la session suivante |

« Bureau KDE » et « Steam » posent une session ponctuelle via le D-Bus de steamos-manager
(`SetTemporarySession`), puis la session Noosfin se termine et SDDM se reconnecte sur la cible.
À la déconnexion de Plasma ou à la sortie de Steam, on revient à Noosfin. Les raccourcis Bazzite
(« Passer au bureau » dans Steam, « Retour au mode jeu » dans Plasma) suivent le même chemin.
Si steamos-manager ne répond pas, `noosfin-session` lance Plasma ou Steam lui-même.

## Installation

```bash
./session/install.sh              # compile, installe dans ~/.local/bin, puis session SDDM (sudo)
./session/install.sh --user-only  # sans toucher à SDDM
./session/install.sh --uninstall  # retire la session (Bazzite redémarre comme avant)
```

Au premier lancement : **Paramètres → Connecter Jellyfin (Quick Connect)**, puis saisir le code
dans Jellyfin (Profil → Quick Connect).

## HDR

- La session lance `kwin_wayland` seul : il reprend la configuration d'écran de Plasma
  (`~/.config/kwinoutputconfig.json`), HDR compris. Le bouton HDR du tableau de bord passe par `kscreen-doctor`.
- La lecture passe par `~/.local/bin/mpv` (Flatpak mpv, `gpu-next` + Vulkan Wayland, `target-colorspace-hint`) :
  les flux HDR10 / Dolby Vision sont envoyés tels quels à KWin.

## Moteur de sources intégré

Remplace Torrentio / Lumio / StreamFusion : `src/scraper/`.

- **Débrideurs** : AllDebrid (cache vérifié par lot), TorBox (`checkcached`), Real-Debrid (sans vérification, l'API ne le permet plus).
- **Trackers privés** : Torznab (C411 `https://c411.org/api/torznab`, Tr4ker `https://tr4ker.net/api/torznab/all`, UNIT3D,
  Ygégé…). Les `.torrent` (passkey) sont envoyés tels quels au débrideur ; quotas respectés (C411 : 15 requêtes / min).
- **Sources publiques** : YTS, EZTV, The Pirate Bay (apibay), Nyaa, Knaben (méta-moteur : 1337x, TorrentGalaxy,
  LimeTorrents, RuTracker…), Torrents-CSV, SubsPlease.
- **Trackers intégrés** : 35 définitions de trackers publics (format Cardigann de Prowlarr, reprises une fois dans
  `assets/indexers/`) interprétées par `src/scraper/cardigann.rs` : gabarits Go, sélecteurs CSS (`:contains`, `:has`),
  réponses HTML / JSON / XML, page de détail → magnet. Aucun Prowlarr / Jackett à l'exécution.
  `noosfin --test-trackers [id…]` les teste tous. Les sites « lents » passent par FlareSolverr (≈ 10 s chacun).
- **Réseau** : DNS chiffré (DoH Cloudflare / Quad9 / Google) contre les blocages DNS des FAI ; FlareSolverr **local**
  (conteneur podman `noosfin-flaresolverr` sur 127.0.0.1:8191, lancé au premier besoin) pour les pages Cloudflare et en
  dernier recours quand la connexion directe échoue.
- **Tri** : critères ordonnés (cache, langue, sous-titres, résolution, qualité, HDR, codec, seeders, taille, groupe),
  exclusions (CAM, langues, résolutions, taille). L'épisode exact passe toujours avant un pack.
- **Animes** : titres romaji + anglais, saison déduite d'AniList (« 2nd Season »), Nyaa + trackers FR, groupes favoris,
  choix du bon fichier dans les packs.
- **Configuration** : *Paramètres → Sources & débrideur* affiche un QR code vers une page web locale (jeton aléatoire,
  clés masquées) ; `noosfin --sources-web` la lance hors interface.
- **Test en console** : `noosfin --search movie tt0816692 "Interstellar" 2014 --resolve 0`,
  `noosfin --search episode tt0944947 "Game of Thrones" 3 9`, `noosfin --search anime "Sousou no Frieren 2nd Season" 5`.

Fichier : `~/.config/noosfin/sources.json` (droits 600).

## Catalogues (TMDB)

Avec une clé API TMDB (saisie sur la page du téléphone) : tendances, au cinéma, populaires, mieux notés, recherche, fiches
et épisodes en français, titres anglais / originaux transmis au moteur de sources. Sans clé : AIOStreams, sinon Cinemeta.

## Clavier physique

*Paramètres → Clavier physique* (AZERTY France par défaut). Le script de session écrit
`~/.local/share/noosfin/xdg/kxkbrc`, lu par le KWin de la session via `XDG_CONFIG_DIRS` : la disposition de Plasma n'est
pas touchée. Appliqué à la relance de l'interface (le rechargement à chaud de KWin s'est montré instable).

## Animes

- Catalogues AniList (saison en cours, tendances, populaires), épisodes et vignettes via Kitsu.
- Sources : moteur intégré (voir plus haut) ; AIOStreams (`kitsu:ID:EP`) seulement s'il est coché en appoint.
- Liste des sources triée selon `[prefs]` (VOSTFR / MULTI, HDR, résolution, cache debrid).
- Suivi local des épisodes vus : `~/.local/share/noosfin/state.json` (rangée « Animes en cours »).
- Les épisodes présents dans la bibliothèque Jellyfin « anime » sont marqués **JELLYFIN** et lus depuis le NAS.

## Lecture (overlay mpv)

`noosfin-mpv` lance le Flatpak mpv comme `~/.local/bin/mpv` (HDR, gpu-next) mais sans le pont manette
Python : Noosfin relaie la manette à l'overlay `noosfin-osc.lua` (barre, boutons, pistes audio /
sous-titres, informations de lecture sur Select). Décodage matériel selon le GPU (`hwdec = "auto"`).

## Historique et reprise

Historique de 10 jours (`~/.local/share/noosfin/state.json`), onglet *Historique* : Ⓐ rouvre la fiche,
Ⓨ retire une entrée, carte « Effacer l'historique ». À la fin d'un épisode, l'épisode suivant est
proposé (lecture automatique après 10 s).

## Favoris

Enregistrés sur le PC (`~/.local/share/noosfin/state.json`) : Ⓧ ou « ☆ Ajouter aux favoris » sur une fiche ; rangées
« Mes favoris » dans Accueil et Animes. Les chaînes et contenus IPTV gardent leurs propres favoris.

## Saisons d'animes

AniList sépare chaque saison : la fiche suit les liens suite / préquelle et numérote les saisons comme TheTVDB (via
ani.zip), le découpage utilisé par TMDB, Plex et Jellyfin (« Saison 3 · partie 2 »). Résumés et épisodes en français via TMDB.

## Commandes

| Manette | Clavier | Action |
|---|---|---|
| A | Entrée | Ouvrir / lire |
| B | Échap | Retour |
| Y | Tab | Liste des sources |
| X | F | Favori |
| LB / RB | Page ↑ / ↓ | Onglet précédent / suivant |
| Start / Guide | Début | Tableau de bord |

## Fichiers

- `~/.config/noosfin/config.toml` : Jellyfin, URL du manifest AIOStreams, lecteur, préférences
- `~/.local/state/noosfin/session.log` : journal de la session
- `noosfin --windowed` : lancer dans une fenêtre (tests) ; `--media` : ouvrir directement la médiathèque

## Compilation sur Bazzite

L'image n'a pas `fontconfig-devel` : `build-sys/` fournit un `fontconfig.pc` qui lie la bibliothèque
système (activé par `.cargo/config.toml`).
