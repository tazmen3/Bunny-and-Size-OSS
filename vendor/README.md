# Backend Slint 1.18.1 — correctif #94

Copie **complète** de la crate publiée `i-slint-backend-winit` 1.18.1, sans
modification du cache Cargo. Les autres crates Slint restent en 1.18.1 ; Winit
0.30.13 et softbuffer 0.4.8 restent verrouillés. Le workspace GUI applique
`[patch.crates-io]` par chemin relatif, utilisable depuis un clone propre.

Provenance : <https://static.crates.io/crates/i-slint-backend-winit/i-slint-backend-winit-1.18.1.crate>,
SHA-256 `dae4fcf1b19e9cb109984d2af87d2b9485388c619265bff71dd523133888bee2`,
révision amont `372cf0ee5577c3dfec309a45e7b778ba4e81b734`.
`provenance.json` conserve les empreintes originales et livrées de chaque fichier.
Le diff sémantique complet est dans `windows-redraw-94.patch`.

Deux fichiers seulement sont modifiés :

- `winitwindowadapter.rs` : rétroport exact des 33 lignes de
  [Slint #13568](https://github.com/slint-ui/slint/pull/13568), commit
  `08a297b7151e52b98b500b4424a8dee770fc7b58`, fusionné le 28 septembre 2026.
- `renderer/sw.rs` : garde Windows avant resize/acquisition du buffer/rendu,
  conservant le cache et les invalidations pendant la minimisation. Retour
  `Success` intentionnel : `Skipped` déclencherait une nouvelle demande de redraw.

Les notices et les trois licences alternatives de la crate restent intactes
dans `LICENSES/`. SortSize utilise l'option **GPL-3.0-only**. La contribution amont
est publiée selon les règles de contribution Slint (MIT-0), et intégrée à sa
distribution sous les licences de Slint ; aucune notice d'origine n'est retirée.

Maintenance : ce rétroport est temporaire. À chaque montée de version de Slint,
vérifier la présence de #13568 et le comportement minimisé, puis exécuter le test
natif `windows_redraw`, les recettes produit et les gates Linux/Windows avant de
retirer l'override et cette copie. Ne pas mélanger les versions internes de Slint.
La dernière release consultée le 23 octobre 2026 est
[1.18.1](https://github.com/slint-ui/slint/releases/tag/v1.18.1), antérieure au correctif.
Un pin Git de tout Slint embarquerait des évolutions sans rapport ; une modification
du cache ou un patch manuel après checkout ne serait pas livrable.

Vérification hors ligne : `python scripts/verify_redraw_vendor.py`.
Avec l'archive d'origine déjà téléchargée, ajouter `--archive CHEMIN.crate` pour
comparer aussi chaque fichier publié. Aucun téléchargement implicite.

Le packaging historique V0.1 fondé sur un audit exclusivement registry ne certifie
pas cette dépendance locale : revalider ses notices et sa collecte des sources avant
toute future distribution. Cette PR ne fabrique ni ne publie de Release.
