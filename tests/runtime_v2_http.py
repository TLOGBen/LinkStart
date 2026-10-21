"""Public CLI -> real HTTP integration; never imports a product internal module."""
import hashlib, json, os, pathlib, socket, subprocess, sys, tempfile, threading, time, unittest, urllib.request, urllib.error
ROOT=pathlib.Path(__file__).resolve().parents[1]
BIN=pathlib.Path(os.environ.get('LINKSTART_BIN',str(ROOT/'target/debug/linkstart.exe')))

class RuntimeHttp(unittest.TestCase):
    def native_fixture_args(self, name, marker=None):
        # One local Python child, no provider/model or executable-specific shell.
        # Restart reuses these args and appends its actual PID to the same log.
        release=pathlib.Path(self.tmp.name)/(name+'-cleanup-release')
        pids=pathlib.Path(self.tmp.name)/(name+'-native-pids')
        script=pathlib.Path(self.tmp.name)/(name+'-child.py')
        script.write_text("""import os,pathlib,queue,sys,threading,time
release=pathlib.Path(sys.argv[1])
with pathlib.Path(sys.argv[2]).open('a',encoding='utf-8') as log:
 log.write(str(os.getpid())+'\\n');log.flush()
if len(sys.argv)>3:
 print(sys.argv[3],flush=True)
 sys.exit(0)
commands=queue.Queue()
def read_commands():
 for line in sys.stdin:commands.put(line.strip())
threading.Thread(target=read_commands,daemon=True).start()
print('NATIVE_READY',flush=True)
deadline=time.monotonic()+30
try:
 while not release.exists() and time.monotonic()<deadline:
  try:line=commands.get(timeout=.05)
  except queue.Empty:continue
  if line=='quit':break
  if line=='marker':print('RECOVERY_58317',flush=True)
  elif line.startswith('double '):print(int(line.split()[1])*2,flush=True)
except KeyboardInterrupt:pass
print('NATIVE_EXIT',flush=True)
os._exit(0)  # No interpreter-finalization race with the blocked input daemon thread.
""",encoding='utf-8')
        self.native_fixtures.append((release,pids))
        args=['-u',str(script),str(release),str(pids)]
        if marker is not None:args.append(marker)
        return {'command':sys.executable,'args':args,'name':name}

    def wait_terminal(self, call, eid, *, text=None, exited=False):
        for _ in range(100):
            status,view=call('terminal.observe',{'endpoint_id':eid,'cursor':0})
            self.assertEqual(status,200,view)
            if (text is None or text in view['text']) and (not exited or view['status']=='exited'):
                return view
            time.sleep(.05)
        self.fail('native terminal did not reach expected output/exit: '+repr(view))

    def exit_terminal(self, call, eid):
        status,lease=call('terminal.acquire',{'endpoint_id':eid})
        self.assertEqual(status,200,lease)
        status,result=call('terminal.write',dict(endpoint_id=eid,input='quit\r\n',**lease))
        self.assertEqual(status,200,result)
        status,result=call('terminal.release',dict(endpoint_id=eid,**lease))
        self.assertEqual(status,200,result)
        self.wait_terminal(call,eid,exited=True)

    def assert_force_platform(self, call, eid):
        status,result=call('terminal.force',{'endpoint_id':eid})
        if os.name=='nt':
            self.assertEqual(status,200,result)
            self.wait_terminal(call,eid,exited=True)
        else:
            self.assertEqual(status,409,result)
            self.assertEqual(result['error']['code'],'NativeError',result)
            self.assertIn('Unsupported',result['error']['message'])
            # Unsupported still fences the old lease. Reacquire for intentional exit.
            self.assertEqual(self.wait_terminal(call,eid)['status'],'live')
            self.exit_terminal(call,eid)

    def cli_control(self, action):
        args=[str(BIN),'daemon',action,'--json','--state-dir',self.state_dir]
        if action!='stop':args+=['--port',str(self.port)]
        return subprocess.run(args,capture_output=True,text=True,timeout=12)

    def cli_cleanup(self):
        # Only this test's fresh state directory is addressed.
        self.cli_control('stop')

    def recorded_terminal(self, name, script):
        pids=pathlib.Path(self.tmp.name)/(name+'-native-pids')
        self.native_fixtures.append((pathlib.Path(self.tmp.name)/(name+'-cleanup-release'),pids))
        script="import os,pathlib\nwith pathlib.Path("+repr(str(pids))+").open('a',encoding='utf-8') as log:log.write(str(os.getpid())+'\\n');log.flush()\n"+script
        owner=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        status,space=self.post(dict(request_id=name+'-space',tool='spaces.create',args={'name':name}),owner)
        self.assertEqual(status,200,space)
        sid=space['space']['id']
        status,created=self.post(dict(request_id=name+'-terminal',tool='terminal.create',space_id=sid,args={'command':sys.executable,'args':['-u','-c',script]}),owner)
        self.assertEqual(status,200,created)
        return owner,sid,created

    def test_cli_stop_and_restart_drain_active_terminal_and_persist_final_tail(self):
        print(json.dumps({'evidence':'cli-binary','path':str(BIN.resolve()),'sha256':hashlib.sha256(BIN.read_bytes()).hexdigest()}),flush=True)
        for action in ['restart','stop']:
            with self.subTest(action=action):
                self.addCleanup(self.cli_cleanup)
                release=pathlib.Path(self.tmp.name)/('release-'+action)
                self.native_fixtures.append((release,pathlib.Path(self.tmp.name)/('cli-'+action+'-native-pids')))
                tail_time=pathlib.Path(self.tmp.name)/('tail-time-'+action)
                marker='CLI_FINAL_TAIL_'+action
                script="import pathlib,time\np=pathlib.Path("+repr(str(release))+")\nprint('CLI_ACTIVE_READY',flush=True)\ntry:\n while not p.exists():time.sleep(.01)\nexcept KeyboardInterrupt:pass\nprint("+repr(marker)+",flush=True)\npathlib.Path("+repr(str(tail_time))+").write_text(str(time.time_ns()))\n"
                token,sid,created=self.recorded_terminal('cli-'+action,script)
                eid=created['endpoint']['id']; session=created['session']['id']
                def call(tool,args):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
                for _ in range(80):
                    status,view=call('terminal.observe',{'endpoint_id':eid,'cursor':0})
                    if status==200 and 'CLI_ACTIVE_READY' in view['text']:break
                    time.sleep(.05)
                self.assertIn('CLI_ACTIVE_READY',view['text'])
                self.assertEqual(view['status'],'live')
                before=json.loads((pathlib.Path(self.state_dir)/'daemon.json').read_text())
                timer=threading.Timer(.15,release.touch);timer.start()
                cli_started=time.time_ns()
                result=self.cli_control(action)
                cli_returned=time.time_ns()
                timer.join()
                self.assertEqual(result.returncode,0,result.stderr+' '+result.stdout)
                control=json.loads(result.stdout)
                if self.child.pid==before['pid']:self.child.wait(timeout=5)
                self.assertFalse(self.pid_alive(before['pid']))
                self.assertFalse(self.pid_alive(created['session']['process_id']))
                root=pathlib.Path(self.state_dir)/'connections'/eid/'sessions'/session
                saved=json.loads((root/'checkpoint.json').read_text())
                self.assertEqual(saved['status'],'exited',saved)
                events=[json.loads(line) for line in (root/'events.jsonl').read_text().splitlines()]
                raw=bytes(b for event in events for b in event.get('bytes',[]))
                self.assertIn(marker.encode(),raw)
                self.assertGreater(saved['durable_cursor'],0)
                print(json.dumps({'evidence':'cli-drain','action':action,'old_daemon_pid':before['pid'],'terminal_pid':created['session']['process_id'],'cli_started_ns':cli_started,'tail_flushed_ns':int(tail_time.read_text()),'cli_returned_ns':cli_returned,'exit_observed_ns':time.time_ns(),'prepare_received_ns':'UNKNOWN: no runtime instrumentation','tail_persisted_after_daemon_exit':True,'durable_cursor':saved['durable_cursor'],'checkpoint_status':saved['status']}),flush=True)
                self.assertEqual((pathlib.Path(self.state_dir)/'v2-owner.token').read_text(),token)
                if action=='restart':
                    self.assertEqual(control['state'],'started',control)
                    self.assertEqual(control['stoppedBeforeRestart'],True)
                    self.assertNotEqual(before['pid'],control['pid'])
                    self.assertTrue(self.pid_alive(control['pid']))
                    status,page=call('history.read',{'endpoint_id':eid,'session_id':session})
                    self.assertEqual(status,200,page)
                    self.assertIn(marker.encode(),bytes(b for event in page['events'] for b in event['bytes']))
                else:
                    self.assertEqual(control,{'stopped':True})
                    self.assertFalse((pathlib.Path(self.state_dir)/'daemon.json').exists())

    @staticmethod
    def pid_alive(pid):
        if os.name=='nt':
            import ctypes
            kernel=ctypes.WinDLL('kernel32',use_last_error=True)
            kernel.OpenProcess.restype=ctypes.c_void_p
            kernel.GetExitCodeProcess.argtypes=[ctypes.c_void_p,ctypes.POINTER(ctypes.c_ulong)]
            kernel.CloseHandle.argtypes=[ctypes.c_void_p]
            handle=kernel.OpenProcess(0x1000,False,pid)
            if not handle:return False
            code=ctypes.c_ulong()
            ok=kernel.GetExitCodeProcess(handle,ctypes.byref(code));kernel.CloseHandle(handle)
            return bool(ok) and code.value==259
        try:os.kill(pid,0);return True
        except ProcessLookupError:return False

    def test_cli_pending_history_barrier_keeps_old_recorded_daemon_without_replacement(self):
        print(json.dumps({'evidence':'cli-binary','path':str(BIN.resolve()),'sha256':hashlib.sha256(BIN.read_bytes()).hexdigest()}),flush=True)
        self.addCleanup(self.cli_cleanup)
        token,sid,created=self.recorded_terminal('cli-pending',"print('CLI_PENDING_TAIL',flush=True)")
        eid=created['endpoint']['id'];session=created['session']['id']
        root=pathlib.Path(self.state_dir)/'connections'/eid/'sessions'/session
        checkpoint=root/'checkpoint.json'
        for _ in range(80):
            self.post(dict(request_id=str(time.time_ns()),tool='terminal.observe',space_id=sid,args={'endpoint_id':eid,'cursor':0}),token)
            status,page=self.post(dict(request_id=str(time.time_ns()),tool='history.read',space_id=sid,args={'endpoint_id':eid,'session_id':session}),token)
            if status==200 and page['checkpoint']['status']=='exited' and checkpoint.exists():break
            time.sleep(.05)
        self.assertEqual(status,200,page)
        self.assertEqual(page['checkpoint']['status'],'exited',page)
        record=(pathlib.Path(self.state_dir)/'daemon.json').read_bytes()
        old=json.loads(record)['pid']
        checkpoint.unlink();checkpoint.mkdir()
        try:
            for action in ['stop','restart']:
                result=self.cli_control(action)
                self.assertNotEqual(result.returncode,0,result.stdout)
                self.assertEqual(json.loads(result.stdout)['state'],'failed')
                self.assertTrue(self.pid_alive(old),'pending cleanup must preserve the old daemon')
                self.assertEqual((pathlib.Path(self.state_dir)/'daemon.json').read_bytes(),record)
                self.assertEqual((pathlib.Path(self.state_dir)/'v2-owner.token').read_text(),token)
                print(json.dumps({'evidence':'cli-pending','action':action,'recorded_pid':old,'recorded_pid_alive':True,'discovery_unchanged':True,'exit_code':result.returncode,'result':json.loads(result.stdout)}),flush=True)
        finally:
            # Restore only this test-owned checkpoint fault, even after RED.
            if checkpoint.is_dir():checkpoint.rmdir()
        result=self.cli_control('stop')
        self.assertEqual(result.returncode,0,result.stdout+' '+result.stderr)
        self.assertEqual(json.loads(result.stdout),{'stopped':True})
        self.child.wait(timeout=5)
        self.assertFalse(self.pid_alive(old))
        self.assertEqual(json.loads(checkpoint.read_text())['status'],'exited')

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
        self.native_fixtures=[]
        self.owned_terminals=[]
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
        # Independent fixture release survives RED, revoked leases and draining.
        # Address only files created in this test's private temp directory.
        for release,_ in self.native_fixtures:release.touch()
        until=time.monotonic()+8
        while time.monotonic()<until:
            for _,sid,eid,token in self.owned_terminals:
                if self.child.poll() is None:
                    try:self.post(dict(request_id=str(time.time_ns()),tool='terminal.observe',space_id=sid,args={'endpoint_id':eid}),token)
                    except OSError:pass
            pids={pid for pid,_,_,_ in self.owned_terminals if pid is not None}
            for _,log in self.native_fixtures:
                if log.exists():pids.update(int(line) for line in log.read_text().splitlines())
            if not any(self.pid_alive(pid) for pid in pids):break
            time.sleep(.05)
        alive=[pid for pid in pids if self.pid_alive(pid)]
        if self.child.poll() is None:
            try:self.cli_cleanup()
            except (OSError,subprocess.TimeoutExpired):pass
        if self.child.poll() is None:self.child.terminate();self.child.wait(timeout=10)
        print(json.dumps({'evidence':'http-owned-child-cleanup','tracked_pids':sorted(pids),'alive_before_daemon_cleanup':alive,'alive_after_daemon_cleanup':[pid for pid in pids if self.pid_alive(pid)]}),flush=True)
        remaining=[pid for pid in pids if self.pid_alive(pid)]
        self.log.close()
        if not remaining:self.tmp.cleanup()
        self.assertEqual(remaining,[],'test-owned native children remain; fixture directory '+self.tmp.name)
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
            with urllib.request.urlopen(req,timeout=5) as r:
                value=json.load(r)
                if body['tool'] in ['terminal.create','terminal.restart']:
                    item=(value['session']['process_id'],body.get('space_id'),value['endpoint']['id'],credential)
                    if item not in self.owned_terminals:self.owned_terminals.append(item)
                return r.status,value
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
        args=self.native_fixture_args('owned arithmetic')
        created=call('terminal.create',args,'create-terminal',sid)
        eid=created['endpoint']['id']
        replay=call('terminal.create',args,'create-terminal',sid)
        self.assertEqual(created['session']['id'],replay['session']['id'])
        lease=call('terminal.acquire',{'endpoint_id':eid},space_id=sid)
        self.wait_terminal(lambda tool,args:self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token),eid,text='NATIVE_READY')
        call('terminal.write',dict(endpoint_id=eid,input='double 314159\r\n',**lease),space_id=sid)
        for _ in range(60):
            output=call('terminal.observe',{'endpoint_id':eid,'cursor':0},space_id=sid)
            if '628318' in output['text']:break
            time.sleep(.1)
        self.assertIn('628318',output['text'])
        call('terminal.release',dict(endpoint_id=eid,**lease),space_id=sid)
        self.assert_force_platform(lambda tool,args:self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token),eid)

    def test_history_restart_recovery_and_revocation(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args={},credential=token,rid=None):
            return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,space=self.post(dict(request_id='history-space',tool='spaces.create',args={'name':'history'}),token)
        sid=space['space']['id']
        status,created=call('terminal.create',self.native_fixture_args('recorded',marker='HISTORY_71629'))
        self.assertEqual(status,200,created)
        eid=created['endpoint']['id']; old=created['session']['id']
        for _ in range(60):
            status,page=call('history.read',{'endpoint_id':eid,'session_id':old,'cursor':0,'limit':100})
            if status==200 and b'HISTORY_71629' in bytes(b for e in page['events'] for b in e['bytes']):break
            time.sleep(.1)
        self.assertEqual(status,200,page)
        self.assertIn(b'HISTORY_71629',bytes(b for e in page['events'] for b in e['bytes']))
        self.wait_terminal(call,eid,exited=True)
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
        self.wait_terminal(call,eid,exited=True)
        # All owned children exited; use the authenticated normal gate on Linux.
        # Windows retains runtime.force_shutdown coverage here and in fault recovery.
        for _ in range(10):
            _,prepared=call('runtime.prepare_shutdown')
            status,result=call('runtime.force_shutdown' if os.name=='nt' else 'runtime.confirm_shutdown',{'ticket':prepared['ticket']})
            self.assertEqual(status,200,result)
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

    def test_cli_stop_waits_for_bounded_v1_sse_transport_drain(self):
        def v1(path,body,token=None):
            headers={'Content-Type':'application/json'}
            if token:headers['Authorization']='Bearer '+token
            req=urllib.request.Request(self.base+path,data=json.dumps(body).encode(),headers=headers)
            with urllib.request.urlopen(req,timeout=5) as response:return json.load(response)
        connection=v1('/v1/connections',{'protocolMajor':'v1','callsign':'cli-drain'})
        app=v1('/v1/apps',{'protocolMajor':'v1','connectionId':connection['connectionId'],'manifest':{'appId':'cli-drain','displayName':'cli-drain','originPolicy':{'exactOrigin':self.base}}},connection['capability'])
        req=urllib.request.Request(self.base+'/v1/apps/'+app['instanceId']+'/stream',headers={'Authorization':'Bearer '+app['capability'],'Origin':self.base})
        stream=urllib.request.urlopen(req,timeout=5)
        self.addCleanup(stream.close)
        started=time.time_ns()
        result=self.cli_control('stop')
        state_file=pathlib.Path('/proc')/str(self.child.pid)/'status'
        state=next((line for line in state_file.read_text().splitlines() if line.startswith('State:')),'UNKNOWN') if sys.platform=='linux' and state_file.exists() else 'not Linux /proc'
        print(json.dumps({'evidence':'cli-sse-drain','binary_sha256':hashlib.sha256(BIN.read_bytes()).hexdigest(),'started_ns':started,'returned_ns':time.time_ns(),'exit_code':result.returncode,'daemon_native_state_before_parent_wait':state,'result':json.loads(result.stdout)}),flush=True)
        self.assertEqual(result.returncode,0,result.stderr+' '+result.stdout)
        self.assertEqual(json.loads(result.stdout),{'stopped':True})
        if sys.platform=='linux':
            self.assertIn('Z (zombie)',state,'fixture parent must not reap before the CLI exit witness')
        self.child.wait(timeout=5)
        self.assertEqual(self.child.returncode,0)
        self.assertFalse((pathlib.Path(self.state_dir)/'daemon.json').exists())

    def test_restart_refuses_live_stale_session_and_old_lease(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='restart-space',tool='spaces.create',args={'name':'restart'}),token)
        sid=space['space']['id']
        def call(tool,args={},rid=None):return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
        status,created=call('terminal.create',self.native_fixture_args('restart live'))
        self.assertEqual(status,200,created)
        eid=created['endpoint']['id']; old=created['session']['id']
        _,lease=call('terminal.acquire',{'endpoint_id':eid})
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old})
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'SessionAlive')
        self.wait_terminal(call,eid,text='NATIVE_READY')
        self.assert_force_platform(call,eid)
        status,restarted=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},'restart-shell')
        self.assertEqual(status,200,restarted)
        status,value=call('terminal.write',dict(endpoint_id=eid,input='double 1\r\n',**lease))
        self.assertEqual(status,409,value)
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old})
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'StaleSession')
        status,value=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old,'extra':1},'restart-shell')
        self.assertEqual(status,409,value);self.assertEqual(value['error']['code'],'RequestConflict')
        self.wait_terminal(call,eid,text='NATIVE_READY')
        self.assert_force_platform(call,eid)

    def test_shutdown_checkpoint_failure_is_pending(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='failure-space',tool='spaces.create',args={'name':'checkpoint failure'}),token)
        sid=space['space']['id']
        def call(tool,args={}):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
        status,created=call('terminal.create',self.native_fixture_args('checkpoint live'))
        self.assertEqual(status,200,created)
        eid=created['endpoint']['id'];session=created['session']['id']
        self.wait_terminal(call,eid,text='NATIVE_READY')
        if os.name!='nt':
            # Exercise documented force rejection while still genuinely live.
            status,result=call('terminal.force',{'endpoint_id':eid})
            self.assertEqual(status,409,result)
            self.assertEqual(result['error']['code'],'NativeError',result)
            self.assertIn('Unsupported',result['error']['message'])
            self.assertEqual(self.wait_terminal(call,eid)['status'],'live')
        status,lease=call('terminal.acquire',{'endpoint_id':eid})
        self.assertEqual(status,200,lease)
        status,result=call('terminal.write',dict(endpoint_id=eid,input='marker\r\n',**lease))
        self.assertEqual(status,200,result)
        status,result=call('terminal.release',dict(endpoint_id=eid,**lease))
        self.assertEqual(status,200,result)
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
        self.addCleanup(lambda: checkpoint.rmdir() if checkpoint.is_dir() else None)
        if os.name!='nt':
            # Finish input before draining forbids further acquire/write.
            self.exit_terminal(call,eid)
        shutdown_tool='runtime.force_shutdown' if os.name=='nt' else 'runtime.confirm_shutdown'
        _,prepared=call('runtime.prepare_shutdown')
        status,result=call(shutdown_tool,{'ticket':prepared['ticket']})
        self.assertEqual(status,200,result)
        self.assertEqual(result['status'],'pending',result)
        self.assertIn('history_error',result)
        self.assertIsNone(self.child.poll())
        # Repair the storage target; a fresh durable barrier must recover without
        # pretending the earlier failed checkpoint was persisted successfully.
        checkpoint.rmdir()
        for _ in range(10):
            _,prepared=call('runtime.prepare_shutdown')
            status,result=call(shutdown_tool,{'ticket':prepared['ticket']})
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
