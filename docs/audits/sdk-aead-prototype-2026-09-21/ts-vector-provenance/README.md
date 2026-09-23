# Investigation AEAD v2/v3 — 21 septembre 2026

## Conclusion

Les primitives AEAD Rust sont compatibles avec celles du TypeScript officiel sur les cas croisés exécutés. Le lecteur d’entités Rust ne les appelle pas : `EntityFacadeImpl::decrypt_and_parse_value` utilise toujours `GenericAesKey::decrypt_data`, donc le chemin AES-CBC historique. Ce chemin n’accepte comme marqueur de MAC que `1`. Avec une clé AES-256 et un ciphertext marqué `3` (ou `2`), il retourne `MacError` parce que le format authentifié historique est absent, **avant de vérifier le tag AEAD**.

Ce résultat n’établit ni une mauvaise clé, ni un message corrompu, ni un défaut de l’algorithme AEAD. Les fichiers concernés du lecteur et des primitives sont identiques entre la release 359 testée et le master officiel observé (`46270557c251d1a31157d72e0aaf0cc63bb33ecf`). Les cinq propositions précédentes ne les modifient pas.

## Ce que signifie v3

AEAD signifie « Authenticated Encryption with Associated Data » : le chiffrement authentifie aussi un contexte, pour empêcher qu’une valeur soit déplacée dans un autre champ sans détection.

Dans ce protocole Tuta :

- v1 : AES-CBC + HMAC ; le lecteur Rust l’utilise déjà.
- v2 : AES-CTR + BLAKE3 ; sous-clés dérivées d’une clé de groupe versionnée et d’un `_kdfNonce`.
- v3 : AES-CTR + BLAKE3 ; sous-clés dérivées de la clé de session de l’entité.

Le « 3 » est un marqueur de format Tuta, pas une version du SDK ni d’AES. Pour le sujet d’un Mail, le contexte de dérivation v3 est `SK instanceSessionKey\x1ftutanota/97` et les données associées sont `attributeEncSK\x1f105`. Le type `97` et l’attribut `105` sont les identifiants du modèle. Pour un champ imbriqué, les associations et IDs d’agrégats s’ajoutent au chemin.

## Preuves exécutées

1. Le test existant du sujet de mail a été relancé : la fixture historique se déchiffre ; le même sujet réencodé en AEAD v3 passe par la primitive Rust, puis échoue dans le lecteur d’entités avec `MacError` (`entity-regression.log`). C’est une **construction synthétique**, pas la capture d’un mail réel reçu en AEAD.
2. Quatre cas supplémentaires : v2/v3, champ simple/chemin imbriqué. Les primitives Rust les déchiffrent ; le point d’entrée historique utilisé par le lecteur les rejette (`rust-probe.log`). Le témoin v1 passe. Les mauvaises données associées et les tags altérés sont rejetés.
3. Les classes TS officielles `InstanceDecryptor`, `ValueDecryptor`, `ParsedCiphertext`, `SymmetricKeyDeriver` et `AeadFacade` ont été exécutées avec les implémentations SJCL et BLAKE3 vendoriées par Tuta : les quatre messages Rust se déchiffrent (`ts-results.json`). Mauvais champ, mauvais type d’entité, mauvaise clé et tag altéré sont rejetés.
4. Quatre messages produits à leur tour par ce TS se déchiffrent en Rust (`rust-cross-check.log`). Les clés sont synthétiques et publiques (`0x11` répété), sans compte utilisateur.

Le harnais TS enlève les imports pour assembler les modules et transforme la syntaxe TypeScript via Node ; les corps cryptographiques ne sont pas réécrits. Des utilitaires, classes d’erreur et un générateur aléatoire Node sont injectés. Il ne s’agit pas d’exécuter toute l’application Tuta. Le test de chemin imbriqué valide le contexte de la primitive ; il ne prétend pas valider une traversée d’agrégats dans le lecteur Rust actuel, qui ne possède pas encore cette intégration.

## Impact réel et rapport avec 474

474 est `InvalidSoftwareVersionError`, une erreur HTTP de version client. `MacError` ici est une erreur locale de déchiffrement. Ce sont deux mécanismes distincts ; nous n’avons pas établi que l’erreur de l’issue 37 implique AEAD.

Le TS conserve AES-CBC comme valeur initiale et active AEAD via le rollout `EncryptionOfAttributesViaAead` (type `5`), chargé depuis `RolloutService` pour l’utilisateur. Une fois ce mode activé, les chemins génériques de création/mise à jour choisissent des sous-clés de groupe, donc v2. **Corriger uniquement le test v3 ne suffirait pas à prendre en charge ce déploiement.**

