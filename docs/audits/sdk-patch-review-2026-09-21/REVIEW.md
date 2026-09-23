# Revue des patchs SDK — 21 septembre 2026

## Conclusion

L’ancien fork fournit des fonctionnalités utiles, mais son organisation mélangeait protocole SDK, orchestration du bridge et API de confort. Déplacer seulement ces méthodes aurait conservé plusieurs défauts. Le prototype revu conserve quatre extensions SDK ciblées et ajoute un correctif protocole indépendant. Le reste appartient au module local `tutabridge-tuta`.

Il ne faut pas supprimer une fonctionnalité nécessaire simplement pour diminuer le nombre de fichiers. À l’inverse, refaire une cryptographie, un sérialiseur ou une authentification parallèle dans le bridge pour obtenir « zéro patch » déplacerait la dette.

Aucune PR, branche distante, publication ou prise de contact n’a été effectuée. Le checkout de production et son sous-module sont inchangés. Le travail exécutable est dans le laboratoire temporaire et dans cette archive.

## Périmètre et références

- Bridge : `b375f1c275b8162ee8f04008144e3dd2c72a10d6`.
- Ancien fork : modifications fonctionnelles `202ca648f3..1036d6f2`, puis épinglage `d1bae4750cfac28fdf89c585155b5d8c44e60a6f`. Le changement isolé de numéro de version n’est pas une migration de protocole.
- Base officielle de la réécriture : `aea5846b93a1412451e885bf99002401c3b087e8`, release `359.260904.0`.
- Contrôle d’application supplémentaire : snapshot `e274b36407fbb9cacd158c01e854f9a3bf87f020`, `360.260917.0`. Ce contrôle ne remplace pas compilation et validation du protocole.
- Comparaison avec le client officiel TypeScript pour le contrat réseau ; l’implémentation reste en Rust.

## Défauts concrets et corrections

### Lecture batch : paniques sur des réponses réseau

L’ancien `EntityClient::load_multiple` utilisait `expect("no body")`, `expect("invalid response")` et une assertion sur le type d’entité. Une réponse absente ou JSON incorrecte pouvait donc faire paniquer l’appel au lieu de retourner une erreur SDK.

La nouvelle méthode utilise le transport et le sérialiseur existants, encode les paramètres et retourne des erreurs. Les identifiants sont découpés en lots de 100. Une liste vide ne déclenche pas de requête ; les résultats incomplets ou réordonnés par le serveur restent autorisés. Le wrapper de déchiffrement réutilise `process_server_response`.

Le module `entity_client/batch.rs` concentre ajout et tests. Le fichier central ne reçoit que sa déclaration et le contrat du mock. Tests : 100/1, absence de requête, omission/réordonnancement, corps absent/malformé et arrêt après erreur serveur.

### Blobs : portée des jetons incorrecte pour les pièces jointes

L’ancien chemin demandait un jeton d’archive, sans utiliser effectivement `archive_data_type` pour établir la portée annoncée. Or Tuta distingue l’accès à une archive possédée par l’utilisateur et l’accès aux blobs référencés par un fichier qu’il possède. Le deuxième cas n’implique pas de posséder l’archive.

La nouvelle API de téléchargement reçoit le fichier référent et demande un jeton avec `archiveId`, `instanceListId`, `instanceIds` et `archiveDataType`. Le cache est indexé sur toute cette portée : un jeton pour le fichier A n’est pas réutilisé pour le fichier B. La lecture des entités blob d’une archive possédée reste une opération distincte et documentée.

