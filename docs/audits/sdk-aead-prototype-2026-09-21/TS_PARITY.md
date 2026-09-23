# Audit de conformité au client TypeScript officiel

La comparaison a été menée sur les cinq propositions Rust, puis sur leurs dépendances directes de protocole : formats des requêtes, tokens, retries, parsing, ouverture de session et second facteur. Elle a trouvé et corrigé des écarts supplémentaires dans le prototype précédent. Une compilation et des mocks cohérents avec le code Rust ne suffisaient pas à démontrer la conformité au client officiel.

## Références verrouillées

- SDK Rust testé : release `359.260904.0`, commit `aea5846b93a1412451e885bf99002401c3b087e8`.
- TypeScript : même release et `master` officiel observé le 21 septembre 2026, commit `46270557c251d1a31157d72e0aaf0cc63bb33ecf`.
- Les **16 corps de méthodes/fonctions** recensés dans `evidence/source-audit.json` sont identiques entre ces deux références. Les changements de `master` dans EntityRestClient concernent notamment les clés des mises à jour, en dehors des méthodes de lecture auditées.
- Les sources sont récupérées par Git depuis `tutao/tutanota`. Aucun numéro de version ou modèle généré n’est réécrit.

## Matrice de conformité

| Proposition | Référence TypeScript | Résultat |
|---|---|---|
| Batch | `EntityRestClient.loadMultipleParsedInstances` et `loadMultiple` | Chemin du modèle en minuscules, lots de 100, paramètre `ids`, réponse potentiellement incomplète, ordre retourné conservé, pipeline de parsing/déchiffrement existant. Périmètre Rust volontairement limité aux ListElement. |
| Tokens de lecture | `BlobAccessTokenFacade.requestReadTokenBlobs`, `requestReadTokenArchive`, `createQueryParams` | Même distinction archive possédée / fichier référent ; même contenu des requêtes et auth dans les paramètres des requêtes blobs. Cache Rust existant réutilisé avec clés typées. |
| Transport blobs | `BlobFacade.downloadBlobsOfOneArchive`, `RestClient.request` | **Corrigé : GET avec JSON dans `_body`, sans corps HTTP ; HTTP 200 uniquement.** Lots de 100 conservés. |
| Reprise des blobs | `tryServers`, `doBlobRequestWithRetry` | **Corrigé : une seule reprise de la lecture entière d’une archive**, incluant le chargement du token ; éviction sur 403. Failover sur connexion/500/404 ; refus immédiat des autres erreurs. |
| Format binaire | `parseMultipleBlobsResponse` | Compteurs et tailles signés i32 big endian, ID 9 octets, hash 6 octets, taille 4 octets. Contrôles de limites avant copie ; deux réponses malformées sont intentionnellement rejetées plus strictement. |
| Session interactive | `LoginFacade.createSession`, `loadUserPassphraseKey`, identifiants de session | Même requête persistante Argon2 et primitives Rust officielles. **Corrigé : identifiant du client fourni par l’appelant**, comme en TS. Le wrapper `create_session` garde son comportement historique sur ce nom. |
| TOTP / polling | `SecondFactorAuthDialog.onConfirmOtp`, `LoginFacade.authenticateWithSecondFactor`, `waitUntilSecondFactorApproved` | Même service, type TOTP, ID de session et donnée de polling. L’API Rust expose une requête de statut, pas la boucle/UI TS complète. |
| Entrée `parse_raw` | `TypeMapper.parseServerJson`, appelée dans `EntityRestClient._handleLoadResult` | Réutilisation du JsonSerializer Rust existant. Aucun nouveau format ou pipeline de sérialisation. |
| Valeurs optionnelles vides | `CryptoMapper.decryptValue` | Même règle : valeur chiffrée optionnelle vide → null. Le correctif n’affecte pas les valeurs obligatoires ni les valeurs chiffrées présentes. |

Sources officielles : [EntityRestClient](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/network/EntityRestClient.ts), [BlobAccessTokenFacade](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/network/BlobAccessTokenFacade.ts), [BlobFacade](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/applications/common/api/worker/facades/lazy/BlobFacade.ts), [RestClient](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/rest-client/RestClient.ts), [LoginFacade](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/base/facades/LoginFacade.ts), [CryptoMapper](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/instance-pipeline/CryptoMapper.ts).

## Les écarts corrigés

### JSON dans le GET

