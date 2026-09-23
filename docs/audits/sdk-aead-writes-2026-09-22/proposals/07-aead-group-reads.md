# Load versioned group keys for AEAD entity attribute reads

Local technical draft only; not submitted. Prepared with AI assistance, disclosed here. Depends on proposal 06.

Version-2 attributes carry a group-key version and use the instance KDF nonce. Inspect encrypted attributes and aggregates, load the distinct requested versions through KeyLoaderFacade, and reuse AeadFacade with the group-key domain and root type context. Instances encrypted entirely with group keys do not require an artificial session key.

Wire this context into CryptoEntityClient and expose decryption of already parsed responses for blob and inline consumers. Preserve bucket-key resolution where it also establishes sender identity and attachment keys. Refuse legacy writes of instances carrying a KDF nonce because AEAD writes are not implemented.

Validation covers TS-generated vectors, wrong nonce/key version, per-version caching, required-key collection, key-loader calls and write rejection. Bridge integration tests read a full mail and nested draft body without a parent session key. The patch applies on top of 06 alone; assembled-series build/test results are recorded in ../VALIDATION.md.

This proposal needs a dedicated API review: its scope is larger than one primitive decoder change. It does not port every TS permission/session-resolution branch or implement AEAD writes. See ../AEAD_PARITY.md.