Référence : [BlobAccessTokenFacade officiel](https://github.com/tutao/tutanota/blob/aea5846b93a1412451e885bf99002401c3b087e8/src/platform-kit/network/BlobAccessTokenFacade.ts), `requestReadTokenBlobs`, `requestReadTokenMultipleInstances` et `requestReadTokenArchive`.

### Blobs : taille des requêtes, parseur et erreurs

- L’ancien téléchargement envoyait tous les IDs d’une archive ensemble. Le client officiel les découpe en lots de 100 ; la nouvelle API fait de même.
- Le parseur réservait un `HashMap` à partir du compteur reçu avant de le borner par la longueur de la réponse. Un très grand compteur dans quelques octets pouvait provoquer une allocation démesurée. Il acceptait aussi certains comptages/fragments supplémentaires incohérents.
- Deux chemins de retry dupliquaient la même logique. Dans le chemin d’entité blob, une erreur temporaire antérieure pouvait masquer un refus d’accès final après renouvellement.

Le nouveau module `blob_facade/read.rs` vérifie la taille minimale, lit exactement le nombre d’entrées annoncé, rejette troncature, IDs dupliqués et octets supplémentaires. Il exige les blobs demandés et rejette les blobs inattendus. L’authenticité du contenu reste vérifiée au déchiffrement par les primitives existantes.

Un seul chemin de transport essaie les autres serveurs sur erreurs temporaires, renouvelle le jeton une seule fois sur 403 et restitue l’erreur finale. Il ne relance pas aveuglément les 401/474. Le cache existant devient générique sur sa clé ; aucun second algorithme de cache n’est ajouté. Le code d’upload reste identique, à la déclaration du sous-module près.

Référence : [BlobFacade officiel](https://github.com/tutao/tutanota/blob/aea5846b93a1412451e885bf99002401c3b087e8/src/applications/common/api/worker/facades/lazy/BlobFacade.ts), `downloadBlobsOfOneArchive`.

Un test d’intégration dans l’adaptateur utilise le vrai SDK, son sérialiseur et ses primitives de chiffrement. Il vérifie l’ordre des morceaux, deux archives partageant un ID de blob, une réponse inversée et le rejet d’un morceau altéré. Les échanges sont entièrement simulés en mémoire.

### Authentification : limiter l’API et réutiliser les services

La logique est regroupée dans `login/session.rs`, avec les types/services générés existants et les annotations UniFFI déjà utilisées par le SDK. `create_session` délègue à l’ouverture de session commune au lieu de dupliquer l’authentification.

L’API conservée couvre ouverture de session, TOTP et interrogation des challenges. `cancel_create_session` n’est appelé nulle part par le bridge ; il est retiré de cette proposition. La validation refuse un TOTP supérieur à six chiffres avant le réseau, et vérifie `kdfVersion` avant de dériver une clé. L’ancienne logique supposait Argon2 ; le SDK ne sait pas encore traiter BCrypt correctement. Le prototype retourne explicitement une erreur pour ce cas, sans prétendre ajouter BCrypt.

Tests avec les vrais formats des services : session avec/sans challenge, normalisation de l’adresse, credentials retournés avant reprise de connexion, KDF inconnu/BCrypt sans création de session, sérialisation TOTP et polling. Aucun compte réel utilisé. Les bindings mobiles générés ne sont pas validés ici.

### Parseur : deux sujets indépendants

`EntityClient::parse_raw` ajoute seulement une entrée vers le sérialiseur existant : 14 lignes, déclaration du mock comprise. Elle permet aux corps blob et aux données inline de rejoindre le pipeline de déchiffrement sans en recopier le code. Les tests d’intégration existants passent par cette méthode.

Un cinquième patch corrige séparément les valeurs chiffrées optionnelles vides. Elles doivent devenir `Null` et non une suite d’octets vide à déchiffrer. C’est le comportement explicite du [CryptoMapper TypeScript officiel](https://github.com/tutao/tutanota/blob/aea5846b93a1412451e885bf99002401c3b087e8/src/platform-kit/instance-pipeline/CryptoMapper.ts), notamment après des changements historiques de cardinalité. Les tests couvrent `null`, chaîne vide, contenu encodé valide et base64 invalide. Le test d’acceptation du corps de brouillon passe désormais.

## Ce dont le bridge a réellement besoin

| Sujet | Emplacement proposé | Peut-on supprimer le patch ? |
|---|---|---|
| WebSocket, heartbeat, IDs MailSetEntry | Module local | Oui, déjà sorti du SDK dans le prototype. |
| Arbre des dossiers, MOVE, orchestration corps/brouillons/pièces jointes | Module local | Oui, en composant les API et services publics. |
| `load_encrypted`, `decrypt_with_owner_key`, déchiffrement inline ajouté à CryptoEntityClient | Module local | Oui, les API publiques de clés, EntityFacade et InstanceMapper suffisent. |
| Batch de lecture | SDK, patch 01 | Facultatif fonctionnellement, mais supprimer implique jusqu’à une requête par mail et une perte de performance. À conserver pour le comportement actuel. |
| Lecture des blobs | SDK, patch 02 | Nécessaire pour les corps récents/pièces jointes ; transport et tokens manquent côté Rust. |
| Connexion interactive 2FA | SDK, patch 03 | Nécessaire aux comptes avec second facteur ; la supprimer réduirait les comptes pris en charge. |
| Entrée `parse_raw` | SDK, patch 04 | Nécessaire à l’adaptateur actuel ; la remplacer par un sérialiseur local dupliquerait le protocole. |
| Valeurs optionnelles vides | SDK, patch 05 | Correctif indépendant, utile même sans bridge. |
| Intégration du déchiffrement AEAD v3 | Sujet SDK distinct, non implémenté ici | Blocage protocole connu ; ne pas le masquer par un bump de version. |

`MailFacade`, son constructeur, `FolderSystem`, les modèles générés, les dépendances et la version officielle ne sont pas modifiés par ces cinq patchs.

## Taille et découpage

Ancien fork fonctionnel : 3506 ajouts / 113 suppressions, tests compris. Premier prototype extrait : 1389 / 94. Cette révision : **1305 / 78**, y compris le nouveau correctif optionnel et des tests supplémentaires.

Le gain depuis le premier prototype est donc modeste en nombre de lignes. Le gain principal est la justesse du protocole et la réduction des zones modifiées dans les fichiers centraux. Davantage de petits fichiers est ici un choix d’isolation, pas davantage de fonctionnalités.

| Proposition locale | Ajouts / suppressions | Contenu |
|---|---:|---|
| 01-batch | 202 / 0 | Transport batch, wrapper, tests. |
| 02-blob-reads | 693 / 12 | Tokens, cache générique, lecture, parseur, tests. |
| 03-interactive-session | 347 / 66 | Extraction de logique existante, API 2FA, tests. |
| 04-parse-raw | 14 / 0 | Point d’entrée du sérialiseur. |
| 05-optional-empty | 49 / 0 | Correctif protocole et régressions. |

Le patch blobs reste le plus gros parce qu’il ajoute une capacité réseau complète. Le fragmenter davantage en propositions de cache, tokens et lecture produirait surtout des dépendances entre PR. Il peut néanmoins être relu selon ces trois responsabilités.

## Chances d’acceptation upstream

Sur le plan technique, commencer par le correctif optionnel puis par une API de parsing motivée est plus facile à défendre qu’une grosse modification de MailFacade. Batch, blobs et 2FA doivent être justifiés par un usage souhaité par les mainteneurs, avec leurs tests. La convention de formatage du dépôt est respectée ; pas de nouveaux crates ni de modèles édités à la main. C’est une appréciation technique, pas un accord des mainteneurs sur cette API.

Il existe cependant un obstacle explicite indépendant de la qualité du diff : les réponses sur les PR [10854](https://github.com/tutao/tutanota/pull/10854#issuecomment-4874477285), [10870](https://github.com/tutao/tutanota/pull/10870#issuecomment-4874479814) et [10871](https://github.com/tutao/tutanota/pull/10871#issuecomment-4874479164) refusent les contributions impliquant une assistance LLM et indiquent ne pas souhaiter d’ajouts au SDK Rust, dans la perspective de s’en éloigner.

Cette réécriture a été réalisée avec une assistance IA : elle ne satisfait donc pas la politique exprimée. La relire ou changer son auteur ne change pas sa provenance. Une nouvelle soumission ne se justifierait qu’après clarification/changement de cette position, en restant transparent. Aucune démarche n’a été effectuée. En attendant, le résultat est utile comme petite série locale explicite ; « plus aucun patch » ne peut pas être promis.

## Validation et limites

Voir `VALIDATION.md` et les journaux conservés. Les tests isolés vérifient que chaque proposition s’applique et compile sans les quatre autres. La série complète est également comparée octet par octet au SDK du prototype.

Le test de sujet AEAD v3 échoue encore avec `MacError`. Ce point est distinct des corrections de lecture et d’authentification. Le script de sélection conserve ce test obligatoire : aucun candidat ne doit être promu sur la seule base de l’application des patchs ou d’une compilation réussie.

Les tests ne remplacent pas un essai authentifié avec un compte de test. Pas de validation des téléchargements réels, d’un compte BCrypt, des bindings mobiles, ni de toutes les évolutions du protocole après cette base. L’environnement testé utilise Rust/Cargo 1.95.0 sur macOS ; le MSRV complet n’a pas été testé.
