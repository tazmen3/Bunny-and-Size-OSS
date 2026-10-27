# Cause racine constatée avant correction

Le 27 octobre 2026, analyse du commit 0862798 et du rapport v04-publication-20261027/REPORT.md.
`authorize()` accepte deux contrats incompatibles (schema 1 diffusion et schema 2 préparation).
`package()` embarque le résultat complet, hash du registre compris, dans BUILDINFO.authorization.
`verify()` compare ensuite par égalité ce registre embarqué au registre fourni. La vraie décision
finale est différente du registre de préparation, donc « Public unsigned authorization mismatch ».
Réécrire BUILDINFO modifie FILES.json, SHA256SUMS et les archives déjà approuvées : circularité.
Les booléens de préparation ne constituent pas une décision de publication.

Correction prévue : schema 2 exclusivement pour préparer ; validation technique des archives
immuables ; schema 3 externe pour les sept noms, tailles et SHA-256, la provenance et les sources.
Aucune autorisation finale antérieure n'est reconduite aux nouveaux octets.

## Contrat corrigé

1. `authorize()` accepte exclusivement schema=2, scope=`local-package-preparation`,
   mode=`public-unsigned`, approved=true et distribution_approved=false. Source_commit,
   version, SHA de l'archive source, built_from_commit, empreinte canonique des inputs
   et six exécutables qualifiés sont liés. Le registre complet et son SHA restent dans
   BUILDINFO.authorization comme preuve de préparation, jamais comme décision finale.
2. `package()` puis `verify()` qualifient les archives immuables. BUILDINFO indique
   packaging_validated=true, intended_audience=public-prerelease et
   release_approval_required=external-final-assets. public_release,
   redistribution_ready et distribution_approved restent false. Le statut de sortie
   est **READY FOR OWNER RELEASE APPROVAL**, public_authorized=false.
3. `verify-delivery` contrôle les sept fichiers réels, les checksums, les manifests,
   les sources correspondantes, inputs et binaires, la provenance et les signatures
   natives Windows. Avec `--require-publication`, une décision externe schema=3,
   scope=final-asset-publication, approved=true et synthetic=false est obligatoire.
   Elle reprend exactement le `context` calculé (version/mode, commits distincts,
   SHA source, input hash, six binaires avec tailles et SHA, sept assets avec tailles
   et SHA), plus owner=tazmen3, date ISO et decision_reference non vide.
   Seule cette voie retourne **AUTHORIZED FOR PUBLICATION**. Aucun fichier n'est réécrit.

Le registre final reste hors du dossier des sept assets et hors des archives.
SHA256SUMS couvre les six autres fichiers ; son hash est lié par le registre externe.
Le fichier `.pending.json` proposé au propriétaire porte approved=false : il échoue.
Les décisions JSON sont des transcriptions d'une décision humaine dont l'origine doit
être vérifiée par l'opérateur ; ce ne sont pas des signatures cryptographiques.
Une fixture `synthetic=true` est refusée par le CLI. Son contrôle unitaire explicite
n'accorde jamais public_authorized=true et porte le statut SYNTHETIC TEST ONLY.
L'ancienne autorisation finale liée aux anciens octets n'est pas réutilisable.

```text
python scripts/v04_distribution.py verify ARCHIVE --mode public-unsigned --authorization OWNER_PREPARATION_AUTHORIZATION.json
python scripts/v04_distribution.py verify-delivery --delivery DELIVERY --version 0.4.0-rc.1 --mode public-unsigned
python scripts/v04_distribution.py verify-delivery --delivery DELIVERY --version 0.4.0-rc.1 --mode public-unsigned --require-publication --release-approval OWNER_RELEASE_APPROVAL.json
```

La vérification du ZIP exige Windows natif, y compris en mode non signé. La suite
technique des sept assets se termine donc dans la VM ; Ubuntu contrôle en plus les
mêmes octets transférés. Le mode public-signed conserve ses exigences Authenticode,
certificat éditeur et SignTool ; aucune décision non signée ne satisfait ce mode.
Les commandes ci-dessus ne publient rien, ne signent rien et ne créent aucun tag.
Un futur outil de publication doit exiger le dernier gate juste avant envoi et
utiliser exclusivement les sept fichiers exacts contrôlés, sans les régénérer.

## Reproductibilité

Le conditionnement seul est répété avec les mêmes arbres, même racine d'archive,
même version Python/zlib et même plateforme. TAR/GZIP : ordre trié, uid/gid/mtime=0,
nom gzip vide ; ZIP : ordre trié, date 1980-01-01, permissions et compression fixées.
La reproductibilité intermachine/interversion zlib n'est pas revendiquée.
L'export source utilise une sélection Git committée et propre, Cargo vendor hors
ligne et les textes légaux épinglés. Les exécutables qualifiés ne sont pas rebâtis
si les 26 998 entrées de compilation sont strictement identiques.
