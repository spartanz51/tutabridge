import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { stripTypeScriptTypes } from 'node:module';
import assert from 'node:assert/strict';
import sjcl from './sjcl.mjs';
import { blake3 } from './noble-hashes-2.0.1.mjs';

const root = new URL('../', import.meta.url);
const manifest = JSON.parse(readFileSync(new URL('reference.json', root)));
for (const entry of manifest.files) {
  const data = readFileSync(new URL(`reference/${entry.path}`, root));
  assert.equal(createHash('sha256').update(data).digest('hex'), entry.sha256, entry.path);
}
const symmetric = 'src/platform-kit/crypto/encryption/symmetric/';
const pipeline = 'src/platform-kit/instance-pipeline/';
const files = [
  symmetric + 'SymmetricCipherVersion.ts', symmetric + 'AesKey.ts',
  symmetric + 'ParsedCiphertext.ts', symmetric + 'SymmetricCipherUtils.ts',
  'src/platform-kit/crypto/CryptoTypes.ts', 'src/platform-kit/crypto/hashes/Blake3.ts',
  symmetric + 'SymmetricKeyDeriver.ts', symmetric + 'AeadFacade.ts',
  pipeline + 'EncryptionContextPath.ts', pipeline + 'CanonicalId.ts',
  pipeline + 'InstanceTypeContext.ts', pipeline + 'ValueAssociatedData.ts',
];
let source = '';
for (const file of files) {
  const ts = readFileSync(new URL(`reference/${file}`, root), 'utf8')
    .replace(/^import\s+[\s\S]*?\s+from\s+["'][^"']+["']\s*;?\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  source += stripTypeScriptTypes(ts, { mode: 'transform' }) + '\n';
}
// Only utility/environment dependencies are supplied here. Path construction,
// canonicalization, key derivation, encryption and decryption are upstream code.
const dependencies = {
  sjcl, blake3, TsBrand: class {},
  CryptoError: class CryptoError extends Error {},
  ProgrammingError: class ProgrammingError extends Error {},
  AppNameEnum: { Tutanota: 'tutanota' },
  stringToUtf8Uint8Array: text => new TextEncoder().encode(text),
  uint8ArrayToArrayBuffer: data => data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength),
  hexToUint8Array: hex => new Uint8Array(Buffer.from(hex, 'hex')),
  concat: (...arrays) => new Uint8Array(Buffer.concat(arrays)),
  assertNotNull: value => { assert.notEqual(value, null); return value; },
  isKeyVersion: value => Number.isInteger(value) && value >= 0 && value <= 255,
  random: { generateRandomData: count => new Uint8Array(count).fill(0x44) },
};
source += '\nreturn {RootPath, ValueAssociatedData, SymmetricKeyDeriver, AeadFacade, uint8ArrayToKey, keyToUint8Array, parseVersionedCiphertext, makeKeyDerivationContext};';
const api = new Function(...Object.keys(dependencies), source)(...Object.values(dependencies));
const makePath = (segments, attribute) => {
  let path = new api.RootPath('tutanota');
  for (const { association, aggregate } of segments) {
    path = path.addAssociationId({ id: association, transferredAttributeId: null }).addAggregateId(aggregate);
  }
  return path.addValueId({ id: attribute, transferredAttributeId: null });
};
const paths = [{ segments: [], attribute: 105, path: '105' }];
for (const aggregate of ['first', 'second']) {
  const segments = [{ association: 3, aggregate }];
  paths.push({ segments, attribute: 2, path: `3/${aggregate}/2` });
  for (const child of ['child1', 'child2']) {
    paths.push({ segments: [...segments, { association: 9, aggregate: child }], attribute: 17, path: `3/${aggregate}/9/${child}/17` });
  }
}
for (const vector of paths) assert.equal(makePath(vector.segments, vector.attribute).getPath(), vector.path);
const sharedParent = new api.RootPath('tutanota').addAssociationId({ id: 3, transferredAttributeId: null });
assert.equal(sharedParent.addAggregateId('first').addValueId({ id: 2, transferredAttributeId: null }).getPath(), '3/first/2');
assert.equal(sharedParent.addAggregateId('second').addValueId({ id: 2, transferredAttributeId: null }).getPath(), '3/second/2');
const mapped = sharedParent.addAggregateId('first').addAssociationId({ id: 4, transferredAttributeId: 44 })
  .addAggregateId('second').addValueId({ id: 2, transferredAttributeId: 42 });