La façade TS passe un `RestTextBody` à son transport, mais le transport convertit ce contenu en paramètre `_body` avant la requête HTTP. Le NativeRestClient Rust ne réalise pas cette conversion. Notre ancienne lecture directe envoyait donc un corps HTTP alors que le TS envoyait un paramètre de requête.

La nouvelle lecture sérialise `BlobGetIn` avec InstanceMapper/JsonSerializer, puis place le JSON dans `_body` via l’encodeur de paramètres existant. Les tests vérifient explicitement `body == None`, les paramètres décodés, `v`, `cv`, le token d’accès et le blobAccessToken. Le test d’intégration des pièces jointes vérifie aussi les vrais champs numériques de la demande de token (archive, liste, fichier, type Attachments, absence de grant d’écriture et IDs d’agrégats).

### Une seule reprise

Le prototype renouvelait le token séparément pour chaque lot, ce qui pouvait donner plusieurs renouvellements sur une grande lecture. Le helper TS encadre toute l’opération. La nouvelle fonction `with_read_token` reprend toute la lecture de l’archive une fois, même si le 403 vient du service de token. La fonction de failover reste distincte, comme `tryServers` en TS.

Le test de 101 blobs impose la séquence 100/1/100/1 lorsqu’un 403 arrive au second lot. Le second 403 termine l’opération. Aucun retry n’est ajouté sur 401 ou 474.

### Statut HTTP et représentation du format

Le client TS accepte 200 pour un GET, et réserve 201 à POST. Les lectures blobs Rust rejettent maintenant 201/204/206 même si la réponse contient un corps. Les champs binaires sont explicitement lus comme des i32 signés, conformément au `DataView.getInt32` TS ; les valeurs négatives sont rejetées avant conversion en taille Rust.

### Identité du client

`initiate_session` reçoit désormais `client_identifier`, conformément au principe TS. Le bridge transmet `TutaBridge`. Le wrapper Rust préexistant `create_session` continue de transmettre `Linux Desktop`, afin de ne pas changer silencieusement son API. Le nom fourni, l’adresse normalisée, la longueur de la clé d’accès, le TOTP et les identifiants exacts de session sont vérifiés dans les tests de service.

## Preuve par exécution du TypeScript

`harness/generate_parity.mjs` extrait depuis Git et exécute les corps officiels de `parseMultipleBlobsResponse`, `tryServers`, `doBlobRequestWithRetry`, `ofClass`, `getSessionListId` et `getSessionElementId`. Node efface les types TypeScript. Le code des fonctions n’est pas réécrit en JavaScript.

Le harnais injecte les dépendances de test : classes d’erreurs correspondant aux statuts, conversions base64 et SHA-256 Node. Ce n’est pas l’exécution de l’application Tuta entière, ni de toute sa suite TS. Les hashes des fichiers sources, la référence et tous les résultats figurent dans `evidence/parity-vectors.json`.

- 11 cas binaires : entrées valides, zéro, troncatures, compte négatif/immense/incohérent, taille négative et deux réponses atypiques.
- 8 séquences d’erreurs : succès après 404/erreur réseau, arrêt sur 401/474, renouvellement après 403, échec après deux 403, dernier serveur en erreur et erreur finale après failover.
- Un vecteur d’identifiants de session calculé par le code TS, vérifié intégralement dans la requête TOTP Rust.

Les 19 cas de blobs sont consommés par les tests Rust via la fixture versionnée `tests/fixtures/blob_read_ts.json`. Les deux différences de parsing sont explicites dans `rust_accept`, pas masquées par une modification des résultats TS.

## Différences conservées, et pourquoi

