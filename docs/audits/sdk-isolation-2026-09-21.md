# Isolation des extensions Rust — 21 septembre 2026

## Résultat vérifié

Une crate indépendante compile contre le SDK officiel **359.260904.0**, commit
`aea5846b93a1412451e885bf99002401c3b087e8`, sans aucune modification du SDK.
Son `git status --porcelain` est vide après les tests. La comparaison porte sur
cette version précise, sans recherche d'une nouvelle release depuis l'audit précédent.

| Fonction | Expérience | Preuve et limites |
|---|---|---|
| WebSocket + heartbeat | Fichier déplacé sans changement | 28 tests passent ; pas de connexion réelle |
| Codec MailSetEntry | Fichier déplacé sans changement, imports SDK réexportés | 8 tests passent, dont vecteur TypeScript |
| Arbre de dossiers | Type local, trait local pour les méthodes de MailSet, fixtures locales | 7 tests passent |
| MOVE arbitraire | Fonction externe utilisant get_service_executor et MoveMailService | Compilation ; pas de requête réelle |
| Lecture des brouillons | Fonction externe réutilisant les API publiques de clés, chargement, déchiffrement et mapping | Compilation ; pas de test fonctionnel de brouillon |

Total : **43 tests réussis**. Ce n'est pas une validation du bridge complet.
Le dossier temporaire est `/var/folders/tf/f990vmwn0ynfcwpnzym6xlzm0000gn/T/tutabridge-isolation-nai1t0sm`.
L'archive voisine `sdk-isolation-2026-09-21.tar.gz` conserve les sources, le lockfile,
les instructions de reproduction et le journal, sans embarquer le SDK ou ses objets Git.

## Pourquoi les patches actuels se gênent

La série compte 12 commits fonctionnels, hors bump de version, pour un diff net de
3506 ajouts / 113 suppressions dans 14 fichiers Rust/Cargo (tests inclus), entre
`202ca648f3` et `1036d6f2`.

Le patch MOVE est petit : 57 ajouts / 5 suppressions dans mail_facade.rs. Le conflit
avec 359 est une insertion de tests au même endroit que les nouveaux tests Archive.
Le problème structurel est ailleurs : le patch blob modifie MailFacade::new de
3 à 6 paramètres et réécrit les tests existants. Les tests Archive ajoutés upstream
conservent la signature à 3 arguments. Une résolution textuelle ne suffit donc pas.

Le découpage en commits ne rend pas ces changements indépendants : plusieurs
fonctionnalités partagent le constructeur modifié, les helpers et les tests.

## Une dépendance que l'on peut supprimer

La lecture des brouillons semble dépendre du patch blob parce que celui-ci introduit
decrypt_with_owner_key et les dépendances supplémentaires de MailFacade. L'expérience
montre que ce n'est pas une obligation de l'API officielle :

- LoggedInSdk.get_entity_client().load fournit déjà l'entité sans déchiffrement.
  Notre CryptoEntityClient.load_encrypted est un relais redondant dans ce contexte.
- mail_facade().get_crypto_entity_client().get_crypto_facade().get_key_loader_facade()
  donne accès au chargement de clés versionnées.
- EntityFacadeImpl et ResolvedSessionKey sont publics ; on peut réutiliser le
  décodeur officiel sans recopier ses algorithmes.
- instance_mapper et type_model_provider sont déjà accessibles sur LoggedInSdk.

La fonction expérimentale crée une EntityFacadeImpl avec le fournisseur de modèles
de la session. Dans une intégration, cette instance peut appartenir à notre adaptateur.
L'utilisation de ces API publiques reste un couplage d'API à tester à chaque bump.
Le décodeur conserve ses limitations : extraire le code ne corrige ni les champs
optionnels vides ni les formats AEAD étudiés dans l'audit du 17 septembre.

## Ce qui reste à isoler ou à patcher

| Zone | Constat de lecture | Recommandation |
|---|---|---|
| Chargement batch | Repose sur prepare_and_fire, transport et parsing privés | Garder d'abord le patch étroit ; éviter de le remplacer par N requêtes qui perdraient son intérêt |
| Déchiffrement inline | Pipeline crypto public en partie, accès au parsing JSON absent via l'EntityClient officiel | Évaluer une petite API parse_raw ou une façade typée de décodage ; extraction non validée ici |
| Transport blobs | Cache de tokens, headers et clients internes privés ; mêmes ressources que l'upload officiel | Garder initialement les primitives de lecture dans BlobFacade ; sortir l'orchestration Mail/File côté bridge |
| 2FA | Création de session, exécuteur non authentifié et parse_session_id internes | Conserver une API SDK ciblée pour l'amorçage ; éviter de réimplémenter l'authentification HTTP pour éliminer artificiellement un patch |
| Correction BlobGetIn._id | Corrige notre chemin de téléchargement ajouté | La correction doit suivre le code extrait, sans être oubliée ou traitée comme patch upstream indépendant |
| Décodage protocole | Défauts internes identifiés dans l'audit précédent | Patches de correction séparés, avec tests de compatibilité |

Ces propositions pour les zones restantes sont issues de la lecture, pas d'une
extraction complète compilée. On ne peut pas encore annoncer un nombre final de patches.

## Organisation recommandée

Une seule crate locale `tutabridge-tuta` pour les extensions et leur adaptation au
SDK suffit au départ : move, dossiers, événements, identifiants, orchestration de
lecture. Utiliser des fonctions ou des traits locaux : Rust ne permet pas d'ajouter
un impl inhérent à un type défini dans une autre crate. Un trait local évite aussi
de remplacer l'enum officielle MailSetKind pour nos besoins de classement.

Conserver les constructeurs upstream. Préférer quelques opérations ciblées de
parsing/transport aux exports massifs de champs privés. Les tests des extensions
appartiennent à la crate locale ; ceux des correctifs SDK restent près du code corrigé.

Ordre proposé : extraire les fonctions validées ici ; rétablir le constructeur
officiel de MailFacade en sortant l'orchestration ; réduire ensuite les patches
batch/inline/blob/2FA ; faire tourner la sélection automatique sur cette surface réduite.

L'automatisation doit distinguer échec d'application, échec d'API/compilation et
échec fonctionnel/protocole. Une version sans conflit Git n'est pas automatiquement
compatible. Les numéros de client et de modèles doivent toujours provenir du SDK
sélectionné, y compris pour notre WebSocket désormais externe.

Aucun changement du code de production, aucune opération sur un compte, aucun push.
