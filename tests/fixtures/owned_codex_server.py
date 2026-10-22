"""Subprocess protocol boundary only. Never connects to any provider."""
import json, sys
mode=sys.argv[1]
thread='owned-thread'
history=[]
if mode=='startup-descendant':
 import os,subprocess,pathlib,ctypes
 from ctypes import wintypes
 child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(600)'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 kernel=ctypes.WinDLL('kernel32',use_last_error=True)
 kernel.GetProcessTimes.argtypes=[wintypes.HANDLE]+[ctypes.POINTER(wintypes.FILETIME)]*4
 kernel.GetProcessTimes.restype=wintypes.BOOL
 times=[wintypes.FILETIME() for _ in range(4)]
 assert kernel.GetProcessTimes(wintypes.HANDLE(int(child._handle)),*[ctypes.byref(t) for t in times])
 created=(times[0].dwHighDateTime<<32)|times[0].dwLowDateTime
 pathlib.Path('startup-child.json').write_text(json.dumps({'pid':child.pid,'creation_time':created,'root_pid':os.getpid()}),encoding='utf-8')
def send(v):
 print(json.dumps(v),flush=True)
for line in sys.stdin:
 m=json.loads(line)
 if 'method' not in m: continue
 method=m['method']; rid=m.get('id'); p=m.get('params',{})
 if method=='initialize': send({'id':rid,'result':{'userAgent':'fake/9.0' if mode=='bad-version' else 'Codex Desktop/0.163.0 fake','platformFamily':'windows','platformOs':'windows'}})
 elif method in ('thread/start','thread/resume'):
  if mode=='workspace':
   assert p['sandbox']=='workspace-write' and p['approvalPolicy']=='never' and p['config']['features.shell_tool'] is True and p['config']['sandbox_workspace_write.network_access'] is False
  sandbox={'type':'workspaceWrite' if p.get('sandbox')=='workspace-write' else 'readOnly','networkAccess':False,'writableRoots':[]}
  if mode=='bad-sandbox':sandbox={'type':'dangerFullAccess'}
  send({'id':rid,'result':{'thread':{'id':thread},'model':'gpt-6-luna','cwd':p.get('cwd'),'approvalPolicy':'never','sandbox':sandbox}})
 elif method=='turn/start':
  if mode=='eof': sys.exit(0)
  if mode=='history':
   turn_id='turn-'+str(len(history)+1)
   history.append({'id':turn_id,'status':'completed' if not history else 'inProgress','items':[{'type':'userMessage','content':p['input']}]})
   send({'method':'turn/started','params':{'threadId':thread,'turn':{'id':turn_id}}})
   send({'id':rid,'result':{'turn':{'id':turn_id}}})
   if len(history)==1:send({'method':'turn/completed','params':{'threadId':thread,'turn':{'id':turn_id,'status':'completed'}}})
   continue
  send({'method':'turn/started','params':{'threadId':thread,'turn':{'id':'turn-1'}}})
  if mode in ('forum','forum-double'):
   request=json.loads(p['input'][0]['text'].split('\n',1)[1])
   send({'id':rid,'method':'item/tool/call','params':{'tool':'linkstart_call','callId':'real-facade','threadId':thread,'turnId':'turn-1','arguments':request}})
   if mode=='forum-double':
    request=dict(request,request_id=request['request_id']+'-late')
    send({'id':rid+100,'method':'item/tool/call','params':{'tool':'linkstart_call','callId':'late-facade','threadId':thread,'turnId':'turn-1','arguments':request}})
   send({'id':rid,'result':{'turn':{'id':'turn-1'}}})
   send({'method':'turn/completed','params':{'threadId':thread,'turn':{'id':'turn-1','status':'completed'}}})
   continue
  if mode=='lost': continue
  for wrong in ('thread','turn','none'):
   send({'id':rid,'method':'item/tool/call','params':{'tool':'linkstart_call','callId':wrong,'threadId':'foreign' if wrong=='thread' else thread,'turnId':'foreign' if wrong=='turn' else 'turn-1','arguments':{'request_id':'call','tool':'identity.current','space_id':'space','args':{}}}})
  send({'id':rid,'result':{'turn':{'id':'turn-1'}}})
 elif method=='thread/read':
  send({'id':rid,'result':{'thread':{'id':thread,'turns':history if mode=='history' else [{'id':'turn-1','status':'completed','items':[{'type':'userMessage','content':[{'type':'text','text':'[linkstart-operation:lost-op]\nwork'}]}]}]}}})
  if mode=='history':send({'id':rid,'method':'item/tool/call','params':{'tool':'linkstart_call','callId':'active-B','threadId':thread,'turnId':'turn-2','arguments':{}}})
 elif method=='turn/interrupt':send({'id':rid,'result':{'interruptedTurn':p['turnId']}})
