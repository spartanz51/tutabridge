from pathlib import Path
import subprocess,tempfile,os,json,shutil,hashlib
p=Path(__file__).resolve().parent;repo=p.parent/'bridge/tuta-repo';trees=json.loads((p/'trees.json').read_text())
# Include the raw-string aggregate-ID correction in the first proposal itself.
path='tuta-sdk/rust/sdk/src/entities/entity_facade.rs'
s=subprocess.check_output(['git','show',trees['v3']+':'+path],cwd=repo,text=True)
s=s.replace('ElementValue::IdCustomId(id) => Some(id.0.as_str()),','ElementValue::String(id) => Some(id.as_str()),\n\t\t\t\t\t\t\t\tElementValue::IdCustomId(id) => Some(id.0.as_str()),',1)
s=s.replace('ElementValue::IdCustomId(crate::CustomId("sender-id".into()))','ElementValue::String("sender-id".into())').replace('ElementValue::IdCustomId(crate::CustomId("moved".into()))','ElementValue::String("moved".into())')
blob=subprocess.check_output(['git','hash-object','-w','--stdin'],input=s,text=True,cwd=repo).strip()
with tempfile.TemporaryDirectory() as t:
 env=dict(os.environ,GIT_INDEX_FILE=t+'/index');subprocess.run(['git','read-tree',trees['v3']],cwd=repo,env=env,check=True)
 subprocess.run(['git','update-index','--add','--cacheinfo','100644',blob,path],cwd=repo,env=env,check=True)
 trees['v3_review']=subprocess.check_output(['git','write-tree'],cwd=repo,env=env,text=True).strip()
(p/'trees.json').write_text(json.dumps(trees,indent=2)+'\n')
for name,left,right in [('06-aead-session-reads.patch','before','v3_review'),('07-aead-group-reads.patch','v3_review','v2')]:
 data=subprocess.check_output(['git','diff','--binary',trees[left],trees[right]],cwd=repo)
 (p/'selector/patches'/name).write_bytes(data)
 print(name,subprocess.check_output(['git','diff','--shortstat',trees[left],trees[right]],cwd=repo,text=True).strip())