assert.equal(mapped.getPath(), '44/second/42');

const key = api.uint8ArrayToKey(new Uint8Array(32).fill(0x11));
const group = { object: key, version: 7 };
const nonce = new Uint8Array(32).fill(0x22);
const deriver = new api.SymmetricKeyDeriver();
const aead = new api.AeadFacade();
const vectors = [];
const oldVectors = JSON.parse(readFileSync(new URL('fixtures/359-360-mapper-vectors.json', root)));
const cases = [
  { type: 97, segments: [], attribute: 105, plaintext: 'Sujet écrit — 🦀' },
  { type: 97, segments: [], attribute: 105, plaintext: '' },
  { type: 97, segments: [{ association: 111, aggregate: 'sender-id' }], attribute: 94, plaintext: 'Alice' },
  { type: 1290, segments: [{ association: 1297, aggregate: 'details-id' }], attribute: 1256, plaintext: 'draft' },
];
for (const version of [2, 3]) {
  for (const entry of cases) {
    const path = makePath(entry.segments, entry.attribute);
    const context = api.makeKeyDerivationContext({ app: 'tutanota', id: entry.type, name: 'Test' });
    const subKeys = version === 2 ? deriver.deriveSubKeysAeadWithInstanceKeyFromGroupKey(group, nonce, context)
      : deriver.deriveSubKeysAeadWithSessionKey(key, context);
    if (version === 2) {
      const direct = deriver.deriveSubKeysAeadWithInstanceKeyFromInstanceKey(deriver.deriveInstanceKey(group, nonce), context);
      assert.deepEqual(api.keyToUint8Array(subKeys.encryptionKey), api.keyToUint8Array(direct.encryptionKey));
      assert.deepEqual(api.keyToUint8Array(subKeys.authenticationKey), api.keyToUint8Array(direct.authenticationKey));
    }
    const associatedData = new api.ValueAssociatedData(path);
    const ciphertext = aead.encrypt(subKeys, new TextEncoder().encode(entry.plaintext), associatedData);
    assert.equal(new TextDecoder().decode(aead.decrypt(subKeys, api.parseVersionedCiphertext(ciphertext), associatedData)), entry.plaintext);
    const encoded = Buffer.from(ciphertext).toString('base64');
    if (entry.type === 97) {
      const old = oldVectors.find(v => v.version === version && v.path === path.getPath() && v.plaintext === entry.plaintext);
      assert.ok(old);
      if (version === 3) assert.equal(encoded, old.ciphertext);
      else assert.notEqual(encoded, old.ciphertext);
    } else {
      assert.equal(context, 'tutanota/1298');
      assert.ok(new TextDecoder().decode(associatedData.asBytes(version)).endsWith('1305/details-id/1256'));
    }
    vectors.push({ version, instance_type: `tutanota/${entry.type}`, derivation_context: context, path: path.getPath(),
      plaintext: entry.plaintext, ciphertext: encoded, key: Buffer.alloc(32, 0x11).toString('base64'),
      kdf_nonce: Buffer.alloc(32, 0x22).toString('base64'),
      associated_data: Buffer.from(associatedData.asBytes(version)).toString('base64'),
      compatible_with_359: version === 3 && entry.type === 97 });
  }
}
mkdirSync(new URL('fixtures/', root), { recursive: true });
writeFileSync(new URL('fixtures/crypto_dev_paths.json', root), JSON.stringify(paths, null, 2) + '\n');
writeFileSync(new URL('fixtures/crypto_dev_attributes.json', root), JSON.stringify(vectors, null, 2) + '\n');
console.log(JSON.stringify({ commit: manifest.commit, path_vectors: paths.length, attribute_vectors: vectors.length,
  ordinary_v3_byte_identical: true, v2_incompatible: true, draft_context_changed: true,
  scope: 'Pinned upstream TS paths, canonicalization, KDF and AEAD executed with deterministic random and utility adapters; no whole application or server.' }, null, 2));
