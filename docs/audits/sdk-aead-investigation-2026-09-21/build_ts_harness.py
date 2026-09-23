from pathlib import Path
import subprocess,json,hashlib,sys
out=Path(__file__).resolve().parent; repo=Path(sys.argv[1]).resolve(); ref='46270557c251d1a31157d72e0aaf0cc63bb33ecf'
files=['encryption/symmetric/SymmetricCipherVersion.ts','encryption/symmetric/AesKey.ts','encryption/symmetric/ParsedCiphertext.ts','encryption/symmetric/SymmetricCipherUtils.ts','hashes/Blake3.ts','encryption/symmetric/SymmetricKeyDeriver.ts','encryption/symmetric/AeadFacade.ts','instance-pipeline-crypto/decryption/SubKeyCache.ts','instance-pipeline-crypto/decryption/ValueDecryptor.ts','instance-pipeline-crypto/decryption/InstanceDecryptor.ts']
sources=[]
for relative in files+['internal/sjcl.js','internal/noble-hashes-2.0.1.js']:
 path='src/platform-kit/crypto/'+relative
 data=subprocess.check_output(['git','show',ref+':'+path],cwd=repo)
 name=Path(relative).name
 if name.endswith('.js'):name=name[:-3]+'.mjs'
 (out/name).write_bytes(data)
 sources.append({'file':path,'local_file':name,'sha256':hashlib.sha256(data).hexdigest()})
(out/'ts-sources.json').write_text(json.dumps({'commit':ref,'sources':sources},indent=2))
