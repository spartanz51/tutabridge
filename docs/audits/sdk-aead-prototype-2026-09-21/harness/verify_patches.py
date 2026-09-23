from pathlib import Path
import subprocess, tempfile, os, json
phase=Path(__file__).resolve().parent
sdk=phase.parent/'bridge/tuta-repo'
out=Path('/Users/anthony/GIT/tutabridge/docs/audits/sdk-aead-prototype-2026-09-21')
base='aea5846b93a1412451e885bf99002401c3b087e8'
master='46270557c251d1a31157d72e0aaf0cc63bb33ecf'
results=[]
def verify(label, repo, ref, patches, expected=None):
 fd,index=tempfile.mkstemp(dir=phase); os.close(fd); os.unlink(index)
 env=dict(os.environ,GIT_INDEX_FILE=index)
 def git(*args): return subprocess.check_output(['git',*args],cwd=repo,env=env,stderr=subprocess.STDOUT)
 try:
  git('read-tree',ref)
  for patch in patches: git('apply','--cached','--whitespace=error',str(patch))
  tree=git('write-tree').decode().strip()
  if expected and tree != expected:raise RuntimeError(f'Tree mismatch: {tree} != {expected}')
  results.append(dict(label=label,base=ref,passed=True,tree=tree,patches=[p.name for p in patches]))
 except subprocess.CalledProcessError as error:
  results.append(dict(label=label,base=ref,passed=False,error=error.output.decode(),patches=[p.name for p in patches]))
 finally:Path(index).unlink(missing_ok=True)
patches=sorted((out/'patches').glob('*.patch'))
verify('full_series_359',sdk,base,patches,json.loads((phase/'trees.json').read_text())['v2'])
verify('full_series_master',sdk,master,patches)
verify('v3_standalone',sdk,base,patches[-2:-1])
verify('v2_on_v3_only',sdk,base,patches[-2:])
verify('bridge_overlay',phase.parent/'bridge','b375f1c275b8162ee8f04008144e3dd2c72a10d6',[out/'bridge.patch'])
(out/'reports/patch-application.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
