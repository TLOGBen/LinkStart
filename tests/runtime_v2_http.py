"""Public CLI -> real HTTP integration; never imports a product internal module."""
import hashlib, json, os, pathlib, socket, subprocess, sys, tempfile, threading, time, unittest, urllib.request, urllib.error
ROOT=pathlib.Path(__file__).resolve().parents[1]
BIN=pathlib.Path(os.environ.get('LINKSTART_BIN',str(ROOT/'target/debug/linkstart.exe')))
WINDOWS=os.name=='nt'
SHELL={'command':'cmd.exe','args':['/Q']} if WINDOWS else {'command':'/bin/sh','args':[]}
def shell_once(line):return {'command':'cmd.exe','args':['/Q','/C',line]} if WINDOWS else {'command':'/bin/sh','args':['-c',line]}

class RuntimeHttp(unittest.TestCase):
    def cli_control(self, action):
        args=[str(BIN),'daemon',action,'--json','--state-dir',self.state_dir]
        if action!='stop':args+=['--port',str(self.port)]
        return subprocess.run(args,capture_output=True,text=True,timeout=12)

    def cli_cleanup(self):
        # Only this test's fresh state directory is addressed.
        self.cli_control('stop')

    def recorded_terminal(self, name, script):
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
                tail_time=pathlib.Path(self.tmp.name)/('tail-time-'+action)
                marker='CLI_FINAL_TAIL_'+action
                # Once out of the wait loop the child ignores Ctrl-C: under load the
                # shutdown interrupt can land after release, while the tail is being
                # written, and cut it (hypothesis load-deadlines-2, B-1: 34/236).
                script="import pathlib,signal,time\np=pathlib.Path("+repr(str(release))+")\nprint('CLI_ACTIVE_READY',flush=True)\ntry:\n while not p.exists():time.sleep(.01)\n signal.signal(signal.SIGINT,signal.SIG_IGN)\nexcept KeyboardInterrupt:signal.signal(signal.SIGINT,signal.SIG_IGN)\nprint("+repr(marker)+",flush=True)\npathlib.Path("+repr(str(tail_time))+").write_text(str(time.time_ns()))\n"
                token,sid,created=self.recorded_terminal('cli-'+action,script)
                eid=created['endpoint']['id']; session=created['session']['id']
                # (pid, creation time), so a live PID at the check can be told apart
                # from a reused one (hypothesis load-deadlines-2, B).
                terminal_identity=self.process_identity(created['session']['process_id'])
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
                if result.returncode!=0 and 'did not exit after interrupt' in result.stderr:
                    # The runtime gives terminals 500 ms after the interrupt and
                    # otherwise reports this pending and stays draining; wait for the
                    # terminal to exit, then ask again (hypothesis load-deadlines-2, B-2).
                    until=time.time()+60
                    while True:
                        status,view=call('terminal.observe',{'endpoint_id':eid,'cursor':0})
                        self.assertEqual(status,200,view)
                        if view['status']=='exited':break
                        self.assertLess(time.time(),until,view)
                        time.sleep(.05)
                    result=self.cli_control(action)
                    cli_returned=time.time_ns()
                self.assertEqual(result.returncode,0,result.stderr+' '+result.stdout)
                control=json.loads(result.stdout)
                if self.child.pid==before['pid']:self.child.wait(timeout=5)
                self.assertFalse(self.pid_alive(before['pid']))
                at_check=self.process_identity(created['session']['process_id'])
                self.assertFalse(at_check is not None and at_check==terminal_identity,
                    'terminal process still alive after daemon '+action+': '+json.dumps({'created':terminal_identity,'at_check':at_check,'daemon':result.stdout+' '+result.stderr}))
                if at_check is not None:
                    print(json.dumps({'evidence':'terminal-pid-reused','action':action,'created':terminal_identity,'at_check':at_check}),flush=True)
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

    def kill_if_alive(self,pid):
        # Only the stubborn terminal this test started, if RED left it running.
        if self.pid_alive(pid):
            if os.name=='nt':subprocess.run(['taskkill','/F','/T','/PID',str(pid)],capture_output=True)
            else:
                try:os.kill(pid,9)
                except ProcessLookupError:pass

    @classmethod
    def process_identity(cls,pid):
        # (pid, creation time) of a live process, or None if pid is not alive.
        if not cls.pid_alive(pid):return None
        if os.name=='nt':
            import ctypes
            from ctypes import wintypes
            kernel=ctypes.WinDLL('kernel32',use_last_error=True)
            kernel.OpenProcess.restype=ctypes.c_void_p
            kernel.GetProcessTimes.argtypes=[ctypes.c_void_p]+[ctypes.POINTER(wintypes.FILETIME)]*4
            kernel.CloseHandle.argtypes=[ctypes.c_void_p]
            handle=kernel.OpenProcess(0x1000,False,pid)
            if not handle:return None
            times=[wintypes.FILETIME() for _ in range(4)]
            ok=kernel.GetProcessTimes(handle,*[ctypes.byref(t) for t in times]);kernel.CloseHandle(handle)
            return [pid,(times[0].dwHighDateTime<<32)|times[0].dwLowDateTime] if ok else None
        try:stat=pathlib.Path('/proc/%d/stat'%pid).read_text()
        except OSError:return None
        return [pid,int(stat.rsplit(')',1)[1].split()[19])]

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
            # Shutdown was admitted, so every health endpoint says so and start never reuses it.
            self.assertEqual(self.get('/v1/health')[1]['status'],'draining')
            self.assertEqual(self.get('/v2/health')[1]['status'],'draining')
            result=self.cli_control('start')
            self.assertNotEqual(result.returncode,0,result.stdout)
            self.assertIn('runtime_draining',result.stdout+result.stderr)
            self.assertEqual((pathlib.Path(self.state_dir)/'daemon.json').read_bytes(),record)
        finally:
            # Restore only this test-owned checkpoint fault, even after RED.
            if checkpoint.is_dir():checkpoint.rmdir()
        result=self.cli_control('stop')
        self.assertEqual(result.returncode,0,result.stdout+' '+result.stderr)
        self.assertEqual(json.loads(result.stdout),{'stopped':True})
        self.child.wait(timeout=5)
        self.assertFalse(self.pid_alive(old))
        self.assertEqual(json.loads(checkpoint.read_text())['status'],'exited')

    def test_cli_stop_names_force_shutdown_when_a_terminal_never_exits(self):
        # K11: a terminal that ignores the graceful interrupt keeps shutdown pending;
        # the stop error must name the `linkstart tool` + runtime.force_shutdown way out.
        script="import signal,threading\nsignal.signal(signal.SIGINT,signal.SIG_IGN)\nprint('CLI_STUBBORN_READY',flush=True)\nthreading.Event().wait()\n"
        token,sid,created=self.recorded_terminal('cli-stubborn',script)
        eid=created['endpoint']['id']
        self.addCleanup(self.kill_if_alive,created['session']['process_id'])
        for _ in range(80):
            status,view=self.post(dict(request_id=str(time.time_ns()),tool='terminal.observe',space_id=sid,args={'endpoint_id':eid,'cursor':0}),token)
            if status==200 and 'CLI_STUBBORN_READY' in view['text']:break
            time.sleep(.05)
        self.assertIn('CLI_STUBBORN_READY',view['text'])
        old=json.loads((pathlib.Path(self.state_dir)/'daemon.json').read_text())['pid']
        result=self.cli_control('stop')
        self.assertNotEqual(result.returncode,0,result.stdout)
        error=json.loads(result.stdout)['error']
        tokenfile=pathlib.Path(self.state_dir)/'v2-owner.token'
        for hint in ['linkstart tool','"request_id"','runtime.prepare_shutdown','runtime.force_shutdown','"ticket"','draining','--state-dir "'+self.state_dir+'"','--credential-file "'+str(tokenfile)+'"']:
            self.assertIn(hint,error)
        self.assertNotIn(token.strip(),result.stdout+result.stderr)
        self.assertTrue(self.pid_alive(old))
        # The named way out works: prepare, then force with that ticket.
        def tool(name,args):
            run=subprocess.run([str(BIN),'tool','--state-dir',self.state_dir,'--credential-file',str(tokenfile)],input=json.dumps({'request_id':str(time.time_ns()),'tool':name,'args':args}),text=True,capture_output=True,timeout=10)
            self.assertEqual(run.returncode,0,run.stderr)
            return json.loads(run.stdout)
        prepared=tool('runtime.prepare_shutdown',{})
        forced=tool('runtime.force_shutdown',{'ticket':prepared['ticket']})
        self.assertEqual(forced['status'],'stopping',forced)
        self.child.wait(timeout=8)
        self.assertFalse(self.pid_alive(created['session']['process_id']))

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

    def test_cli_tool_direct_address_needs_no_private_state(self):
        # A sandboxed agent holds only an address and its own credential file.
        owner=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        agent_dir=pathlib.Path(self.tmp.name)/'agent-only'
        agent_dir.mkdir()
        credential=agent_dir/'agent.cred'
        credential.write_text(owner)
        missing=str(agent_dir/'no-such-state')
        request=json.dumps({'request_id':'direct-list','tool':'spaces.list','args':{}})
        result=subprocess.run([str(BIN),'tool','--address','127.0.0.1:%d'%self.port,'--state-dir',missing,'--credential-file',str(credential)],input=request,text=True,capture_output=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('items',json.loads(result.stdout))
        self.assertFalse(pathlib.Path(missing).exists())
        # Invite tokens and member credentials prove the Runtime the same way.
        _,space=self.post(dict(request_id='direct-space',tool='spaces.create',args={'name':'direct'}),owner)
        sid=space['space']['id']
        _,invite=self.post(dict(request_id='direct-invite',tool='members.invite',space_id=sid,args={'roles':['observer']}),owner)
        credential.write_text(invite['invite_token'])
        registered=subprocess.run([str(BIN),'tool','--address','127.0.0.1:%d'%self.port,'--credential-file',str(credential)],input=json.dumps({'request_id':'direct-reg','tool':'members.register','space_id':sid,'args':{'name':'sandboxed'}}),text=True,capture_output=True,timeout=10)
        self.assertEqual(registered.returncode,0,registered.stderr)
        credential.write_text(json.loads(registered.stdout)['credential'])
        current=subprocess.run([str(BIN),'tool','--address','127.0.0.1:%d'%self.port,'--credential-file',str(credential)],input=json.dumps({'request_id':'direct-me','tool':'identity.current','space_id':sid,'args':{}}),text=True,capture_output=True,timeout=10)
        self.assertEqual(current.returncode,0,current.stderr)
        refused=subprocess.run([str(BIN),'tool','--address','192.0.2.1:%d'%self.port,'--credential-file',str(credential)],input=request,text=True,capture_output=True,timeout=10)
        self.assertNotEqual(refused.returncode,0)
        self.assertIn('loopback',refused.stderr)
        self.assertNotIn(owner,refused.stdout+refused.stderr)

    def test_direct_address_never_sends_credential_to_unproven_runtime(self):
        # K8: another local process holding the address, or relaying to the real
        # Runtime, must not receive the credential.
        owner=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        credential=pathlib.Path(self.tmp.name)/'impostor.cred'
        credential.write_text(owner)
        request=json.dumps({'request_id':'impostor-list','tool':'spaces.list','args':{}})
        real_host=b'127.0.0.1:%d'%self.port
        def serve(mode):
            # fake: answers by itself; relay: forwards bytes unchanged; rewrite:
            # forwards with Host rewritten, so the real Runtime does answer.
            listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen(4);listener.settimeout(10)
            own_host=b'127.0.0.1:%d'%listener.getsockname()[1]
            sent,answered=[],[]
            def run():
                try:
                    while True:
                        conn,_=listener.accept()
                        upstream=socket.create_connection(('127.0.0.1',self.port)) if mode!='fake' else None
                        def pump(src,dst,log,outbound):
                            try:
                                while (chunk:=src.recv(65536)):
                                    log.append(chunk)
                                    if outbound and mode=='rewrite':chunk=chunk.replace(b'Host: '+own_host,b'Host: '+real_host)
                                    if dst:dst.sendall(chunk)
                                    else:src.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}')
                            except OSError:pass
                        if upstream:
                            threading.Thread(target=pump,args=(upstream,conn,answered,False),daemon=True).start()
                        threading.Thread(target=pump,args=(conn,upstream,sent,True),daemon=True).start()
                except OSError:pass
            threading.Thread(target=run,daemon=True).start()
            return listener,sent,answered
        for mode in ('fake','relay','rewrite'):
            listener,sent,answered=serve(mode)
            try:
                result=subprocess.run([str(BIN),'tool','--address','127.0.0.1:%d'%listener.getsockname()[1],'--credential-file',str(credential)],input=request,text=True,capture_output=True,timeout=15)
            finally:listener.close()
            self.assertNotEqual(result.returncode,0,(mode,result.stdout))
            self.assertIn('runtime_identity_unverified',result.stderr,mode)
            self.assertTrue(sent,mode)
            self.assertNotIn(owner.encode(),b''.join(sent),mode)
            self.assertNotIn(owner,result.stdout+result.stderr)
            if mode=='rewrite':
                # The real Runtime proved itself, but for its own address, not this one.
                self.assertIn(b'"proof"',b''.join(answered))

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
        args=dict(SHELL,name='owned shell')
        created=call('terminal.create',args,'create-terminal',sid)
        eid=created['endpoint']['id']
        replay=call('terminal.create',args,'create-terminal',sid)
        self.assertEqual(created['session']['id'],replay['session']['id'])
        lease=call('terminal.acquire',{'endpoint_id':eid},space_id=sid)
        call('terminal.write',dict(endpoint_id=eid,input=('set /a 314159*2' if WINDOWS else 'echo $((314159*2))')+'\r\n',**lease),space_id=sid)
        for _ in range(60):
            output=call('terminal.observe',{'endpoint_id':eid,'cursor':0},space_id=sid)
            if '628318' in output['text']:break
            time.sleep(.1)
        self.assertIn('628318',output['text'])
        call('terminal.release',dict(endpoint_id=eid,**lease),space_id=sid)
        call('terminal.force',{'endpoint_id':eid},space_id=sid)

    def test_slow_terminal_operation_does_not_stall_other_agents(self):
        # Agent A's terminal ignores Ctrl-C, so each graceful stop waits its full deadline.
        stubborn="import signal,time\nsignal.signal(signal.SIGINT,signal.SIG_IGN)\nprint('STUBBORN_READY',flush=True)\ntime.sleep(60)\n"
        token,sid,slow=self.recorded_terminal('slow',stubborn)
        _,_,fast=self.recorded_terminal('fast',"print('FAST_READY',flush=True)\nimport time\ntime.sleep(60)\n")
        slow_eid=slow['endpoint']['id']; fast_eid=fast['endpoint']['id']
        def observe(eid,space):
            return self.post(dict(request_id=str(time.time_ns()),tool='terminal.observe',space_id=space,args={'endpoint_id':eid,'cursor':0}),token)
        slow_space=[s for s in self.post(dict(request_id='ls',tool='spaces.list',args={}),token)[1]['items'] if s['name']=='slow'][0]['id']
        fast_space=[s for s in self.post(dict(request_id='lf',tool='spaces.list',args={}),token)[1]['items'] if s['name']=='fast'][0]['id']
        for eid,space,marker in [(slow_eid,slow_space,'STUBBORN_READY'),(fast_eid,fast_space,'FAST_READY')]:
            for _ in range(100):
                status,view=observe(eid,space)
                if status==200 and marker in view['text']:break
                time.sleep(.05)
            self.assertIn(marker,view['text'])
        stopping=threading.Event()
        def stop_loop():
            while not stopping.is_set():
                self.post(dict(request_id=str(time.time_ns()),tool='terminal.stop',space_id=slow_space,args={'endpoint_id':slow_eid}),token)
        worker=threading.Thread(target=stop_loop);worker.start()
        latencies=[];health=[]
        try:
            time.sleep(.1)
            for _ in range(15):
                started=time.monotonic();status,_=observe(fast_eid,fast_space);latencies.append(time.monotonic()-started)
                self.assertEqual(status,200)
                started=time.monotonic()
                with urllib.request.urlopen(self.base+'/v2/health',timeout=5) as r:json.load(r)
                health.append(time.monotonic()-started)
        finally:
            stopping.set();worker.join(timeout=10)
        latencies.sort();health.sort()
        self.assertLess(latencies[int(len(latencies)*.9)],.12,latencies)
        self.assertLess(health[int(len(health)*.9)],.12,health)
        for eid,space in [(slow_eid,slow_space),(fast_eid,fast_space)]:
            self.post(dict(request_id=str(time.time_ns()),tool='terminal.force',space_id=space,args={'endpoint_id':eid}),token)

    def test_observe_keeps_request_id_and_grant_checks(self):
        token,sid,created=self.recorded_terminal('observe-checks',"print('OBS_READY',flush=True)\nimport time\ntime.sleep(60)\n")
        eid=created['endpoint']['id']
        status,value=self.post(dict(request_id='',tool='terminal.observe',space_id=sid,args={'endpoint_id':eid,'cursor':0}),token)
        self.assertEqual(status,400,value)
        _,invite=self.post(dict(request_id='obs-invite',tool='members.invite',space_id=sid,args={'roles':['observer']}),token)
        _,member=self.post(dict(request_id='obs-reg',tool='members.register',space_id=sid,args={'name':'watcher'}),invite['invite_token'])
        self.post(dict(request_id='obs-grant',tool='endpoints.grant',space_id=sid,args={'endpoint_id':eid,'member_id':member['id'],'operations':['observe']}),token)
        status,_=self.post(dict(request_id='obs-1',tool='terminal.observe',space_id=sid,args={'endpoint_id':eid,'cursor':0}),member['credential'])
        self.assertEqual(status,200)
        self.post(dict(request_id='obs-revoke',tool='endpoints.grant',space_id=sid,args={'endpoint_id':eid,'member_id':member['id'],'operations':[]}),token)
        status,value=self.post(dict(request_id='obs-2',tool='terminal.observe',space_id=sid,args={'endpoint_id':eid,'cursor':0}),member['credential'])
        self.assertEqual(status,403,value)
        self.post(dict(request_id='obs-force',tool='terminal.force',space_id=sid,args={'endpoint_id':eid}),token)

    def test_revocation_ends_held_control_and_next_operation(self):
        token,sid,created=self.recorded_terminal('revoke',"print('REV_READY',flush=True)\nwhile True:\n    print('GOT=%r'%input(),flush=True)\n")
        eid=created['endpoint']['id']
        def call(tool,args,credential=token):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,invite=call('members.invite',{'roles':['manager']})
        _,member=call('members.register',{'name':'agent'},invite['invite_token'])
        mid,cred=member['id'],member['credential']
        def held_then_revoked(revoke,denied):
            status,lease=call('terminal.acquire',{'endpoint_id':eid},cred)
            self.assertEqual(status,200,lease)
            self.assertEqual(call('terminal.write',dict(endpoint_id=eid,input='x\r',**lease),cred)[0],200)
            status,value=revoke()
            self.assertEqual(status,200,value)
            status,value=call('terminal.write',dict(endpoint_id=eid,input='y\r',**lease),cred)
            self.assertEqual(status,denied,value)
            # The revoked holder's lease must not keep a human or another agent out.
            status,mine=call('terminal.acquire',{'endpoint_id':eid})
            self.assertEqual(status,200,mine)
            self.assertEqual(call('terminal.release',dict(endpoint_id=eid,**mine))[0],200)
        held_then_revoked(lambda:call('members.set_roles',{'member_id':mid,'roles':['implementer']}),403)
        call('endpoints.grant',{'endpoint_id':eid,'member_id':mid,'operations':['send','control']})
        held_then_revoked(lambda:call('endpoints.grant',{'endpoint_id':eid,'member_id':mid,'operations':[]}),403)
        call('endpoints.grant',{'endpoint_id':eid,'member_id':mid,'operations':['send','control']})
        held_then_revoked(lambda:call('members.revoke_credential',{'member_id':mid,'expected_epoch':member.get('credential_epoch',0)}),401)
        # Time-based expiry has no revoking call: the next acquire must fence it.
        _,invite=call('members.invite',{'roles':['manager']})
        _,member=call('members.register',{'name':'expiring'},invite['invite_token'])
        status,rotated=call('members.rotate_credential',{'member_id':member['id'],'expected_epoch':member.get('credential_epoch',0),'ttl_seconds':2})
        self.assertEqual(status,200,rotated)
        cred=rotated['credential']
        held_then_revoked(lambda:(time.sleep(2.5),(200,'expired'))[1],401)
        call('terminal.force',{'endpoint_id':eid})

    def test_resent_rotation_keeps_the_lease_taken_with_the_new_credential(self):
        # K14: replaying a rotation must not fence the lease its new credential took.
        token,sid,created=self.recorded_terminal('rotate-replay',"print('READY',flush=True)\nwhile True:\n    print('GOT=%r'%input(),flush=True)\n")
        eid=created['endpoint']['id']
        def call(tool,args,credential=token,rid=None):return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,invite=call('members.invite',{'roles':['manager']})
        _,member=call('members.register',{'name':'agent'},invite['invite_token'])
        rotate={'member_id':member['id'],'expected_epoch':member.get('credential_epoch',0)}
        status,old_lease=call('terminal.acquire',{'endpoint_id':eid},member['credential'])
        self.assertEqual(status,200,old_lease)
        status,rotated=call('members.rotate_credential',rotate,rid='k14-rotate')
        self.assertEqual(status,200,rotated)
        cred=rotated['credential']
        # The first rotation still ends the old credential's lease at once.
        status,mine=call('terminal.acquire',{'endpoint_id':eid})
        self.assertEqual(status,200,mine)
        self.assertEqual(call('terminal.release',dict(endpoint_id=eid,**mine))[0],200)
        status,lease=call('terminal.acquire',{'endpoint_id':eid},cred)
        self.assertEqual(status,200,lease)
        status,again=call('members.rotate_credential',rotate,rid='k14-rotate')
        self.assertEqual(status,200,again)
        self.assertEqual(again['credential'],cred)
        status,value=call('terminal.write',dict(endpoint_id=eid,input='still mine\r',**lease),cred)
        self.assertEqual(status,200,value)
        # A real rotation later, under a new request_id, still ends that lease.
        status,again=call('members.rotate_credential',{'member_id':member['id'],'expected_epoch':rotated['credential_epoch']},rid='k14-rotate-2')
        self.assertEqual(status,200,again)
        status,mine=call('terminal.acquire',{'endpoint_id':eid})
        self.assertEqual(status,200,mine)
        self.assertEqual(call('terminal.release',dict(endpoint_id=eid,**mine))[0],200)
        call('terminal.force',{'endpoint_id':eid})

    def test_terminal_request_id_is_bounded(self):
        # K2: every receipt keeps its request_id, so its size must be bounded.
        owner,sid,created=self.recorded_terminal('bounded-id','import time; time.sleep(5)')
        eid=created['endpoint']['id']
        for rid in ['x'*257,'nul\0id']:
            status,value=self.post(dict(request_id=rid,tool='terminal.force',space_id=sid,args={'endpoint_id':eid}),owner)
            self.assertEqual((status,value['error']['code']),(400,'Invalid'),value)
        status,value=self.post(dict(request_id='x'*256,tool='terminal.force',space_id=sid,args={'endpoint_id':eid}),owner)
        self.assertEqual(status,200,value)

    def test_terminal_write_delivers_whitespace_only_keys(self):
        script="print('KEYS_READY',flush=True)\nfor n in (1,2):\n    print('LINE%d=%r'%(n,input()),flush=True)\n"
        token,sid,created=self.recorded_terminal('keys',script)
        eid=created['endpoint']['id']
        def call(tool,args):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),token)
        def wait_for(text):
            for _ in range(100):
                status,view=call('terminal.observe',{'endpoint_id':eid,'cursor':0})
                if status==200 and text in view['text']:return view
                time.sleep(.05)
            self.fail(text+' absent: '+repr(view))
        wait_for('KEYS_READY')
        status,lease=call('terminal.acquire',{'endpoint_id':eid})
        self.assertEqual(status,200,lease)
        status,value=call('terminal.write',dict(endpoint_id=eid,input='',**lease))
        self.assertEqual(status,400,value)
        for key in ['\r',' ','\r']:
            status,value=call('terminal.write',dict(endpoint_id=eid,input=key,**lease))
            self.assertEqual(status,200,(repr(key),value))
        wait_for("LINE1=''")
        wait_for("LINE2=' '")
        call('terminal.release',dict(endpoint_id=eid,**lease))
        call('terminal.force',{'endpoint_id':eid})

    def test_history_restart_recovery_and_revocation(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def call(tool,args={},credential=token,rid=None):
            return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,space=self.post(dict(request_id='history-space',tool='spaces.create',args={'name':'history'}),token)
        sid=space['space']['id']
        status,created=call('terminal.create',dict(shell_once('echo HISTORY_71629'),name='recorded'))
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
        _,created=call('terminal.create',SHELL)
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
        _,created=call('terminal.create',SHELL)
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

    def test_agents_launch_only_human_approved_terminals(self):
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='launch-space',tool='spaces.create',args={'name':'launch'}),token)
        sid=space['space']['id']; human=space['administrator']['credential']
        def call(tool,args,credential,rid=None):return self.post(dict(request_id=rid or str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,invite=call('members.invite',{'roles':['session_maintainer','manager']},token)
        _,agent=call('members.register',{'name':'agent'},invite['invite_token'])
        launch=dict(shell_once('echo APPROVED_LAUNCH'),name='approved')
        # An agent, even a manager, cannot choose an executable, arguments or workspace.
        status,value=call('terminal.create',launch,agent['credential'])
        self.assertEqual(status,403,value)
        status,value=call('endpoints.register',{'name':'smuggled','kind':'terminal','config':{'command':launch['command'],'args':launch['args'],'cwd':None}},agent['credential'])
        self.assertEqual(status,403,value)
        # A human administrator approves it by creating it; the agent may then relaunch exactly that.
        status,created=call('terminal.create',launch,human)
        self.assertEqual(status,200,created)
        eid=created['endpoint']['id']; old=created['session']['id']
        self.assertEqual(created['endpoint']['launch_approval']['approved_by'],space['administrator']['id'])
        for _ in range(100):
            _,listing=call('endpoints.list',{},human)
            if next(e for e in listing['items'] if e['id']==eid)['status']=='exited':break
            time.sleep(.05)
        status,restarted=call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},agent['credential'])
        self.assertEqual(status,200,restarted)
        self.assertNotEqual(restarted['session']['id'],old)

    def test_restart_needs_an_approver_who_is_still_trusted(self):
        # K10: an approval is only as good as its approver today.
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        def setup(name):
            # Each space has exactly one human administrator, made with the space.
            _,space=self.post(dict(request_id=name+'-space',tool='spaces.create',args={'name':name}),token)
            sid=space['space']['id']; admin=space['administrator']
            def call(tool,args,credential):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
            _,invite=call('members.invite',{'roles':['session_maintainer','manager']},token)
            _,agent=call('members.register',{'name':'agent'},invite['invite_token'])
            def launch(credential):
                status,created=call('terminal.create',dict(shell_once('echo K10'),name=name),credential)
                self.assertEqual(status,200,created)
                eid=created['endpoint']['id']
                for _ in range(100):
                    _,listing=call('endpoints.list',{},token)
                    if next(e for e in listing['items'] if e['id']==eid)['status']=='exited':break
                    time.sleep(.05)
                return eid,created['session']['id']
            def restart(eid,old):return call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},agent['credential'])
            return call,admin,agent,launch,restart
        def denied(status,value):
            self.assertEqual((status,value['error']['code']),(403,'Denied'),value)
            self.assertIn('no longer trusted',value['error']['message'])
        # Approver's credential revoked.
        call,admin,agent,launch,restart=setup('k10-revoked')
        eid,old=launch(admin['credential'])
        status,value=call('members.revoke_credential',{'member_id':admin['id'],'expected_epoch':0},token)
        self.assertEqual(status,200,value)
        denied(*restart(eid,old))
        # A new credential restores the person, not what the revoked one approved
        # (a leaked credential's launches must not come back).
        status,value=call('members.rotate_credential',{'member_id':admin['id'],'expected_epoch':1},token)
        self.assertEqual(status,200,value)
        denied(*restart(eid,old))
        # What the person approves with the new credential counts.
        eid,old=launch(value['credential'])
        status,value=restart(eid,old)
        self.assertEqual(status,200,value)
        # The owner's approval stays valid in the same space.
        eid,old=launch(token)
        status,value=restart(eid,old)
        self.assertEqual(status,200,value)
        # Approver demoted to an agent role.
        call,admin,agent,launch,restart=setup('k10-demoted')
        eid,old=launch(admin['credential'])
        # Routine rotation is not a break of trust: the approval still counts.
        status,value=call('members.rotate_credential',{'member_id':admin['id'],'expected_epoch':0},token)
        self.assertEqual(status,200,value)
        admin['credential']=value['credential']
        status,value=restart(eid,old)
        self.assertEqual(status,200,value)
        old=value['session']['id']
        for _ in range(100):
            _,listing=call('endpoints.list',{},token)
            if next(e for e in listing['items'] if e['id']==eid)['status']=='exited':break
            time.sleep(.05)
        status,value=call('members.set_roles',{'member_id':admin['id'],'roles':['observer']},token)
        self.assertEqual(status,200,value)
        denied(*restart(eid,old))
        # The agent, a manager, cannot promote the human back to restore the approval.
        status,value=call('members.set_roles',{'member_id':admin['id'],'roles':['human_admin']},agent['credential'])
        self.assertEqual((status,value['error']['code']),(403,'Denied'),value)
        denied(*restart(eid,old))
        # Only an owner can restore the human; the old approval still does not count,
        # but what the restored human approves now does.
        status,value=call('members.set_roles',{'member_id':admin['id'],'roles':['human_admin']},token)
        self.assertEqual(status,200,value)
        denied(*restart(eid,old))
        eid,old=launch(admin['credential'])
        status,value=restart(eid,old)
        self.assertEqual(status,200,value)

    def test_a_current_human_can_approve_an_existing_terminal_again(self):
        # K15: re-approve in place, so the terminal and its history are kept.
        token=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,space=self.post(dict(request_id='k15-space',tool='spaces.create',args={'name':'k15'}),token)
        sid=space['space']['id']; admin=space['administrator']
        def call(tool,args,credential):return self.post(dict(request_id=str(time.time_ns()),tool=tool,space_id=sid,args=args),credential)
        _,invite=call('members.invite',{'roles':['session_maintainer','manager']},token)
        _,agent=call('members.register',{'name':'agent'},invite['invite_token'])
        status,created=call('terminal.create',dict(shell_once('echo K15'),name='k15'),admin['credential'])
        self.assertEqual(status,200,created)
        eid,old=created['endpoint']['id'],created['session']['id']
        for _ in range(100):
            _,listing=call('endpoints.list',{},token)
            if next(e for e in listing['items'] if e['id']==eid)['status']=='exited':break
            time.sleep(.05)
        config=created['endpoint']['config']
        call('members.revoke_credential',{'member_id':admin['id'],'expected_epoch':0},token)
        def restart():return call('terminal.restart',{'endpoint_id':eid,'expected_session_id':old},agent['credential'])
        status,value=restart()
        self.assertEqual(status,403,value)
        self.assertIn('endpoints.approve_launch',value['error']['message'])
        def approve(credential,expected):
            args={'endpoint_id':eid}
            if expected is not None:args['expected_config']=expected
            return call('endpoints.approve_launch',args,credential)
        # An agent, even a manager, cannot approve a launch.
        status,value=approve(agent['credential'],config)
        self.assertEqual((status,value['error']['code']),(403,'Denied'),value)
        self.assertEqual(restart()[0],403)
        # A human approves the program they read, never an ID alone.
        status,value=approve(token,None)
        self.assertEqual((status,value['error']['code']),(400,'Invalid'),value)
        status,value=approve(token,dict(config,args=config['args']+['--extra']))
        self.assertEqual((status,value['error']['code']),(409,'Conflict'),value)
        self.assertEqual(restart()[0],403)
        # The same person with a new credential is trusted at a new generation:
        # their old approval stays dead, a fresh one counts.
        status,rotated=call('members.rotate_credential',{'member_id':admin['id'],'expected_epoch':1},token)
        self.assertEqual(status,200,rotated)
        self.assertEqual(restart()[0],403)
        status,value=approve(rotated['credential'],config)
        self.assertEqual(status,200,value)
        self.assertEqual(value['config'],config)
        self.assertEqual(value['launch_approval']['approved_by'],admin['id'])
        self.assertEqual(value['launch_approval']['approver_generation'],1)
        self.assertEqual(value['launch_approval']['previous_approval']['approver_generation'],0)
        status,value=restart()
        self.assertEqual(status,200,value)
        self.assertEqual(value['previous_session_id'],old)
        # A terminal that names no program has no launch to approve.
        status,bare=call('endpoints.register',{'name':'bare','kind':'terminal','config':{}},token)
        self.assertEqual(status,200,bare)
        status,value=call('endpoints.approve_launch',{'endpoint_id':bare['id'],'expected_config':{}},token)
        self.assertEqual((status,value['error']['code']),(400,'Invalid'),value)

if __name__=='__main__':unittest.main()
