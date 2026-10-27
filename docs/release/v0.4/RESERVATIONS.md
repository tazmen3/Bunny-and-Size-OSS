# Réserves connues — 0.4.0-rc.1

Le propriétaire accepte ces réserves le 27 octobre 2026 pour une prerelease
d'évaluation. La préparation locale est autorisée ; la publication exige un
accord distinct lié aux SHA-256 finaux. `distribution_approved=false`.

- D1 #139 : pied parfois visuellement périmé après changement de langue Windows,
  **UNRESOLVED**. Non reproduit lors des scénarios physiques RC.1 du 26 octobre ;
  aucune correction ni absence générale du défaut démontrée. Capturer les pixels
  réels avant UIA, Alt+Tab ou redimensionnement en cas de nouvelle reproduction.
- SignPath Foundation a refusé la candidature, ne signe pas cette version et
  n'approuve pas le projet. Windows non signé, éditeur non vérifié ; SmartScreen
  peut avertir ou bloquer, les postes administrés peuvent interdire le lancement.
  Les SHA-256 contrôlent l'intégrité sans remplacer une signature numérique.
  Defender, SmartScreen et les politiques doivent rester actifs.
- Qualification Windows physique partielle (26 octobre, écran à 125 %) :
  redimensionnement libre, lecteur d'écran/focus UIA, autres DPI/multi-écran et
  SMB physique non qualifiés. Parcours public avec Mark of the Web NOT_RUN.
  Android, Wayland natif et reproductibilité intermachine restent non qualifiés.
- Recherche/statistiques sur l'inventaire retenu borné, doublons exacts à la
  demande ; limites/erreurs/annulation explicites, aucune suppression implicite.
- Les issues #89 et #139 restent ouvertes. Ces limites ne sont pas des PASS.
