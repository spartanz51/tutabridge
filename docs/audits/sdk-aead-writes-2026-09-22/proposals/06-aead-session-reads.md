# Read AEAD session-key attributes using existing Rust primitives

Local technical draft only; not submitted. Prepared with AI assistance, disclosed here.

A valid version-3 attribute currently reaches the legacy CBC decoder and fails with MacError. Dispatch this format to the existing AeadFacade, using the root instance type and authenticated field path followed by the TypeScript InstanceDecryptor/CryptoMapper pipeline. Preserve existing CBC reads and reject invalid authentication contexts without fallback.

The patch includes TypeScript-generated golden vectors, negative context/authentication checks and an aggregate traversal test. It applies directly to official SDK commit `aea5846b93a1412451e885bf99002401c3b087e8`, without the bridge extension series. Compilation and full test execution reported here cover the assembled series, not an independently built standalone proposal.

Scope: entity attribute reads only. No changes to cryptographic primitives, MailFacade, FolderSystem or SDK version. See ../AEAD_PARITY.md for exact scope differences and ../VALIDATION.md for evidence.
