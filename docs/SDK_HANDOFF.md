# TutaBridge — passation du chantier SDK Rust

Document préparé le 23 septembre 2026 pour reprendre le travail dans Claude Code ou avec un autre développeur. Il synthétise les investigations et prototypes des 17–22 septembre. Les états upstream mentionnés sont ceux vérifiés pendant ces investigations, pas une nouvelle vérification des branches au 23 septembre.

## 1. Objectif et décisions

**Conserver le SDK Rust officiel, réduire nos modifications au strict nécessaire et automatiser la recherche d’une version réellement compatible.** À terme, faire accepter les correctifs génériques par Tuta pour supprimer progressivement nos patchs.

Le problème à résoudre est la maintenance récurrente d’un fork qui vieillit : tous les quelques mois, le serveur peut refuser l’ancienne version ou les évolutions du SDK peuvent entrer en conflit avec nos ajouts.

Décisions prises avec Anthony :

- Avancer par phases en Rust. Une migration vers un processus TypeScript n’est pas décidée ; elle sera réévaluée en cas d’abandon confirmé ou de blocage concret du Rust.
- Utiliser le client TypeScript officiel comme référence du protocole et des comportements, tout en respectant les conventions propres au Rust de Tuta.
- Sortir du SDK ce qui appartient au bridge et se compose avec les API publiques.
- Garder des patchs SDK ciblés lorsque les capacités nécessaires sont absentes ou privées. Ne pas recopier la cryptographie, le transport ou le sérialiseur pour afficher artificiellement « zéro patch ».
- Chercher la version upstream la plus récente qui passe les contrôles, plutôt que tirer la dernière version et modifier aveuglément le numéro annoncé.
- Préparer des PR petites, motivées et testées. **Ne pas ouvrir de nouvelle PR Rust, pousser ou publier sans demande explicite d’Anthony.** Il veut relire avant soumission.

Le résultat actuel est un **prototype local reproductible**, pas une migration du SDK de production. La disparition de tous les patchs reste un objectif dépendant des choix upstream.

## 2. Origine : trois problèmes à ne pas confondre