1. **Surface API restreinte.** Le batch Rust ne gère que les ListElement avec GeneratedId. TS couvre également d’autres entités, les clés fournies par l’appelant et des options de transport. Le patch ne prétend pas porter toute cette API.
2. **Ordonnancement.** Le TS parallélise certains lots ; Rust les traite séquentiellement et s’arrête sur la première erreur. Même contenu demandé, mais pas le même profil de débit ni de requêtes déjà parties après une erreur.
3. **Contrôle des chemins.** La nouvelle méthode batch et la lecture des blobs utilisent le nom du modèle en minuscules, comme `EntityUtils.typeModelToRestPath`. Le test batch vérifie explicitement cette casse. Les méthodes Rust préexistantes restent inchangées.
4. **Cache plus conservateur.** Le TS peut mutualiser un token d’archive reçu après une demande pour un fichier. Rust le garde sous la portée complète de la demande. Cela peut provoquer davantage de demandes de tokens ; aucune permission plus large n’est supposée. L’expiration suit le cache Rust existant, avec la même comparaison stricte `expires > now` que le TS.
5. **Granularité de l’opération.** La nouvelle API Rust télécharge une archive par appel. La reprise est donc bornée à cet appel ; le TS peut englober un fichier réparti sur plusieurs archives. L’adaptateur garde les archives déjà lues et reconstitue le fichier dans son ordre initial.
6. **Réponses invalides.** Le TS accepte un compteur zéro suivi d’octets supplémentaires, et certains IDs dupliqués masqués par Map. Rust rejette ces deux formes. Il rejette aussi les blobs inattendus et manquants au niveau transport. Ces choix sont documentés comme validation supplémentaire, pas comme stricte identité du parseur TS.
7. **Session persistante Argon2.** Le SDK Rust ne sait pas dériver BCrypt ; le patch refuse ce cas avant POST. Il n’ajoute ni migration de KDF, ni session éphémère, ni UI, ni annulation de challenge, ni prise en charge U2F/WebAuthn complète. Le TOTP utilise le type numérique du modèle Rust généré ; un code avec des zéros initiaux est représenté par sa valeur numérique.
8. **Polling, délais et annulation.** Le TS possède sa boucle de challenge, des retries réseau, des options de suspension, du progress et des contrôleurs d’annulation. Ces aspects ne sont pas réimplémentés dans les extensions SDK. Ils restent à l’appelant/au transport Rust existant. Aucun comportement absent n’est annoncé comme acquis.
9. **Erreur sans serveur.** Le TS finit par jeter null sur une liste vide ; Rust retourne une erreur SDK structurée. Cela respecte la convention d’erreur Rust.
10. **Pipeline cryptographique existant.** `parse_raw` et le correctif optionnel ne réécrivent pas CryptoMapper. Le déchiffrement AEAD v3 des entités reste un blocage du SDK Rust, explicitement conservé dans le contrôle de promotion.

## Conventions et absence de réinvention

Le style Rust est celui de `rustfmt.toml` et des lints workspace Tuta : édition du crate 2021, tabulations, largeur 100, noms snake_case, ApiCallError/LoginError, types/services générés, Arc et signatures async. Les signatures exposées de session suivent UniFFI. Les constructeurs et fichiers générés restent inchangés.

Les conventions TypeScript portent ici sur le contrat et la répartition des responsabilités, pas sur l’imitation syntaxique de TypeScript en Rust. Le téléchargement lit ; le déchiffrement utilise les primitives existantes ; l’adaptateur assemble les données propres au bridge. Le cache est réutilisé. Aucune nouvelle bibliothèque de crypto, de HTTP ou de sérialisation n’est ajoutée.

Le fichier `VALIDATION.md` précise les contrôles effectués et leurs limites. Les patchs sont localement préparés pour revue, pas déclarés acceptés upstream.

## Proposition de présentation aux mainteneurs

Pour chaque proposition : décrire une lacune précise, citer la méthode TS équivalente, fournir le scénario avant/après, les tests ciblés et les différences de périmètre ci-dessus. Commencer par `05-optional-empty`, puis discuter les extensions d’API séparément. La lecture des blobs est le plus gros changement ; la majorité de son volume est constituée de tests et fixtures.

L’argument solide est que le patch réutilise leurs mécanismes et porte un contrat déjà présent en TS. « Cela ne coûte rien » est inexact : revue et maintenance leur reviennent encore. La conformité technique ne supprime pas leur politique explicite sur les contributions assistées par IA ; cette assistance doit rester transparente.

Un usage externe supplémentaire est vérifié dans [Cargo.toml de caldir-provider-tuta](https://github.com/t4t5/caldir-provider-tuta/blob/ea3824a0c26b310cc6a055c718aabbf60cc358e7/Cargo.toml) : le projet dépend de son SDK Tuta Rust vendorié. Avec TutaBridge, cela donne des exemples concrets. La recherche publique réalisée ne permet pas d’affirmer « beaucoup de projets ». Les copies du dépôt tutanota ne sont pas comptées comme autant de consommateurs indépendants.

Aucune PR, aucun push et aucun message à Tuta n’ont été effectués.
