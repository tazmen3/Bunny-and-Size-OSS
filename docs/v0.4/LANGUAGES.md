# Langues internes V0.4

Le lot #135 ajoute italien **it-IT** et néerlandais **nl-NL**. Le choix borne la
vague à deux langues LTR ; les 27 langues de #116 restent un objectif progressif.
Les variantes it/nl reconnues utilisent les mêmes conventions canoniques, sans
promesse de gérer toute convention régionale distincte (p. ex. Belgique).

La finition fonctionnelle porte chaque PO à 350 clés communes Slint/Rust, sans vide/fuzzy ni paramètre
perdu. Pluriels `n != 1`, deux formes ; noms de mois localisés, groupement par
point et décimale virgule, horodatages UTC et valeurs natives inchangés. La GPL
intégrale et les avis tiers authentiques restent intacts. Les endonymes des sept
langues ne sont volontairement pas traduits. Les indices historiques 0–5 restent
identiques ; it/nl ajoutés en 6/7. Aucun fichier de préférences utilisateur réécrit
au chargement ; tests v1/v2 et nouvelles sessions couvrent les sept langues.

Relecture par l'agent : sens, accords, terminologie, pluriels 0/1/2/7, paramètres,
accents, textes accessibles et notices. Ce n'est pas une certification par des
relecteurs humains de langue maternelle. Les captures natives Winit et la matrice
de recette sont reliées au rapport final ; aucune validation RTL, IME, lecteur
d'écran exhaustif, Wayland ou appareil Android n'en est déduite.

La fenêtre d'analyse réutilise la transaction de langue sans I/O : textes Slint
et valeurs Rust (résultats, barres et unités) changent ensemble, chemins et IDs
restent bruts. Les erreurs OS et détails techniques du moteur restent opaques.

Les états de recherche, lecture, annulation, critères invalides et résultats
périmés sont conservés sous forme typée puis retraduits à chaud. Tests contrôleur
sept langues : page et sélection conservées, erreurs traduites sans scan.
