# Validation du prototype AEAD

Date : 21 septembre 2026. Le code de production et la référence du sous-module dans le dépôt principal sont inchangés.

## Tests exécutés

| Suite | Résultat | Preuve |
| --- | --- | --- |
| SDK Rust, tests unitaires | 329 passent ; 1 préexistant ignoré | `reports/sdk-final.log` |
| Sessions interactives SDK | 3 passent | `reports/sdk-final.log` |
| Bridge core | 362 passent | `reports/bridge-final.log` |
| Adaptateur Tuta, tests unitaires | 43 passent | `reports/bridge-final.log` |
| Diagnostic AEAD | 1 passe | `reports/bridge-final.log` |
| Intégration mail, avec les cas de protocole explicitement activés | 12 passent | `reports/protocol-final.log` |
| Sélecteur Python | 12 passent | `reports/selector-tests.log` |

Total Rust distinct : 750 tests passent. La suite mail ordinaire en exécute 10 et en ignore 2 ; le rejeu avec `--include-ignored` exécute les 12. Les passages répétés ne sont pas additionnés au total.

La compilation du binaire CLI réussit. Clippy `--lib --test interactive_session_test` réussit avec 23 avertissements SDK conservés dans le journal ; aucune prétention à un SDK globalement sans avertissements. `rustfmt --check` sur les cinq fichiers Rust affectés par les propositions AEAD réussit avec la configuration amont et `--edition 2021` (édition déclarée dans Cargo). Les vérifications `git diff --check` des clones SDK et bridge passent.

## Rejeu intégral du sélecteur depuis des clones neufs

Le script livré dans ce dossier a réellement été exécuté avec le tag `tutanota-release-359.260904.0`, avec v2 et v3 explicitement exigés. Il a récupéré le tag auprès du dépôt officiel et vérifié le commit attendu. Des caches de compilation SDK et bridge séparés ont été réutilisés.

Résultat : **`candidate_for_review`**, six contrôles validés : patchs, compilation, tests bridge, tests SDK, réseau, protocole. Le sélecteur a produit un [candidate.lock.json](reports/selector-replay/candidate.lock.json) local. Aucun déploiement/publication automatique.

Le smoke public a reçu HTTP 200 de `https://app.tuta.com/rest/base/applicationtypesservice` avec `cv=359.260904.0`, `v=2`. Il confirme l'acceptation de cette version pour cette requête à cet instant. Ce n'est ni un test de connexion à un compte ni une preuve que toutes les routes serveur acceptent tous les usages.

Le rapport marque v3 comme amélioration par rapport à la référence mesurée et v2 comme capacité nouvellement vérifiée. `known_limitations: []` ne concerne que les trois cas de protocole déclarés ; cela ne signifie pas absence de limites dans tout le SDK. L'écriture AEAD reste notamment hors périmètre.

Rapport complet, catalogue des tags et journaux : `reports/selector-replay/`.

## Application des patchs

`reports/patch-application.json` vérifie via un index Git temporaire :

1. Les sept patchs sur 359 : arbre produit identique au code testé (`48579032ee69fe8c497089d9fb83bf661311df8f`).
2. Les sept patchs sur le commit master audité `46270557c251d1a31157d72e0aaf0cc63bb33ecf` : application réussie, sans revendication de compilation de cette cible.
3. Le patch 06 seul sur 359 : application réussie.
4. Les patchs 06 + 07 seuls sur 359 : application réussie.
5. L'overlay bridge sur `b375f1c275b8162ee8f04008144e3dd2c72a10d6` : application réussie.

Les tests complets portent sur la série assemblée. L'application indépendante de 06 ou 06 + 07 ne vaut pas validation séparée de leur compilation.

Les empreintes des fichiers exécutables sont dans `manifest.json` et correspondent à `prototype_manifest_sha256` dans le rapport. `SHA256SUMS` couvre l'ensemble du dossier. Les scripts de packaging et de vérification sont conservés sous `harness/` pour l'audit ; ils contiennent les chemins locaux de cette expérience, contrairement au CLI réutilisable `prototype.py`.
