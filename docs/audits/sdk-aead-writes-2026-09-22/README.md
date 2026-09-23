# Prototype d'écriture AEAD — 22 septembre 2026

L'écriture AEAD est implémentée dans le prototype Rust temporaire. Aucun push, aucune PR et aucun changement du SDK de production.

## Ce qui fonctionne dans les tests

- Chiffrement des attributs **v2 (clé de groupe)** et **v3 (clé de session)** avec les primitives Rust officielles.
- Contexte authentifié du type racine et du chemin de chaque champ, y compris les agrégats. Les valeurs vides sont chiffrées ; elles ne sont pas remplacées par le vieux marqueur CBC.
- Création d'une entité AEAD avec un nouveau nonce, même si l'entrée provenait d'une copie d'une autre entité.
- Migration d'une ancienne entité lors d'une mise à jour : appel au service officiel `UpdateKdfNonceService`, utilisation du nonce **renvoyé par le serveur**, puis chiffrement et PUT.
- Mise à jour d'une entité déjà AEAD via `CryptoEntityClient::update_instance`, sans nouvel enregistrement du nonce.
- Refus d'une réponse de nonce invalide, propagation d'un échec du service et absence de PUT dans ces cas.

Les tests passent par le mapper, le sérialiseur et le client HTTP Rust réels, avec des réponses réseau simulées. Ils ne prouvent pas l'acceptation d'une création de Mail par le serveur réel : cette fixture sert à vérifier le pipeline générique de création, et l'envoi d'un mail reste une opération de service distincte.

## Deux propositions séparées

1. [08 — Chiffrement AEAD des attributs](patches/08-aead-attribute-writes.patch) : réutilise la traversée existante et les primitives Tuta ; tests des sorties TS, des valeurs vides et des chemins d'agrégats.
2. [09 — Écriture des entités et gestion du nonce](patches/09-aead-entity-writes.patch) : façade `AeadEntityWriter` et mise à jour automatique des entités portant déjà un nonce ; tests du transport et des erreurs.

Ces patchs prolongent les sept patchs du [prototype de lecture](../sdk-aead-prototype-2026-09-21/README.md), repris sans modification. Les anciennes propositions décrivent leurs étapes respectives : le garde-fou « lecture seulement » de 07 est remplacé par le chemin d'écriture ajouté ici. Les propositions restent locales et nécessitent une revue d'API avant toute soumission.

## Conventions et TypeScript

Voir [STYLE_AND_PARITY.md](STYLE_AND_PARITY.md). La configuration rustfmt, l'édition Cargo 2021, les modèles de services générés, l'injection des dépendances et les primitives existantes sont utilisés. Aucune dépendance cryptographique ni modification des modèles générés n'est ajoutée.

**Correction de l'explication précédente : les requêtes de services restent en CBC dans le TypeScript audité.** `ServiceExecutor` appelle `InstancePipeline.mapAndEncrypt`, qui choisit explicitement CBC. Le chemin d'envoi actuel de TutaBridge reste donc inchangé, conformément à Tuta. La présence de primitives v3 ne signifie pas que tous les services doivent être basculés en v3.

Le harness a également reproduit une incohérence dans la boucle d'écriture des agrégats TS : le chemin du deuxième élément accumule l'identifiant du premier. Le Rust utilise le chemin indépendant attendu par le lecteur TS. Ce cas et l'écart volontaire sont documentés et testés ; aucune correction n'a été envoyée à Tuta.

## Activation et limites

L'intégration du rollout par compte n'est pas inventée : un appelant choisit explicitement `AeadEntityWriter` après avoir décidé d'utiliser AEAD. Le défaut historique de création reste CBC. Les mises à jour d'une entité déjà munie de `_kdfNonce` utilisent en revanche AEAD automatiquement, pour conserver son contexte.

```rust
let client = logged_in.mail_facade().get_crypto_entity_client();
let writer = AeadEntityWriter::new(
    &client,
    logged_in.get_service_executor(),
    RandomizerFacade::from_core(rand_core::OsRng),
);
writer.update_instance(entity, None).await?;
```

`None` demande la clé actuelle du groupe propriétaire. Une clé versionnée explicitement fournie permet un contexte de propriétaire résolu par l'appelant ; sur une mise à jour, son choix de version doit correspondre au contrat du fournisseur de clé propriétaire TS. La résolution complète de tous les cas de partage n'est pas portée ni validée par ce prototype.

La migration utilise les identifiants persistants générés acceptés par le modèle de service Rust. Les contraintes d'adressage de `EntityClient::create_instance` restent celles du SDK existant, notamment l'identifiant portant la liste cible. Les versions de clé supérieures à 255 sont refusées, conformément au format actuellement pris en charge par la primitive AEAD Rust.

Aucun compte réel n'a été utilisé. Le prototype n'active aucun rollout serveur et ne change pas le comportement SMTP de TutaBridge.

## Validation et rejeu

[VALIDATION.md](VALIDATION.md) détaille les tests et preuves. Le sélecteur livré exige désormais aussi les tests d'écriture `aead_writes` dans son contrôle SDK. Il produit seulement un candidat pour revue.

```sh
python3 prototype.py \
  --bridge-source /chemin/vers/tutabridge \
  --sdk-cache /chemin/vers/tutanota \
  --output /tmp/tutabridge-aead-write-run \
  --tag tutanota-release-359.260904.0 \
  --require-capability aead_v2 \
  --require-capability aead_v3
```

Le dossier de sortie doit être nouveau. Les clones de travail sont isolés. Le code modifié est consultable sous `sdk-source/`, les sources TS et le harness sous `ts-reference/`.
