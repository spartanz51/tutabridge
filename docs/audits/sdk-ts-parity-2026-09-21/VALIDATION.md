# Validation du prototype aligné sur TypeScript

## Périmètre et références

Tests Rust exécutés sur le SDK officiel `aea5846b93a1412451e885bf99002401c3b087e8` (release 359), avec les cinq patchs assemblés, et sur l’intégration TutaBridge issue de `b375f1c275b8162ee8f04008144e3dd2c72a10d6`. Le dépôt principal n’a reçu aucune modification de code de production. Les travaux exécutables ont été réalisés dans une copie temporaire.

Les cinq patchs finaux s’appliquent chacun indépendamment sur 359. La série complète s’applique également sur le master officiel observé le 21 septembre, `46270557c251d1a31157d72e0aaf0cc63bb33ecf`. **L’application sur master n’est pas une compilation ni un test de master.** Cette itération valide le code assemblé ; les compilations isolées de l’itération précédente ne doivent pas être présentées comme une recompilation indépendante des cinq nouvelles versions.

## Résultats

| Contrôle | Résultat | Preuve |
|---|---|---|
| SDK, tests unitaires | 320 réussis, 1 test préexistant ignoré | `reports/ts-parity-sdk-final.log` |
| SDK, session interactive | 3 réussis | Même journal |
| TutaBridge, core | 362 réussis | `reports/ts-parity-bridge-v1.log` |
| Adaptateur Tuta, unitaires | 43 réussis | Même journal |
| Adaptateur, intégration | 8 réussis ; les 2 tests de protocole sont exécutés séparément ci-dessous | Même journal |
| Construction CLI | Réussie | `reports/ts-parity-build.log` |
| Protocole, champ chiffré optionnel vide | Réussi | `reports/ts-parity-protocol.log` |
| Protocole, sujet de mail AEAD v3 | **Échec : MacError** | Même journal |
| TypeScript officiel | 11 cas binaires, 8 séquences de reprise et 1 vecteur d’identifiants exécutés | `evidence/parity-vectors.json`, `harness/generate_parity.mjs` |
| Comparaison des versions TS | 16 corps de méthodes/fonctions identiques entre 359 et master | `evidence/source-audit.json` |
| rustfmt et whitespace | 15 fichiers Rust modifiés/ajoutés conformes à la configuration upstream ; git diff --check réussi | `reports/ts-parity-format.log` |
| Clippy | Commande réussie ; aucun avertissement localisé dans les lignes ajoutées/modifiées | `reports/ts-parity-clippy-final.log`, `reports/ts-parity-clippy-changed-lines.json` |
| UniFFI Kotlin et Swift | Génération réussie avec la nouvelle signature de session | `reports/ts-parity-bindings-final.log`, `reports/bindings-api.txt` |
| Sélecteur Python | 7 tests unitaires réussis | `reports/selector-unit-tests.log` |
| Livrables | Application des patchs, overlay bridge, hashes et correspondance patchs/sources vérifiés | Journaux d’application, `manifest-verification.log`, `source-roundtrip.log` |

Le total des suites Rust ordinaires est de **736 succès**, hors répétitions ciblées et hors contrôle protocole. Le test AEAD reste bloquant : **ce prototype ne constitue pas un candidat promouvable**.

La dernière correction de casse du chemin batch a été ajoutée après la suite complète ; une exécution ciblée des cinq tests batch valide cette modification finale dans `reports/ts-parity-batch-final.log`. Les autres fichiers testés sont inchangés.

## Commandes et limites

SDK :

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/chemin/vers/target-sdk cargo test -p tuta-sdk --lib --test interactive_session_test
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/chemin/vers/target-sdk cargo test -p tuta-sdk --lib entity_client::batch::tests
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/chemin/vers/target-sdk cargo clippy -p tuta-sdk --lib --test interactive_session_test
```

La vérification Clippy porte sur la bibliothèque et le test de session ; elle ne revendique pas tous les targets. Les avertissements conservés sont visibles dans le journal. La compilation des tests unitaires signale également que la méthode `parse_raw` du mock généré est inutilisée ; le bilan Clippy ci-dessus ne doit donc pas être lu comme une absence de tout avertissement dans tous les targets. Le dernier changement de casse batch est postérieur à cette exécution Clippy.

La génération UniFFI utilise `uniffi-bindgen generate --library .../libtutasdk.dylib --language kotlin --language swift --out-dir ...`, exécuté depuis la racine du checkout SDK. `ktlint` et `swiftformat` sont absents : la génération a réussi, son formatage automatique a été omis. **Aucune compilation Android/iOS n’est revendiquée.**

Les tests d’intégration utilisent des transports simulés et vérifient notamment les paramètres HTTP et les champs sérialisés réels. Le harnais TS exécute les corps de fonctions officiels avec des dépendances de test injectées ; il ne lance pas l’application Tuta ni toute sa suite de tests. Les différences de périmètre et les deux rejets binaires plus stricts sont détaillés dans `TS_PARITY.md`.

Aucun test avec compte Tuta authentifié n’a été effectué pendant cette revue. Le sélecteur automatique complet et le smoke test réseau n’ont pas été relancés de bout en bout dans cette itération. Aucune PR, aucun push et aucun message externe n’ont été envoyés.
