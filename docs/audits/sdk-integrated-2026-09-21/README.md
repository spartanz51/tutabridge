# Prototype intégré : SDK officiel + extensions locales

Le prototype est un laboratoire local. Il construit un bridge réel à partir du
commit `b375f1c275b8162ee8f04008144e3dd2c72a10d6`, sans changer le checkout principal.
Il ne pousse rien et ne publie aucune release.

## Architecture exécutée

- `crates/tuta` (`tutabridge-tuta`) : WebSocket/heartbeat, identifiants MailSetEntry,
  dossiers, MOVE, lecture des corps/brouillons/pièces jointes, déchiffrement inline.
- SDK officiel : quatre patches versionnés et contrôlés par SHA-256.
  1. Chargement batch : transport + wrapper de déchiffrement existants.
  2. Transport blobs : cache/tokens de lecture, failover/retry, téléchargement,
     décodage binaire. Inclut la correction de `_id` des BlobId.
  3. Sessions 2FA : méthodes regroupées dans `session_ext.rs`, connexion au Sdk
     existant et parse_session_id accessible à la crate.
  4. `EntityClient.parse_raw` : entrée ciblée dans le parseur SDK existant.
- `MailFacade`, son constructeur et `FolderSystem` upstream restent identiques
  au SDK officiel. Aucun modèle ni numéro de version n'est falsifié.

La surface ajoutée dans le SDK passe de 3506 à 1389 lignes, tests compris, dans
9 fichiers au lieu de 14. Une partie du code est déplacée, pas supprimée.
Les méthodes `load_encrypted`, `decrypt_with_owner_key` et le déchiffrement inline
ne nécessitent plus de patch CryptoEntityClient dédié. L'adaptateur utilise les
API publiques de clés, d'EntityFacade et d'InstanceMapper.

## Résultats du laboratoire

Sur `tutanota-release-359.260904.0` / `aea5846b93a1412451e885bf99002401c3b087e8` :

- CLI compilée.
- 362 tests core, 43 tests des modules extraits et 7 tests d'intégration réussis.
- 316 tests SDK réussis ; 1 test SDK ignoré préexistant.
- Smoke public sans identifiants : HTTP 200, cv réel 359.260904.0 et modèle base 2.
- Deux tests d'acceptation protocole échouent : sujet AEAD v3 (`MacError`) et texte
  optionnel vide dans un corps de brouillon (`InvalidDataSizeError`).

Les tests d'intégration utilisent des fixtures publiques upstream et un transport
entièrement en mémoire. Ils vérifient le vrai format sérialisé MOVE, le découpage
50/50/1, l'absence de requête pour une liste vide, l'arrêt sur erreur, le décodage
inline, et un brouillon chiffré synthétique dont seule la clé du mail parent est
utilisable. La corruption de cette clé doit échouer. Aucun compte réel utilisé.

Les deux tests protocole sont annotés `ignore` pour séparer les diagnostics de la
migration des critères de promotion. **Le sélecteur les exécute obligatoirement**
avec `--ignored`, contrôle que les deux sont exécutés, et rejette le candidat si
l'un échoue. Un `cargo test` ordinaire vert ne suffit donc jamais à le sélectionner.
Les primitives AEAD ont un contrôle positif ; le format v3 utilisé correspond au
champ Mail.subject (contexte tutanota/97, données associées attributeEncSK + 0x1f + 105).

## Rejouer la sélection

Prérequis : Git, Rust/Cargo, Python 3.11+ avec certificats TLS fonctionnels et accès
réseau. Les deux caches Git locaux sont lus sans modifier leur checkout.

```sh
python3 -m unittest -v test_prototype.py
python3 prototype.py \
  --bridge-source /chemin/vers/tutabridge \
  --sdk-cache /chemin/vers/tutabridge/tuta-repo \
  --output /chemin/vers/un-nouveau-dossier \
  --tag tutanota-release-359.260904.0
```

Sans `--tag`, le script découvre les tags stables officiels, trie numériquement,
écarte les versions inférieures à 359.260904.0 et tente les trois plus récentes.
`--limit` change cette limite ; `--target-dir` permet de réutiliser un cache Cargo.
Un clone neuf est créé par candidat. Aucun fallback sous le minimum n'est autorisé.
Le tri/fallback et les refus de promotion partielle sont couverts par 7 tests Python.
Le fallback à un candidat fonctionnel est testé par simulation, pas sur une deuxième
release réelle compatible.

Les étapes sont application stricte des patches (pas de résolution automatique),
build CLI, tests bridge/extensions, tests SDK, smoke réseau et acceptation protocole.
La première version qui satisfait tous ces critères produit `candidate.lock.json`
pour revue, avec SHA SDK, SHA bridge et empreintes. Le rapport est toujours écrit
après les tentatives ; aucune sélection produit un code de sortie 1.
Le SDK doit conserver la version de son tag officiel et les interfaces protégées.

La reproduction sur 359 doit se terminer en `protocol_failed`, avec `selected: null`.
Ce résultat démontre le refus de promotion d'un SDK compilable mais incompatible.
Le script ne traite pas encore les incidents d'infrastructure comme une stratégie
CI complète (reprise, rétention, notifications, support multi-plateforme).

## Limites et suite utile

Le build GUI, les connexions WebSocket réelles, les comptes avec 2FA et les échanges
réels de mails/pièces jointes n'ont pas été validés. Le prototype conserve les
sémantiques existantes (notamment limite de 100 dossiers et déduplication adjacente
MOVE) pour éviter de mélanger migration et changements fonctionnels.

Les blobs restent dans le SDK parce qu'ils partagent tokens, cache, transport et
mapping avec l'upload officiel. Sortir cette couche intégralement dupliquerait ces
mécanismes ; ce n'est pas nécessaire pour restaurer le constructeur de MailFacade.
La 2FA reste également côté SDK pour réutiliser l'amorçage de session et la dérivation
des clés, mais dispose maintenant de son propre fichier et de ses propres tests.

Il reste à corriger et tester séparément les deux lacunes du décodeur. L'extraction
réduit les conflits de maintenance ; elle ne rend pas un SDK incomplet compatible
avec tous les formats serveur. Ces corrections précèdent toute promotion réelle.

## Résultat du rejeu depuis un clone neuf

`reports/official-359.json` confirme : patch, build, tests bridge, tests SDK et
réseau passent ; protocole échoue ; `selected` reste null. Aucun candidate.lock.json
n'est créé. `reports/logs/` contient les journaux de ce rejeu.

Les quatre patches s'appliquent aussi strictement au snapshot upstream 360.260917.0
(`e274b36407fbb9cacd158c01e854f9a3bf87f020`), sans compilation ni tests sur ce snapshot.
Ce contrôle de portabilité textuelle ne le rend pas éligible à une release.

`adapter/` est une copie lisible de la nouvelle crate. Le script reconstruit le
prototype à partir de `bridge.patch`, qui contient aussi cette crate et les
modifications du bridge. Les empreintes couvrent les entrées réellement utilisées.
