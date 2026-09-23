# SDK : revue et réécriture minimale des patchs

Lire [REVIEW.md](REVIEW.md) pour l’analyse, la justification de chaque extension et les limites d’acceptation upstream. Les cinq propositions techniques sont dans `proposals/`, leurs diffs indépendants dans `patches/`. **Aucune PR soumise.**

Le prototype conserve Rust et les primitives du SDK officiel. Il retire du SDK les extensions propres au bridge, corrige les erreurs de lecture et isole la connexion interactive. Ce dossier contient aussi `bridge.patch`, l’adaptateur local et les fichiers nécessaires pour reconstruire le laboratoire.

Base SDK : `aea5846b93a1412451e885bf99002401c3b087e8` (`359.260904.0`). Base bridge : `b375f1c275b8162ee8f04008144e3dd2c72a10d6`.

## Vérification manuelle d’un patch

Dans une copie jetable du SDK officiel à la révision ci-dessus :

```sh
git apply --check /chemin/vers/patches/02-blob-reads.patch
git apply /chemin/vers/patches/02-blob-reads.patch
CARGO_TARGET_DIR=/chemin/vers/un-target-isole cargo test -p tuta-sdk --lib blobs::
```

Chaque patch est généré contre la même base officielle, sans nécessiter les autres. Pour la série complète, appliquer les cinq fichiers dans l’ordre numérique, puis lancer :

```sh
cargo test -p tuta-sdk --lib --test interactive_session_test
```

Les commandes ciblées réellement exécutées pour chaque patch figurent dans `reports/independent-results.json`. Utiliser un répertoire Cargo target distinct par workspace pour éviter les collisions de cache rencontrées lors de cet audit.

## Reconstruire le bridge et ses critères de promotion

Le sélecteur du prototype précédent est fourni avec les nouveaux patchs et le test d’intégration des sessions ajouté au contrôle SDK. Les caches Cargo sont séparés par candidat et par workspace (bridge/SDK). Il crée ses propres clones ; il ne pousse rien. Prérequis : Python 3.11+, Git, Rust/Cargo, accès réseau et caches Git contenant les bases.

```sh
python3 -m unittest -v test_prototype.py
python3 prototype.py \
  --bridge-source /chemin/vers/tutabridge \
  --sdk-cache /chemin/vers/tutabridge/tuta-repo \
  --output /chemin/vers/un-nouveau-laboratoire \
  --tag tutanota-release-359.260904.0
```

Le résultat attendu pour cette base reste un refus de promotion : le test AEAD v3 échoue. Le correctif des valeurs optionnelles vides passe. Les deux tests de protocole sont exécutés explicitement avec `--ignored` par le sélecteur ; leur annotation ne les rend pas facultatifs pour promouvoir un SDK.

Le sélecteur complet actualisé n’a pas été réexécuté de bout en bout sur le réseau lors de cette revue des patchs. Ses sept tests passent ; compilation, suites Rust et critères protocole ont été exécutés séparément. Voir `VALIDATION.md` et les journaux.
