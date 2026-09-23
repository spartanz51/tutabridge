from pathlib import Path
import os,subprocess,tempfile,json,sys
p=Path(__file__).resolve().parent;repo=p.parent/'bridge/tuta-repo'
with tempfile.TemporaryDirectory() as tmp:
 env=dict(os.environ,GIT_INDEX_FILE=tmp+'/index')
 subprocess.run(['git','read-tree','HEAD'],cwd=repo,env=env,check=True)
 subprocess.run(['git','add','tuta-sdk/rust/sdk'],cwd=repo,env=env,check=True)
 tree=subprocess.check_output(['git','write-tree'],cwd=repo,env=env,text=True).strip()
state=p/'trees.json'; data=json.loads(state.read_text()) if state.exists() else {}
data[sys.argv[1]]=tree;state.write_text(json.dumps(data,indent=2)+'\n');print(sys.argv[1],tree)
