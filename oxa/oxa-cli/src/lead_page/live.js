const badge=document.getElementById('connection');
const live=!!document.querySelector('meta[name=oxa-live-events]');
const digest=value=>typeof value==='string'&&/^[0-9a-f]{64}$/.test(value);
let runningSha=document.querySelector('meta[name=oxa-running-sha256]')?.content;
if(!digest(runningSha))runningSha=null;
let timer,pending,stream,reconnectTimer,streamOk=false,reloadNeeded=false;
function unavailable(){
  badge.textContent='Refresh unavailable';
  const notice=document.getElementById('refresh-error');if(notice)notice.hidden=false;
}
function reloadWhenIdle(){
  clearTimeout(timer);if(document.hidden)return;
  if(readerActive()||document.querySelector('#lead-message')?.value){
    badge.textContent='Update ready';timer=setTimeout(reloadWhenIdle,15000);
  }else location.reload();
}
function readerActive(){return document.querySelector('main').contains(document.activeElement)&&document.activeElement.matches('a,button,input,textarea,select,summary,[tabindex]');}
async function refresh(){
  clearTimeout(timer);
  if(document.hidden||(stream&&!streamOk))return;
  if(reloadNeeded){reloadWhenIdle();return;}
  if(readerActive()){timer=setTimeout(refresh,15000);return;}
  if(pending)pending.abort();
  const active=new AbortController();pending=active;
  const target=new URL(location.href);target.searchParams.set('fragment','1');
  try{
    const response=await fetch(target,{cache:'no-store',signal:active.signal});
    if(!response.ok)throw new Error('refresh refused');
    const doc=new DOMParser().parseFromString(await response.text(),'text/html');
    const next=doc.querySelector('main');
    if(!next)throw new Error('refresh unavailable');
    if(pending===active&&!active.signal.aborted&&!document.hidden&&!readerActive()&&(!stream||streamOk)){
      const current=document.querySelector('main'),work=current.querySelector('details.home-work');
      const replacement=next.querySelector('details.home-work');
      if(work&&replacement)replacement.open=work.open;
      const draft=current.querySelector('#lead-message');
      if(draft?.value){
        const nextDraft=next.querySelector('#lead-message');
        if(!nextDraft)return;
        nextDraft.value=draft.value;
      }
      const currentThread=current.querySelector('.thread');
      const scroll=currentThread?currentThread.scrollTop:0;
      for(const box of current.querySelectorAll('details.tools[open],details.sessions[open]')){
        const key=box.dataset.key;
        if(!key)continue;
        const same=next.querySelector(`details[data-key="${key}"]`);
        if(same)same.open=true;
      }
      current.replaceWith(next);badge.textContent='Connected';
      const fresh=document.querySelector('.thread');
      if(fresh&&currentThread)fresh.scrollTop=pinned?fresh.scrollHeight:scroll;
      if(pinned)pill.hidden=true;
      grow(document.getElementById('lead-message'));
    }
  }catch(error){if(!active.signal.aborted)unavailable();}
  finally{if(pending===active){pending=null;if(!document.hidden&&(!stream||readerActive()))timer=setTimeout(refresh,15000);}}
}
function connect(){
  stream=new EventSource('/events/live');streamOk=false;
  const activeStream=stream;
  const waitForEvent=()=>{reconnectTimer=null;if(stream===activeStream&&!document.hidden&&!streamOk)unavailable();};
  reconnectTimer=setTimeout(waitForEvent,15000);
  stream.addEventListener('lead-update',async event=>{
    if(document.hidden||stream!==activeStream)return;
    let value;
    try{
      if(typeof event.data!=='string'||event.data.length>128||/[\r\n]/.test(event.data))throw new Error();
      value=JSON.parse(event.data);
      if(!value||Array.isArray(value)||Object.keys(value).length!==1||!Object.prototype.hasOwnProperty.call(value,'running_sha256')||
        (value.running_sha256!==null&&!digest(value.running_sha256)))throw new Error();
    }catch{streamOk=false;unavailable();return;}
    streamOk=true;clearTimeout(reconnectTimer);reconnectTimer=null;
    if(value.running_sha256!==null){
      if(runningSha&&runningSha!==value.running_sha256)reloadNeeded=true;
      if(!reloadNeeded)runningSha=value.running_sha256;
    }
    if(reloadNeeded)reloadWhenIdle();else await refresh();
  });
  stream.onerror=()=>{
    if(stream!==activeStream)return;
    streamOk=false;if(!reconnectTimer)reconnectTimer=setTimeout(waitForEvent,15000);
  };
}
document.addEventListener('visibilitychange',()=>{
  clearTimeout(timer);clearTimeout(reconnectTimer);reconnectTimer=null;if(pending)pending.abort();
  if(stream){stream.close();stream=null;streamOk=false;}
  if(!document.hidden){if(live&&typeof EventSource==='function')connect();else refresh();}
});
if(live&&typeof EventSource==='function')connect();else timer=setTimeout(refresh,15000);