Le dépôt public montre ce mécanisme ; il ne dit pas à quels comptes il est effectivement activé. Aucune session réelle ni réponse de RolloutService n’a été inspectée. On ne peut donc conclure ni que tous les comptes sont actuellement bloqués, ni qu’aucun ne l’est. Le TS teste aussi explicitement le mode v3.

La précédente formule « candidat non promouvable » désignait le résultat du contrôle strict du prototype, qui impose ce test. Elle ne prouve pas que la lecture actuelle des comptes en ancien format échoue. Ce contrôle doit rester explicite ; il ne faut ni masquer son échec, ni le présenter comme la cause démontrée du 474.

## Ce qu’il faudrait corriger, sans réinventer la crypto

1. Introduire une sélection explicite du format de ciphertext, conforme au parseur TS (y compris les anciennes valeurs sans marqueur), avec des erreurs compréhensibles et des tests des formats invalides. Éviter un fallback de déchiffrement après un échec d’authentification.
2. Brancher la lecture v3 sur l’AeadFacade Rust existante. Transporter un contexte d’entité : type racine, clé de session, sous-clés réutilisables et chemin authentifié du champ. Pour les agrégats, conserver le type racine et construire les chemins comme CryptoMapper ; utiliser le type de chaque agrégat pour dériver les clés serait incorrect.
3. Ajouter séparément v2 : récupérer `_kdfNonce`, la version de clé indiquée par le ciphertext et la bonne clé de groupe. Le lecteur actuel exige généralement une clé de session résolue ; ce prérequis doit être revu pour les entités dont les attributs utilisent directement une clé de groupe. Cela touche aussi la résolution des clés, pas seulement un `if` dans `decrypt_data`.
4. Traiter l’écriture et l’activation du mode dans une proposition distincte, après la lecture et ses tests. Conserver des régressions v1 et des tests croisés TS/Rust, agrégats, versions de clés, nonces absents, ciphertexts altérés et contextes incorrects.

Les deux premières étapes peuvent former des propositions ciblées ; v2 réclame davantage de raccordement. Ce dossier n’implémente pas ces changements dans le SDK et ne change pas le contrôle de promotion. Il contient un diagnostic vérifié et les tests nécessaires pour cadrer la suite. Aucune PR ni publication.

## Reproduire

Sur Node 22.22.1 : `node verify_ts.mjs`. Les sources officielles, hashes et vecteurs sont inclus. Pour les récupérer à nouveau depuis un clone contenant le commit verrouillé : `python3 build_ts_harness.py /chemin/vers/tutanota`.

Copier `aead_diagnostic.rs` dans `crates/tuta/tests/` du prototype SDK/bridge décrit dans `../sdk-ts-parity-2026-09-21/`. Puis, depuis ce prototype :

```sh
AEAD_RUST_VECTORS=/chemin/vers/rust-vectors.json CARGO_INCREMENTAL=0 cargo test -p tutabridge-tuta --features testing --test aead_diagnostic -- --nocapture
```

Exécuter le harnais JS dans ce dossier avec ces vecteurs, puis :

```sh
AEAD_TS_VECTORS=/chemin/vers/ts-vectors.json CARGO_INCREMENTAL=0 cargo test -p tutabridge-tuta --features testing --test aead_diagnostic -- --nocapture
```

Le test synthétique du lecteur s’exécute séparément et doit encore échouer :

```sh
cargo test -p tutabridge-tuta --features testing --test mail_extensions protocol_accepts_valid_aead_v3_mail_subject -- --ignored --nocapture
```

## Sources principales

- [Rust EntityFacade](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/tuta-sdk/rust/sdk/src/entities/entity_facade.rs#L387)
- [Rust AES, reconnaissance du MAC et rejet](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/tuta-sdk/rust/crypto-primitives/src/aes.rs#L542)
- [TS InstanceDecryptor](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/crypto/instance-pipeline-crypto/decryption/InstanceDecryptor.ts#L36)
- [TS CryptoMapper et chemins des champs](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/instance-pipeline/CryptoMapper.ts#L151)
- [Activation par rollout](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/applications/common/api/worker/EventBusEventCoordinator.ts#L161)
- [Écriture en AEAD avec clé de groupe](https://github.com/tutao/tutanota/blob/46270557c251d1a31157d72e0aaf0cc63bb33ecf/src/platform-kit/network/EntityRestClient.ts#L714)
