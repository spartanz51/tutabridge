# Revue du style et de la parité Tuta

Sources TS auditées au commit `46270557c251d1a31157d72e0aaf0cc63bb33ecf`. Base Rust compilée : `aea5846b93a1412451e885bf99002401c3b087e8`, version officielle 359.260904.0. Les chemins et SHA-256 des sources sont dans `ts-reference/ts-sources.json`.

## Conventions vérifiées

| Convention | Application |
| --- | --- |
| Configuration de formatage du dépôt | rustfmt avec `rustfmt.toml` amont et édition Cargo 2021 ; indentation par tabulations. |
| Nommage Rust | Types en PascalCase, méthodes/champs propres au portage en snake_case. Les champs camelCase appartiennent aux modèles générés et ne sont pas renommés. |
| Dépendances injectées | Le client, le service executor et la source aléatoire sont injectés dans `AeadEntityWriter`, comme les façades existantes. |
| Services et modèles existants | `UpdateKdfNonceService`, `UpdateKdfNoncePostIn`, `InstanceKdfNonce`, `TypeInfo` et `ExtraServiceParams` sont réutilisés. |
| Pipeline de données | Réutilisation de `InstanceMapper`, `EntityFacade`, `JsonSerializer`, `EntityClient` et des conversions de valeurs déjà testées. |
| Cryptographie | Utilisation de `AeadFacade::encrypt` et des dérivations officielles ; aucune réimplémentation AES/MAC/KDF. |
| Gestion des erreurs | Retours `Result<_, ApiCallError>` ; les échecs d'enregistrement du nonce empêchent le PUT. |
| Tests | Tests unitaires proches du module et tests d'intégration utilisant les fixtures de connexion amont et un transport en mémoire. |

`doc/HACKING.md` recommande notamment l'injection des interfaces réseau et décrit les responsabilités des façades. Il ne fournit pas un guide Rust exhaustif : l'adaptation aux conventions Rust est aussi fondée sur les modules voisins et les lints du workspace. Cela ne constitue pas une garantie d'acceptation par les mainteneurs.

## Correspondance fonctionnelle

| Comportement TS | Portage Rust |
| --- | --- |
| `CryptoMapper.encryptParsedInstance` et `encryptValue` | Traversée commune à CBC/AEAD ; type racine, chemin de champ, compression/conversion existante. |
| `SubKeyProvider` | Une dérivation par instance, réutilisée pour ses champs et agrégats. |
| `EntityRestClient.getSubKeyInfoOnSetup` | Clé du propriétaire ; génération d'un nonce neuf pour une création. |
| `EntityRestClient.getSubKeyInfoOnUpdate` | Clé actuelle ou explicitement résolue ; conservation du nonce existant ou enregistrement serveur. |
| Réponse de `postUpdateKdfNonceService` | Le nonce renvoyé est utilisé même s'il diffère du nonce proposé, afin de respecter une attribution concurrente. |
| `ServiceExecutor` → `InstancePipeline.mapAndEncrypt` | Le CBC des services est conservé. Aucun changement dans l'envoi de brouillons du bridge. |

Le choix de l'API Rust `AeadEntityWriter` est une adaptation de structure, pas une copie de la classe TS. Il évite d'ajouter un état de rollout fictif au SDK. Son intégration à la future politique de chiffrement du SDK reste un sujet de revue d'API.

## Preuves croisées

Le harness `ts-reference/verify_writes.mjs` exécute le code amont `CryptoMapper.encryptParsedInstance`, `encryptValue`, la traversée des agrégats, `SubKeyProvider`, les primitives AEAD et `InstanceDecryptor`. Il fournit de petits adaptateurs de valeurs parsées, modèles, utilitaires et aléatoire ; il ne démarre pas l'application entière.

Il produit huit cas de sortie v2/v3 (texte Unicode, chaîne vide, champ imbriqué). Le test Rust du mapper compare les octets produits avec ces sorties en fixant la même source aléatoire. Un autre test compare les quatre vecteurs cryptographiques de l'audit précédent, avec leurs nonces exacts.

## Écart volontaire : agrégats frères

Dans `CryptoMapper.encryptAggregateAssociation`, le TS audité réaffecte `fieldPathPrefix` à chaque itération. Avec les identifiants `first`, `second`, il écrit :

- premier champ : `111/first/94` ;
- deuxième champ : `111/first/second/94`.

Le lecteur `decryptAggregateAssociation` construit indépendamment `111/second/94`. Le harness reproduit l'échec d'authentification sur ce chemin et le succès avec le chemin accumulé. Ce constat porte sur les fonctions exécutées ; il ne prouve pas un incident déployé chez les utilisateurs.

Le Rust écrit `111/first/94` puis `111/second/94`. Les tests vérifient ces chemins, leur déchiffrement et le rejet du chemin accumulé. Aucun repli permissif ni tentative sur plusieurs contextes n'est ajouté au lecteur.

## Limites de parité

- Le rollout par utilisateur n'est pas automatiquement activé. L'écriture AEAD de nouvelles entités est explicitement sélectionnée.
- La résolution des contextes de partage reste une responsabilité de l'appelant lorsqu'il fournit la clé versionnée.
- La validation Rust des valeurs requises reste stricte. Tous les mécanismes d'erreur par champ du TS ne sont pas portés.
- Les contraintes existantes de création et d'identifiants du SDK ne sont pas élargies.
- Ces patchs ne portent pas l'ensemble de l'authentification des instances ni le chiffrement des octets de pièces jointes.
