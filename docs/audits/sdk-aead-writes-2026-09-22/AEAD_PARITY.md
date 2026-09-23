> Note historique du prototype précédent. Pour l’état actuel, voir [STYLE_AND_PARITY.md](STYLE_AND_PARITY.md).

# Correspondance avec le SDK TypeScript

Référence des sources TS : `46270557c251d1a31157d72e0aaf0cc63bb33ecf`, hashes et chemins dans `ts-vector-provenance/ts-sources.json`. Base Rust compilée : `aea5846b93a1412451e885bf99002401c3b087e8` (359.260904.0).

| Sujet | Référence TS | Choix Rust |
| --- | --- | --- |
| Version du ciphertext | `ParsedCiphertext.ts` | Préserver le CBC à longueur paire ; reconnaître les marqueurs AEAD 2/3 sur les formats versionnés. Refuser une version inconnue. |
| Clé de session v3 | `InstanceDecryptor`, `SymmetricKeyDeriver` | Réutiliser `AeadSubKeys::derive_from_session_key`, clé AES-256, type racine `app/id`. |
| Clé de groupe v2 | `ValueDecryptor`, `SymmetricKeyDeriver` | Version portée par le ciphertext, `_ownerGroup` de l'instance, nonce de 32 octets, `KeyLoaderFacade` et `AeadSubKeys::derive_from_group_key`. |
| Données authentifiées | `InstanceDecryptor`, `CryptoMapper` | Domaines `attributeEncSK` / `attributeEncGK` suivis de U+001F et du chemin numérique du champ. Les agrégats incluent association, identifiant d'agrégat et attribut. |
| Cache de sous-clés | `SubKeyCache` | Cache local à l'instance ; une dérivation session, une dérivation par version de clé de groupe. |
| Instance purement v2 | `InstanceDecryptor.canAttemptDecryption` | Pas de clé de session inventée ; les attributs de groupe utilisent leur propre contexte. |
| Ancien format | Déchiffreur CBC existant | Conserver le chemin Rust existant ; aucun repli vers CBC après échec d'authentification AEAD. |

Les quatre vecteurs (v2/v3, champ simple/imbriqué) sont produits par l'exécution des fonctions TS officielles, avec un petit harness qui adapte les imports. Ils sont consommés dans les tests Rust. La provenance inclut aussi le précédent contrôle inverse Rust → TS. Ce n'est pas une exécution de toute l'application TS ni une preuve de parité de toutes les branches du protocole.

Les tests négatifs couvrent notamment un type ou chemin incorrect, un MAC altéré, un agrégat déplacé ou sans identifiant, une mauvaise version de clé de groupe, un nonce invalide et une clé de session AES-128. Les identifiants d'agrégats arrivent notamment comme `ElementValue::String` depuis le sérialiseur réel ; ce cas est traité et testé.

Le client préserve la résolution des bucket keys lorsqu'elles existent, même si les attributs sont tous v2 : cette étape sert aussi à l'identité de l'expéditeur et au cache des clés de pièces jointes. En dehors de ce cas, la résolution d'une clé de session n'est demandée que si des attributs l'exigent. Cette approche minimale ne constitue pas un portage complet des chemins TS de permissions, partage et récupération des clés de session.

Le comportement Rust d'échec de l'entité reste strict. Les conventions TS consistant à conserver certaines erreurs au niveau des champs ne sont pas intégralement portées. Le marqueur CBC réservé 0 ne reçoit pas un nouveau support dans ce prototype. Le format de version de clé est celui accepté par la primitive Rust actuelle ; une extension future de ce format devra aussi être intégrée.

L'écriture AEAD, sa politique de déploiement et ses mécanismes d'authentification d'instance restent hors périmètre. Le garde-fou `_kdfNonce` empêche que l'ancienne écriture CBC réécrive silencieusement ces instances.
