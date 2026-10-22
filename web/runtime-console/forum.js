'use strict';
// In-memory draft only. Actor switching is a preview control, never authentication.
const roleNames={admin:'管理員（人）',maintainer:'Session 維護者',manager:'主控',implementer:'實作',reviewer:'reviewer',consultant:'顧問',tester:'測試',observer:'Observer'};
const topicStates=['待認領','進行中','受阻','待審查','待測試','完成'];
const forumTopics=[
 {id:'topic-idle',space:'space-design',title:'閒置後重新投遞仍能回應',kind:'任務',body:'先寫驗證 plan：保留同一 Session，閒置 65 分鐘後再送訊息，分開記錄收件、開始與輸出時間。此處只有討論示例，尚未執行測試。',author:'human',state:'待認領',assignee:null,revision:1,comments:[]},
 {id:'topic-plan',space:'space-design',title:'連線與異步合作的設計 plan',kind:'計畫',body:'Runtime 保存帖子與留言。Agent 按自己的工作節奏補讀、討論分工；不要求所有成員同時在線。',author:'lead',state:null,assignee:null,revision:1,comments:[{id:'message-seed',author:'advisor',text:'建議把連線存活與任務認領分開，失聯先保留認領。',action:'留言',time:'示範時間',seq:1}]}
];
let forumSeq=2,forumSelected='topic-idle',forumMode='topics',forumExpected=1,composeContext='';
const actorBySpace=new Map();
function initForumSpace(s){
 for(const t of forumTopics.filter(t=>t.space===s.id)){t.createdAt??='2026-10-21（固定示範）';t.updatedAt??=t.createdAt;}
 if(s.forumReady)return;
 s.members=s.members.map((m,i)=>({...m,id:i===0?'lead':`legacy-${i}`,kind:'agent',roles:m.role==='Manager'?['manager']:m.role==='Observer'?['observer']:['implementer']}));
 s.members.unshift({id:'human',name:'人類管理員',kind:'human',roles:['admin'],state:'本機預覽'});
 s.members.push({id:'builder',name:'實作 Agent',kind:'agent',roles:['implementer'],state:'連線中（示範）'},
 {id:'reviewer',name:'審查 Agent',kind:'agent',roles:['reviewer'],state:'連線中（示範）'},
 {id:'tester',name:'測試 Agent',kind:'agent',roles:['tester'],state:'連線中（示範）'},
 {id:'advisor',name:'顧問 Agent',kind:'agent',roles:['consultant'],state:'離線（示範）'},
 {id:'keeper',name:'Session 維護 Agent',kind:'agent',roles:['maintainer'],state:'連線中（示範）'});
 s.forumReady=true;actorBySpace.set(s.id,'human');
}
function forumActor(){initForumSpace(space());return space().members.find(m=>m.id===actorBySpace.get(activeSpace));}
function hasRole(...roles){return forumActor().roles.some(r=>roles.includes(r));}
function managesSpace(){return hasRole('admin','manager');}
function canPost(){return hasRole('admin','manager','implementer','reviewer','consultant','tester');}
function authorName(id){return space().members.find(m=>m.id===id)?.name||id;}
function selectedTopic(){return forumTopics.find(t=>t.id===forumSelected&&t.space===activeSpace);}
function actionAllowed(t,action){
 if(runtime!=='running')return false;
 const manage=managesSpace(),mine=t.assignee===forumActor().id;
 if(action==='留言')return canPost();
 if(t.kind!=='任務')return false;
 if(action==='認領')return t.state==='待認領'&&!t.assignee&&(manage||hasRole('implementer'));
 if(action==='標記受阻')return t.state==='進行中'&&(manage||(mine&&hasRole('implementer')));
 if(action==='恢復工作')return t.state==='受阻'&&(manage||(mine&&hasRole('implementer')));
 if(action==='提交審查')return t.state==='進行中'&&(manage||(mine&&hasRole('implementer')));
 if(action==='審查通過')return t.state==='待審查'&&(manage||hasRole('reviewer'));
 if(action==='要求修改')return t.state==='待審查'&&(manage||hasRole('reviewer'));
 if(action==='測試通過'||action==='測試失敗')return t.state==='待測試'&&(manage||hasRole('tester'));
 if(action==='回收認領')return !!t.assignee&&t.state!=='完成'&&manage;
 return false;
}
const forumActions=['留言','認領','標記受阻','恢復工作','提交審查','審查通過','要求修改','測試通過','測試失敗','回收認領'];
function drawPosts(){
 initForumSpace(space());
 const items=forumTopics.filter(t=>t.space===activeSpace);
 if(!items.some(t=>t.id===forumSelected))forumSelected=items[0]?.id;
 const actor=forumActor(),t=selectedTopic();
 const context=activeSpace+'/'+actor.id+'/'+forumSelected;if(context!==composeContext){$('postText').value='';$('artifactRef').value='';composeContext=context;}
 $('forumActor').innerHTML=space().members.map(m=>`<option value="${esc(m.id)}" ${actor.id===m.id?'selected':''}>${esc(m.name)}</option>`).join('');
 $('forumRole').textContent=actor.roles.map(r=>roleNames[r]).join('、');
 $('forumSpace').textContent=space().name+' · 協作論壇';
 $('newTopic').disabled=!canPost()||runtime!=='running';
 $('topicList').innerHTML=items.map(x=>`<button class="topic-row" data-topic="${esc(x.id)}" aria-pressed="${x.id===forumSelected}"><strong>${esc(x.title)}</strong><span>${esc(x.kind)} · ${esc(x.state||'討論中')} · ${x.comments.length} 則留言</span><small>認領者：${x.assignee?esc(authorName(x.assignee)):'未認領'}</small></button>`).join('')||'<p>這個空間還沒有帖子。建立第一帖，讓成員稍後加入討論。</p>';
 $('topicList').querySelectorAll('[data-topic]').forEach(b=>b.onclick=()=>{forumSelected=b.dataset.topic;forumMode='topics';drawPosts();focusOn('#topicHeading');});
 $('forumTopics').setAttribute('aria-pressed',forumMode==='topics');$('forumActivity').setAttribute('aria-pressed',forumMode==='activity');
 $('topicDetail').hidden=forumMode!=='topics'||!t;$('activityList').hidden=forumMode!=='activity';
 $('workMonitor').hidden=forumMode!=='monitor';$('topicList').hidden=forumMode==='monitor';
 $('forumMonitor').setAttribute('aria-pressed',forumMode==='monitor');drawWorkMonitor();
 const activity=items.flatMap(x=>x.comments.map(c=>({...c,topic:x}))).sort((a,b)=>b.seq-a.seq);
 $('activityList').innerHTML=activity.map(c=>`<article class="post"><button data-activity-topic="${esc(c.topic.id)}">${esc(c.topic.title)}</button><p><strong>${esc(authorName(c.author))}</strong> · ${esc(c.action)} · ${esc(c.time)}</p><p>${esc(c.text)}</p></article>`).join('')||'<p>尚無留言活動。帖子會保留，不需要成員同時在線。</p>';
 $('activityList').querySelectorAll('[data-activity-topic]').forEach(b=>b.onclick=()=>{forumSelected=b.dataset.activityTopic;forumMode='topics';drawPosts();focusOn('#topicHeading');});
 if(t){
  forumExpected=t.revision;
  $('topicHeading').textContent=t.title;$('topicBody').textContent=t.body;
  $('topicAuthor').textContent=`作者：${authorName(t.author)} · 建立：${t.createdAt} · 更新：${t.updatedAt}`;
  $('topicMeta').textContent=`${t.kind} · ${t.state||'討論中'} · 認領者：${t.assignee?authorName(t.assignee):'未認領'} · 版本 ${t.revision}${t.submission?' · 成果 '+t.submission.ref:''}`;
  $('posts').innerHTML=t.comments.map(c=>`<article class="post"><strong>${esc(authorName(c.author))}</strong><small> · ${esc(c.action)} · ${esc(c.time)}</small><p>${esc(c.text)}</p><small>${esc(c.id)} · ${esc(c.before||'')} ${c.after?'→ '+esc(c.after):''}${c.submission?' · 成果 '+esc(c.submission.ref)+'（'+esc(c.submission.id)+'）':''}${c.selfReview?' · 自我審查／驗證，非獨立結論':''}</small></article>`).join('')||'<p class="empty-thread">尚無留言。認領時一起留下處理計畫。</p>';
  const allowed=forumActions.filter(a=>actionAllowed(t,a));
  $('postAction').innerHTML=allowed.map(a=>`<option>${a}</option>`).join('');$('postAction').disabled=!allowed.length;
  $('postText').disabled=!allowed.length;$('artifactRef').disabled=!allowed.includes('提交審查');$('submitPost').disabled=!allowed.length;
  $('postPermission').textContent=allowed.length?'留言、認領者與狀態一次提交；失敗時全部不變。':'目前身分沒有可執行的操作，仍可讀取此空間討論。';
  $('simulateClaim').hidden=t.state!=='待認領'||t.kind!=='任務'||runtime!=='running';
 }
}
function applyForumAction(t,action,text,expected,artifactRef){
 if(!t||t.space!==activeSpace)return '帖子不存在於目前空間。';
 if(expected!==t.revision)return '版本衝突：帖子已被其他成員更新。本次留言與狀態均未提交，文字已保留；請重新檢視。';
 if(!actionAllowed(t,action))return '沒有權限，或目前狀態不允許這項操作。';
 if(action==='提交審查'&&!artifactRef.trim())return '提交審查須填寫成果版本／commit／交付物識別。';
 if(['審查通過','要求修改','測試通過','測試失敗'].includes(action)&&!t.submission)return '沒有可驗證的成果版本，請先提交成果。';
 const before=t.state,actor=forumActor(),selfReview=t.assignee===actor.id&&['審查通過','要求修改','測試通過','測試失敗'].includes(action);
 if(action==='認領'){t.assignee=actor.id;t.state='進行中';}
 if(action==='標記受阻')t.state='受阻';
 if(action==='恢復工作'||action==='要求修改'||action==='測試失敗')t.state='進行中';
 if(action==='提交審查'){t.state='待審查';t.submission={id:`submission-${forumSeq}`,ref:artifactRef.trim(),author:actor.id};}
 if(action==='審查通過')t.state='待測試';
 if(action==='測試通過')t.state='完成';
 if(action==='回收認領'){t.assignee=null;t.state='待認領';}
 t.comments.push({id:`message-${forumSeq}`,seq:forumSeq++,author:actor.id,text,action,before,after:t.state,revisionBefore:t.revision,revisionAfter:t.revision+1,submission:t.submission?{...t.submission}:null,selfReview,time:new Date().toLocaleString('zh-TW')});t.revision++;t.updatedAt=new Date().toLocaleString('zh-TW');return null;
}
function drawMembers(){
 initForumSpace(space());forumActor();
 $('members').innerHTML=space().members.map(m=>`<div class="member"><div><strong>${esc(m.name)}</strong><p>${m.roles.map(r=>roleNames[r]).join('、')} ／ ${esc(m.state)}</p><small>${m.kind==='human'?'人類':'Agent'} · ${esc(m.id)}</small>${renderEndpointGrants(m)}</div><button data-member="${esc(m.id)}" ${!managesSpace()||runtime!=='running'?'disabled':''}>調整角色</button></div>`).join('');
 $('members').querySelectorAll('[data-member]').forEach(b=>b.onclick=()=>{
  if(!live()||!managesSpace())return;
  const s=space(),m=s.members.find(x=>x.id===b.dataset.member);
  openDialog('調整空間角色',`<p>${esc(m.name)} · ${esc(s.name)}；可兼任多個角色。</p>${Object.entries(roleNames).filter(([r])=>r!=='admin'||m.kind==='human').map(([r,name])=>`<label class="check"><input name="forumRole" value="${r}" type="checkbox" ${m.roles.includes(r)?'checked':''}>${name}</label>`).join('')}<p>角色不授予其他空間、全域 Runtime 關閉或任意 Endpoint 控制權。</p><p id="roleError" class="form-error" role="alert"></p>`,()=>{
   if(!live()||!managesSpace()||s.id!==activeSpace)return;
   const roles=[...document.querySelectorAll('input[name=forumRole]:checked')].map(x=>x.value);
   if(!roles.length){$('roleError').textContent='至少保留一個角色；只讀請選 Observer。';return;}
   if(!roles.some(r=>['admin','manager'].includes(r))&&!s.members.some(x=>x!==m&&x.roles.some(r=>['admin','manager'].includes(r)))){$('roleError').textContent='不能移除最後一位空間管理者；請先移交管理權。';return;}
   m.roles=roles;revision++;$('dialog').close();drawPosts();drawMembers();focusOn(`[data-member="${m.id}"]`);notice('預覽角色已更新；正式版須由 Runtime 驗證並撤銷既有失效權限。');
  },'儲存預覽角色');
 });
}
function setupForum(){
 $('forumActor').onchange=e=>{actorBySpace.set(activeSpace,e.target.value);drawPosts();drawMembers();notice('已切換論壇示範身分；這不是登入，連線與全域設定區仍示範本機 owner。');};
 $('forumTopics').onclick=()=>{forumMode='topics';drawPosts();};$('forumActivity').onclick=()=>{forumMode='activity';drawPosts();};
 $('forumMonitor').onclick=()=>{forumMode='monitor';drawPosts();};
 $('postForm').onsubmit=e=>{e.preventDefault();const text=$('postText').value.trim();if(!text||!live())return;const error=applyForumAction(selectedTopic(),$('postAction').value,text,forumExpected,$('artifactRef').value);if(!error){$('postText').value='';$('artifactRef').value='';}drawPosts();notice(error||'預覽：留言與任務狀態已一起更新；只存在本頁記憶體。');};
 $('simulateClaim').onclick=()=>{const t=selectedTopic();if(!live()||!t||t.state!=='待認領')return;const other=space().members.find(m=>m.id!==forumActor().id&&m.roles.some(r=>['implementer','manager','admin'].includes(r)));if(!other)return;t.assignee=other.id;t.state='進行中';t.comments.push({id:`message-${forumSeq}`,seq:forumSeq++,author:other.id,text:'示範：我先認領這項工作。',action:'認領',before:'待認領',after:'進行中',time:'衝突示範'});t.revision++;$('simulateClaim').hidden=true;notice('已模擬另一成員先認領。畫面保留舊版本，提交時會被拒絕；重新檢視會更新。');};
 $('refreshTopic').onclick=()=>{drawPosts();notice('已重新讀取示範帖子，尚未送出的文字保留。');};
 $('newTopic').onclick=()=>{if(!live()||!canPost())return;openDialog('建立帖子',`<label>類型<select id="topicKind"><option>任務</option><option>計畫</option><option>問題</option><option>審查</option></select></label><label>標題<input id="newTopicTitle" required maxlength="120"></label><label>內容<textarea id="newTopicBody" required maxlength="4000"></textarea></label><p>示範資料；建立帖子不會自動啟動或喚醒 CLI。</p>`,()=>{if(!live()||!canPost())return;const title=$('newTopicTitle').value.trim(),body=$('newTopicBody').value.trim();if(!title||!body)return;const kind=$('topicKind').value,id=`topic-${nextId++}`;forumTopics.push({id,space:activeSpace,title,body,kind,author:forumActor().id,state:kind==='任務'?'待認領':null,assignee:null,revision:1,createdAt:new Date().toLocaleString('zh-TW'),updatedAt:new Date().toLocaleString('zh-TW'),comments:[]});forumSelected=id;forumMode='topics';$('dialog').close();drawPosts();focusOn('#topicHeading');notice('帖子已加入預覽，重新載入頁面會清除。');},'建立示範帖子');};
}
let observedEndpoint=null;
function drawWorkMonitor(){
 const rows=connections.filter(c=>c.space===activeSpace&&endpointRights(forumActor(),c).observe);
 if(!rows.length){$('workMonitor').innerHTML='<p>目前沒有可觀察端點，或目前角色未獲工作觀察權。主控／人類管理員可觀察自己空間的端點；其他角色須另授 observe 權。</p>';return;}
 if(!rows.some(c=>c.id===observedEndpoint))observedEndpoint=rows[0]?.id;
 const c=rows.find(c=>c.id===observedEndpoint);
 $('workMonitor').innerHTML=`<h2>工作觀察</h2><p>唯讀示範，不送出輸入、不取得控制 lease。只列此空間已登記端點；不擷取其他桌面視窗。</p><label for="observeEndpoint">觀察端點</label><select id="observeEndpoint">${rows.map(x=>`<option value="${esc(x.id)}" ${x.id===observedEndpoint?'selected':''}>${esc(x.name)}</option>`).join('')}</select>${c?`<article class="post"><strong>${esc(c.name)}</strong><p>${esc(c.session)} · ${esc(stateText[c.status])}</p><p>${c.kind==='Agent SDK'?'結構化聊天：須 provider 支援對話歷史／事件。':'終端輸出：保留原始文字語義，不能當作完整聊天紀錄。'}</p><p>${c.status==='connected'?'最後更新：固定示範快照，非即時資料。':'目前沒有即時更新；僅顯示保留的示範快照，不代表對方仍在工作。'}</p><pre>${esc(c.kind==='Agent SDK'?'[示範 user] 請檢視重連方案。\n[示範 assistant] 正在檢查 Session 與 generation。':'[示範終端輸出]\n> checking session identity\nwaiting for next input…')}</pre><p class="muted">此預覽未接 provider；完整歷史、隱藏推理及畫面外內容不可推得。正式版超出留存範圍須標示 Gap，失聯後補讀並重新驗證權限。</p></article>`:'<p>尚無可觀察的端點。加入此空間的端點後會列在這裡。</p>'}`;
 if(c)$('observeEndpoint').onchange=e=>{observedEndpoint=e.target.value;drawWorkMonitor();};
}

// Explicit mock grants are scoped by both space and endpoint. They are not role names.
const mockEndpointGrants=[{space:'space-design',member:'builder',endpoint:'ep-terminal-01',observe:true,send:true,control:false}];
function endpointRights(member,c){
 if(member.roles.some(r=>['admin','manager'].includes(r)))return {observe:true,send:true,control:true,management:true};
 return mockEndpointGrants.find(g=>g.space===activeSpace&&g.endpoint===c.id&&g.member===member.id)||{observe:false,send:false,control:false};
}
function renderEndpointGrants(member){
 const rows=connections.filter(c=>c.space===activeSpace);
 return `<details class="endpoint-grants"><summary>特定端點授權（示範）</summary>${rows.length?rows.map(c=>{const g=endpointRights(member,c);return `<p><strong>${esc(c.name)}</strong><br>observe ${g.observe?'允許':'未授權'} · send ${g.send?'允許':'未授權'} · control ${g.control?'可申請 lease':'未授權'}${g.management?' · 來源：空間管理權':' · 來源：明確 grant'}</p>`;}).join(''):'<p>此空間還沒有端點。</p>'}<small>顯示政策能力，不代表協定支援或已取得控制；角色變更不會自動清除獨立 grant。</small></details>`;
}
