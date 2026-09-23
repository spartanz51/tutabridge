# Reprise du prototype Rust après comparaison à `crypto/dev`

Cette étape adapte les chemins d’agrégats du prototype et précise sa frontière de compatibilité. Elle ne porte pas encore le nouveau protocole de `crypto/dev` et ne modifie pas le SDK utilisé par TutaBridge en production.

## Références figées

- Base Rust compilée : `aea5846b93a1412451e885bf99002401c3b087e8` (359).
- Point de départ : les neuf patchs de `../sdk-aead-writes-2026-09-22`, arbre `c72f2f8f6ab02df7bf69379546b9a7a734df246a`.
- Référence examinée : [`crypto/dev` au commit e1ad5f9](https://github.com/tutao/tutanota/tree/e1ad5f9351946e3c9f96558adcbc154c384eb489).
- Les fichiers upstream sont conservés sans modification dans `reference/`, avec leurs SHA-256 dans `reference.json`. Le profil du prototype reste `359-360-pre-instance-key` ; le nom décrit les sources de référence, pas une validation sur serveur.

## Deux changements séparés

**10 — Chemins AEAD immuables.** `InstancePath` centralise la construction des chemins dans le lecteur et le writer Rust. Ajouter un agrégat produit un nouveau chemin ; le parent reste réutilisable pour le frère suivant. Même principe que leur [`EncryptionContextPath.ts`](https://github.com/tutao/tutanota/blob/e1ad5f9351946e3c9f96558adcbc154c384eb489/src/platform-kit/instance-pipeline/EncryptionContextPath.ts), avec une représentation Rust minimale : un type privé, deux méthodes et les `AttributeId` existants. Pas de nouvelle dépendance ni de hiérarchie de classes copiée du TS. Sept chemins de référence couvrent racine, frères et deux niveaux d’agrégats.

**11 — Frontière de compatibilité.** Un test utilise huit ciphertexts produits par leur TypeScript : trois cas v3 ordinaires doivent être identiques octet par octet et lisibles ; les quatre cas de la nouvelle v2 et le cas v3 de brouillon doivent échouer à l’authentification avec le profil actuel. Ce rejet documente une incompatibilité, il ne la résout pas. Ces tests sont destinés au prototype ; ils ne constituent pas une proposition upstream de conserver leur ancien protocole.

## Ce qui change chez Tuta

| Sujet | Résultat de la comparaison | Conséquence pour le prototype |
| --- | --- | --- |
| Chemins ordinaires | Construction immuable ; mêmes chaînes dans les sept vecteurs | Refactorisation commune lecteur/writer intégrée |
| AEAD v3 ordinaire | Dérivation et ciphertexts identiques dans les trois vecteurs comparés | Comportement conservé |
| AEAD v2 | Même octet `2`, mais passage par une clé d’instance et changement du domaine authentifié | Ne pas identifier le protocole uniquement par son numéro |
| Brouillons | Type `1290` ramené à `1298`, préfixe `1297/` ramené à `1305/` pour les contextes cryptographiques | Incompatible dans les vecteurs v2 et v3 ; port à faire avec le métamodèle correspondant |
| Attributs transférés | `transferredAttributeId`, types cibles et troncature du chemin | Cas TS vérifié ; pas implémenté dans ce prototype Rust |
| Services | `InstancePipeline` choisit le schéma selon l’utilisateur ; `ServiceExecutor` lui fournit aussi la clé propriétaire | Aucun basculement automatique des services CBC |

L’ancienne v2 dérive directement depuis clé de groupe + nonce avec `GK and nonce instanceMessageKey\x1f` et le type. La nouvelle dérive d’abord une clé d’instance avec `GK and nonce instanceKey\x1f`, puis les sous-clés avec `IK instanceInstanceKey\x1f` et le type. Le domaine des attributs passe de `attributeEncGK\x1f` à `attributeEncIK\x1f`. Voir [`SymmetricKeyDeriver.ts`](https://github.com/tutao/tutanota/blob/e1ad5f9351946e3c9f96558adcbc154c384eb489/src/platform-kit/crypto/encryption/symmetric/SymmetricKeyDeriver.ts) et [`ValueAssociatedData.ts`](https://github.com/tutao/tutanota/blob/e1ad5f9351946e3c9f96558adcbc154c384eb489/src/platform-kit/instance-pipeline/ValueAssociatedData.ts).

Les primitives correspondantes existent **déjà dans leur Rust** : [`derive_instance_key` et dérivations AEAD](https://github.com/tutao/tutanota/blob/e1ad5f9351946e3c9f96558adcbc154c384eb489/tuta-sdk/rust/crypto-primitives/src/aead_facade.rs). Le futur port devra les réutiliser, puis adapter le mapper, les contextes des brouillons et les services. Il ne faut pas réécrire ces primitives ni tenter plusieurs contextes à l’aveugle lors d’un échec d’authentification.

## Vérification reproductible

Avec Node 24, depuis ce dossier :

```sh
node harness/verify_crypto_dev.mjs
python3 harness/verify_patches.py /chemin/vers/un/clone/tutanota
```

Le harness exécute les sources TS figées de chemins, canonicalisation, dérivation et AEAD. Il fournit les dépendances utilitaires et un aléa déterministe. Ce n’est pas l’exécution de toute leur suite TS ou de leur application. `fixtures/359-360-mapper-vectors.json` est une copie inchangée des vecteurs TS de l’audit précédent ; les deux autres fixtures sont régénérées par ce harness. Les bibliothèques JS locales sont reprises du harness précédent (`sjcl` et `noble-hashes` 2.0.1).

Le script de replay applique les neuf patchs précédents puis les deux nouveaux dans un index Git temporaire et compare les arbres attendus. Il ne touche pas au checkout. Le dossier précédent doit être disponible à côté de celui-ci. Pour compiler, appliquer la même série dans un checkout isolé de la base 359, puis exécuter depuis sa racine :

```sh
CARGO_INCREMENTAL=0 cargo test -p tuta-sdk --lib --test aead_writes --test interactive_session_test
```

**Résultat : 343 tests Rust réussis, zéro échec, un ignoré** (336 unitaires SDK, 4 écritures AEAD, 3 sessions interactives). Le harness TS valide 7 chemins et 8 vecteurs cryptographiques. Le replay des 11 patchs, `rustfmt --check` et `git diff --check` passent. Clippy termine avec succès : 23 avertissements de bibliothèque identiques au prototype précédent ; 61 pour la cible de tests, dont 21 doublons, dans le code préexistant. Aucun diagnostic dans les nouvelles lignes. Les rapports de cette exécution sont dans `reports/`. Aucun test réseau d’écriture ni essai avec un compte Tuta n’a été effectué pour cette étape. Le rollout effectivement actif sur le serveur n’est pas déduit de cette branche de développement.

## Conséquence pour les futures mises à jour

Un candidat qui compile et passe un smoke test réseau n’est pas pour autant compatible avec un nouveau protocole cryptographique. Le sélecteur doit conserver la dernière version validée tant que le profil des primitives, du métamodèle et des services n’est pas couvert par des vecteurs TS et des tests Rust correspondants. Le champ de profil ajouté au manifeste de cet audit documente cette contrainte ; il n’est pas encore un nouveau mécanisme du sélecteur automatique.

Pour préparer une PR Rust ciblée, intégrer la gestion des chemins directement au patch du mapper concerné, plutôt que soumettre une refactorisation dépendant de tout notre prototype. Le port des clés d’instance et celui des agrégats transférés sont des étapes distinctes. Aucun de ces changements n’a été poussé ni proposé en PR.
