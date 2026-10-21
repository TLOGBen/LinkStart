"""Public CLI -> real HTTP integration; never imports a product internal module."""
import json, os, pathlib, socket, subprocess, tempfile, time, unittest, urllib.request, urllib.error
ROOT=pathlib.Path(__file__).resolve().parents[1]
BIN=pathlib.Path(os.environ.get('LINKSTART_BIN',str(ROOT/'target/debug/linkstart.exe')))

class RuntimeHttp(unittest.TestCase):
    def test_operation_receipt_http_and_cli_share_current_identity_scope(self):
        owner=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        status,space=self.post(dict(request_id='receipt-space',tool='spaces.create',args={'name':'receipts','manager_name':'other'}),owner)
        self.assertEqual(status,200,space)
        sid=space['space']['id']; admin=space['administrator']['credential']; other=space['manager']['credential']
        def call(rid,tool,args,credential=admin):
            return self.post(dict(request_id=rid,tool=tool,space_id=sid,args=args),credential)
        status,endpoint=call('receipt-endpoint','endpoints.register',{'name':'forum','kind':'terminal'})
        self.assertEqual(status,200,endpoint)
        status,topic=call('receipt-create','topics.create',{'title':'retained','body':'HTTP receipt','kind':'task'})
        self.assertEqual(status,200,topic)
        args=dict(endpoint_id=endpoint['id'],source_kind='forum',operation_request_id='receipt-create')
        status,receipt=call('receipt-query','operations.get',args)
        self.assertEqual(status,200,receipt)
        self.assertEqual(receipt['state'],'confirmed')
        self.assertEqual(receipt['receipt'],topic)
        status,foreign=call('receipt-other','operations.get',args,other)
        self.assertEqual(status,200,foreign)
        self.assertEqual(foreign['state'],'unknown')
        credential_file=pathlib.Path(self.tmp.name)/'member.token'
        credential_file.write_text(admin,encoding='utf-8')
        result=subprocess.run([str(BIN),'tool','--state-dir',self.state_dir,'--credential-file',str(credential_file)],input=json.dumps(dict(request_id='receipt-cli',tool='operations.get',space_id=sid,args=args)),text=True,capture_output=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(json.loads(result.stdout),receipt)
        status,missing=call('receipt-missing','operations.get',dict(args,source_kind='source_job',operation_request_id='absent'))
        self.assertEqual(status,200,missing)
        self.assertEqual(missing['state'],'unknown')

    def test_watch_listing_is_a_scoped_public_tool(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,created=self.post(dict(request_id='watch-space',tool='spaces.create',args={'name':'watch space'}),token)
        status,result=self.post(dict(request_id='watch-list',tool='sources.watch.list',space_id=created['space']['id'],args={}),token)
        self.assertEqual(status,200,result)
        self.assertEqual(result['items'],[])
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix='linkstart-http-')
        # Python 3.13+ applies explicit Windows ACEs to its private temp root.
        # Give Runtime a new child directory whose storage ACL it owns.
        self.state_dir=str(pathlib.Path(self.tmp.name)/'state')
        with socket.socket() as s:
            s.bind(('127.0.0.1',0)); self.port=s.getsockname()[1]
        self.base=f'http://127.0.0.1:{self.port}'
        self.log=open(pathlib.Path(self.tmp.name)/'daemon.log','w',encoding='utf-8')
        self.child=subprocess.Popen([str(BIN),'daemon','run','--state-dir',self.state_dir,'--port',str(self.port)],stdout=self.log,stderr=self.log)
        self.addCleanup(self.cleanup)
        for _ in range(100):
            if self.child.poll() is not None:
                self.log.flush()
                diagnostic=(pathlib.Path(self.tmp.name)/'daemon.log').read_text(encoding='utf-8',errors='replace')
                self.fail(f'owned daemon exited during startup ({self.child.returncode}):\n{diagnostic}')
            try:
                if self.get('/v1/health')[0]==200: break
            except OSError: pass
            time.sleep(.1)
        else: self.fail('owned daemon did not become healthy')
    def cleanup(self):
        if self.child.poll() is None: self.child.terminate();self.child.wait(timeout=10)
        self.log.close();self.tmp.cleanup()
    def get(self,path):
        try:
            with urllib.request.urlopen(self.base+path,timeout=5) as r: return r.status,json.load(r)
        except urllib.error.HTTPError as e: return e.code,{}
    def test_v2_ready_on_existing_daemon(self):
        status,body=self.get('/v2/health')
        self.assertEqual(status,200)
        self.assertEqual(body['status'],'ready')
        self.assertTrue((pathlib.Path(self.state_dir)/'v2-owner.token').is_file())

    def test_discovery_reports_candidates_without_claiming_connections(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        status,value=self.post(dict(request_id='discover',tool='runtime.discover',args={}),token)
        self.assertEqual(status,200,value)
        self.assertIsInstance(value['candidates'],list)
        for candidate in value['candidates']:
            self.assertEqual(candidate['status'],'available_command')
            self.assertFalse(candidate['connected'])

    def test_planner_cannot_run_as_runtime_owner(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='planner-space',tool='spaces.create',args={'name':'planner'}),token)
        status,value=self.post(dict(request_id='planner',tool='agent.run',space_id=space['space']['id'],args={'job_id':'planner','mission_topic_id':'untrusted'}),token)
        self.assertEqual(status,403,value)

    def test_source_writeback_is_disabled_before_any_external_call(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args,space_id=None):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=space_id,args=args),token)
        _,space=call('spaces.create',{'name':'source restrictions'})
        sid=space['space']['id']
        _,ep=call('endpoints.register',{'name':'read only repo','kind':'github','config':{'repository':'TLOGBen/LinkStart','allow_write_back':False}},sid)
        status,value=call('sources.writeback',{'endpoint_id':ep['id'],'number':1,'body':'must never be sent'},sid)
        self.assertEqual(value['error']['code'],'WriteBackDisabled',value)

    def test_cli_tool_uses_same_server_and_stdin_contract(self):
        tokenfile=pathlib.Path(self.state_dir)/'v2-owner.token'
        result=subprocess.run([str(BIN),'tool','--state-dir',self.state_dir,'--credential-file',str(tokenfile)],input=json.dumps({'request_id':'cli-create','tool':'spaces.create','args':{'name':'CLI created'}}),text=True,capture_output=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        created=json.loads(result.stdout)
        status,listing=self.post({'request_id':'cli-readback','tool':'spaces.list','args':{}},tokenfile.read_text())
        self.assertEqual(listing['items'][0]['id'],created['space']['id'])

    def test_console_serves_real_module_without_secrets(self):
        with urllib.request.urlopen(self.base+'/console') as response:
            html=response.read().decode()
            self.assertIn('live.js',html)
            self.assertNotIn((pathlib.Path(self.state_dir)/'v2-owner.token').read_text(),html)
        with urllib.request.urlopen(self.base+'/console/live.js') as response:
            self.assertIn('javascript',response.headers['Content-Type'])
            self.assertIn('/v2/tools',response.read().decode())

    def test_shutdown_requires_current_impact_then_exits_owned_daemon(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args={}):return self.post(dict(request_id=str(time.time_ns()),tool=tool,args=args),token)
        status,prepared=call('runtime.prepare_shutdown')
        self.assertEqual(status,200,prepared)
        call('spaces.create',{'name':'new impact'})
        status,stale=call('runtime.confirm_shutdown',{'ticket':prepared['ticket']})
        self.assertEqual(status,409,stale)
        status,prepared=call('runtime.prepare_shutdown')
        status,result=call('runtime.confirm_shutdown',{'ticket':prepared['ticket']})
        self.assertEqual(status,200,result)
        self.child.wait(timeout=8)
        self.assertEqual(self.child.returncode,0)
    def post(self,body,credential=None,origin=None):
        headers={'Content-Type':'application/json'}
        if credential:headers['Authorization']='Bearer '+credential
        if origin:headers['Origin']=origin
        req=urllib.request.Request(self.base+'/v2/tools',data=json.dumps(body).encode(),headers=headers)
        try:
            with urllib.request.urlopen(req,timeout=5) as r:return r.status,json.load(r)
        except urllib.error.HTTPError as e:
            with e:
                raw=e.read()
                return e.code,json.loads(raw) if raw else {}
    def test_same_tool_contract_requires_credential_and_rejects_foreign_origin(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        request={'request_id':'create-one','tool':'spaces.create','space_id':None,'args':{'name':'HTTP space'}}
        self.assertEqual(self.post(request)[0],401)
        self.assertEqual(self.post(request,token,'https://untrusted.example')[0],403)
        status,created=self.post(request,token,self.base)
        self.assertEqual(status,200)
        sid=created['space']['id']
        status,listing=self.post({'request_id':'list','tool':'spaces.list','args':{}},token)
        self.assertEqual(status,200,listing)
        self.assertEqual(listing['items'][0]['id'],sid)

    def test_owned_terminal_computes_and_replays_create_without_duplicate(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args,request_id=None,space_id=None):
            status,value=self.post({'request_id':request_id or str(time.time_ns()),'tool':tool,'space_id':space_id,'args':args},token)
            self.assertEqual(status,200,value)
            return value
        sid=call('spaces.create',{'name':'terminal'})['space']['id']
        args={'command':'cmd.exe','args':['/Q'],'name':'owned shell'}
        created=call('terminal.create',args,'create-terminal',sid)
        eid=created['endpoint']['id']
        replay=call('terminal.create',args,'create-terminal',sid)
        self.assertEqual(created['session']['id'],replay['session']['id'])
        lease=call('terminal.acquire',{'endpoint_id':eid},space_id=sid)
        call('terminal.write',dict(endpoint_id=eid,input='set /a 314159*2\r\n',**lease),space_id=sid)
        for _ in range(60):
            output=call('terminal.observe',{'endpoint_id':eid,'cursor':0},space_id=sid)
            if '628318' in output['text']:break
            time.sleep(.1)
        self.assertIn('628318',output['text'])
        call('terminal.release',dict(endpoint_id=eid,**lease),space_id=sid)
        call('terminal.force',{'endpoint_id':eid},space_id=sid)

    def test_history_restart_recovery_and_revocation(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args={},credential=token,rid=None):
            return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,space=self.post(dict(request_id='history-space',tool='spaces.create',args={'name':'history'}),token)
        sid=space['space']['id']
        status,created=call('terminal.create',{'command':'cmd.exe','args':['/Q','/C','echo HISTORY_71629'],'name':'recorded'})
        self.assertEqual(status,200,created)
        eid=created['endpoint']['id']; old=created['session']['id']
        for _ in range(60):
            status,page=call('history.read',{'endpoint_id':eid,'session_id':old,'cursor':0,'limit':100})
            if status==200 and b'HISTORY_71629' in bytes(b for e in page['events'] for b in e['bytes']):break
            time.sleep(.1)
        self.assertEqual(status,200,page)
        self.assertIn(b'HISTORY_71629',bytes(b for e in page['events'] for b in e['bytes']))
        _,invite=call('members.invite',{'roles':['observer']})
        _,member=call('members.register',{'name':'reader'},invite['invite_token'])
        credential=member['credential']
        call('endpoints.grant',{'endpoint_id':eid,'member_id':member['id'],'operations':['observe']})
        for tool in ['history.list','history.read','history.checkpoint']:
            self.assertEqual(call(tool,{'endpoint_id':eid,'session_id':old},credential)[0],200)
        self.assertEqual(call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},credential)[0],403)
        call('endpoints.grant',{'endpoint_id':eid,'member_id':member['id'],'operations':[]})
        for tool in ['history.list','history.read','history.checkpoint']:
            self.assertEqual(call(tool,{'endpoint_id':eid,'session_id':old},credential)[0],403)
        args={'endpoint_id':eid,'expected_session_id':old}
        status,restarted=call('terminal.restart',args,rid='restart-one')
        self.assertEqual(status,200,restarted)
        self.assertNotEqual(old,restarted['session']['id'])
        self.assertEqual(call('terminal.restart',args,rid='restart-one')[1],restarted)
        _,listing=call('history.list',{'endpoint_id':eid})
        self.assertEqual(len(listing['items']),2)
        # Close through the public shutdown gate, then restart our owned process handle.
        for _ in range(10):
            _,prepared=call('runtime.prepare_shutdown')
            _,result=call('runtime.force_shutdown',{'ticket':prepared['ticket']})
            if result['status']=='stopping':break
            time.sleep(.1)
        self.assertEqual(result['status'],'stopping',result)
        self.child.wait(timeout=8)
        self.child=subprocess.Popen([str(BIN),'daemon','run','--state-dir',self.state_dir,'--port',str(self.port)],stdout=self.log,stderr=self.log)
        for _ in range(100):
            try:
                if self.get('/v2/health')[0]==200:break
            except OSError:pass
            time.sleep(.1)
        status,page=call('history.read',{'endpoint_id':eid,'session_id':old})
        self.assertEqual(status,200,page)
        self.assertIn(b'HISTORY_71629',bytes(b for e in page['events'] for b in e['bytes']))
        self.assertEqual(page['checkpoint']['process_resume'],'unsupported')
        self.assertEqual(call('terminal.restart',args,rid='restart-one')[0],409)

    def test_v1_sse_cannot_hold_safe_shutdown_forever(self):
        def v1(path,body,token=None):
            headers={'Content-Type':'application/json'}
            if token:headers['Authorization']='Bearer '+token
            req=urllib.request.Request(self.base+path,data=json.dumps(body).encode(),headers=headers)
            with urllib.request.urlopen(req,timeout=5) as response:return json.load(response)
        connection=v1('/v1/connections',{'protocolMajor':'v1','callsign':'drain'})
        app=v1('/v1/apps',{'protocolMajor':'v1','connectionId':connection['connectionId'],'manifest':{'appId':'drain','displayName':'drain','originPolicy':{'exactOrigin':self.base}}},connection['capability'])
        req=urllib.request.Request(self.base+'/v1/apps/'+app['instanceId']+'/stream',headers={'Authorization':'Bearer '+app['capability'],'Origin':self.base})
        stream=urllib.request.urlopen(req,timeout=5)
        self.addCleanup(stream.close)
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,prepared=self.post(dict(request_id='prepare-sse',tool='runtime.prepare_shutdown',args={}),token)
        _,result=self.post(dict(request_id='confirm-sse',tool='runtime.confirm_shutdown',args={'ticket':prepared['ticket']}),token)
        self.assertEqual(result['status'],'stopping',result)
        self.child.wait(timeout=5)
        self.assertEqual(self.child.returncode,0)

    def test_restart_refuses_live_stale_session_and_old_lease(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='restart-space',tool='spaces.create',args={'name':'restart'}),token)
        sid=space['space']['id']
        def call(tool,args={},rid=None):return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
        _,created=call('terminal.create',{'command':'cmd.exe','args':['/Q']})
        eid=created['endpoint']['id']; old=created['session']['id']
        _,lease=call('terminal.acquire',{'endpoint_id':eid})
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old})
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'SessionAlive')
        call('terminal.force',{'endpoint_id':eid})
        status,restarted=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},'restart-shell')
        self.assertEqual(status,200,restarted)
        status,value=call('terminal.write',dict(endpoint_id=eid,input='echo stale\r\n',**lease))
        self.assertEqual(status,409,value)
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old})
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'StaleSession')
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old,'extra':1},'restart-shell')
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'RequestConflict')
        call('terminal.force',{'endpoint_id':eid})

    def test_shutdown_checkpoint_failure_is_pending(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='failure-space',tool='spaces.create',args={'name':'checkpoint failure'}),token)
        sid=space['space']['id']
        def call(tool,args={}):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
        _,created=call('terminal.create',{'command':'cmd.exe','args':['/Q']})
        eid=created['endpoint']['id'];session=created['session']['id']
        _,lease=call('terminal.acquire',{'endpoint_id':eid})
        call('terminal.write',dict(endpoint_id=eid,input='echo RECOVERY_58317\r\n',**lease))
        for _ in range(50):
            status,page=call('history.read',{'endpoint_id':eid,'session_id':session})
            if status==200 and b'RECOVERY_58317' in bytes(b for e in page['events'] for b in e['bytes']):break
            time.sleep(.1)
        self.assertEqual(status,200,page)
        self.assertIn(b'RECOVERY_58317',bytes(b for e in page['events'] for b in e['bytes']))
        before_cursor=page['checkpoint']['durable_cursor']
        checkpoint=pathlib.Path(self.state_dir)/'connections'/eid/'sessions'/session/'checkpoint.json'
        for _ in range(50):
            if checkpoint.exists():break
            time.sleep(.1)
        # External filesystem fault: replace the exact checkpoint target with a directory.
        checkpoint.unlink();checkpoint.mkdir()
        _,prepared=call('runtime.prepare_shutdown')
        status,result=call('runtime.force_shutdown',{'ticket':prepared['ticket']})
        self.assertEqual(status,200,result)
        self.assertEqual(result['status'],'pending',result)
        self.assertIn('history_error',result)
        self.assertIsNone(self.child.poll())
        # Repair the storage target; a fresh durable barrier must recover without
        # pretending the earlier failed checkpoint was persisted successfully.
        checkpoint.rmdir()
        for _ in range(10):
            _,prepared=call('runtime.prepare_shutdown')
            status,result=call('runtime.force_shutdown',{'ticket':prepared['ticket']})
            if result['status']=='stopping':break
            time.sleep(.1)
        self.assertEqual(result['status'],'stopping',result)
        self.assertEqual(result['history'],'durable',result)
        self.child.wait(timeout=8)
        saved=json.loads(checkpoint.read_text(encoding='utf-8'))
        self.assertEqual(saved['session_id'],session)
        self.assertEqual(saved['status'],'exited')
        self.assertGreaterEqual(saved['durable_cursor'],before_cursor)
        self.child=subprocess.Popen([str(BIN),'daemon','run','--state-dir',self.state_dir,'--port',str(self.port)],stdout=self.log,stderr=self.log)
        for _ in range(100):
            try:
                if self.get('/v2/health')[0]==200:break
            except OSError:pass
            time.sleep(.1)
        status,page=call('history.read',{'endpoint_id':eid,'session_id':session})
        self.assertEqual(status,200,page)
        self.assertIn(b'RECOVERY_58317',bytes(b for e in page['events'] for b in e['bytes']))

    def test_session_maintainer_cannot_register_github_authority(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='source-authority-space',tool='spaces.create',args={'name':'authority'}),token)
        sid=space['space']['id']
        def call(tool,args,credential):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,invite=call('members.invite',{'roles':['session_maintainer']},token)
        _,member=call('members.register',{'name':'terminal maintainer'},invite['invite_token'])
        status,value=call('endpoints.register',{'name':'forged source','kind':'github','config':{'repository':'TLOGBen/LinkStart','allow_write_back':True,'source_authority':True}},member['credential'])
        self.assertEqual(status,403,value)
        self.assertEqual(value['error']['code'],'Denied')

if __name__=='__main__':unittest.main()
