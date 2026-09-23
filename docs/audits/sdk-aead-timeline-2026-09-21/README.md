# Pourquoi AEAD apparaît maintenant

La limitation n’a pas été introduite par les cinq patchs. Les quatre fonctions déterminantes sont identiques octet pour octet dans le fork de production `d1bae475`, le SDK officiel 359 `aea5846` et le master examiné `46270557` : `decrypt_and_map_inner`, `decrypt_and_parse_value`, `GenericAesKey::decrypt_data`, `has_mac`. Voir `decoder-comparison.json` pour les hashes et lignes. Les différences des fichiers alentours sont essentiellement des renommages d’IV ; elles n’ajoutent pas la sélection du décodeur AEAD. Le fork ancien n’a pas été recompilé dans cette étape : la comparaison est une preuve de code, les expériences exécutées sont conservées dans le dossier d’investigation précédent.

## Chronologie vérifiée

- Le fork utilisé par TutaBridge dérive du SDK `348.260528.0`. Ses extensions de bridge datent de mai.
- Le commit officiel `81f115f75fe408480ec2322d3b495df1eb344a49`, intitulé « Instance attributes encryption with AEAD », a une date d’auteur au 24 avril et une date de commit au **3 juin 2026**. Il est présent dans l’histoire de la release 359, absent de celle du fork de mai. Cette date de commit n’est pas une preuve du calendrier d’activation sur les comptes. Le lecteur Rust des entités est resté sur le chemin historique.
- Le 16 septembre, l’issue 37 constate le rejet HTTP 474 des clients sous la version 357, avant authentification. Le correctif `d1bae475` modifie uniquement `Cargo.toml` : `348.260528.0` devient `359.260904.0`. Il ne met à jour aucune fonction de déchiffrement (`version-only-bump.patch`).
- Pendant notre audit, un nouveau test synthétique construit un sujet AEAD v3 et impose sa lecture. Ce test échoue, révélant une capacité manquante déjà présente dans l’ancien lecteur. Il a également été ajouté comme condition obligatoire au sélecteur expérimental : c’est cette exigence nouvelle qui bloque sa sélection.

L’ancien fonctionnement est cohérent avec l’utilisation de données au format historique pris en charge. Cela n’établit pas le format de tous les mails de tous les utilisateurs : nous n’avons pas inspecté leurs comptes. Nos anciens tests ne démontraient pas le support AEAD.

## Trois problèmes distincts

| Symptôme | Ce qui est établi | Ce qui n’est pas établi |
|---|---|---|
| HTTP 474 | Refus serveur de la version annoncée, décrit et bisecté dans l’issue | Aucun lien causal démontré avec le déchiffrement AEAD |
| InvalidDataSizeError sur un brouillon après connexion | Symptôme rapporté par le contributeur ; le cas du champ chiffré optionnel vide est reproduit et corrigé localement | Sans fixture du cas utilisateur, ni champ vide ni AEAD ne peuvent être déclarés sa cause certaine |
| MacError du nouveau test AEAD v3 | Le lecteur Rust utilise le mauvais chemin pour ce format ; primitives TS/Rust compatibles sur les tests croisés | Aucun incident v3 réel ni date d’activation serveur n’ont été observés |

L’attribution dans le commentaire du 17 septembre à un « June AEAD rollout » était trop affirmative. La présence du code et du mécanisme de rollout ne prouve ni son activation sur le compte concerné, ni l’origine de son InvalidDataSizeError. Un simple rebase du SDK ne garantit pas la correction AEAD, puisque le lecteur officiel récent conserve la même limite.

## Recommandation

1. Continuer le plan Rust et les petites propositions indépendantes. Le défaut de champs optionnels vides, les lectures batch/blobs et les APIs de session ne doivent pas devenir une énorme PR cryptographique.
2. Pour le sélecteur, distinguer la non-régression face à la version de référence, l’acceptation réseau et les capacités supplémentaires du protocole. Conserver AEAD visible comme capacité absente sur les deux versions. Son échec doit interdire l’étiquette « compatible AEAD », mais il ne prouve pas qu’un candidat est moins fonctionnel que le SDK livré aujourd’hui. Des succès sur mocks et smoke public seuls ne suffisent pas à affirmer une compatibilité utilisateur complète ni à déployer automatiquement.
3. Traiter AEAD dans un chantier dédié, sans attendre un incident : sélection des formats et contexte authentifié, lecture v3 avec les primitives existantes, puis v2 avec clés de groupe/version/nonce. Les intégrer progressivement tout en conservant les tests négatifs et la compatibilité historique. V2 est nécessaire pour couvrir le mode d’écriture activable par rollout dans le TS ; v3 seul serait insuffisant.
4. Pour attribuer le bug de brouillon réel, recueillir uniquement un diagnostic structurel ciblé : type/attribut fautif, valeur absente ou vide, longueur, marqueur de chiffrement reconnu selon le parseur officiel, présence du nonce et métadonnées de version. Ne pas confondre le premier octet aléatoire d’un ancien ciphertext non versionné avec un marqueur v2/v3. Aucune clé, valeur de mail, jeton ou passphrase n’est nécessaire dans les logs. Ce diagnostic n’a pas encore été instrumenté.

Cette note est une analyse et une proposition de politique. Le sélecteur, le code de production et les cinq patchs archivés restent inchangés. Aucune PR ni commentaire GitHub n’a été publié.

Sources : [issue 37](https://github.com/spartanz51/tutabridge/issues/37), [symptôme du brouillon](https://github.com/spartanz51/tutabridge/issues/37#issuecomment-5706305913), [attribution à AEAD](https://github.com/spartanz51/tutabridge/issues/37#issuecomment-5717352364), [commit AEAD des attributs](https://github.com/tutao/tutanota/commit/81f115f75fe408480ec2322d3b495df1eb344a49), [preuves d’interopérabilité](../sdk-aead-investigation-2026-09-21/README.md).
