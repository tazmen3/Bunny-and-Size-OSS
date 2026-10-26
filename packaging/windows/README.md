# Bunny and Size v0.3.0-rc.1 — candidat portable Windows x64

Cette archive candidate se lance sans Rust, Cargo ou Visual Studio.
Elle est destinée aux essais privés ; la conformité de distribution et la
validation sur Windows propre ne sont pas encore démontrées.

## Installer et lancer

1. Utiliser Windows x64 (recette réalisée sur Windows 11 25H2 ; autres versions
   non testées). Extraire **tout** le ZIP dans un dossier accessible en écriture.
2. Installer, si nécessaire, le **Microsoft Visual C++ Redistributable x64 v14**
   officiel : <https://aka.ms/vc14/vc_redist.x64.exe>. Aucune DLL n'est incluse.
   La recette sur Windows 11 propre a été exécutée avec **14.44.35211.0**
   (installeur Microsoft signé, séparé du ZIP). Le minimum absolu n'est pas
   établi ; utiliser une version officielle au moins aussi récente.
3. Double-cliquer sur **bunny-and-size.exe**. La fenêtre graphique s'ouvre sans console.
   Choisir un dossier avec **Parcourir**, puis **Analyser**. Le choix seul ne
   déclenche aucun scan. **Annuler** interrompt l'analyse entre deux accès disque.
4. Dans un terminal ouvert dans ce dossier, utiliser la CLI distincte :

```powershell
.\bunny-and-size-cli.exe --help
.\bunny-and-size-cli.exe scan 'C:\chemin\dossier avec espaces' --format json
```

Sans runtime compatible, Windows peut signaler `VCRUNTIME140.dll` introuvable
avant l'ouverture de l'application. Installer le paquet Microsoft officiel,
puis relancer ; ne pas télécharger une DLL isolée sur un site tiers.
L'installation du prérequis peut demander une élévation ou un redémarrage.
La portabilité de Bunny and Size n'inclut pas une installation autonome du runtime.
Référence Microsoft : <https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist>.

## Usage et limites

Arbre/tableau, parent, colonnes et tris, Treemap logique, Top 100 global,
volumes, dates UTC, allocations et propriétaires optionnels, préférences locales,
exports CLI JSON/CSV/TSV résumé ou CSV/TSV détaillé. Voir **USER_GUIDE.md**,
**COMPATIBILITY.md** et **RESERVATIONS.md** pour les contrats et limites.
Le logiciel analyse en lecture seule. La résolution NSS/AD est un consentement
séparé pouvant entraîner une consultation réseau ; elle reste désactivée par défaut.

## Licence et provenance

Le code original Bunny and Size et la voie Slint choisie relèvent de **GPL-3.0-only**.
**AUCUNE GARANTIE**, dans la mesure permise par la loi ; lire `LICENSE`.
Les contributeurs consignés par Git figurent dans `AUTHORS.txt` ; chaque
composant tiers conserve ses titulaires et conditions dans
`THIRD_PARTY_NOTICES.txt` et `licences/`.

`BUILDINFO.json` identifie le commit exact, la chaîne, les hashes des binaires
et l'archive source correspondante `Bunny-and-Size-v0.3.0-rc.1-source-windows-x86_64.zip`.
Les deux archives doivent être fournies ensemble aux destinataires ; un lien
vers le dépôt privé ne remplace pas cet accès. `FILES.json` permet de contrôler
le contenu extrait. Les SHA-256 des deux ZIP sont fournis à côté des archives.
Les binaires ne sont pas signés ; aucune exception de sécurité Windows n'est
installée par ce paquet. L'expérience SmartScreen d'un téléchargement public
reste à vérifier avant diffusion.

Le bouton **À propos / Licences**, visible dans la GUI, affiche les avis et la
GPLv3 intégrale hors ligne. Les mentions originales restent dans les fichiers
livrés ; AUTHORS.txt ne détermine pas la propriété des droits.

L'accord du propriétaire du 25 octobre 2026 couvre les seuls dérivés du lapin
listés avec leurs empreintes dans PROVENANCE.json. Voir BRANDING.md pour CC BY-SA
4.0 et la compatibilité des adaptations incorporées vers GPLv3. La disponibilité
des droits graphiques ne vaut pas autorisation de publier une nouvelle release.
Les anciens candidats `redistribution_ready=false` restent des preuves privées.

Mascotte adaptée : Louis Lejuez / TazmenWorld, CC BY-SA 4.0.
Adaptations incorporées : licence d'adaptateur GPL-3.0-only, voir BRANDING.md.
Texte CC intégral et empreintes dans licences/artwork/ ; maître et conversions
fournis avec les sources. Original complet et portrait exclus des livrables.
