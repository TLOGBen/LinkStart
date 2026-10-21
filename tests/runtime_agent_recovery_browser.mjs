// Live console proof. Chromium is launched exclusively by baransu:draw's seeded helper.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const session = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const {loadPlaywright, chromiumPath, launchChromium} = await import(pathToFileURL(process.argv[3]).href);
const pw = await loadPlaywright();
assert(pw, 'Playwright must be installed before running the browser gate');
const browser = await launchChromium(pw, {executablePath: chromiumPath()});
const credential = fs.readFileSync(session.credential_file, 'utf8');
const admin = fs.readFileSync(session.admin_file, 'utf8');
const errors = [];
const actions = [];
let sequence = 0;
async function call(tool, args, token = credential) {
  const response = await fetch(session.base + '/v2/tools', {method:'POST', headers:{'Content-Type':'application/json', Authorization:`Bearer ${token}`},
    body:JSON.stringify({request_id:`browser-proof-${++sequence}`,tool,space_id:session.space_id,args})});
  const result = await response.json(); assert.equal(response.status,200,JSON.stringify(result)); return result;
}
const intent = {job_id:session.job_id,mission_topic_id:session.mission_topic_id};
const page = await browser.newPage({viewport:{width:1280,height:900},reducedMotion:'reduce'});
page.on('pageerror', error=>errors.push(error.message));
const recoveries = [];
page.on('request', request=>{if(request.method()==='POST'&&request.url().endsWith('/v2/tools')){
  const body=request.postDataJSON();if(body?.tool==='agent.recover')recoveries.push(body.request_id);
}});
async function mission(){await page.locator('#topics button').filter({hasText:'Recover saved decomposition'}).click();await page.waitForFunction(()=>document.querySelector('#topicTitle').textContent==='Recover saved decomposition');}
async function inspect(){await page.waitForFunction(()=>!document.querySelector('#inspectDecomposition').disabled);await page.locator('#decompositionJobId').fill(session.job_id);await page.locator('#inspectDecomposition').click();}
try {
  await page.goto(session.base+'/console');
  await page.locator('#credential').fill(credential);
  await page.locator('#loginForm button').click();
  await page.locator('#workspace').waitFor({state:'visible'});
  await mission();await inspect();
  await page.locator('#recoverDecomposition').waitFor({state:'visible'});
  assert.equal(await page.locator('#decompositionChildren button').count(),2);
  assert.match(await page.locator('#recoveryPurpose').textContent(),/沿用已保存計畫/);
  await page.screenshot({path:path.join(session.evidence,'desktop-eligible.png'),fullPage:true});
  await page.setViewportSize({width:400,height:800});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  await page.screenshot({path:path.join(session.evidence,'phone-eligible.png'),fullPage:true});
  await page.setViewportSize({width:1280,height:900});
  actions.push('Current authorized decomposer inspected original mission/job; two actual committed children; explicit saved-plan control visible on desktop and phone');

  let releaseDelayed;
  let delayedArrived;
  const arrived = new Promise(resolve=>{delayedArrived=resolve;});
  const release = new Promise(resolve=>{releaseDelayed=resolve;});
  let delayed = false;
  await page.route('**/v2/tools',async route=>{
    const body=route.request().postDataJSON();
    if(!delayed&&body?.tool==='agent.get'){
      delayed=true;const response=await route.fetch();delayedArrived();await release;await route.fulfill({response});
    }else await route.continue();
  });
  await page.locator('#inspectDecomposition').click();await arrived;
  await page.locator('#topics button').filter({hasText:'One'}).click();
  releaseDelayed();
  await page.waitForFunction(()=>document.querySelector('#decompositionJobId').value===''&&document.querySelector('#decompositionChildren').children.length===0);
  await page.waitForFunction(()=>!document.querySelector('#inspectDecomposition').disabled);
  await page.unroute('**/v2/tools');
  actions.push('Delayed agent.get response after a mission switch did not restore old job/child response');
  await mission();await inspect();await page.locator('#recoverDecomposition').waitFor({state:'visible'});

  let dropped = false;
  await page.route('**/v2/tools',async route=>{
    const body=route.request().postDataJSON();
    if(!dropped&&body?.tool==='agent.recover'){
      dropped=true;const response=await route.fetch();assert.equal(response.status(),200);await route.abort('failed');
    }else await route.continue();
  });
  await page.locator('#recoverDecomposition').click();
  await page.waitForFunction(()=>document.querySelector('#decompositionState').textContent.includes('unknown'));
  // This is an observation after an intentionally lost response, never a second recovery POST.
  await page.locator('#inspectDecomposition').click();
  await page.waitForFunction(()=>document.querySelector('#decompositionState').textContent.includes('completed'));
  await page.unroute('**/v2/tools');
  assert.equal(recoveries.length,1,'unknown transport must not generate another attempt automatically');
  const latest=await call('agent.get',intent);
  assert.equal(latest.status,'completed');assert.equal(latest.topic_ids.length,3);
  assert.equal(latest.error,null,'completed shared tool result must distinguish prior failure from current outcome');
  assert(!Object.hasOwn(latest,'prior_errors'),'internal error history stays private');
  assert.equal(latest.recovery.attempt_id,recoveries[0]);
  assert.equal(await page.locator('#decompositionChildren button').count(),3);
  await page.locator('#topics button').filter({hasText:'Three'}).waitFor();
  await page.screenshot({path:path.join(session.evidence,'desktop-completed.png'),fullPage:true});
  actions.push('One real agent.recover POST committed despite lost HTTP response; agent.get observed completed with matching attempt_id and three Engine children; board refreshed');

  await call('members.set_roles',{member_id:session.member_id,roles:['observer']},admin);
  await page.locator('#inspectDecomposition').click();
  await page.waitForFunction(()=>document.querySelector('#decompositionChildren').children.length===0&&document.querySelector('#recoverDecomposition').hidden);
  assert(!await page.locator('#decompositionState').textContent().then(text=>latest.topic_ids.some(id=>text.includes(id))));
  await page.locator('#refresh').click();
  await page.locator('#decompositionPanel').waitFor({state:'hidden'});
  await page.screenshot({path:path.join(session.evidence,'desktop-revoked.png'),fullPage:true});
  actions.push('Live role revocation denied get, removed sensitive child response, and hid recovery controls');
  await call('members.set_roles',{member_id:session.member_id,roles:['task_decomposer']},admin);
  await page.locator('#logout').click();
  assert.equal(await page.locator('#decompositionChildren button').count(),0);
  actions.push('Logout cleared prior identity results');
  assert.deepEqual(errors,[]);
  fs.writeFileSync(path.join(session.evidence,'browser-proof.json'),JSON.stringify({status:'PASS',actions,recovery_posts:recoveries.length,
    attempt_id:recoveries[0],topic_ids:latest.topic_ids,page_errors:errors,phone_horizontal_overflow:false},null,2));
  console.log('Live recovery console PASS: explicit action, lost response, scope switch, role revocation, three child IDs, desktop/phone');
} catch(error) {
  await page.screenshot({path:path.join(session.evidence,'browser-failure.png'),fullPage:true});
  fs.writeFileSync(path.join(session.evidence,'browser-failure.json'),JSON.stringify({message:error.message,actions,recovery_posts:recoveries.length,
    display:await page.locator('#decompositionState').textContent(),notice:await page.locator('#notice').textContent()},null,2));
  throw error;
} finally {
  await browser.close();
}
