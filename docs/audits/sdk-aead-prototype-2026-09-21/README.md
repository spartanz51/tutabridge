# Prototype Rust : lectures AEAD et sélection de SDK

État local au 21 septembre 2026. Aucun push, aucune PR, aucune modification du SDK utilisé en production. Le code exécutable a été développé dans un clone temporaire ; ce dossier conserve les patchs, l'adaptateur, les tests et les preuves.

## Résultat

Le test qui échouait sur un sujet AEAD v3 passe désormais. Le prototype lit aussi des attributs AEAD v2 avec une clé de groupe, y compris un mail complet et un corps de brouillon imbriqué sans clé de session du parent. Les primitives cryptographiques viennent du SDK Rust officiel ; aucune nouvelle implémentation d'AES, MAC ou dérivation de clés n'est ajoutée.

Les deux nouveaux patchs sont séparés :

- [06 — lecture avec clé de session, v3](patches/06-aead-session-reads.patch) : sélection du déchiffreur, contexte du type racine et chemin authentifié des champs/agrégats. S'applique seul sur le SDK officiel.
- [07 — lecture avec clé de groupe, v2](patches/07-aead-group-reads.patch) : inspection des clés nécessaires, nonce de l'instance, chargement des versions de clés indiquées dans les attributs et intégration au client d'entités. Dépend de 06, sans dépendre des cinq anciens patchs pour s'appliquer.

Ce sont des propositions techniques à relire, pas des PR prêtes à être promises comme acceptables par Tuta. Le patch 07 est sensiblement plus large que 06 : son intégration implique le chargement des clés et le comportement du client, au-delà du déchiffrement d'un attribut.

Les cinq propositions antérieures sont reprises sans modification. Voir [leur audit TS antérieur](TS_PARITY.md) et [les nouvelles notes de parité](AEAD_PARITY.md).

## Sélecteur

[prototype.py](prototype.py) essaie les tags stables officiels par version décroissante, applique les patchs et exige compilation, tests bridge, tests SDK, smoke réseau public et tests de protocole. Il ne réécrit pas la version du SDK.

La comparaison repose sur une référence documentée et des journaux hachés : l'ancien prototype 359 à cinq patchs, **pas un compte réel ni la production**. Elle distingue :

- une régression d'une capacité précédemment validée : bloque ;
- une limite connue qui persiste : visible dans le résultat ;
- une capacité ajoutée ou nouvellement vérifiée : signalée ;
- un test absent, une compilation ratée ou une erreur non reconnue : bloque.

`--require-capability` permet d'exiger explicitement v2 et v3. Même après réussite, le résultat est un `candidate.lock.json` **pour revue**, sans publication automatique. Le prototype ne décide pas à lui seul d'un déploiement.

## Validation et limites

Voir [VALIDATION.md](VALIDATION.md) et les journaux dans [reports](reports).

- 750 tests Rust distincts passent en incluant explicitement les deux tests de protocole habituellement ignorés. Un autre test SDK préexistant reste ignoré.
- 12 tests Python du sélecteur passent.
- La série complète s'applique sans conflit au tag 359 et au commit `master` audité ; seule la version 359 fait l'objet de la compilation et des suites complètes de ce prototype.
- Les nouveaux fichiers respectent la configuration rustfmt amont, avec l'édition Cargo 2021. Clippy termine avec des avertissements ; ils restent visibles dans le journal.

**Portée : lecture AEAD uniquement.** L'écriture AEAD n'est pas implémentée. Les instances portant `_kdfNonce` sont explicitement refusées par l'ancien chemin d'écriture CBC, y compris `update_instance`, pour éviter une réécriture incohérente. Ce prototype ne démontre donc pas que toutes les opérations IMAP fonctionnent sur des messages AEAD.

Les tests utilisent des fixtures, des transports simulés et les fonctions cryptographiques TypeScript officielles pour les vecteurs. Aucun compte Tuta authentifié n'a été utilisé. Le smoke public vérifie l'acceptation de la version et des réponses attendues, pas la lecture d'une vraie boîte mail. Les variantes de permissions/partage et le chiffrement des octets de pièces jointes ne sont pas étendus par ces deux patchs.

## Rejouer

Python 3.11+, Git et la toolchain Rust compatible avec le SDK sont nécessaires. Donner deux dépôts Git locaux existants ; le script les utilise comme sources et crée ses propres clones. Le dossier de sortie doit être nouveau.

```sh
python3 prototype.py \
  --bridge-source /chemin/vers/tutabridge \
  --sdk-cache /chemin/vers/tutanota \
  --output /tmp/tutabridge-aead-run \
  --tag tutanota-release-359.260904.0 \
  --require-capability aead_v2 \
  --require-capability aead_v3
```

Retirer `--tag` permet d'essayer jusqu'aux trois derniers tags éligibles (`--limit` règle cette limite). Le repli entre versions est testé unitairement ; ce dossier ne revendique pas un essai réseau complet de plusieurs tags.

Le snapshot sous `ts-vector-provenance/` conserve l'investigation précédente et ses sources ; ses anciens journaux d'échec sont une preuve historique, pas l'état de ce prototype. Les résultats actuels sont dans `reports/`.
