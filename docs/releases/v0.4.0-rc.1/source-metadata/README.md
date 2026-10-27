# Bunny and Size V0.4 — sources correspondantes 0.4.0-rc.1

Sources correspondantes de la prerelease publique en mode `public-unsigned`.
Le packaging validé ne confère aucun droit de publication : une décision
propriétaire externe lie les sept noms, tailles et SHA-256 définitifs.
Les archives conservent `distribution_approved=false` ; la décision externe
reste valable sans les modifier. Code GPL-3.0-only ;
voir LICENSE, CREDITS.md, BRANDING.md et les licences des composants. Les sources
Cargo verrouillées, leurs textes originaux et les compléments légaux sont
fournis. Les licences alternatives des tiers ne changent pas la voie GPL retenue.

Les fichiers sont exportés sans dépôt Git, historique, preuves ou identifiants.
SOURCE_SELECTION.json conserve les empreintes des blobs sélectionnés et des
rendus publics ; FILES.json et SHA256SUMS couvrent l’ensemble des fichiers.
`distribution-legal/` contient les notices/textes audités par plateforme.

Préparer Python >= 3.11, Rust 1.99.0 natif x64 et les outils système : Ubuntu
24.04 (C/C++, pkg-config, fontconfig/X11), ou MSVC 14.44 et SDK Windows
10.0.26100.0. Les compilateurs/runtime système ne sont pas redistribués.
Depuis le dossier de ces sources :

```text
python scripts/v04_distribution.py build --source . --output ../build-neuf
```

Deux jobs, CARGO_HOME et target neufs, dépendances vendoriées, `--frozen` et
réseau Cargo désactivé ; chemins de sources et de build remappés. Sous Windows,
le linker emploie `/Brepro`. Le répertoire de sortie doit être neuf. BUILD.json
fournit le SHA du code et les empreintes des trois exécutables. Les binaires
obtenus ne sont pas automatiquement autorisés pour diffusion.

Tests ciblés du packaging :
`python -m unittest discover -s scripts/tests -p test_v04_distribution.py -v`.
Guide fonctionnel : docs/v0.4/USAGE.md et LANGUAGES.md. Sécurité Windows et
réserves : docs/release/v0.4/. Aucun script ne publie, crée un tag ou signe.

## Qualification et réserves acceptées

Code qualifié : `e69cea52f7249ffe3310d931e67ba06990ab3b1e`.
Les sources finales ne modifient aucun input de compilation ; seuls les documents
et outils de préparation évoluent. BUILDINFO distingue source_commit et
built_from_commit et lie les empreintes de tous les inputs de compilation.

SignPath Foundation a refusé la candidature ; il ne signe ni n'approuve le projet.
Les trois PE Windows sont non signés, éditeur non vérifié. SmartScreen peut
avertir ou bloquer ; les politiques des postes administrés peuvent interdire
le lancement. Les SHA-256 contrôlent l'intégrité et ne remplacent pas une signature.
Conserver Defender, SmartScreen et les politiques de sécurité actifs.

D1 #139 reste **UNRESOLVED** : pied parfois périmé après changement de langue.
Non reproduit dans les scénarios physiques RC.1 du 26 octobre, sans correction
démontrée. La qualification physique reste partielle : redimensionnement libre,
lecteur d'écran/focus UIA, DPI autres que 125 %, multi-écran et SMB physique
non qualifiés. Ces réserves sont acceptées par le propriétaire le 27 octobre
pour cette prerelease d'évaluation, sans certification stable ni autorisation
de distribution. Le parcours de téléchargement public SmartScreen reste NOT_RUN.
Voir CODE_SIGNING.md, WINDOWS_SECURITY.md et RESERVATIONS.md.

Contrat et commandes du gate : docs/release/v0.4/DISTRIBUTION_GATE.md.
