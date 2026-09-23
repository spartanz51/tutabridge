# Validation du prototype revu

Environnement : macOS, Rust/Cargo 1.95.0. SDK officiel 359.260904.0 à la révision verrouillée dans le rapport. Journaux dans `reports/`.

| Contrôle | Résultat |
|---|---|
| CLI `cargo build -p tutabridge` | Réussi. |
| Tests core | 362 réussis. |
| Tests unitaires de l’adaptateur | 43 réussis. |
| Intégration adaptateur | 8 réussis ; 2 critères protocole exécutés séparément. |
| SDK complet `--lib` | 315 réussis ; 1 test ignoré préexistant. |
| SDK `interactive_session_test` | 3 réussis. |
| Protocole : valeur optionnelle vide | Réussi. |
| Protocole : sujet chiffré AEAD v3 | **Échec, MacError : candidat non promouvable.** |
| Cinq patchs appliqués séparément + suites ciblées | Réussis ; voir `independent-results.json`. |
| Cinq patchs appliqués ensemble | Réussi ; sources identiques octet par octet au prototype testé. |
| Application de la série au snapshot 360.260917.0 | Réussie ; pas de compilation ni de tests sur ce snapshot. |
| Sélecteur Python | 7 tests réussis. |

Les suites ordinaires représentent 731 tests Rust réussis. Les tests isolés des patchs sont des répétitions ciblées de certaines validations, pas des tests supplémentaires à additionner. Le test de corps optionnel est un succès supplémentaire dans la suite protocole, mais cette suite reste rouge à cause d’AEAD.

Les transports des tests Rust sont simulés en mémoire. Aucun compte réel n’a été utilisé. Le smoke public réseau et le sélecteur complet n’ont pas été relancés pour cette itération centrée sur les patchs ; le succès réseau de l’itération précédente n’est pas présenté comme un nouveau test.

Un premier essai des tests isolés avec un répertoire Cargo target partagé entre plusieurs workspaces a rencontré des incohérences de types rand_core/crypto-primitives issus du cache de compilation. Les cinq tests isolés ont été repris dans un target neuf et dédié, avec compilation incrémentale désactivée. Les résultats conservés sont ceux de cette reprise, tous réussis. Aucun contournement de code ni de dépendance n’a été ajouté pour faire passer cette erreur de cache.

Les warnings SDK existants `BinaryBlobWrapperSerializationError` inutilisé et durée de vie implicite d’`Unexpected` restent présents. Le MSRV, les bindings mobiles et l’ensemble des variantes de comptes ne sont pas validés ici.
