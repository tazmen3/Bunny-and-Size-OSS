# Windows — exécutables non signés

SignPath Foundation a refusé la candidature ; SignPath ne fournit pas de
signature et n’approuve pas le projet. Si la publication est autorisée,
Bunny and Size V0.4 Windows sera distribué sans signature Authenticode.
Les SHA-256 contrôlent l’intégrité ; ils ne remplacent pas une signature numérique.

Les exécutables `bunny-and-size.exe` (GUI), `bunny-and-size-cli.exe` et `sortsize.exe`
(alias CLI) de cette candidate Windows sont **non signés**. Leur éditeur n’est
**pas vérifié par Authenticode**. `CompanyName=TazmenWorld` est une ressource
descriptive et ne constitue ni certificat ni identité vérifiée.

La source GitHub officielle de distribution est
https://github.com/tazmen3/Bunny-and-Size-OSS ; seule une future release
explicitement autorisée y fera foi. Cette préparation privée ne constitue pas
une release publique. Conserver les sources correspondantes et les notices.

SmartScreen peut afficher « Windows a protégé votre ordinateur », application
non reconnue ou éditeur inconnu. Une empreinte conforme établit l’intégrité
par rapport à la référence consultée ; elle ne crée aucune réputation Windows.
Aucune garantie d’exécution sur les postes d’entreprise administrés : WDAC,
AppLocker ou Smart App Control peuvent interdire ces exécutables.

Vérifier la provenance, la version et l’empreinte SHA-256 de l’archive avec la
référence officielle, puis les empreintes des fichiers extraits :

```powershell
Get-FileHash .\candidate.zip -Algorithm SHA256
Get-FileHash .\bunny-and-size.exe -Algorithm SHA256
Get-AuthenticodeSignature .\bunny-and-size.exe
Get-AuthenticodeSignature .\bunny-and-size-cli.exe
Get-AuthenticodeSignature .\sortsize.exe
```

Les trois états attendus sont `NotSigned`. Consulter `BUILDINFO.json`,
`FILES.json` et `SHA256SUMS`. Les trois empreintes doivent correspondre.
Ne pas exécuter en cas de provenance ou d’intégrité incertaine. Conserver
Defender et SmartScreen actifs ; ne pas contourner une politique d’entreprise.
En cas de blocage, transmettre à l’administrateur la version, l’empreinte et le
message affiché. Un avertissement SmartScreen ne démontre pas à lui seul une
infection ; un signalement antivirus doit être examiné séparément.

Prérequis : Windows 11 x64 et runtime Microsoft Visual C++ x64 v14 officiel.
Les bibliothèques système/runtime restent externes ; aucun installateur tiers
n’est livré. Les captures et diagnostics de test ne sont pas distribués.

## Décision de préparation du 27 octobre 2026

Le propriétaire accepte Windows non signé, SmartScreen et les restrictions des
postes administrés comme risques connus de cette prerelease d'évaluation.
Les archives `public-unsigned` sont préparées localement ; cette décision ne
vaut pas autorisation de distribution (`distribution_approved=false`).
Le parcours public avec Mark of the Web reste NOT_RUN. D1 #139 reste UNRESOLVED,
avec qualification physique partielle ; voir RESERVATIONS.md. Les issues #89
et #139 restent ouvertes. Aucune protection n'est abaissée.
