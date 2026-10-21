export function createClient({ credential, fetchImpl = globalThis.fetch.bind(globalThis), requestId = () => crypto.randomUUID() }) {
  async function call(tool, spaceId, args = {}, id = requestId()) {
    const sentCredential = credential();
    const payload = { request_id: id, tool, ...(spaceId ? { space_id: spaceId } : {}), args };
    const response = await fetchImpl('/v2/tools', { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${sentCredential}` }, body: JSON.stringify(payload), cache: 'no-store' });
    const value = await response.json();
    if (credential() !== sentCredential) throw new Error('登入身分已變更，忽略先前的回覆');
    if (!response.ok) {
      const error = new Error(value.error?.message || `HTTP ${response.status}`);
      error.code = value.error?.code || 'HttpError';
      error.status = response.status;
      throw error;
    }
    return value;
  }
  return { call, listSpaces: () => call('spaces.list'), getSettings: () => call('settings.get'), setSettings: settings => call('settings.set', null, {settings}),discover:()=>call('runtime.discover') };
}

export function createActionCommand(requestId = () => crypto.randomUUID()) {
  let previous;
  return { prepare(topic, action, comment, artifactRef) {
    if (!comment.trim()) throw new Error('請填寫留言／操作理由');
    const args = { topic_id: topic.id, action, comment, expected_revision: topic.revision };
    if (action === 'submit_review') {
      if (!artifactRef.trim()) throw new Error('送審必須提供成果版本引用');
      args.artifact_ref = artifactRef.trim();
    }
    if (['approve_review', 'request_changes', 'pass_test', 'fail_test'].includes(action)) {
      if (!topic.current_submission) throw new Error('此帖子尚無可驗證的成果版本');
      args.expected_submission = typeof topic.current_submission === 'object' ? topic.current_submission.id : topic.current_submission;
    }
    const signature = JSON.stringify(args);
    if (previous?.signature !== signature) previous = { signature, request_id: requestId(), args };
    return { request_id: previous.request_id, args: { ...previous.args } };
  }};
}

export function createLeaseKeeper(renew) {
  let lease = null;
  return {
    assign(value) { lease = value; }, clear() { lease = null; }, current() { return lease; },
    async heartbeat() {
      const previous = lease;
      if (!previous) return;
      try { const renewed = await renew(previous); if (lease === previous) lease = {...previous,...renewed}; }
      catch (error) { if (lease === previous) lease = null; throw error; }
    }
  };
}

export function shutdownView(result) {
  return { confirm: result.status === 'prepared', force: result.status === 'pending', message: `Runtime 狀態：${result.status}；程序停止仍未確認。\n${JSON.stringify(result,null,2)}` };
}

export function createDecompositionJob(run, jobId = () => crypto.randomUUID()) {
  let state = null;
  let attempt = null;
  return {
    current() { return state ? {...state} : null; },
    async inspect(get, knownId, expectedMission) {
      const previous=state || (knownId?{job_id:knownId,status:'unknown',topic_ids:[]}:null);
      if(!previous)throw new Error('尚無拆分 job');
      const mission=expectedMission||previous.mission_topic_id;
      const result=await get({job_id:previous.job_id,mission_topic_id:mission});
      if(mission&&result.mission_topic_id!==mission)throw new Error('job 回覆屬於其他母任務');
      if(result.job_id!==previous.job_id)throw new Error('job 回覆 ID 不符');
      state={...previous,...result};if(attempt)attempt.inspected=true;return {...state};
    },
    async recover(recover) {
      if(!state||state.recovery?.eligibility!=='eligible')throw new Error('此 job 目前不可恢復，請先查詢既有結果');
      if(attempt?.unknown&&!attempt.inspected)throw new Error('恢復結果尚未確認，請先查詢既有 job');
      const id=attempt?.unknown?attempt.id:jobId();
      attempt={id,unknown:true,inspected:false};
      try{const result=await recover({job_id:state.job_id,mission_topic_id:state.mission_topic_id},id);
        if(result.job_id!==state.job_id||result.mission_topic_id!==state.mission_topic_id)throw new Error('恢復回覆範圍不符');
        state={...state,...result};attempt.unknown=false;return {...state};
      }catch(error){state={...state,status:'unknown',error:error.message};throw error;}
    },
    async start(missionId) {
      if (state) throw new Error('此母任務已有單次拆分 job，請查詢既有結果；不自動重跑');
      state = {job_id:jobId(),mission_topic_id:missionId,status:'unknown',topic_ids:[]};
      try { const result=await run({job_id:state.job_id,mission_topic_id:missionId});state={...state,...result};return {...state}; }
      catch(error) {state={...state,status:'unknown',error:error.message};throw error;}
    }
  };
}

export function canDecompose(identity) {
  return identity?.kind !== 'owner' && Array.isArray(identity?.roles) && identity.roles.includes('task_decomposer');
}

export function createPagedView(fetchPage) {
  let state={items:[],next_cursor:null},initialized=false,followTail=false,queue=Promise.resolve();
  async function read(append) {
    if(append&&initialized&&state.next_cursor==null)return {...state};
    if(append&&initialized){
      const page=await fetchPage({cursor:state.next_cursor});
      state={...page,items:[...state.items,...page.items]};
      if(page.next_cursor==null)followTail=true;
    }else{
      const target=Math.max(1,state.items.length),wasFollowing=followTail;
      let page=await fetchPage({}),items=[...page.items];
      const seen=new Set();
      while(page.next_cursor!=null&&items.length<target){
        if(seen.has(page.next_cursor))throw new Error('分頁 cursor 未前進');seen.add(page.next_cursor);
        page=await fetchPage({cursor:page.next_cursor});items.push(...page.items);
      }
      // At most one extra page per refresh catches up new tail without unbounded reads.
      if(wasFollowing&&page.next_cursor!=null){page=await fetchPage({cursor:page.next_cursor});items.push(...page.items);}
      state={...page,items};
      if(!initialized)followTail=page.next_cursor==null;
    }
    initialized=true;return {...state};
  }
  return {current:()=>({...state}),read(append=false){const next=queue.then(()=>read(append));queue=next.catch(()=>{});return next;}};
}

export function eventReferences(event) {
  const refs=[...(event.artifact_refs||[])];
  const evidence=event.submission||event.current_submission||event.artifact_ref;
  if(evidence)refs.push(typeof evidence==='object'?`${evidence.id} · ${evidence.artifact_ref||''}`:String(evidence));
  return [...new Set(refs)];
}

export function endpointMetadata(endpoint) {
  return JSON.stringify({status:endpoint.status??'未提供',session:endpoint.session??null,capabilities:endpoint.capabilities??[]},null,2);
}

export function createMemberHeartbeat(send,requestId=()=>crypto.randomUUID()) {
  let state=null,busy=false,interval=2000;const registrationId=requestId();
  return {current:()=>state,intervalMs:()=>interval,async beat(){
    if(busy)return state;busy=true;
    try{const result=await send(state?{attachment_id:state.attachment_id}:{},state?undefined:registrationId);state=result;interval=Math.min(20000,Math.max(1000,Math.floor((result.heartbeat_seconds??6)*1000/3)));return result;}finally{busy=false;}
  }};
}

export function createHistoryReader(query) {
  let target=null,epoch=0,state={events:[],checkpoint:null,next_cursor:0};
  return {select(value){target=value?{...value}:null;epoch++;state={events:[],checkpoint:null,next_cursor:0};},current:()=>({...state}),async read(first=false){
    if(!target)throw new Error('請選擇歷史 Session');const owner=epoch,cursor=first?0:state.next_cursor;
    try{const page=await query({...target,cursor,limit:100});if(owner!==epoch)return null;state={...page};return {...state};}
    catch(error){if(owner===epoch){state={events:[],checkpoint:null,next_cursor:0};}throw error;}
  }};
}

export function createExplicitRequest(send,requestId=()=>crypto.randomUUID()){
  let state=null;
  return {current:()=>state?{...state}:null,async start(tool,args){
    if(state)throw new Error('此操作已有 request，請查詢結果；不會自動重送');
    state={request_id:requestId(),status:'unknown'};
    try{const value=await send(tool,args,state.request_id);state={...state,...value};return value;}catch(error){state={...state,status:'unknown',error:error.message};throw error;}
  }};
}

export function watchStartRequest(watchId,endpointId,interval){
  if(!watchId?.trim())throw new Error('請輸入 Watch ID');
  if(!Number.isInteger(interval)||interval<30||interval>86400)throw new Error('Watch 間隔必須30–86400秒');
  return {request_id:watchId,tool:'sources.watch.start',args:{endpoint_id:endpointId,interval_seconds:interval}};
}

if (typeof document !== 'undefined') startConsole();

function startConsole() {
  const $ = id => document.getElementById(id);
  const roles = ['human_admin','session_maintainer','manager','task_decomposer','implementer','reviewer','consultant','tester','observer'];
  const labels = {human_admin:'人類管理員',session_maintainer:'Session 維護者',manager:'主控',task_decomposer:'任務拆分者',implementer:'實作',reviewer:'Reviewer',consultant:'顧問',tester:'測試',observer:'Observer'};
  let token = '', space = null, topic = null, basis = null, identity = null, view = 'forum', endpoint = null, outputCursor = 0, shutdownTicket = null, pollBusy = false, observeBusy = false, decompositionBusy = false, decompositionEpoch = 0, leaseExpires = 0;
  const pages = {}, drafts = new Map(), roleDrafts = new Map(), jobs = new Map(), completedJobs = new Set(),ranges=new Map(),heartbeats=new Map();
  let spaces = [], topics = [], comments = [];
  let historyOwner=null,historySessions=[],sourceOperation=null,sourceSnapshot=null,watchSnapshot=null,sourcePollBusy=false;
  const restartOperations=new Map();
  const historyReader=createHistoryReader(args=>scoped('history.read',args));
  const actionCommand = createActionCommand();
  const client = createClient({ credential: () => token });
  const staleDecompositionResponse = new Error('畫面範圍已變更，忽略先前拆分回覆');
  let spacesRange=createPagedView(args=>client.call('spaces.list',null,args));
  function range(key,read){if(!ranges.has(key)){const owner=space?.id;ranges.set(key,createPagedView(args=>{if(space?.id!==owner)throw new Error('分頁所屬空間已變更');return read(args);}));}return ranges.get(key);}
  const leaseKeeper = createLeaseKeeper(value => scoped('terminal.renew', value));
  function node(tag, text, className) { const n = document.createElement(tag); if (text != null) n.textContent = text; if (className) n.className = className; return n; }
  function notice(message, error = false) { $('notice').textContent = message; $('notice').classList.toggle('error', error); }
  function clearDecomposition(){decompositionEpoch++;jobs.clear();$('decompositionJobId').value='';$('decompositionState').textContent='';$('recoveryPurpose').textContent='';$('decompositionChildren').replaceChildren();$('recoverDecomposition').hidden=true;}
  function failure(error) { if(error===staleDecompositionResponse)return;if(['Denied','Unauthenticated'].includes(error.code)){clearDecomposition();$('decompositionState').textContent='目前未獲授權，已清除先前拆分結果。';}notice(`${error.code || 'Error'}：${error.message}`, true); }
  async function run(action, button) {
    if (button) button.disabled = true;
    try { await action(); } catch (error) { failure(error); } finally { if (button) button.disabled = false; renderDecomposition(); renderSourceActions(); }
  }
  function bindForm(id, action) { $(id).addEventListener('submit', event => { event.preventDefault(); run(() => action(new FormData(event.currentTarget)), event.submitter); }); }
  function bindButton(id, action) { $(id).addEventListener('click', event => run(action, event.currentTarget)); }
  function textValue(form, name) { return String(form.get(name) || '').trim(); }
  function requireSpace() { if (!space) throw new Error('請先選擇任務空間'); return space.id; }
  async function scoped(tool, args = {}, requestId) {
    const selected = requireSpace(), credential = token;
    const result = await client.call(tool, selected, args, requestId);
    if (space?.id !== selected || credential !== token) throw new Error('畫面範圍已變更，已忽略舊回覆');
    return result;
  }
  function showSecrets(value) {
    $('secretFields').replaceChildren();
    function append(data, prefix = '') {
      for (const [key, item] of Object.entries(data || {})) {
        if (item && typeof item === 'object') { append(item, `${prefix}${key}.`); continue; }
        const label = node('label', `${prefix}${key}`), input = node('input');
        input.value = item == null ? '無' : String(item); input.readOnly = true;
        input.type = /credential|token/.test(key) ? 'password' : 'text'; input.autocomplete = 'off';
        label.append(input); $('secretFields').append(label);
        if (key === 'credential' && item) {
          const button = node('button', `使用 ${data.name || prefix} credential 登入`);
          button.addEventListener('click', () => { const credential = String(item); closeSecrets(); run(() => login(credential)); });
          $('secretFields').append(button);
        }
      }
    }
    append(value); $('secrets').showModal();
  }
  function closeSecrets() { $('secretFields').replaceChildren(); $('secrets').close(); }
  $('secrets').addEventListener('cancel', () => $('secretFields').replaceChildren());
  bindButton('closeSecrets', closeSecrets);
  async function health() {
    try { const response = await fetch('/v2/health', { cache: 'no-store' }); if (!response.ok) throw new Error(); $('connection').textContent = 'Runtime 已連線 · 論壇 5 秒／終端即時'; }
    catch { $('connection').textContent = shutdownTicket ? '連線中斷 · 關閉結果未確認' : 'Runtime 無法連線'; }
  }
  function resetData() {
    saveDraft(); spaces = []; topics = []; comments = []; space = null; topic = null; basis = null; endpoint = null; leaseKeeper.clear(); leaseExpires=0; shutdownTicket = null;
    drafts.clear(); roleDrafts.clear(); jobs.clear(); completedJobs.clear(); ranges.clear();heartbeats.clear();spacesRange=createPagedView(args=>client.call('spaces.list',null,args));identity=null; for (const key of Object.keys(pages)) delete pages[key];
    $('decompositionPanel').hidden=true;$('decompositionJobId').value='';$('decompositionState').textContent='';$('decompositionChildren').replaceChildren();
    for (const id of ['topics','comments','members','activity','endpoints','output']) $(id).replaceChildren();
    $('spaceSelect').replaceChildren(); $('topicDetail').hidden = true; $('observation').hidden = true; $('confirmShutdown').hidden = true; $('forceShutdown').hidden = true;
    $('comment').value = ''; $('artifact').value = ''; $('stdin').value = ''; $('shutdownImpact').textContent = '';
    for (const id of ['topicForm','spaceForm','terminalForm','inviteForm','settingsForm']) { $(id).reset(); const details=$(id).closest('details'); if(details)details.open=false; }
    $('settingsForm').hidden = true; $('settingsMeta').textContent = ''; $('settingSources').replaceChildren();
    $('discoveryCandidates').replaceChildren();$('discoveryMeta').textContent='';
    clearEndpointExtras();restartOperations.clear();
    closeSecrets();
  }
  async function login(credential) {
    await releaseControl();
    token = credential; resetData();
    try { await loadSpaces(); $('loginPanel').hidden = true; $('workspace').hidden = false; $('logout').hidden = false; notice('已通過 Runtime 驗證；操作受目前 credential 權限限制。'); }
    catch (error) { token = ''; $('loginPanel').hidden = false; $('workspace').hidden = true; throw error; }
  }
  bindForm('loginForm', async () => { const credential = $('credential').value.trim(); $('credential').value = ''; await login(credential); });
  bindButton('logout', async () => { await releaseControl(); token = ''; resetData(); $('workspace').hidden = true; $('loginPanel').hidden = false; $('logout').hidden = true; $('registerForm').reset(); notice('已清除 credential 與本頁工作資料。'); });
  bindForm('registerForm', async form => {
    const invite = textValue(form, 'invite_token'), client = createClient({ credential: () => invite });
    const registered = await client.call('members.register', textValue(form, 'space'), { name: textValue(form, 'name') });
    $('registerForm').reset(); showSecrets(registered); notice('註冊成功，請保存 credential。');
  });
  async function loadSpaces(append = false) {
    const value = await spacesRange.read(append);
    spaces = value.items; pages.spaces = value.next_cursor;
    $('spaceSelect').replaceChildren(node('option','請選擇空間'));
    $('spaceSelect').firstChild.value = '';
    spaces.forEach(item => { const option = node('option', item.name); option.value = item.id; $('spaceSelect').append(option); });
    $('spaceSelect').value = space?.id || ''; $('moreSpaces').hidden = !pages.spaces;
    if (!space && spaces.length) await selectSpace(spaces[0].id);
  }
  bindButton('moreSpaces', () => loadSpaces(true));
  $('spaceSelect').addEventListener('change', event => run(() => selectSpace(event.target.value)));
  async function selectSpace(id) {
    await releaseControl();
    clearDecomposition();saveDraft(); space = spaces.find(item => item.id === id) || null; topic = null; basis = null; identity=null; endpoint = null; topics = []; comments = [];
    ranges.clear();
    clearEndpointExtras();
    $('spaceSelect').value = space?.id || '';
    $('topicDetail').hidden = true; $('observation').hidden = true; $('spaceName').textContent = space?.name || '選擇空間'; $('spaceId').textContent = space?.id || '';
    for (const key of ['topics','comments','activity','members','endpoints']) { delete pages[key]; $(key).replaceChildren(); }
    if (space) await refresh();
  }
  bindForm('spaceForm', async form => {
    const args = { name: textValue(form,'name') };
    for (const key of ['administrator_name','manager_name']) if (textValue(form,key)) args[key] = textValue(form,key);
    const result = await client.call('spaces.create', null, args); $('spaceForm').reset(); await loadSpaces(); await selectSpace(result.space.id); showSecrets(result); notice('空間已建立。請保存人類管理員與可選主控 credential。');
  });
  document.querySelectorAll('[data-view]').forEach(button => button.addEventListener('click', () => run(async () => {
    if (button.dataset.view !== 'endpoints') await releaseControl();
    view = button.dataset.view;
    document.querySelectorAll('[data-view]').forEach(item => item.setAttribute('aria-current', item === button ? 'page' : 'false'));
    for (const name of ['forum','activity','members','endpoints','settings']) $(`view-${name}`).hidden = name !== view;
    await refresh();
  })));
  async function refresh() {
    if (view === 'settings') return;
    if (!space) return;
    identity = await scoped('identity.current');
    await heartbeatCurrent();
    if (view === 'forum') { await loadTopics(); if (topic) await readTopic(topic.id, false, true); }
    if (view === 'activity') await loadActivity();
    if (view === 'members') await loadMembers();
    if (view === 'endpoints') await loadEndpoints();
  }
  bindButton('refresh', async () => { await health(); await refresh(); notice('已讀取 Runtime 最新資料；未送出的留言保留。'); });
  async function heartbeatCurrent(){
    if(!token||!space||!identity||identity.kind==='owner')return;
    const key=`${space.id}/${identity.id}`;let entry=heartbeats.get(key);
    if(!entry){const owner=space.id;entry={next:0,beat:createMemberHeartbeat((args,id)=>{if(space?.id!==owner)throw new Error('browser attachment 所屬空間已變更');return scoped('members.heartbeat',args,id);})};heartbeats.set(key,entry);}
    if(Date.now()<entry.next)return;entry.next=Date.now()+entry.beat.intervalMs();
    try{await entry.beat.beat();}finally{entry.next=Date.now()+entry.beat.intervalMs();}
  }
  setInterval(()=>{if(token&&space&&identity)heartbeatCurrent().catch(failure);},500);
  function currentKey() { return space && topic ? `${space.id}/${topic.id}` : null; }
  function saveDraft() { const key = currentKey(); if (key && basis) drafts.set(key, { comment: $('comment').value, artifact: $('artifact').value, action: $('action').value, basis }); }
  for (const id of ['comment','artifact','action']) $(id).addEventListener('input', saveDraft);
  async function loadTopics(append = false) {
    const result = await range('topics',args=>scoped('topics.list',args)).read(append);
    topics = result.items; pages.topics = result.next_cursor;
    $('topics').replaceChildren();
    if (!topics.length) $('topics').append(node('p','此空間尚無帖子。建立第一帖開始協作。','muted'));
    for (const item of topics) {
      const button = node('button',item.title); button.classList.toggle('selected',item.id === topic?.id);
      button.append(node('small',`${item.kind} · ${item.state} · r${item.revision}`));
      button.addEventListener('click', () => run(() => readTopic(item.id))); $('topics').append(button);
    }
    $('moreTopics').hidden = !pages.topics;
    const select = $('parentTopic');
    if (select) { const old = select.value; select.replaceChildren(node('option','無（獨立任務）')); select.firstChild.value = ''; for (const item of topics) { const option = node('option', item.title); option.value = item.id; select.append(option); } select.value = old; }
  }
  bindButton('moreTopics', () => loadTopics(true));
  async function readTopic(id, append = false, polling = false) {
    const oldId = topic?.id;
    if(!polling&&!append&&oldId!==id)clearDecomposition();
    if (!polling && !append) saveDraft();
    const result = await range(`comments:${id}`,async args=>{const value=await scoped('topics.read',{topic_id:id,...args});return {...value,items:value.comments};}).read(append);
    if (polling && topic?.id !== id) return;
    topic = result.topic; comments = result.items; pages.comments = result.next_cursor;
    if (!polling && !append && oldId !== id) {
      $('decompositionJobId').value='';
      const draft = drafts.get(currentKey()); basis = draft?.basis || structuredClone(topic);
      $('comment').value = draft?.comment || ''; $('artifact').value = draft?.artifact || ''; $('action').value = draft?.action || 'comment';
    }
    if (!basis) basis = structuredClone(topic);
    renderTopic();
  }
  function describeSubmission(value) { return value ? `${value.id || value} · ${value.artifact_ref || ''}` : '無'; }
  function renderEvent(event) {
    const article = node('article');
    article.dataset.eventId=event.id;
    article.append(node('p',`${event.author?.name || event.author || '未提供作者'} · ${event.created_at || event.timestamp || ''} · ${event.action || 'comment'}`,'muted'));
    article.append(node('p',event.comment || event.body || event.text || '','prose'));
    if(event.reply_to){const button=node('button',`回覆：${event.reply_to}`);button.className='reply-reference';button.addEventListener('click',()=>{const target=[...$('comments').children].find(item=>item.dataset.eventId===event.reply_to);if(target)target.scrollIntoView({block:'center'});else notice(`回覆對象 ${event.reply_to} 尚未載入此帖子畫面，請開啟原帖並載入更多留言。`);});article.append(button);}
    for(const evidence of eventReferences(event))article.append(node('p',`成果引用：${evidence}`,'artifact-reference'));
    if (event.self_review) article.append(node('p','self_review：同一身分審查','warning'));
    return article;
  }
  function renderTopic() {
    $('topicDetail').hidden = false; $('topicTitle').textContent = topic.title; $('topicBody').textContent = topic.body;
    $('topicMeta').textContent = `${topic.kind} · ${topic.state} · 認領者 ${topic.assignee || '無'} · r${topic.revision} · 作者 ${topic.author?.name || topic.author} · 建立 ${topic.created_at} · 更新 ${topic.updated_at} · 成果 ${describeSubmission(topic.current_submission)}`;
    const parentLink = $('parentLink'); if (parentLink) parentLink.remove();
    if (topic.parent_topic_id) { const button = node('button',`母任務：${topic.parent_topic_id}`); button.id='parentLink'; button.addEventListener('click', () => run(() => readTopic(topic.parent_topic_id))); $('topicBody').after(button); }
    if (topic.dependencies?.length) $('topicMeta').textContent += ` · 依賴 ${topic.dependencies.join(', ')}`;
    $('comments').replaceChildren(...comments.map(renderEvent)); $('moreComments').hidden = !pages.comments;
    $('actionBasis').textContent = `操作基準 r${basis.revision} · 成果 ${describeSubmission(basis.current_submission)}。權限及可用狀態由 Runtime 檢查。`;
    $('staleDraft').hidden = basis.revision === topic.revision;
    for (const option of $('action').options) option.disabled = topic.kind !== 'task' && option.value !== 'comment';
    document.querySelectorAll('#topics button').forEach(button => button.classList.toggle('selected', button.firstChild?.textContent === topic.title));
    renderDecomposition();
  }
  function jobKey(){return space&&topic?`${space.id}/${topic.id}`:null;}
  function decompositionCall(name,args,id){
    const key=jobKey(),epoch=decompositionEpoch,credential=token;
    const current=()=>jobKey()===key&&decompositionEpoch===epoch&&token===credential;
    return scoped(name,args,id).then(value=>{if(!current())throw staleDecompositionResponse;return value;},
      error=>{if(!current())throw staleDecompositionResponse;throw error;});
  }
  function makeJob(){return createDecompositionJob(args=>decompositionCall('agent.run',args));}
  function renderDecomposition(){
    const allowed=canDecompose(identity)&&['plan','task'].includes(topic?.kind);
    $('decompositionPanel').hidden=!allowed;if(!allowed){clearDecomposition();return;}
    const state=jobs.get(jobKey())?.current();$('runDecomposition').disabled=Boolean(state)||decompositionBusy;
    $('decompositionJobId').readOnly=Boolean(state);if(state)$('decompositionJobId').value=state.job_id;
    $('decompositionState').textContent=state?JSON.stringify(state,null,2):'尚未開始。執行一次會啟動真實 Luna low 工作；請保存 job ID。';
    $('recoverDecomposition').hidden=state?.recovery?.eligibility!=='eligible';$('recoverDecomposition').disabled=decompositionBusy;
    const reasons={missing_durable_plan:'缺少已保存計畫，無法恢復剩餘任務',invalid_saved_plan:'已保存計畫未通過驗證',incompatible_saved_plan:'已保存計畫版本或內容不相容',receipt_plan_conflict:'已建立子任務與保存計畫不一致',unresolved_receipt_evidence:'部分建立結果無法核對',unresolved_child_evidence:'部分已知子任務無法核對'};
    $('recoveryPurpose').textContent=state?`既有 job ${state.job_id}；已知子任務 ${state.topic_ids?.length||0} 筆。${state.recovery?.publication==='pending'?'結果尚未成功保存，請先查詢；不能視為完成。':state.recovery?.eligibility==='completed'?'已完成已保存計畫，所有子任務建立結果均已確認。':state.recovery?.eligibility==='needs_attention'?`需要處理：${reasons[state.recovery.reason]||state.recovery.reason}。保留已知子任務。`:'恢復會沿用已保存計畫，繼續建立剩餘子任務。'}`:'';
    $('decompositionChildren').replaceChildren();
    for(const childId of state?.topic_ids||[]){const button=node('button',`子任務 ${childId}`);button.addEventListener('click',()=>run(()=>readTopic(childId)));$('decompositionChildren').append(button);}
  }
  bindButton('runDecomposition',async()=>{
    if(!canDecompose(identity)||!['plan','task'].includes(topic?.kind))throw new Error('此登入身分未具有 task_decomposer 或未選擇母任務');
    const key=jobKey();let job=jobs.get(key);if(!job){job=makeJob();jobs.set(key,job);}
    decompositionBusy=true;
    try{const pending=job.start(topic.id);renderDecomposition();await pending;notice('Runtime 已接受單次拆分；尚未代表子任務建立完成。');}
    finally{decompositionBusy=false;if(jobKey()===key)renderDecomposition();}
  });
  async function inspectDecomposition(){
    if(decompositionBusy||!canDecompose(identity)||!topic)return;
    const key=jobKey(),mission=topic.id;let job=jobs.get(key);const knownId=$('decompositionJobId').value.trim();
    if(!job){if(!knownId)throw new Error('請先執行一次，或輸入既有 job ID 補讀');job=makeJob();jobs.set(key,job);}
    decompositionBusy=true;
    try{const result=await job.inspect(args=>decompositionCall('agent.get',args),knownId,mission);
      if(result.status==='completed'&&!completedJobs.has(result.job_id)){completedJobs.add(result.job_id);await loadTopics();notice('單次拆分已完成，Runtime 已回傳子任務；尚未執行實作、審查或測試。');}
      if(result.status==='failed')notice(`單次拆分失敗：${typeof result.error==='string'?result.error:JSON.stringify(result.error)}；不自動重跑。`,true);
    }finally{decompositionBusy=false;if(jobKey()===key)renderDecomposition();}
  }
  bindButton('inspectDecomposition',inspectDecomposition);
  bindButton('recoverDecomposition',async()=>{
    if(!canDecompose(identity)||!topic)throw new Error('目前未具 task_decomposer 權限');
    const key=jobKey(),job=jobs.get(key);if(!job)throw new Error('請先查詢既有 job');
    decompositionBusy=true;
    try{await job.recover((args,id)=>decompositionCall('agent.recover',args,id));notice('已提交明確恢復；沿用已保存計畫，請查詢確認結果。');}
    finally{decompositionBusy=false;if(jobKey()===key)renderDecomposition();}
  });
  setInterval(()=>{const state=jobs.get(jobKey())?.current();if(token&&view==='forum'&&!document.hidden&&canDecompose(identity)&&state&&['queued','accepted','running','applying','unknown'].includes(state.status)&&!decompositionBusy)inspectDecomposition().catch(failure);},2000);
  bindButton('moreComments', () => readTopic(topic.id,true));
  bindButton('rebase', async () => { if (!topic) return; await readTopic(topic.id,false,true); basis = structuredClone(topic); saveDraft(); renderTopic(); notice('已更新操作基準；草稿保持原樣，請確認成果與內容後送出。'); });
  bindForm('topicForm', async form => {
    const args = { title: textValue(form,'title'), body: textValue(form,'body'), kind: textValue(form,'kind') };
    if (textValue(form,'parent_topic_id')) args.parent_topic_id = textValue(form,'parent_topic_id');
    if (textValue(form,'dependencies')) args.dependencies = textValue(form,'dependencies').split(/[\s,]+/).filter(Boolean);
    const result = await scoped('topics.create', args); $('topicForm').reset(); $('newTopic').open = false; await loadTopics(); await readTopic((result.topic || result).id); notice('帖子已由 Runtime 建立。');
  });
  bindForm('actionForm', async () => {
    if (!basis) throw new Error('請先選擇帖子'); saveDraft();
    const submittedKey=currentKey(),submittedTopic=topic.id;
    const submitted={comment:$('comment').value,artifact:$('artifact').value,action:$('action').value};
    const command = actionCommand.prepare(basis,$('action').value,$('comment').value,$('artifact').value);
    await scoped('task.act',command.args,command.request_id);
    const matches=draft=>draft&&draft.comment===submitted.comment&&draft.artifact===submitted.artifact&&draft.action===submitted.action;
    if(matches(drafts.get(submittedKey)))drafts.delete(submittedKey);
    if(currentKey()===submittedKey&&matches({comment:$('comment').value,artifact:$('artifact').value,action:$('action').value})){
      $('comment').value='';$('artifact').value='';basis=null;
      await readTopic(submittedTopic,false,true);saveDraft();
    }
    await loadTopics(); notice('Runtime 已提交留言與操作。');
  });
  async function loadActivity(append = false) {
    const result = await range('activity',args=>scoped('activity.read',args)).read(append); pages.activity = result.next_cursor;
    $('activity').replaceChildren();
    if (!result.items.length) $('activity').append(node('p','此空間尚無活動。','muted'));
    for (const event of result.items) { const article = renderEvent(event); const button=node('button',`開啟帖子 ${event.topic_id}`); button.addEventListener('click',() => run(async () => { document.querySelector('[data-view="forum"]').click(); await readTopic(event.topic_id); })); article.prepend(button); $('activity').append(article); }
    $('moreActivity').hidden = !pages.activity;
  }
  bindButton('moreActivity', () => loadActivity(true));
  function rolePicker(container, selected = []) {
    container.classList.add('role-grid');
    for (const role of roles) { const label=node('label',labels[role]),input=node('input'); input.type='checkbox'; input.name='roles'; input.value=role; input.checked=selected.includes(role); label.prepend(input); container.append(label); }
  }
  rolePicker($('inviteRoles'));
  async function loadMembers(append = false) {
    const result = await range('members',args=>scoped('members.list',args)).read(append); pages.members=result.next_cursor;
    $('members').replaceChildren();
    for (const member of result.items) {
      const item=node('article',null,'member'),form=node('form'),picker=node('fieldset'); picker.append(node('legend','角色集合（可兼任）')); rolePicker(picker,roleDrafts.get(member.id) || member.roles);
      form.addEventListener('change', () => roleDrafts.set(member.id,new FormData(form).getAll('roles')));
      item.append(node('h3',member.name),node('small',`${member.id} · ${member.created_at || ''}`));
      item.append(node('p',`連線回報 ${member.connection_state??'未提供'} · last_seen ${member.last_seen??'未回報'} · liveness_kind ${member.liveness_kind??'未提供'}`,'member-liveness'));
      if(member.attachments?.length){const details=node('details');details.append(node('summary',`Attachments（${member.attachments.length}）`),node('pre',JSON.stringify(member.attachments,null,2)));item.append(details);}
      form.append(picker,node('button','儲存角色')); form.addEventListener('submit',event => {event.preventDefault();run(async () => {await scoped('members.set_roles',{member_id:member.id,roles:new FormData(form).getAll('roles')}); roleDrafts.delete(member.id); notice('Runtime 已更新成員角色。'); await loadMembers();},event.submitter);}); item.append(form);
      if (member.grants) {
        const grants=node('details');
        grants.className='member-grants';
        grants.append(node('summary',`端點授權（${member.grants.length}）`));
        for(const grant of member.grants) grants.append(node('p',`${grant.endpoint_name} · ${grant.endpoint_id} · ${grant.operations.join(' / ')} · 來源：${grant.source==='space_manager_role'?'任務空間管理角色':'獨立端點授權'}`));
        if(!member.grants.length) grants.append(node('p','目前沒有端點授權。'));
        item.append(grants);
      }
      else item.append(node('p','特定 Endpoint grant 未由此成員查詢回傳；不推導為具備控制權。','muted'));
      $('members').append(item);
    }
    $('moreMembers').hidden=!pages.members;
  }
  bindButton('moreMembers',()=>loadMembers(true));
  bindForm('inviteForm',async form=>{const result=await scoped('members.invite',{roles:form.getAll('roles')});showSecrets({space_id:space.id,...result});notice('邀請已建立，請保存並提供給受邀者。');});
  async function loadEndpoints(append=false) {
    const result=await range('endpoints',args=>scoped('endpoints.list',args)).read(append);pages.endpoints=result.next_cursor;
    $('endpoints').replaceChildren();
    if(!result.items.length)$('endpoints').append(node('p','此空間沒有可列出的端點。','muted'));
    for(const item of result.items){const card=node('article',null,'endpoint'),button=node('button','唯讀觀察');card.append(node('h3',item.name),node('p',`${item.id} · ${item.kind}`,'muted'),node('pre',endpointMetadata(item),'endpoint-metadata'));button.addEventListener('click',()=>run(async()=>{await releaseControl();endpoint=item;outputCursor=0;$('output').textContent='';$('endpointName').textContent=`${item.name} · ${item.id}`;$('observation').hidden=false;await observe();}));if(item.kind==='terminal')card.append(button);if(item.kind==='github'){const sourceButton=node('button','檢視 GitHub 來源');sourceButton.addEventListener('click',()=>{$('githubEndpoint').value=item.id;clearSourceResults();$('githubPanel').scrollIntoView({block:'start'});});card.append(sourceButton);}if(item.kind==='terminal'){const historyButton=node('button','查看歷次 Session／保存紀錄');historyButton.className='history-button';historyButton.addEventListener('click',()=>run(()=>openHistory(item)));card.append(historyButton);}$('endpoints').append(card);}
    $('moreEndpoints').hidden=!pages.endpoints;updateGithubOptions(result.items);
  }
  bindButton('moreEndpoints',()=>loadEndpoints(true));
  function clearHistory(){historyOwner=null;historySessions=[];historyReader.select(null);$('historyPanel').hidden=true;$('historySession').replaceChildren();for(const id of ['historyOutput','historyMeta','historyCheckpointMeta','historyEndpoint','restartState'])$(id).textContent='';}
  function clearSourceResults(){sourceOperation=null;sourceSnapshot=null;watchSnapshot=null;for(const id of ['sourceJobState','sourceWatchState'])$(id).textContent='';$('sourceTopics').replaceChildren();$('sourceWatchList').replaceChildren();$('sourceJobId').value='';$('sourceWatchId').value=crypto.randomUUID();}
  function clearSourceReadData(){sourceSnapshot=null;watchSnapshot=null;for(const id of ['sourceJobState','sourceWatchState'])$(id).textContent='';$('sourceTopics').replaceChildren();$('sourceWatchList').replaceChildren();}
  function clearEndpointExtras(){clearHistory();clearSourceResults();$('githubEndpoint').replaceChildren();}
  function historyCurrent(owner){return historyOwner===owner&&space?.id===owner?.space_id;}
  async function openHistory(item){clearHistory();historyOwner={space_id:requireSpace(),endpoint_id:item.id};$('historyPanel').hidden=false;$('historyEndpoint').textContent=`${item.name} · ${item.id}`;await loadHistorySessions();}
  async function loadHistorySessions(){const owner=historyOwner;if(!owner)throw new Error('請先選擇 Endpoint 歷史');try{const result=await scoped('history.list',{endpoint_id:owner.endpoint_id});if(!historyCurrent(owner))return;historySessions=result.items;const old=$('historySession').value;$('historySession').replaceChildren();for(const item of historySessions){const option=node('option',`${item.session.id} · ${item.checkpoint.status} · generation ${item.session.generation}`);option.value=item.session.id;$('historySession').append(option);}if(historySessions.some(item=>item.session.id===old))$('historySession').value=old;else if(historySessions.length)$('historySession').value=historySessions.at(-1).session.id;if(!historySessions.length){$('historyMeta').textContent='沒有可讀取的已保存 Session。';historyReader.select(null);}else{selectHistorySession();await readHistory(true);}}catch(error){if(historyCurrent(owner))clearHistory();throw error;}}
  function selectHistorySession(){if(!historyOwner)return;historyReader.select({endpoint_id:historyOwner.endpoint_id,session_id:$('historySession').value});$('historyOutput').textContent='';$('historyCheckpointMeta').textContent='';}
  async function readHistory(first=false){const owner=historyOwner;if(!owner)return;try{const page=await historyReader.read(first);if(!page||!historyCurrent(owner))return;const checkpoint=page.checkpoint||{};$('historyMeta').textContent=`Session ${$('historySession').value} · Gap=${String(page.gap)} · retained_from=${page.retained_from} · next_cursor=${page.next_cursor} · unknown_tail=${String(checkpoint.unknown_tail)} · process_resume=${checkpoint.process_resume??'unsupported'}（不續接原模型聊天）`;$('historyOutput').textContent=page.events.map(event=>`[${event.cursor} ${event.kind} ${event.at}]\n${event.bytes?.length?new TextDecoder().decode(new Uint8Array(event.bytes)):JSON.stringify(event.detail)}`).join('\n').slice(-250000);$('historyNext').disabled=!page.events.length;$('historyCheckpointMeta').textContent=JSON.stringify(checkpoint,null,2);}catch(error){if(historyCurrent(owner))clearHistory();throw error;}}
  $('historySession').addEventListener('change',()=>run(async()=>{selectHistorySession();await readHistory(true);}));
  bindButton('reloadHistorySessions',loadHistorySessions);bindButton('historyFirst',()=>readHistory(true));bindButton('historyNext',()=>readHistory());
  bindButton('historyCheckpoint',async()=>{const owner=historyOwner,session_id=$('historySession').value;if(!owner||!session_id)throw new Error('請選擇 Session');try{const checkpoint=await scoped('history.checkpoint',{endpoint_id:owner.endpoint_id,session_id});if(historyCurrent(owner)&&$('historySession').value===session_id)$('historyCheckpointMeta').textContent=JSON.stringify(checkpoint,null,2);}catch(error){if(historyCurrent(owner))clearHistory();throw error;}});
  function renderSourceActions(){$('syncSource').disabled=Boolean(sourceOperation?.current());const owner=historyOwner,latest=historySessions.at(-1)?.session.id;$('restartSession').disabled=!owner||!latest||Boolean(restartOperations.get(`${owner.space_id}/${owner.endpoint_id}/${latest}`)?.current());}
  bindButton('restartSession',async()=>{const owner=historyOwner,latest=historySessions.at(-1)?.session.id;if(!owner||!latest)throw new Error('請先讀取最新 Session 清單');if(!confirm(`為 Endpoint ${owner.endpoint_id} 啟動新的 Session？\n舊Session ${latest} 的保存紀錄仍保留；不支援續接原模型聊天。仍存活的程序會由伺服器拒絕。`))return;const key=`${owner.space_id}/${owner.endpoint_id}/${latest}`;let operation=restartOperations.get(key);if(!operation){operation=createExplicitRequest((tool,args,id)=>scoped(tool,args,id));restartOperations.set(key,operation);}try{const result=await operation.start('terminal.restart',{endpoint_id:owner.endpoint_id,expected_session_id:latest});if(!historyCurrent(owner))return;$('restartState').textContent=`已建立新 Session ${result.session.id}；previous_session_id=${result.previous_session_id}；process_resume=${result.process_resume}。這不是原模型聊天續接。`;await loadHistorySessions();await loadEndpoints();}catch(error){if(historyCurrent(owner)){$('restartState').textContent=`${error.code||'Error'}：${error.message}。request=${operation.current()?.request_id}；先重新查詢歷史，不會自動重送。`;if(error.code==='Denied'||error.code==='Unauthenticated')clearHistory();}throw error;}});
  function updateGithubOptions(items){const old=$('githubEndpoint').value;$('githubEndpoint').replaceChildren();for(const item of items.filter(item=>item.kind==='github')){const option=node('option',`${item.name} · ${item.config?.repository||item.id}`);option.value=item.id;$('githubEndpoint').append(option);}if([...$('githubEndpoint').options].some(option=>option.value===old))$('githubEndpoint').value=old;else if(old!==$('githubEndpoint').value)clearSourceResults();}
  function selectedSource(){const id=$('githubEndpoint').value;if(!id)throw new Error('請選擇 GitHub Endpoint');return id;}
  function sourceOwner(){return {space_id:requireSpace(),endpoint_id:selectedSource()};}
  function sourceCurrent(owner){return space?.id===owner.space_id&&$('githubEndpoint').value===owner.endpoint_id;}
  async function sourceCall(owner,tool,args,id){try{const value=await scoped(tool,args,id);if(!sourceCurrent(owner))throw new Error('GitHub Endpoint 範圍已變更，忽略舊回覆');if(value.endpoint_id&&value.endpoint_id!==owner.endpoint_id)throw new Error('回覆屬於其他 GitHub Endpoint');return value;}catch(error){if(sourceCurrent(owner))clearSourceReadData();throw error;}}
  $('githubEndpoint').addEventListener('change',clearSourceResults);
  bindForm('githubForm',async form=>{await scoped('endpoints.register',{name:textValue(form,'name'),kind:'github',config:{repository:textValue(form,'repository'),allow_write_back:false}});$('githubForm').reset();$('githubForm').parentElement.open=false;await loadEndpoints();notice('GitHub 來源已登記，尚未同步；遠端寫回未啟用。');});
  function showSourceJob(snapshot){sourceSnapshot=snapshot;$('sourceJobState').textContent=JSON.stringify(snapshot,null,2);$('sourceTopics').replaceChildren();for(const id of snapshot.topic_ids||[]){const button=node('button',`開啟來源帖子 ${id}`);button.addEventListener('click',()=>run(async()=>{document.querySelector('[data-view="forum"]').click();await readTopic(id);}));$('sourceTopics').append(button);}}
  bindButton('newSyncRequest',()=>{if(sourceOperation?.current()&&(!sourceSnapshot||['running','applying','unknown'].includes(sourceSnapshot.status)))throw new Error('既有 request 結果未確認，請先查詢；不會替換 ID 重送同步');sourceOperation=null;sourceSnapshot=null;$('sourceJobId').value='';$('sourceJobState').textContent='已準備新的明確同步 request；尚未送出。';});
  bindButton('syncSource',async()=>{const owner=sourceOwner();if(!sourceOperation)sourceOperation=createExplicitRequest((tool,args,id)=>sourceCall(owner,tool,args,id));const operation=sourceOperation;try{const pending=operation.start('sources.sync',{endpoint_id:owner.endpoint_id});$('sourceJobId').value=operation.current().request_id;const result=await pending;if(sourceCurrent(owner))showSourceJob(result);}catch(error){if(sourceCurrent(owner)){$('sourceJobId').value=operation.current().request_id;$('sourceJobState').textContent=JSON.stringify(operation.current(),null,2);}throw error;}});
  async function inspectSourceJob(){const owner=sourceOwner(),job_id=$('sourceJobId').value.trim();if(!job_id)throw new Error('請輸入既有 Source job ID');const result=await sourceCall(owner,'sources.get',{job_id});if($('sourceJobId').value.trim()===job_id)showSourceJob(result);}
  bindButton('inspectSourceJob',inspectSourceJob);
  function watchId(){const id=$('sourceWatchId').value.trim();if(!id)throw new Error('請輸入 Watch ID');return id;}
  function showWatch(value){watchSnapshot=value;$('sourceWatchState').textContent=JSON.stringify(value,null,2);}
  bindButton('startSourceWatch',async()=>{const owner=sourceOwner(),id=watchId(),command=watchStartRequest(id,owner.endpoint_id,Number($('sourceWatchInterval').value));if(watchSnapshot?.status==='unknown'&&!confirm('這個Watch在重啟後狀態未知。重新授權會先檢查既有job，再啟動新的唯讀同步輪次；不重跑原Unknown job。確認重新授權？'))return;const result=await sourceCall(owner,command.tool,command.args,command.request_id);showWatch(result);});
  bindButton('getSourceWatch',async()=>{const owner=sourceOwner(),id=watchId(),result=await sourceCall(owner,'sources.watch.get',{watch_id:id});if(watchId()===id)showWatch(result);});
  bindButton('stopSourceWatch',async()=>{const owner=sourceOwner(),id=watchId(),result=await sourceCall(owner,'sources.watch.stop',{watch_id:id});if(watchId()===id)showWatch(result);});
  bindButton('listSourceWatches',async()=>{const owner=sourceOwner(),result=await sourceCall(owner,'sources.watch.list',{});$('sourceWatchList').replaceChildren();for(const watch of result.items){const button=node('button',`${watch.watch_id} · ${watch.status} · endpoint ${watch.endpoint_id}`);button.addEventListener('click',()=>{if(watch.endpoint_id!==selectedSource()){notice('此 Watch 屬於其他 Endpoint，請先切換對應來源再查詢。',true);return;}$('sourceWatchId').value=watch.watch_id;$('sourceWatchInterval').value=watch.interval_seconds;showWatch(watch);});$('sourceWatchList').append(button);}});
  setInterval(async()=>{if(!token||view!=='endpoints'||document.hidden||sourcePollBusy)return;sourcePollBusy=true;try{if(sourceSnapshot&&['running','applying'].includes(sourceSnapshot.status))await inspectSourceJob();if(watchSnapshot&&['active','stopping'].includes(watchSnapshot.status)){const owner=sourceOwner(),id=watchSnapshot.watch_id,result=await sourceCall(owner,'sources.watch.get',{watch_id:id});if(watchId()===id)showWatch(result);}}catch(error){failure(error);}finally{sourcePollBusy=false;}},3000);
  bindButton('discoverCommands',async()=>{
    const result=await client.discover();$('discoveryCandidates').replaceChildren();
    $('discoveryMeta').textContent=`startup_enabled：${String(result.startup_enabled)} · startup_candidates：${JSON.stringify(result.startup_candidates??[])} · 本次候選 ${result.candidates.length} 筆`;
    for(const candidate of result.candidates){const card=node('article',null,'candidate'),button=node('button','填入建立終端表單');card.append(node('h3',candidate.name),node('p',`${candidate.status} · connected=${String(candidate.connected)} · ${candidate.command}`,'prose'),node('p',`${(candidate.capabilities||[]).join(', ')} · ${candidate.attachment||''}`,'muted'));button.addEventListener('click',()=>{const form=$('terminalForm');form.elements.name.value=candidate.name;form.elements.command.value=candidate.command;form.elements.args.value='';form.elements.cwd.value='';form.parentElement.open=true;form.scrollIntoView({block:'center'});notice('已填入候選命令，尚未建立或連線；請確認參數後明確按「建立並執行」。');});card.append(button);$('discoveryCandidates').append(card);}
    if(!result.candidates.length)$('discoveryCandidates').append(node('p','Runtime 未回傳可用命令候選。','muted'));
    notice('已取得 Runtime 候選；探測不代表連線或 Agent 已執行。');
  });
  async function observe(){
    if(!endpoint||observeBusy)return;
    const id=endpoint.id;observeBusy=true;
    try{
      const result=await scoped('terminal.observe',{endpoint_id:id,cursor:outputCursor});if(endpoint?.id!==id)return;
      outputCursor=result.next_cursor;
      if(result.text)$('output').textContent=($('output').textContent+result.text).slice(-250000);
      $('outputMeta').textContent=`Session ${result.session_id} · ${result.status} · cursor ${result.next_cursor} · ${result.gap?'Gap：部分輸出無法補讀':'此次回覆未回報 Gap'} · 更新 ${new Date().toLocaleTimeString('zh-TW')}`;
    }catch(error){
      if(endpoint?.id===id){endpoint=null;leaseKeeper.clear();leaseExpires=0;$('output').textContent='';$('outputMeta').textContent='觀察失敗或被撤權，已清除畫面並停止補讀。';}
      throw error;
    }finally{observeBusy=false;}
  }
  setInterval(()=>{if(token&&endpoint&&view==='endpoints'&&!document.hidden&&!observeBusy)observe().catch(failure);},150);
  bindButton('observeRefresh',observe);
  bindForm('terminalForm',async form=>{const args={name:textValue(form,'name'),command:textValue(form,'command'),args:String(form.get('args')||'').split(/\r?\n/).filter(Boolean)};if(textValue(form,'cwd'))args.cwd=textValue(form,'cwd');await scoped('terminal.create',args);$('terminalForm').reset();$('terminalForm').parentElement.open=false;await loadEndpoints();notice('Runtime 已建立 owned 終端。');});
  bindButton('acquire',async()=>{if(!endpoint)throw new Error('請先選擇端點');const result=await scoped('terminal.acquire',{endpoint_id:endpoint.id});leaseKeeper.assign({endpoint_id:endpoint.id,lease:result.lease,generation:result.generation});leaseExpires=Date.now()+60000;notice(`已取得 ${endpoint.name} 控制 lease。此控制區開啟時每 20 秒續期。`);});
  function controlArgs(){const lease=leaseKeeper.current();if(!endpoint||!lease||lease.endpoint_id!==endpoint.id||Date.now()>=leaseExpires){leaseKeeper.clear();throw new Error('控制 lease 未取得或已過期，請重新取得；不會自動重送輸入');}return {...lease};}
  bindForm('terminalInput',async()=>{await scoped('terminal.write',{...controlArgs(),input:$('stdin').value+($('appendEnter').checked?'\r':'')});$('stdin').value='';notice('輸入已由 Runtime 接受。');await observe();});
  async function releaseControl(){const lease=leaseKeeper.current();leaseKeeper.clear();leaseExpires=0;if(!lease||!token||!space)return true;try{await scoped('terminal.release',lease);return true;}catch(error){failure(new Error(`釋放控制未確認：${error.message}；本頁已停止續期，伺服器 lease 最遲 60 秒到期。`));return false;}}
  bindButton('release',async()=>{if(await releaseControl())notice('本頁已釋放控制，停止續期。');});
  $('controlPanel').addEventListener('toggle',()=>{if(!$('controlPanel').open)run(releaseControl);});
  setInterval(async()=>{if(!token||view!=='endpoints'||document.hidden||!$('controlPanel').open||!leaseKeeper.current())return;try{await leaseKeeper.heartbeat();if(leaseKeeper.current())leaseExpires=Date.now()+60000;}catch(error){leaseExpires=0;failure(new Error(`控制續期失敗，已清除 lease：${error.message}。請重新取得控制；不會重送輸入。`));}},20000);
  setInterval(()=>{if(!leaseKeeper.current()){$('leaseStatus').textContent='無有效控制 lease；輸入會被拒絕。';return;}const seconds=Math.max(0,Math.ceil((leaseExpires-Date.now())/1000));if(!seconds){leaseKeeper.clear();$('leaseStatus').textContent='控制 lease 已過期。請重新取得；不會重送輸入。';}else $('leaseStatus').textContent=`控制 lease 剩餘 ${seconds} 秒；${document.hidden?'分頁隱藏，暫停續期':'此控制區開啟時每 20 秒續期'}。`;},1000);
  bindButton('stop',async()=>{if(!endpoint)throw new Error('請先選擇端點');if(!confirm(`停止 owned 終端「${endpoint.name}」？這將停止其程序。`))return;await scoped('terminal.stop',{endpoint_id:endpoint.id});leaseKeeper.clear();leaseExpires=0;await observe();notice('停止要求已由 Runtime 接受；狀態以輸出回覆為準。');});
  function renderShutdown(result){const state=shutdownView(result);$('shutdownImpact').textContent=state.message;$('confirmShutdown').hidden=!state.confirm;$('forceShutdown').hidden=!state.force;notice(state.message);}
  bindButton('prepareShutdown',async()=>{const result=await client.call('runtime.prepare_shutdown',null,{});shutdownTicket=result.ticket;renderShutdown(result);});
  bindButton('confirmShutdown',async()=>{if(!shutdownTicket)throw new Error('請先取得關閉 ticket');const result=await client.call('runtime.confirm_shutdown',null,{ticket:shutdownTicket});renderShutdown(result);await health();});
  bindButton('forceShutdown',async()=>{const fresh=await client.call('runtime.prepare_shutdown',null,{});shutdownTicket=fresh.ticket;$('shutdownImpact').textContent=JSON.stringify(fresh,null,2);if(!confirm(`強制關閉目前 Runtime 及其 owned 程序？\n${JSON.stringify(fresh.impact,null,2)}\n此操作需外部確認停止結果。`))return;const result=await client.call('runtime.force_shutdown',null,{ticket:fresh.ticket});renderShutdown(result);await health();});
  function addSource(value = {locator:'',autojoin:false}) {
    const row=node('div',null,'source-row'),locator=node('input'),autojoin=node('input'),label=node('label','允許自動加入'),remove=node('button','移除來源');
    locator.value=value.locator;locator.required=true;locator.setAttribute('aria-label','來源 locator');locator.className='source-locator';
    autojoin.type='checkbox';autojoin.checked=value.autojoin;autojoin.className='source-autojoin';label.className='check';label.prepend(autojoin);remove.type='button';remove.addEventListener('click',()=>row.remove());row.append(locator,label,remove);$('settingSources').append(row);
  }
  function settingsMetadata(result) {$('settingsMeta').textContent=`已保存設定\n範圍：${result.scope || 'runtime'}\nrequires_restart：${String(result.requires_restart)}\nRuntime 套用說明：${result.effect || '未提供'}\n上次讀取／保存：${new Date().toLocaleString('zh-TW')}`;}
  bindButton('loadSettings',async()=>{const result=await client.getSettings(),s=result.settings;$('settingDiscovery').checked=s.discovery_enabled;$('settingHeartbeat').value=s.heartbeat_seconds;$('settingReconnect').value=s.reconnect_seconds;$('settingHistory').value=s.history_limit;$('settingSources').replaceChildren();(s.sources||[]).forEach(addSource);settingsMetadata(result);$('settingsForm').hidden=false;notice('已讀取 Runtime 保存的設定。');});
  bindButton('addSettingSource',()=>addSource());
  bindForm('settingsForm',async()=>{const settings={discovery_enabled:$('settingDiscovery').checked,heartbeat_seconds:Number($('settingHeartbeat').value),reconnect_seconds:Number($('settingReconnect').value),history_limit:Number($('settingHistory').value),sources:[...document.querySelectorAll('#settingSources .source-row')].map(row=>({locator:row.querySelector('.source-locator').value.trim(),autojoin:row.querySelector('.source-autojoin').checked}))};const result=await client.setSettings(settings);settingsMetadata(result);notice('Runtime 已保存設定；實際套用範圍與時機依下方回覆。');});
  health();setInterval(async()=>{await health();if(!token||pollBusy||document.hidden)return;pollBusy=true;try{await refresh();}catch(error){failure(error);if(view==='endpoints'){clearEndpointExtras();$('output').textContent='';$('outputMeta').textContent='讀取被拒絕或連線失敗，停止顯示舊輸出；需重新授權後補讀。';endpoint=null;leaseKeeper.clear();}}finally{pollBusy=false;}},5000);
}








