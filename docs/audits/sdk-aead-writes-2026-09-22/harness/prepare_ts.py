from pathlib import Path
import shutil,subprocess,json,hashlib
p=Path(__file__).resolve().parent
sdk=p.parent/'bridge/tuta-repo'
out=p/'ts-reference';out.mkdir(exist_ok=True)
prior=Path('/Users/anthony/GIT/tutabridge/docs/audits/sdk-aead-investigation-2026-09-21')
metadata=json.loads((prior/'ts-sources.json').read_text())
for item in metadata['sources']:shutil.copy2(prior/item['local_file'],out/item['local_file'])
for source in ['src/platform-kit/instance-pipeline/CryptoMapper.ts','src/platform-kit/crypto/instance-pipeline-crypto/encryption/SubKeyProvider.ts','src/platform-kit/network/EntityRestClient.ts','src/platform-kit/network/ServiceExecutor.ts','src/platform-kit/instance-pipeline/InstancePipeline.ts','doc/HACKING.md','rustfmt.toml','.editorconfig']:
 data=subprocess.check_output(['git','show',f'{metadata["commit"]}:{source}'],cwd=sdk)
 local=source.rsplit('/',1)[-1]
 (out/local).write_bytes(data)
 metadata['sources'].append(dict(file=source,local_file=local,sha256=hashlib.sha256(data).hexdigest()))
(out/'ts-sources.json').write_text(json.dumps(metadata,indent=2)+'\n')
# Reuse the previously audited primitive import/error/utility adapters.
s=(prior/'verify_ts.mjs').read_text()
s=s[:s.index('source +=')]
s=s.replace("metadata.sources.filter(x => x.local_file.endsWith('.ts'))", "metadata.sources.filter(x => x.local_file.endsWith('.ts') && !['EntityRestClient.ts','ServiceExecutor.ts','InstancePipeline.ts'].includes(x.local_file))")
# Above split hit source concatenation inside the loop, so use the known boundary instead.
s=(prior/'verify_ts.mjs').read_text().split("source += '\\nreturn")[0]
s=s.replace("metadata.sources.filter(x => x.local_file.endsWith('.ts'))", "metadata.sources.filter(x => x.local_file.endsWith('.ts') && !['EntityRestClient.ts','ServiceExecutor.ts','InstancePipeline.ts'].includes(x.local_file))")
(out/'verify_writes.mjs').write_text(s)
