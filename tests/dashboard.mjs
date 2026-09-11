// No browser required: exercise the real embedded UI against a controlled transport.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const html=fs.readFileSync(new URL('../src/dashboard.html',import.meta.url),'utf8');
const script=html.split('<script>')[1].split('</script>')[0];
class Node {
 constructor(tag='div'){this.tagName=tag;this.children=[];this.dataset={};this.disabled=false;this.open=false;this.textContent='';this.id='';}
 append(...nodes){this.children.push(...nodes)}
 replaceChildren(...nodes){this.children=nodes}
 showModal(){this.open=true}
 close(){this.open=false}
}
const nodes=new Map();const buttons=[];
for(const id of ['connect','confirm-yes','confirm-no','cleanup']){const n=new Node('button');n.id=id;nodes.set(id,n);buttons.push(n)}
const snapshot={storage_failed:false,memory:{used:1,total:2,available:1,swap_used:0,safety_margin:0},cpu_percent:0,jobs:[],services:[],unknown:[]};
let statusFail=false,hold=null,calls=0,toolResult={status:'OK'};
const context=vm.createContext({console,Set,Date,JSON,Error,location:{hash:'',pathname:'/'},history:{replaceState(){}},sessionStorage:{getItem(){return 'test'},setItem(){}},setInterval(){},document:{getElementById(id){if(!nodes.has(id))nodes.set(id,new Node());return nodes.get(id)},createElement(tag){const n=new Node(tag);if(tag==='button')buttons.push(n);return n},querySelectorAll(selector){return selector==='button'?buttons:[]}},fetch:async(path,opts)=>{
 if(path==='/api/status')return {ok:!statusFail,status:401,json:async()=>structuredClone(snapshot)};
 calls++;if(hold)await hold.promise;
 return {ok:true,json:async()=>({result:{isError:false,structuredContent:toolResult}})};
}});
vm.runInContext(script,context);await new Promise(setImmediate);
assert.equal(nodes.get('cleanup').disabled,false,'online controls enabled');
let resolve;hold={promise:new Promise(r=>resolve=r)};
const one=vm.runInContext("action('services.start',{service_id:'demo'})",context);
await new Promise(setImmediate);
assert.equal(nodes.get('cleanup').disabled,true,'pending operation disables controls');
await vm.runInContext("action('services.start',{service_id:'demo'})",context);
assert.equal(calls,1,'duplicate click causes exactly one transport request');
resolve();await one;hold=null;
assert.equal(nodes.get('cleanup').disabled,false);
toolResult={results:[{service:'demo',result:'STOP_INCOMPLETE'}]};
await vm.runInContext("action('runtime.cleanup',{})",context);
assert.match(nodes.get('notice').textContent,/部分服務未能停止/,'cleanup failure must not claim successful stop');
statusFail=true;await vm.runInContext('refresh()',context);
assert.equal(nodes.get('cleanup').disabled,true);
assert.match(nodes.get('connection').textContent,/上次資料/);
const before=calls;await vm.runInContext("action('runtime.cleanup',{})",context);assert.equal(calls,before,'offline action makes no request');
statusFail=false;snapshot.storage_failed=true;await vm.runInContext('refresh()',context);
assert.equal(nodes.get('cleanup').disabled,true,'storage fault disables controls even while online');
snapshot.storage_failed=false;await vm.runInContext('refresh()',context);assert.equal(nodes.get('cleanup').disabled,false);assert.match(nodes.get('notice').textContent,/已連線/,'successful reconnect clears obsolete fault message');
console.log('PASS: pending/duplicate, cleanup failure, offline, expired token, storage fault, reconnect UI behavior');
