# Add bounded multi-entity loading

Technical proposal only; not submitted. Prepared with AI assistance, disclosed here. See TS_PARITY.md for the reference implementation, scope differences and upstream policy.

Loading a known set of list entities currently requires individual requests. Add EntityClient::load_multiple and a typed CryptoEntityClient wrapper using the existing response pipeline. Requests are split into batches of 100; missing entities and server order are preserved. Invalid responses return an SDK error.

Validation: Five targeted tests cover empty input, omitted/reordered entities, 100/1 batching, invalid bodies and early termination on server failure.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/01-batch.patch`.

TS reference: EntityRestClient.loadMultipleParsedInstances and EntityUtils.typeModelToRestPath. Model names are lowercased, queries use the existing encoder, and chunks contain at most 100 IDs. Rust scope is ListElement/GeneratedId only; scheduling is sequential.