Le point de départ est [l’issue TutaBridge #37](https://github.com/spartanz51/tutabridge/issues/37), puis la revue de la PR #39 et du correctif de version.

| Sujet | Ce qui est établi | Limite de la conclusion |
| --- | --- | --- |
| HTTP 474 | Le serveur refuse une version cliente trop ancienne, avant authentification. | Ce refus ne prouve aucun défaut de déchiffrement AEAD. |
| `InvalidDataSizeError` sur un brouillon | L’erreur est reproductible lorsque le Rust tente de déchiffrer une valeur optionnelle vide que le TS transforme en `null`. | Sans fixture du contributeur, ce cas n’est pas la cause certaine de son incident. |
| Échec AEAD v3 | Un test synthétique démontre que les primitives savent traiter le format mais que le lecteur d’entités Rust audité n’y fait pas appel. | Ce n’est pas la reproduction d’un mail réel ni une preuve de rollout sur un compte. |

Le SDK épinglé dans le bridge pendant l’audit, `d1bae4750cfac28fdf89c585155b5d8c44e60a6f`, dérive du code `348.260528.0`. Le correctif y annonce `359.260904.0` en changeant `Cargo.toml`, sans importer le SDK officiel de septembre. **Un numéro récent ne garantit pas un code ou un protocole récent.**

La lacune AEAD n’a pas été créée par notre refactorisation : le chemin de déchiffrement historique concerné est aussi présent dans l’ancien fork. Le nouveau test a révélé une capacité jusque-là non couverte. Le fonctionnement passé du bridge est compatible avec des données au format historique ; nous n’avons pas établi le format des données de tous ses utilisateurs.

Le smoke test réseau demandé au départ répond au problème 474. Le prototype obtient un HTTP 200 sur `applicationtypesservice` avec la version 359. Ce test public ne vérifie ni une session authentifiée, ni les corps de mails, ni les écritures AEAD. Son existence dans le laboratoire ne doit pas être confondue avec son intégration effective à la CI de la branche livrée.

## 3. Architecture retenue : adaptateur local + petite série de patchs

```text
TutaBridge
  └─ adaptateur local tutabridge-tuta
       ├─ WebSocket / heartbeat
       ├─ dossiers, MOVE, identifiants MailSetEntry
       ├─ orchestration corps / brouillons / pièces jointes
       └─ SDK Rust officiel à une révision figée
            + corrections / capacités manquantes ciblées
```

L’extraction a été prototypée : WebSocket, codec d’identifiants et dossiers ont des tests ; MOVE et la composition de lecture des brouillons ont d’abord été vérifiés par compilation. L’intégration ultérieure contient des tests supplémentaires. Cela ne constitue pas un essai de toutes ces fonctions sur un compte réel.

### Pourquoi les anciens patchs entraient en conflit

Le fork mélangeait ajouts métier, protocole et API de confort dans les mêmes fichiers. Le patch blobs faisait notamment passer `MailFacade::new` de trois à six paramètres, imposant de modifier des tests existants. Les nouveaux tests Archive upstream utilisaient encore le constructeur officiel. Le conflit n’était donc pas seulement textuel.

Le patch MOVE, en revanche, était petit : son conflit venait surtout de tests insérés au même endroit. **Un conflit ne prouve pas à lui seul qu’un patch est trop intrusif.** L’enjeu est de supprimer les dépendances inutiles entre fonctionnalités et les modifications des points centraux.

L’ancien diff fonctionnel représentait environ 3 506 ajouts / 113 suppressions. La première revue minimale des cinq propositions descendait à 1 305 / 78, tests compris. Ces chiffres concernent cette étape historique, avant l’ajout d’AEAD ; ils ne décrivent pas la taille de la série finale.

### Série locale actuelle

| Patch | Responsabilité | Pourquoi côté SDK |
| --- | --- | --- |
| 01 — batch | Chargement par lots de 100, erreurs réseau propres, réutilisation du pipeline existant. | Transport et parsing nécessaires partiellement privés ; éviter une requête par mail. |
| 02 — blob reads | Tokens correctement limités, cache, téléchargement, validation du conteneur et retries. | Réutiliser les mécanismes SDK plutôt que recréer une pile parallèle dans le bridge. |
| 03 — interactive session | Session interactive, TOTP et interrogation des challenges. | Réutiliser services, dérivation et amorçage de session. |
| 04 — parse raw | Petite entrée publique vers le sérialiseur existant. | Faire entrer données inline et blobs dans le pipeline officiel. |
| 05 — optional empty | Valeur chiffrée optionnelle vide → `Null`, comme en TS. | Correctif de protocole indépendant du bridge. |
| 06 — AEAD session reads | Lecture v3 avec clé de session et contexte authentifié. | Intégration au décodeur d’entités. |
| 07 — AEAD group reads | Lecture v2 du profil 359, clés de groupe versionnées et nonce. | Résolution des clés et intégration au pipeline. |
| 08 — AEAD attribute writes | Chiffrement des attributs, valeurs vides et chemins d’agrégats. | Compléter le mapper en utilisant les primitives officielles. |
| 09 — AEAD entity writes | Création, mise à jour et enregistrement du nonce via le service officiel. | Coordonner chiffrement et transport des entités. |
| 10 — immutable paths | Un constructeur de chemins partagé par lecture et écriture. | Refactorisation interne inspirée de `crypto/dev`. |
| 11 — compatibility tests | Vecteurs TS de `crypto/dev`, succès compatibles et rejets attendus. | Garde de compatibilité du prototype, pas fonctionnalité à proposer telle quelle upstream. |

Les cinq premières propositions ont été étudiées avec application/validation indépendante. **Les onze patchs ne sont pas onze PR indépendantes.** Les patchs AEAD forment une progression avec dépendances ; 10 devra probablement être intégré au patch du mapper concerné pour une soumission propre.

La revue a aussi corrigé des défauts fonctionnels : paniques sur réponses batch invalides, portée des tokens blobs, lots trop grands, allocation excessive dans le parseur et erreurs masquées par les retries. Ce n’était pas seulement un déplacement de code.

## 4. AEAD : le travail réalisé et ses limites

AEAD authentifie le contenu **et son contexte**. Ici, les clés dérivées dépendent notamment du type d’instance ; les données associées incluent le domaine et le chemin du champ. Une primitive AES correcte ne suffit pas si le mapper construit le mauvais contexte.

Dans les sources 359/360 auditées :

- CBC est le format historique.
- AEAD v2 utilise une clé de groupe versionnée et un nonce de dérivation.
- AEAD v3 utilise une clé de session.
- Le Rust contient déjà les primitives ; il manquait leur branchement dans la lecture/écriture des attributs d’entités.

Nous avons donc réutilisé `AeadFacade`, les dérivations officielles, les modèles et les services générés. Aucune réimplémentation d’AES, de MAC ou de KDF n’a été ajoutée.

Le prototype couvre la lecture v2/v3 de ce profil, l’écriture des attributs et les opérations génériques suivantes :

- Nouvelle entité : nonce neuf, même si l’entrée est copiée d’une autre entité.
- Migration d’une entité historique : appel à `UpdateKdfNonceService`, puis utilisation du nonce **retourné par le serveur**, qui peut différer de celui proposé.
- Entité déjà AEAD : conservation du nonce pendant la mise à jour.
- Échec du service ou nonce invalide : erreur, sans PUT ultérieur.
- Valeur vide : chiffrement AEAD réel, pas substitution par le marqueur historique CBC.
- Contexte/chemin incorrect ou contenu altéré : rejet, sans essai permissif de plusieurs chemins.

L’API expérimentale `AeadEntityWriter` permet un choix explicite d’AEAD à la création/migration. Le prototype n’invente pas une politique de rollout par compte. Les mises à jour d’entités portant déjà `_kdfNonce` conservent leur contexte AEAD.

**Écrire une entité ne signifie pas envoyer un mail.** Dans le TS 359/360 audité, les requêtes de services suivent encore le chemin CBC ; le chemin d’envoi du bridge n’a pas été basculé arbitrairement en v3. Une fixture de création générique de `Mail` avec transport simulé ne prouve pas que le serveur autorise cette opération directe.

Restent notamment non validés : opérations sur compte réel, tous les contextes de partage, bindings mobiles et compatibilité MSRV complète. BCrypt n’est pas implémenté par la proposition de connexion : le cas non supporté retourne une erreur explicite.

## 5. Le bug TS, la PR upstream et `crypto/dev`

Le harness a reproduit un bug du writer TS : pour deux agrégats frères, le préfixe était modifié dans la boucle. Le deuxième champ recevait par exemple `111/first/second/94`, alors que le lecteur attendait `111/second/94`.

Un correctif TS ciblé avec test de régression a donné lieu à [tutao/tutanota#11539](https://github.com/tutao/tutanota/pull/11539), d’abord ouvert en draft à la demande d’Anthony. Le mainteneur a ensuite fermé la PR en indiquant que le problème était déjà corrigé/refactorisé sur une autre branche et que le chemin concerné était derrière un rollout. **Notre correctif n’a pas été fusionné.**

La vérification a retrouvé la solution dans `crypto/dev`, au commit `e1ad5f9351946e3c9f96558adcbc154c384eb489`. Leur `EncryptionContextPath` construit des chemins immuables et traite aussi les agrégats transférés. Notre patch 10 reprend le principe d’immuabilité avec un type Rust privé minimal, sans copier toute la hiérarchie TS.

### Attention : cette branche change aussi le protocole

| Élément | Profil 359/360 de notre prototype | Référence `crypto/dev` examinée |
| --- | --- | --- |
| V2 | Sous-clés dérivées depuis clé de groupe + nonce. | Clé d’instance intermédiaire, puis sous-clés. |
| Octet de version | `2` | Toujours `2` : ce numéro ne distingue pas les deux protocoles. |
| Domaine des attributs v2 | `attributeEncGK\x1f` | `attributeEncIK\x1f` |
| V3 ordinaire | Clé de session. | Identique octet par octet dans les trois cas comparés. |
| Brouillons | Contexte de type/attribut historique. | Canonicalisation : type `1290` → `1298`, préfixe `1297/` → `1305/`. |
| Transferts d’agrégats | Non portés. | Types cibles, `transferredAttributeId`, troncature du chemin. |
| Services | Chemin CBC de la référence historique. | Choix du schéma selon l’utilisateur, contexte propriétaire fourni au pipeline. |

**Le prototype actuel n’est pas compatible avec tout `crypto/dev`.** Le patch 11 établit précisément cette frontière : trois vecteurs v3 ordinaires passent ; les quatre vecteurs de nouvelle v2 et le vecteur de brouillon v3 échouent à l’authentification avec le profil ancien, comme attendu.

Les nouvelles dérivations existent déjà dans le **Rust upstream** de cette branche (`derive_instance_key`, dérivations depuis clé de groupe ou clé d’instance). La prochaine phase devra les réutiliser et adapter le mapper/métamodèle/services ; il n’y a pas lieu de réécrire leur cryptographie.

## 6. Mise à jour automatique : ce qui existe et ce qui reste à faire

Le sélecteur expérimental essaie des versions officielles dans des clones isolés. Il vérifie six catégories : application des patchs, compilation, tests du bridge, tests SDK, réseau public et capacités du protocole demandées. Il conserve des rapports, révisions et empreintes permettant d’identifier le candidat.

Le rejeu du prototype d’écriture sur la release 359, en exigeant v2 et v3, a produit **`candidate_for_review`** et un `candidate.lock.json`. Cela ne publie, ne déploie et ne met à jour aucun sous-module de production.

Politique visée pour la suite :

1. Détecter les nouvelles versions officielles et essayer les plus récentes en premier.
2. Appliquer la petite série de patchs ; si elle échoue, garder le candidat précédent validé.
3. Compiler et tester le protocole attendu, pas seulement la connectivité.
4. Produire une proposition de mise à jour reproductible pour revue.
5. Retirer les patchs devenus inutiles après intégration upstream, avec contrôle des comportements correspondants.

Un patch qui s’applique et un HTTP 200 ne suffisent pas. La nouvelle signification de v2 en est la démonstration. Le manifeste du dernier audit nomme le profil `359-360-pre-instance-key`, mais **ce champ documentaire n’est pas encore un mécanisme complet de sélection de profils** dans l’automatisation. Le sélecteur embarque actuellement les patchs 01–09 ; l’intégration automatique de 10–11 reste à faire.

## 7. Stratégie de PR minimalistes

L’objectif est de rendre chaque proposition facile à vérifier et utile au SDK, avec un comportement concret avant/après. La propreté ne garantit pas l’acceptation : les mainteneurs ont aussi une direction produit et une volonté de maintenance propres. Ne pas présenter le support futur du Rust ou l’acceptation des PR comme acquis.

Ordre de travail proposé :

1. Revalider le besoin sur la branche cible upstream : correction déjà présente, API déjà exposée, évolution en cours ?
2. Préparer d’abord le correctif autonome des valeurs optionnelles vides.
3. Justifier la petite entrée de parsing, puis traiter batch, blobs et session interactive séparément selon les besoins acceptés.
4. Traiter AEAD comme une série dédiée : lectures/contextes, écriture d’attributs, cycle de vie du nonce. Choisir explicitement le profil et la branche cible avant de rebaser.

Conventions de code à conserver :

- Primitives, types générés, sérialiseur et services upstream réutilisés ; pas de modèles modifiés à la main.
- API publique minimale ; helpers privés et modules ciblés.
- `Result` et erreurs SDK existantes ; pas de panique sur une réponse réseau invalide.
- Injection des dépendances réseau et de l’aléa, comme les façades voisines.
- `rustfmt.toml` upstream, édition déclarée par la crate, noms idiomatiques Rust. Le comportement TS fait référence, pas sa syntaxe.
- Tests de régression et cas négatifs ; vecteurs issus du TS réel pour les questions de format/chiffrement.
- Aucun nettoyage périphérique sans rapport avec le comportement corrigé.

Pour les descriptions de PR : titre factuel, déclencheur du problème, comportement après correction, référence TS précise, tests exécutés et limites pertinentes. Éviter les grands discours de refonte ou les promesses d’adoption ; ne pas affirmer que « beaucoup de projets utilisent le SDK » sans source. Les justifications de dépendance entre PR doivent être explicites.

Les dossiers `proposals/` sont des brouillons historiques, **pas des descriptions prêtes à publier sans relecture**. Certaines conclusions ont été dépassées par les étapes suivantes. Anthony décide des soumissions et de leur présentation.

## 8. Niveau de preuve et état local

| Étape | Résultats conservés | Portée |
| --- | --- | --- |
| Prototype d’écriture 01–09 | 758 tests Rust distincts + 12 Python ; compilation CLI ; rejeu complet du sélecteur. | SDK, bridge et adaptateur sur base 359 ; HTTP public ; transport d’écriture simulé. |
| Alignement 10–11 | 343 tests Rust réussis, zéro échec, un ignoré : 336 unitaires SDK, 4 écritures, 3 sessions interactives. | Suites du SDK modifié ; toute la suite bridge n’a pas été rejouée à cette étape. |
| Référence `crypto/dev` | 7 chemins + 8 vecteurs cryptographiques produits avec leurs sources TS figées. | Harness avec adaptateurs utilitaires et aléa déterministe, pas leur application entière ni toute leur suite TS. |
| Rejeu des patchs | Les 11 patchs produisent l’arbre attendu depuis la base officielle 359. | Application exacte ; ne prouve pas la compilation d’une autre branche. |
| Conventions | Format et `git diff --check` passent ; Clippy réussit. | Avertissements préexistants conservés, aucun diagnostic sur les nouvelles lignes 10–11. |

Ne pas additionner 758 et 343 : il y a recouvrement. Ne pas présenter un test qui attend un rejet comme la preuve d’une capacité prise en charge.

Révisions utiles :

- Base Rust officielle compilée : `aea5846b93a1412451e885bf99002401c3b087e8`.
- Référence TS historique : `46270557c251d1a31157d72e0aaf0cc63bb33ecf`.
- Référence `crypto/dev` : `e1ad5f9351946e3c9f96558adcbc154c384eb489`.
- Arbre après 01–09 : `c72f2f8f6ab02df7bf69379546b9a7a734df246a`.
- Arbre après 01–11 : `c5ed48ae15b0e87af85dfae72d030d791d0ce19a`.

Les deux dernières valeurs sont des **arbres Git locaux**, pas des commits à checkout sur GitHub. Les patchs et manifests permettent de les reconstruire. Les sources du prototype ont été travaillées dans des dossiers temporaires ; privilégier les artefacts conservés sous `docs/audits/` pour reprendre.

Le checkout principal et son sous-module n’ont pas été migrés par ces prototypes. Au moment de cette passation, `docs/audits/` est non suivi par Git : conserver/transmettre ces fichiers explicitement, ne pas supposer qu’ils sont présents sur origin. Aucune PR Rust issue de cette réécriture n’a été ouverte ; la PR TS #11539 est une opération distincte déjà effectuée.

## 9. Par où reprendre concrètement

1. Lire ce document, puis les deux derniers rapports ci-dessous. Inspecter l’état Git sans écraser les travaux locaux.
2. Reproduire la base validée dans un checkout isolé ; ne pas repartir du vieux fork ni d’un ancien prototype incomplet.
3. Relire les patchs pour confirmer nécessité, contrat TS, taille de l’API et dépendances réelles.
4. Pour une PR upstream, choisir une correction indépendante et la revalider sur la branche cible actuelle avant rédaction.
5. Pour le futur protocole, étudier séparément clés d’instance, canonicalisation/transferts et services en réutilisant leur Rust. Ne pas annoncer ce port terminé sur la base des tests de rejet actuels.
6. Intégrer ensuite les garde-fous de profil au sélecteur, puis prévoir une validation authentifiée avec un compte de test avant toute qualification de production.

Références locales prioritaires, relatives à ce fichier :

- [État récent, différences avec crypto/dev, patchs 10–11](audits/sdk-crypto-dev-alignment-2026-09-22/README.md).
- [Prototype lecture/écriture et patchs 01–09](audits/sdk-aead-writes-2026-09-22/README.md).
- [Tests et limites de la validation complète](audits/sdk-aead-writes-2026-09-22/VALIDATION.md).
- [Style Rust et correspondance TS du profil historique](audits/sdk-aead-writes-2026-09-22/STYLE_AND_PARITY.md).
- [Chronologie : 474, champs vides et AEAD](audits/sdk-aead-timeline-2026-09-21/README.md).
- [Isolation des extensions du bridge](audits/sdk-isolation-2026-09-21.md).
- [Revue initiale des patchs](audits/sdk-patch-review-2026-09-21/REVIEW.md) : utile pour les motivations, mais son état « AEAD non implémenté » est historique et dépassé.

Depuis la racine du bridge, avec Node 24 et un clone de Tuta contenant la base officielle :

```sh
node docs/audits/sdk-crypto-dev-alignment-2026-09-22/harness/verify_crypto_dev.mjs
python3 docs/audits/sdk-crypto-dev-alignment-2026-09-22/harness/verify_patches.py /chemin/vers/tutanota
```

La première commande régénère les fixtures TS locales. La seconde vérifie la série dans un index Git temporaire sans modifier le checkout. Pour compiler, appliquer 01–09 du dossier d’écriture puis 10–11 du dossier d’alignement à une copie isolée de la base 359, et lancer depuis sa racine :

```sh
CARGO_INCREMENTAL=0 cargo test -p tuta-sdk --lib --test aead_writes --test interactive_session_test
```

Le sélecteur complet 01–09 et sa commande de rejeu sont décrits dans le README du prototype d’écriture. Les anciens scripts de packaging peuvent contenir les chemins du laboratoire initial ; les manifests, patchs et rapports sont les références pour reconstruire le travail. Prévoir de l’espace disque : la dernière compilation a nécessité de supprimer uniquement le cache incrémental du laboratoire avant de reprendre.
