import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { randomBytes } from 'node:crypto';
import assert from 'node:assert/strict';
import sjcl from './sjcl.mjs';
import { blake3 } from './noble-hashes-2.0.1.mjs';
const metadata = JSON.parse(readFileSync(new URL('./ts-sources.json', import.meta.url)));
let source = '';
for (const item of metadata.sources.filter(x => x.local_file.endsWith('.ts') && !['EntityRestClient.ts','ServiceExecutor.ts','InstancePipeline.ts'].includes(x.local_file))) {
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
// Minimal parsed-value/model adapters. Cryptography and mapper traversal below
// execute unchanged upstream methods, rather than a handwritten TS reimplementation.
class ParsedValue {
  constructor(value) { this.value = value; }
  static fromNull() { return new ParsedValue(null); }
  static fromString(x) { return new ParsedValue(x); }
  static fromByteArray(x) { return new ParsedValue(x); }
  static fromId(x) { return new ParsedValue(x); }
  static fromNestedItems(x) { return new ParsedValue(x); }
  isNull() { return this.value == null; }
  isString() { return typeof this.value === 'string'; }
  asString() { assert.equal(typeof this.value,'string'); return this.value; }
  asByteArray() { return this.value; }
  asId() { return this.value; }
  asNestedObjList() { return this.value; }
}
Object.assign(dependencies, {
  ParsedValue,
  assert:(ok,msg)=>assert.ok(ok,msg), isNotNull:x=>x!=null,
  lazyMemoized:f=>{let ready=false,value;return ()=>{if(!ready){value=f();ready=true;}return value;};},
  InstanceDirection:{IncomingFromServer:0,OutgoingToServer:1},
  AttributeModel:{getAttributeId:(m,n)=>Object.values(m.values).find(x=>x.name===n)?.id},
  AssociationReprType:{Aggregation:'aggregation'},
  getAssociationRepresentationType:x=>x,
  getIdType:()=>0, IdType:{SingleId:0,IdTuple:1},
  ValueTypeEnum:{String:'String',Bytes:'Bytes',CompressedString:'CompressedString'},
  CardinalityEnum:{One:'One',ZeroOrOne:'ZeroOrOne'},
  utf8Uint8ArrayToString:x=>new TextDecoder().decode(x),
  random:{generateRandomData:n=>new Uint8Array(n).fill(0x44)},
});
source += '\nreturn {CryptoMapper, DecryptedParsedInstance, AeadFacade, SymmetricKeyDeriver, InstanceDecryptor, SubKeyProvider, SubKeyInfoWithSessionKeyAead, SubKeyInfoWithGroupKeyAead, uint8ArrayTo256Key};';
const api = new Function(...Object.keys(dependencies),source)(...Object.values(dependencies));
const primitive = new api.AeadFacade(),deriver = new api.SymmetricKeyDeriver();
const key = api.uint8ArrayTo256Key(new Uint8Array(32).fill(0x11)),nonce = new Uint8Array(32).fill(0x22);
const cipher = {
  getSubKeyProvider:(info,type)=>new api.SubKeyProvider(info,deriver,type),
  encryptBytesWithAead:(keys,bytes,aad)=>primitive.encrypt(keys,bytes,aad),
};
const mapper = new api.CryptoMapper(cipher,null,null);
const value=(id,name,encrypted=true)=>({id,name,type:'String',encrypted,cardinality:'One'});
const mailModel = {app:'tutanota',id:97,name:'Mail',values:{105:value(105,'subject')},associations:{111:{id:111,type:'aggregation'}}};
const addressModel = {app:'tutanota',id:92,name:'MailAddress',values:{93:value(93,'_id',false),94:value(94,'name')},associations:{}};
const results=[], vectors=[];
for (const version of [2,3]) {
  const info = version===2 ? new api.SubKeyInfoWithGroupKeyAead({object:key,version:7},nonce) : new api.SubKeyInfoWithSessionKeyAead(key);
  for(const subject of ['Sujet écrit — 🦀','']) {
    const instance=api.DecryptedParsedInstance.outgoingToServer(mailModel);
    instance.addAttributeById(105,ParsedValue.fromString(subject));
    const aggregate=api.DecryptedParsedInstance.outgoingToServer(addressModel);
    aggregate.addAttributeById(93,ParsedValue.fromId('sender-id'));
    aggregate.addAttributeById(94,ParsedValue.fromString('Alice'));
    instance.addAttributeById(111,ParsedValue.fromNestedItems([aggregate]));
    const encrypted=await mapper.encryptParsedInstance(instance,info);
    for(const [path,plaintext,ciphertext] of [
      ['105',subject,encrypted.getAttributeById(105).asByteArray()],
      ['111/sender-id/94','Alice',encrypted.getAttributeById(111).asNestedObjList()[0].getAttributeById(94).asByteArray()],
    ]) {
      const decryptor=new api.InstanceDecryptor(key,nonce,mailModel,null,primitive,deriver);
      const decoded=decryptor.getValueDecryptor(ciphertext,path).getValue(version===2?key:null);
      assert.equal(new TextDecoder().decode(decoded),plaintext);
      vectors.push({version,path,plaintext,key:Buffer.alloc(32,0x11).toString('base64'),kdf_nonce:Buffer.alloc(32,0x22).toString('base64'),ciphertext:Buffer.from(ciphertext).toString('base64')});
    }
  }
  // Characterize the upstream write loop's sibling-path behavior explicitly.
  const instance=api.DecryptedParsedInstance.outgoingToServer(mailModel);
  instance.addAttributeById(105,ParsedValue.fromString('siblings'));
  instance.addAttributeById(111,ParsedValue.fromNestedItems(['first','second'].map(id=>{
    const a=api.DecryptedParsedInstance.outgoingToServer(addressModel);
    a.addAttributeById(93,ParsedValue.fromId(id));a.addAttributeById(94,ParsedValue.fromString(id));return a;
  })));
  const encrypted=await mapper.encryptParsedInstance(instance,info);
  const second=encrypted.getAttributeById(111).asNestedObjList()[1].getAttributeById(94).asByteArray();
  const decryptor=new api.InstanceDecryptor(key,nonce,mailModel,null,primitive,deriver);
  assert.throws(()=>decryptor.getValueDecryptor(second,'111/second/94').getValue(version===2?key:null));
  assert.equal(new TextDecoder().decode(decryptor.getValueDecryptor(second,'111/first/second/94').getValue(version===2?key:null)),'second');
  results.push({version,sibling_path_accumulation_reproduced:true,first_path:'111/first/94',second_written_path:'111/first/second/94',second_reader_expected_path:'111/second/94'});
}
writeFileSync(new URL('./mapper-vectors.json',import.meta.url),JSON.stringify(vectors,null,2));
writeFileSync(new URL('./mapper-results.json',import.meta.url),JSON.stringify({commit:metadata.commit,vectors:vectors.length,results,scope:'Actual upstream CryptoMapper.encryptParsedInstance/encryptValue/aggregate traversal, SubKeyProvider, AEAD, InstanceDecryptor; minimal parsed-value/model adapters. No whole app.'},null,2));
console.log('Official TS CryptoMapper: 8 v2/v3 write vectors verified; sibling path accumulation reproduced in both formats.');
