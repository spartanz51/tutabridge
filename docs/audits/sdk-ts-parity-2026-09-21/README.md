# Patchs Rust : conformité TypeScript

Lire [TS_PARITY.md](TS_PARITY.md) pour la matrice des cinq propositions, les corrections effectuées et les différences restantes. [VALIDATION.md](VALIDATION.md) recense les tests. Aucune PR soumise.

Cette version remplace le prototype de `sdk-patch-review-2026-09-21` pour les téléchargements et la signature de connexion. Les anciens fichiers restent conservés pour comparaison.

Les patchs indépendants sont dans `patches/`, les descriptions locales dans `proposals/`, les sources touchées dans `sdk-source/` et l’intégration bridge dans `bridge.patch` et `adapter/`.

## Reproduire les références TS

Avec Node 22.13+ (support de `stripTypeScriptTypes`) et une copie Git contenant le commit officiel `46270557c251d1a31157d72e0aaf0cc63bb33ecf` :

```sh
node harness/generate_parity.mjs /chemin/vers/tutanota /tmp/parity-vectors.json
```

Le harnais utilise les méthodes officielles, avec dépendances de test explicites. Les hashes et résultats conservés sont dans `evidence/`. La fixture Rust compacte reprend uniquement les cas de blobs.

## Reproduire les patchs

Dans une copie jetable à `aea5846b93a1412451e885bf99002401c3b087e8`, chaque patch peut être appliqué seul. Pour tous les appliquer :

```sh
for patch in /chemin/vers/patches/*.patch; do git apply --check "$patch" && git apply "$patch" || exit 1; done
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/chemin/vers/target-sdk cargo test -p tuta-sdk --lib --test interactive_session_test
```

Le sélecteur précédent est fourni avec ce nouveau jeu de patchs et un manifeste de hashes actualisé. Il conserve le test AEAD bloquant ; aucun candidat ne doit être promu malgré cet échec. Il n’a pas été relancé de bout en bout sur le réseau durant cette revue. Commande de laboratoire :

```sh
python3 prototype.py --bridge-source /chemin/vers/tutabridge --sdk-cache /chemin/vers/tutabridge/tuta-repo --output /chemin/vers/un-nouveau-laboratoire --tag tutanota-release-359.260904.0
```
