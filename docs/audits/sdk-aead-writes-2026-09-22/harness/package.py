from pathlib import Path
import subprocess, tempfile, os, json, hashlib, shutil
phase=Path(__file__).resolve().parent
root=phase.parent;sdk=root/'bridge/tuta-repo'
out=Path('/Users/anthony/GIT/tutabridge/docs/audits/sdk-aead-writes-2026-09-22');out.mkdir(exist_ok=True)
prior=out.parent/'sdk-aead-prototype-2026-09-21'
for name in ['prototype.py','test_prototype.py','network_probe.py','smoke_tuta_api.py','capability-baseline.json','bridge.patch']:
 shutil.copy2(prior/name,out/name)
for name in ['patches','proposals','baseline-evidence','adapter']:
 shutil.copytree(prior/name,out/name,dirs_exist_ok=True)
p=out/'prototype.py';s=p.read_text().replace("'--test','interactive_session_test']", "'--test','interactive_session_test','--test','aead_writes']");assert s!=p.read_text();p.write_text(s)
base='48579032ee69fe8c497089d9fb83bf661311df8f'
files8=['src/entities/entity_facade.rs','src/entities/entity_facade/decryption_keys.rs','src/entities/entity_facade/encryption.rs','tests/fixtures/aead_mapper_ts.json']
files9=['src/crypto_entity_client.rs','src/crypto_entity_client/aead_write.rs','tests/aead_writes.rs']
with tempfile.TemporaryDirectory() as tmp:
 env=dict(os.environ,GIT_INDEX_FILE=tmp+'/index')
 def git(*args):return subprocess.check_output(['git',*args],cwd=sdk,env=env)
 git('read-tree',base)
 git('add',*[f'tuta-sdk/rust/sdk/{name}' for name in files8])
 t8=git('write-tree').decode().strip()
 (out/'patches/08-aead-attribute-writes.patch').write_bytes(git('diff','--binary','--full-index',base,t8))
 git('add',*[f'tuta-sdk/rust/sdk/{name}' for name in files9])
 t9=git('write-tree').decode().strip()
 (out/'patches/09-aead-entity-writes.patch').write_bytes(git('diff','--binary','--full-index',t8,t9))
 git('add','tuta-sdk/rust/sdk')
 assert git('write-tree').decode().strip()==t9,'Unpackaged SDK source changes'
 (phase/'trees.json').write_text(json.dumps(dict(reads=base,attribute_writes=t8,entity_writes=t9),indent=2)+'\n')
shutil.copytree(phase/'ts-reference',out/'ts-reference',dirs_exist_ok=True)
(out/'reports').mkdir(exist_ok=True)
(out/'harness').mkdir(exist_ok=True)
for f in ['package.py','prepare_ts.py']:shutil.copy2(phase/f,out/'harness'/f)
for path in phase.glob('*.log'):shutil.copy2(path,out/'reports'/path.name)
for name in files8+files9:
 target=out/'sdk-source'/name;target.parent.mkdir(parents=True,exist_ok=True)
 shutil.copy2(sdk/'tuta-sdk/rust/sdk'/name,target)
files=['bridge.patch','prototype.py','test_prototype.py','network_probe.py','smoke_tuta_api.py','capability-baseline.json']+[str(p.relative_to(out)) for p in sorted((out/'patches').glob('*.patch'))]+[str(p.relative_to(out)) for p in sorted((out/'baseline-evidence').glob('*')) if p.is_file()]
(out/'manifest.json').write_text(json.dumps({'patches':[p for p in files if p.startswith('patches/')],'files':{p:hashlib.sha256((out/p).read_bytes()).hexdigest() for p in files}},indent=2)+'\n')
print(out);print((phase/'trees.json').read_text())
