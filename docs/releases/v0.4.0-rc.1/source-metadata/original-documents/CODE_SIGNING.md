# Signature de code Windows — V0.4

SignPath Foundation a refusé la candidature de Bunny and Size. SignPath ne
fournit aucune signature pour cette version et n’approuve pas le projet.

Si le propriétaire autorise sa publication, Bunny and Size V0.4 Windows sera
distribué **sans signature Authenticode** : GUI, CLI et alias `sortsize.exe`.
L’éditeur est **non vérifié** ; la mention TazmenWorld dans les ressources PE
est descriptive et ne constitue pas une identité certifiée.

SmartScreen peut avertir ou bloquer l’exécution. Les postes administrés peuvent
interdire le lancement via leurs politiques. Aucune garantie universelle
d’exécution n’est donnée. Conserver SmartScreen et Defender actifs.

Les SHA-256 permettent de vérifier l’intégrité par rapport à une référence
fiable ; ils **ne remplacent pas une signature numérique**, ne certifient pas
l’identité de l’éditeur et ne créent aucune réputation SmartScreen.

La RC.1 est préparée localement en `public-unsigned`, sans autorisation de
distribution. Les réserves sont acceptées le 27 octobre 2026 pour cette
prerelease ; les SHA-256 finaux exigent un accord séparé avant publication.
Voir [la sécurité Windows](docs/release/v0.4/WINDOWS_SECURITY.md) et
[les réserves](docs/release/v0.4/RESERVATIONS.md). D1 #139 reste UNRESOLVED.
