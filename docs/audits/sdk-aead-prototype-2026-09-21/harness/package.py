from pathlib import Path
import subprocess, shutil, tempfile, os, json, hashlib
phase=Path(__file__).resolve().parent
root=phase.parent
out=Path('/Users/anthony/GIT/tutabridge/docs/audits/sdk-aead-prototype-2026-09-21')
out.mkdir(exist_ok=True)
for name in ['prototype.py','test_prototype.py','network_probe.py','smoke_tuta_api.py','capability-baseline.json']:
 shutil.copy2(phase/'selector'/name,out/name)
for name in ['baseline-evidence','patches']:
 shutil.copytree(phase/'selector'/name,out/name,dirs_exist_ok=True)
shutil.copytree(root/'bridge/crates/tuta',out/'adapter',dirs_exist_ok=True)
prior=out.parent/'sdk-ts-parity-2026-09-21'
shutil.copytree(prior/'proposals',out/'proposals',dirs_exist_ok=True)
bridge=root/'bridge'
fd,index=tempfile.mkstemp(dir=phase);os.close(fd);os.unlink(index)
env=dict(os.environ,GIT_INDEX_FILE=index)
def git(*args): return subprocess.check_output(['git',*args],cwd=bridge,env=env)
try:
 git('read-tree','HEAD')
 paths=git('diff','--name-only').decode().splitlines()+git('ls-files','--others','--exclude-standard','crates/tuta').decode().splitlines()
 for path in paths:
  if path=='tuta-repo':continue
  git('add','--',path)
 (out/'bridge.patch').write_bytes(git('diff','--cached','--binary','--full-index'))
finally:Path(index).unlink(missing_ok=True)
files=['bridge.patch','prototype.py','test_prototype.py','network_probe.py','smoke_tuta_api.py','capability-baseline.json']+[str(p.relative_to(out)) for p in sorted((out/'patches').glob('*.patch'))]+[str(p.relative_to(out)) for p in sorted((out/'baseline-evidence').glob('*')) if p.is_file()]
(out/'manifest.json').write_text(json.dumps({'patches':[p for p in files if p.startswith('patches/')],'files':{p:hashlib.sha256((out/p).read_bytes()).hexdigest() for p in files}},indent=2)+'\n')
(out/'reports').mkdir(exist_ok=True)
for path in phase.glob('*-final.log'):shutil.copy2(path,out/'reports'/path.name)
shutil.copy2(phase/'selector-tests.log',out/'reports/selector-tests.log')
shutil.copy2(phase/'trees.json',out/'reports/patch-trees.json')
# Preserve the actual TS vector generator and its source hashes for reproducibility.
shutil.copytree(out.parent/'sdk-aead-investigation-2026-09-21',out/'ts-vector-provenance',dirs_exist_ok=True,ignore=shutil.ignore_patterns('SHA256SUMS','reports','sdk-source','*.tar.gz','*.zip','__pycache__'))
print(out)
