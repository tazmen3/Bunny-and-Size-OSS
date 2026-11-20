# Bunny and Size 0.5.0-rc.2

Préversion d’évaluation pour Ubuntu x86_64 et Windows 11 x64.

- Le scan local lancé depuis la vue Dossiers utilise un inventaire en mémoire, dans la limite du budget sélectionné. Un stockage temporaire prend le relais lorsque le budget est atteint. L’enregistrement de l’instantané dans le catalogue SQLite se fait à l’issue de l’analyse.
- Les mesures enregistrées restent consultables hors ligne lorsque le dossier d’origine est déplacé ou indisponible. Dans le catalogue, « Vérifier la source » relance l’analyse pour une mesure automatique et conserve l’instantané enregistré.
- La vue Dossiers indique la taille logique des fichiers analysés et signale les exclusions. Le préfixe « ≥ » indique une borne basse lorsque le périmètre n’a été analysé qu’en partie.
- Le catalogue permet de comparer des mesures enregistrées et des sources sélectionnées. Les résultats distincts de taille et de contenu restent consultables en lecture seule.
- Le code original passe sous Apache-2.0. Les licences et avis propres aux dépendances et aux visuels sont fournis avec les archives.

## Télécharger et lancer

Extraire intégralement l’archive avant de lancer l’application. Vérifier son empreinte dans `SHA256SUMS` si nécessaire.

- Sous Ubuntu 24.04 x86_64, lancer `./bunny-and-size` dans le dossier extrait. La fenêtre utilise X11 ou XWayland. Le mode Wayland natif reste en qualification.
- Sous Windows 11 x64, ouvrir `bunny-and-size.exe`. Le programme utilise le [runtime Microsoft Visual C++ Redistributable x64](https://aka.ms/vc14/vc_redist.x64.exe).
- L’exécutable Windows de cette préversion est non signé et l’éditeur n’est pas vérifié par Authenticode. SmartScreen peut afficher un avertissement. Certains postes administrés peuvent empêcher le lancement. Conserver les protections Windows actives. Le SHA-256 permet de vérifier l’intégrité de l’archive.

## Périmètre vérifié

Les validations Ubuntu ont couvert les scans locaux, le détail des mesures partielles et la réouverture hors ligne sur X11, ainsi qu’un essai sous XWayland. Les essais réseau Ubuntu ont couvert SMB 3.1.1 avec authentification et signature, ainsi que NFS 3 et NFS 4.0. Les scénarios NFS 4.1 et 4.2 restent en qualification.

Windows 11 a été exercé en usage natif. La recette du candidat sur NTFS en VM a couvert les petits dossiers, les exclusions, un corpus de 50 000 fichiers et la réouverture hors ligne. Le pied de fenêtre peut garder l’affichage de la langue précédente après une bascule (#139).

La couverture complète des lecteurs d’écran et des configurations DPI, ainsi que la campagne de charge de 50 000 à 10 millions de fichiers, restent en cours. Android reste hors du périmètre de cette préversion.

## Sources et licences

Le code original est sous [Apache-2.0](LICENSE). Les exécutables desktop intègrent Slint sous sa licence Royalty-free 2.0, avec attribution dans À propos. Les visuels du lapin adaptés sont sous CC BY-SA 4.0. L’archive de sources fournit les manifestes Cargo verrouillés et les dépendances pour une compilation hors ligne.
