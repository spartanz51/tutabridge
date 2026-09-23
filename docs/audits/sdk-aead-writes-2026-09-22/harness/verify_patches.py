from pathlib import Path
import subprocess,tempfile,os,json,hashlib
phase=Path(__file__).resolve().parent;sdk=phase.parent/'bridge/tuta-repo'
out=Path('/Users/anthony/GIT/tutabridge/docs/audits/sdk-aead-writes-2026-09-22')
base='aea5846b93a1412451e885bf99002401c3b087e8';master='46270557c251d1a31157d72e0aaf0cc63bb33ecf'
patches=sorted((out/'patches').glob('*.patch'));trees=json.loads((phase/'trees.json').read_text());results=[]
for label,ref,ps in [('full_series_359',base,patches),('full_series_master',master,patches),('attributes_on_read_prototype',trees['reads'],patches[-2:-1]),('entity_writes_on_attributes',trees['attribute_writes'],patches[-1:])]:
 with tempfile.TemporaryDirectory() as tmp:
  env=dict(os.environ,GIT_INDEX_FILE=tmp+'/index')
  def git(*args):return subprocess.check_output(['git',*args],cwd=sdk,env=env,stderr=subprocess.STDOUT)
  git('read-tree',ref)
  for p in ps:git('apply','--cached','--whitespace=error',str(p))
  tree=git('write-tree').decode().strip()
  if label=='full_series_359':assert tree==trees['entity_writes']
  results.append(dict(label=label,base=ref,passed=True,tree=tree))
(out/'reports/patch-application.json').write_text(json.dumps(results,indent=2)+'\n')
# Cryptographic primitives and generated protocol types remain official upstream bytes.
paths=['tuta-sdk/rust/crypto-primitives','tuta-sdk/rust/sdk/src/entities/generated','tuta-sdk/rust/sdk/src/services/generated','tuta-sdk/rust/sdk/src/mail_facade.rs','tuta-sdk/rust/sdk/src/folder_system.rs','tuta-sdk/rust/sdk/src/services/service_executor.rs','Cargo.toml']
for path in paths:
 assert not subprocess.check_output(['git','diff','--name-only','HEAD','--',path],cwd=sdk).strip(),path
assert hashlib.sha256((out/'bridge.patch').read_bytes()).hexdigest()==hashlib.sha256((out.parent/'sdk-aead-prototype-2026-09-21/bridge.patch').read_bytes()).hexdigest()
print(json.dumps(results,indent=2));print('Protected upstream paths and existing bridge overlay unchanged')
