# Validation

## Tests exécutés

| Suite | Résultat |
| --- | --- |
| SDK, tests unitaires | 333 passent ; 1 test préexistant ignoré |
| SDK, intégration écriture AEAD | 4 passent |
| SDK, sessions interactives | 3 passent |
| Bridge core | 362 passent |
| Adaptateur Tuta, tests unitaires | 43 passent |
| Diagnostic AEAD | 1 passe |
| Intégration mail, cas de protocole inclus | 12 passent |
| Sélecteur Python | 12 passent |

**758 tests Rust distincts passent**, plus 12 tests Python. La suite mail ordinaire en ignore deux ; `protocol-final.log` les exécute explicitement avec `--include-ignored`. Les passages répétés ne sont pas additionnés.

La compilation du binaire CLI réussit. Les journaux correspondants sont `reports/sdk-final.log`, `bridge-final.log`, `protocol-final.log`, `build-final.log` et `selector-tests.log`.

## Interopérabilité TypeScript

`node ts-reference/verify_writes.mjs` exécute les fonctions TS amont et génère les sorties utilisées par les tests Rust. Les cas couvrent les formats v2/v3, un texte Unicode, une chaîne vide et un champ imbriqué. Le Rust produit les mêmes octets avec la même source aléatoire. Les quatre vecteurs primitifs de l'audit précédent sont également vérifiés.

Les cas d'agrégats frères sont caractérisés séparément : l'accumulation du préfixe dans la boucle TS est reproduite, et le Rust utilise les chemins indépendants attendus par le lecteur. Voir `STYLE_AND_PARITY.md` et `ts-reference/mapper-results.json`.

## Écriture via le transport simulé

Les quatre tests d'intégration ne remplacent pas le chiffrement ou la sérialisation par des mocks : ils utilisent le SDK connecté aux fixtures publiques de connexion et interceptent les requêtes HTTP.

- Migration d'un message CBC : vérification des identifiants/type envoyés à `UpdateKdfNonceService`, utilisation d'un nonce serveur différent de la proposition locale, PUT AEAD, puis comparaison du message relu complet.
- Mise à jour ultérieure par `CryptoEntityClient::update_instance` : conservation du nonce, absence de nouvel appel de migration, sujet vide effectivement chiffré et relu.
- Création : remplacement d'un nonce copié, génération d'un identifiant d'agrégat manquant, chiffrement et relecture.
- Erreurs : réponse serveur en échec, nonce retourné trop court, nonce local malformé et version de clé non prise en charge. Aucun PUT ne suit un échec d'enregistrement du nonce.

Le transport de création simule l'allocation serveur de l'identifiant. Ce test ne prouve pas que le serveur réel autorise une création directe de Mail ; l'envoi de courrier utilise les services de brouillon, conservés en CBC.

## Style et invariants

- `rustfmt --check` réussit sur les fichiers modifiés avec le fichier amont et l'édition 2021 déclarée par Cargo.
- Clippy `--lib --test aead_writes --test interactive_session_test` réussit. Il conserve les 23 avertissements SDK antérieurs ; la comparaison des messages et chemins sources est identique au prototype de lecture. Voir `reports/clippy-comparison.json`.
- `git diff --check` réussit dans les clones SDK et bridge.
- Les primitives cryptographiques, modèles générés, définitions de services générées, `MailFacade`, `FolderSystem`, `ServiceExecutor` et `Cargo.toml` restent les fichiers officiels. L'overlay du bridge est identique à celui du prototype de lecture.

## Patchs et rejeu depuis un clone neuf

Les neuf patchs s'appliquent proprement sur 359 et sur le commit master audité `46270557c251d1a31157d72e0aaf0cc63bb33ecf`. L'arbre 359 produit correspond exactement aux fichiers testés : `c72f2f8f6ab02df7bf69379546b9a7a734df246a`. La compilation de master n'est pas revendiquée.

Le patch 08 s'applique sur le prototype de lecture, puis 09 sur 08. Leurs étapes séparées ont été vérifiées par application ; les tests complets portent sur la série assemblée.

Le sélecteur livré a été rejoué depuis des clones neufs sur le tag officiel `tutanota-release-359.260904.0`, en exigeant v2 et v3. Il inclut désormais `--test aead_writes` dans le contrôle SDK. Les caches de compilation SDK et bridge restent séparés.

Résultat : **`candidate_for_review`** après réussite des six contrôles (patchs, compilation, tests bridge, tests SDK, réseau public, protocole). Rapport, lock local, catalogue des tags et journaux dans `reports/selector-replay/`.

Le smoke public obtient HTTP 200 pour `applicationtypesservice` avec `cv=359.260904.0`. Il ne teste ni un compte authentifié ni l'autorisation serveur d'écritures AEAD. Aucun enregistrement de nonce ni écriture de message n'a été envoyé à un serveur réel.

Les sommes de contrôle du code exécuté sont dans `manifest.json`, dont le hash est enregistré dans le rapport du sélecteur. `SHA256SUMS` couvre le dossier final. Les scripts de packaging conservent les chemins locaux de l'expérience ; le CLI `prototype.py` est réutilisable.
