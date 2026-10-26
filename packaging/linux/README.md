# Bunny and Size v0.3.0-rc.1 — candidat Ubuntu 24.04 LTS x86_64

Extraire toute l’archive, puis lancer `./bunny-and-size`. Rust et Cargo ne sont
pas nécessaires à l’exécution. `bunny-and-size-cli` est le programme en terminal :

```sh
./bunny-and-size-cli --version
./bunny-and-size-cli scan /chemin/du/dossier --format json --output /autre/dossier/rapport.json
```

Dans la GUI : **Parcourir…**, choisir un dossier, puis **Analyser**. Ouvrir les
dossiers dans l’arbre, utiliser Parent et les tris ; Annuler conserve un résultat
partiel. Les scans sont en lecture seule, sans suppression ni télémétrie.
Les métriques logiques et allouées restent distinctes ; une valeur inconnue n’est pas zéro.
Treemap, Top 100, volumes, dates UTC, propriétaires et préférences : voir
USER_GUIDE.md, COMPATIBILITY.md et RESERVATIONS.md. Les exports sont disponibles
dans la CLI ; aucun export GUI. La résolution NSS/AD est un consentement séparé.

Prérequis Ubuntu : glibc, libgcc, Fontconfig (`libfontconfig1`), FreeType
(`libfreetype6`), polices (`fonts-dejavu-core`), session graphique X11 ou XWayland
et bibliothèques clientes X11 (`libx11-6`, `libx11-xcb1`, `libxcb1`, `libxcursor1`,
`libxrandr2`, `libxi6`, `libxfixes3`, `libxrender1`, `libxext6`, `libxkbcommon0`,
`libxkbcommon-x11-0`). Le sélecteur requiert un bus D-Bus de session et
`xdg-desktop-portal` avec un backend adapté, par exemple
`xdg-desktop-portal-gtk` ou `xdg-desktop-portal-kde`. Le portail et son backend
sont des services externes. La saisie manuelle du chemin reste possible.
Aucune bibliothèque système n’est livrée. Les prérequis ne sont pas un
installateur ; le bureau Ubuntu fournit généralement l’essentiel.

Le rendu Slint est logiciel et le backend est X11 ; Wayland natif n’est pas
activé. Si la composition IBus empêche la saisie, essayer ponctuellement
`XMODIFIERS=@im=none ./bunny-and-size`. Le sélecteur KDE peut altérer un nom Unix
non UTF-8 ; le backend GTK a été validé pour ce cas dans #51.
Aucune compatibilité avec une autre distribution n’est revendiquée.

GPL-3.0-only pour le code Bunny and Size et la voie choisie de Slint. **Aucune garantie** ;
voir LICENSE, AUTHORS.txt, THIRD_PARTY_NOTICES.txt et licences/. Les composants
tiers conservent leurs licences et copyrights. BUILDINFO.json indique la version,
le commit source et les empreintes des exécutables et du source correspondant.
FILES.json et SHA256SUMS permettent de vérifier les fichiers de l’archive.

Le source correspondant est fourni dans
`Bunny-and-Size-v0.3.0-rc.1-source-linux-x86_64.tar.gz`, avec dépendances Cargo verrouillées
et recette hors ligne. Fournir cet accès effectif à chaque destinataire ; un lien
vers un dépôt privé ne suffit pas. Candidat local, sans Release autorisée ; la
revue de conformité de distribution doit être achevée avant diffusion.

Le bouton **À propos / Licences**, visible dans la GUI, affiche les avis et la
GPLv3 intégrale hors ligne. Les mentions originales restent dans les fichiers
livrés ; AUTHORS.txt ne détermine pas la propriété des droits.

## Intégration au bureau (facultative)

L'archive fournit `share/applications/bunny-and-size.desktop` et les PNG
`share/icons/hicolor/TAILLE/apps/bunny-and-size.png`. Pour une installation
utilisateur, placer les trois exécutables dans un dossier du PATH, le fichier
.desktop dans `~/.local/share/applications/` et les PNG aux mêmes chemins
sous `~/.local/share/icons/`. Le lancement portable `./bunny-and-size` fonctionne
sans cette installation. Aucune préférence n'est déplacée par le packaging.

L'accord du propriétaire du 25 octobre 2026 couvre les seuls dérivés du lapin
listés avec leurs empreintes dans PROVENANCE.json. Voir BRANDING.md pour CC BY-SA
4.0 et la compatibilité des adaptations incorporées vers GPLv3. La disponibilité
des droits graphiques ne vaut pas autorisation de publier une nouvelle release.
Les anciens candidats `redistribution_ready=false` restent des preuves privées.

Mascotte adaptée : Louis Lejuez / TazmenWorld, CC BY-SA 4.0.
Adaptations incorporées : licence d'adaptateur GPL-3.0-only, voir BRANDING.md.
Texte CC intégral et empreintes dans licences/artwork/ ; maître et conversions
fournis avec les sources. Original complet et portrait exclus des livrables.
