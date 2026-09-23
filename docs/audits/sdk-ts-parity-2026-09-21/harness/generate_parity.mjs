// Execute the audited upstream functions, unchanged except type erasure.
import {execFileSync} from 'node:child_process';
import {stripTypeScriptTypes} from 'node:module';
import {createHash} from 'node:crypto';
import {writeFileSync} from 'node:fs';
const [repo, output] = process.argv.slice(2);
if (!repo || !output) throw new Error('usage: node generate_parity.mjs SDK_REPOSITORY OUTPUT_JSON');
const ref='46270557c251d1a31157d72e0aaf0cc63bb33ecf';
const blobs='src/applications/common/api/worker/facades/lazy/BlobFacade.ts';
const rest='src/platform-kit/network/EntityRestClient.ts';
const login='src/platform-kit/base/facades/LoginFacade.ts';
const utils='src/platform-kit/utils/PromiseUtils.ts';
const sourceHashes={};
function source(path){const s=execFileSync('git',['show',`${ref}:${path}`],{cwd:repo,encoding:'utf8'});sourceHashes[path]=createHash('sha256').update(s).digest('hex');return s;}
function exported(path,name){const s=source(path),start=s.indexOf(`export ${name}`);if(start<0) throw Error(name);return s.slice(start,s.indexOf('\n}',start)+2).replace(/^export /,'');}
function method(path,name){const s=source(path),start=s.indexOf(`\tprivate ${name}`);if(start<0) throw Error(name);return s.slice(start,s.indexOf('\n\t}',start)+3).replace('private ','');}
const code=[exported(blobs,'function parseMultipleBlobsResponse'),exported(rest,'async function tryServers'),exported(rest,'async function doBlobRequestWithRetry'),exported(utils,'function ofClass'),`class LoginProbe {${method(login,'getSessionElementId')}${method(login,'getSessionListId')}}`].join('\n');
const classes=['ConnectionError','InternalServerError','NotFoundError','NotAuthorizedError','NotAuthenticatedError','InvalidSoftwareVersionError'];
const errors=Object.fromEntries(classes.map(n=>[n,{[n]:class extends Error{constructor(){super(n);this.name=n;}}}[n]]));
const b64=bytes=>Buffer.from(bytes).toString('base64');
const ext=value=>value.replace(/[+/]/g,c=>c==='+'?'-':'_').replace(/=+$/,'');
// BASE64_EXT alphabet differs from base64url; use the repository's documented alphabet.
const standard='ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const extended='-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz';
const toExt=s=>s.replace(/=+$/,'').split('').map(c=>extended[standard.indexOf(c)]).join('');
const deps={...errors,console:{log(){}},uint8ArrayToBase64:b64,base64ToBase64Ext:toExt,base64ToBase64Url:ext,base64UrlToBase64:s=>s.replace(/-/g,'+').replace(/_/g,'/'),base64ToUint8Array:s=>new Uint8Array(Buffer.from(s,'base64')),neverNull:x=>{if(x==null)throw Error('null');return x},sha256Hash:x=>new Uint8Array(createHash('sha256').update(x).digest()),GENERATED_ID_BYTES_LENGTH:9};
const js=stripTypeScriptTypes(code,{mode:'strip'});
const api=new Function(...Object.keys(deps),js+';return {parseMultipleBlobsResponse,tryServers,doBlobRequestWithRetry,LoginProbe};')(...Object.values(deps));
function wire(entries,count=entries.length){const parts=[Buffer.alloc(4)];parts[0].writeInt32BE(count);for(const [id,data] of entries){const header=Buffer.alloc(19);Buffer.from(id).copy(header);header.writeInt32BE(data.length,15);parts.push(header,Buffer.from(data));}return Buffer.concat(parts);}
const one=wire([[Array(9).fill(1),[0,127,255]]]);
const badSize=Buffer.from(one);badSize.writeInt32BE(-1,19);
const entries=[
 ['empty',wire([])],['one',one],['two',wire([[Array(9).fill(2),[]],[Array(9).fill(1),[0,127,255]]])],
 ['truncated-count',Buffer.alloc(3)],['negative-count',wire([],-1)],['huge-count',wire([],2147483647)],['wrong-count',wire([[Array(9).fill(1),[1]]],2)],['truncated-payload',one.subarray(0,-1)],['negative-size',badSize],
 ['zero-with-trailing',Buffer.from([0,0,0,0,1]),false],['duplicate-hidden-by-map',wire([[Array(9).fill(1),[1]],[Array(9).fill(1),[2]]],1),false],
];
const binary=entries.map(([name,b,rustAccept])=>{const data=Uint8Array.from(b);try{const result=api.parseMultipleBlobsResponse(data);return {name,hex:b.toString('hex'),ts_accept:true,rust_accept:rustAccept??true,entries:[...result].map(([id,data])=>[id,Buffer.from(data).toString('hex')])};}catch(e){return {name,hex:b.toString('hex'),ts_accept:false,rust_accept:rustAccept??false,error:e.name};}});
const retries=[];
for(const statuses of [[500,403,500,403],[404,200],[401,200],[474,200],[403,200],[500,404],[0,200],[403,403]]){
 let i=0,evictions=0;const calls=[];
 const invoke=()=>api.tryServers([{url:'first'},{url:'second'}],async url=>{calls.push(url);const status=statuses[i++];if(status===200)return 'ok';throw new errors[{0:'ConnectionError',500:'InternalServerError',404:'NotFoundError',403:'NotAuthorizedError',401:'NotAuthenticatedError',474:'InvalidSoftwareVersionError'}[status]]();},'test');
 try{await api.doBlobRequestWithRetry(invoke,()=>evictions++);retries.push({statuses,calls,evictions,error:null});}catch(e){retries.push({statuses,calls,evictions,error:e.name});}
}
const token='ZC2NIBDACUABAdJhibIwclzaPU3fEu-NzQ';const p=new api.LoginProbe();
const session={token,list:p.getSessionListId(token),element:p.getSessionElementId(token)};
const report={reference:ref,source_hashes:sourceHashes,binary,retries,session};
writeFileSync(output,JSON.stringify(report,null,2)+'\n');
process.stdout.write(JSON.stringify({binary:binary.length,retries:retries.length,session,output})+'\n');