// Copy: a code block or a step's output copies its own rendered text. Nothing is read
// from the network and nothing is stored; the button only fills the clipboard.
document.addEventListener('click',event=>{
  const button=event.target.closest&&event.target.closest('.copy[data-copy]');
  if(!button)return;
  const box=button.closest('.out')||button.parentElement;
  const text=box.querySelector('pre,code')?.textContent||box.textContent||'';
  const done=()=>{
    button.textContent='Copied';
    setTimeout(()=>{button.textContent='Copy';},1600);
  };
  if(navigator.clipboard&&navigator.clipboard.writeText)navigator.clipboard.writeText(text.replace(/Copy$/,'').trim()).then(done,()=>{});
});

// Chat (#2341): the composer grows with its text, the thread stays pinned to the newest
// message unless the reader scrolled up, and a pill says when new messages wait below. The
// composer still posts the same form; nothing here changes a request.
const composer=document.getElementById('lead-message'),thread=document.querySelector('.thread');
const pill=document.createElement('a');pill.className='pill new';pill.href='#thread-end';
pill.textContent='New messages ↓';pill.hidden=true;
let pinned=true;
function grow(box){
  if(!box||box.tagName!=='TEXTAREA')return;
  box.style.height='auto';box.style.height=Math.min(Math.max(box.scrollHeight,46),120)+'px';
}
function near(){return !thread||thread.scrollHeight-thread.scrollTop-thread.clientHeight<80;}
if(thread&&thread.parentNode){
  thread.parentNode.appendChild(pill);
  const end=document.createElement('div');end.id='thread-end';thread.appendChild(end);
  thread.addEventListener('scroll',()=>{const at=near();pinned=at;if(at)pill.hidden=true;},{passive:true});
  // Start at the newest message, after the first layout has given the thread a height.
  if(typeof requestAnimationFrame==='function')requestAnimationFrame(()=>{thread.scrollTop=thread.scrollHeight;});
  else thread.scrollTop=thread.scrollHeight;
  pill.addEventListener('click',()=>{thread.scrollTop=thread.scrollHeight;pill.hidden=true;pinned=true;});
  // Every open chat fold keeps its state across a refresh, keyed by what it holds.
  for(const box of document.querySelectorAll('details.tools,details.sessions')){
    const first=box.querySelector('b,summary span,summary');
    box.dataset.key=(box.className+(first?first.textContent:'')).slice(0,120);
  }
}
if(composer){
  grow(composer);
  composer.addEventListener('input',()=>grow(composer));
  composer.addEventListener('keydown',event=>{
    // Enter sends on a desktop keyboard, where Shift+Enter still adds a line.
    if(event.key==='Enter'&&!event.shiftKey&&!event.isComposing&&window.matchMedia('(pointer:fine)').matches){
      event.preventDefault();
      if(composer.value.trim())composer.form.requestSubmit();
    }
  });
  if(window.visualViewport)window.visualViewport.addEventListener('resize',()=>{
    // Keep the composer above the on-screen keyboard instead of under it.
    const inset=Math.max(0,window.innerHeight-window.visualViewport.height-window.visualViewport.offsetTop);
    document.documentElement.style.setProperty('--keyboard',inset+'px');
  },{passive:true});
}
