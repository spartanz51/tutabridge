import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { randomBytes } from 'node:crypto';
import assert from 'node:assert/strict';
import sjcl from './sjcl.mjs';
import { blake3 } from './noble-hashes-2.0.1.mjs';
const metadata = JSON.parse(readFileSync(new URL('./ts-sources.json', import.meta.url)));
let source = '';
for (const item of metadata.sources.filter(x => x.local_file.endsWith('.ts'))) {
  const ts = readFileSync(new URL(item.local_file, import.meta.url), 'utf8')
    .replace(/^import\s+[\s\S]*?\s+from\s+["'][^"']+["']\s*;?\s*$/gm, '')
    .replace(/^export\s+/gm, '');
  source += stripTypeScriptTypes(ts, { mode: 'transform' }) + '\n';
}
const dependencies = {
  sjcl, blake3,
  CryptoError: class CryptoError extends Error {},
  ProgrammingError: class ProgrammingError extends Error {},
  SessionKeyNotFoundError: class SessionKeyNotFoundError extends Error {},
  TsBrand: class {},
  stringToUtf8Uint8Array: s => new TextEncoder().encode(s),
  uint8ArrayToArrayBuffer: x => x.buffer.slice(x.byteOffset, x.byteOffset + x.byteLength),
  hexToUint8Array: h => new Uint8Array(Buffer.from(h, 'hex')),
  concat: (...arrays) => new Uint8Array(Buffer.concat(arrays)),
  assertNotNull: x => { assert.notEqual(x,null); return x },
  isKeyVersion: x => Number.isInteger(x) && x>=0 && x<=255,
  random: { generateRandomData: n => new Uint8Array(randomBytes(n)) },
  AEAD_SESSION_KEY_DERIVATION: 'SK instanceSessionKey\x1f',
  AEAD_GROUP_KEY_NONCE_DERIVATION: 'GK and nonce instanceMessageKey\x1f',
  AEAD_ATTRIBUTE_ON_UNAUTHENTICATED_INSTANCE_SESSION_KEY_DOMAIN: 'attributeEncSK\x1f',
  AEAD_ATTRIBUTE_ON_UNAUTHENTICATED_INSTANCE_GROUP_KEY_DOMAIN: 'attributeEncGK\x1f',
};
source += '\nreturn { AeadFacade, SymmetricKeyDeriver, InstanceDecryptor, uint8ArrayTo256Key };';
const api = new Function(...Object.keys(dependencies), source)(...Object.values(dependencies));
const b64 = s => new Uint8Array(Buffer.from(s, 'base64'));
const vectors = JSON.parse(readFileSync(new URL('./rust-vectors.json', import.meta.url)));
const generated = [], results = [];
for (const v of vectors) {
  const key = api.uint8ArrayTo256Key(b64(v.key));
  const nonce = b64(v.kdf_nonce);
  const type = {app:'tutanota',id:97,name:'Mail'};
  const facade = new api.AeadFacade(), deriver = new api.SymmetricKeyDeriver();
  const subkeys = v.version===3 ? deriver.deriveSubKeysAeadFromSessionKey(key,type) : deriver.deriveSubKeysAeadFromGroupKey({object:key,version:7},nonce,type);
  function decrypt(ct, path=v.path, typeId=type, inputKey=key) {
    const instance = new api.InstanceDecryptor(inputKey,nonce,typeId,null,facade,deriver);
    const value = instance.getValueDecryptor(ct,path);
    assert.equal(value.requiredGroupKeyVersion,v.version===3?null:7);
    return value.getValue(v.version===3?null:inputKey);
  }
  const ct = b64(v.ciphertext);
  assert.equal(new TextDecoder().decode(decrypt(ct)),v.plaintext);
  assert.throws(() => decrypt(ct,v.path+'/wrong'));
  assert.throws(() => decrypt(ct,v.path,{...type,id:98}));
  assert.throws(() => decrypt(ct,v.path,type,api.uint8ArrayTo256Key(new Uint8Array(32).fill(0x33))));
  const corrupt=ct.slice(); corrupt[corrupt.length-1]^=1;
  assert.throws(() => decrypt(corrupt));
  const ciphertext = facade.encrypt(subkeys,new TextEncoder().encode(v.plaintext),new TextEncoder().encode(v.aad));
  generated.push({...v,ciphertext:Buffer.from(ciphertext).toString('base64')});
  results.push({version:v.version,path:v.path,rust_ciphertext_decrypted:true,wrong_field_rejected:true,wrong_type_rejected:true,wrong_key_rejected:true,corrupt_tag_rejected:true});
}
writeFileSync(new URL('./ts-vectors.json',import.meta.url),JSON.stringify(generated,null,2));
writeFileSync(new URL('./ts-results.json',import.meta.url),JSON.stringify({commit:metadata.commit,results,scope:'Actual upstream InstanceDecryptor, ValueDecryptor, key derivation, ciphertext parser and AEAD code; original vendored SJCL and BLAKE3. Utility/error/randomness shims; no whole-app execution.'},null,2));
console.log('Official TS decrypts all 4 Rust v2/v3 vectors; rejects wrong field/type/key/tag; generated 4 TS vectors for Rust.');
