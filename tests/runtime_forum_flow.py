"""Real daemon task handoff through HTTP and the public CLI tool client."""
import concurrent.futures,json,pathlib,sqlite3,subprocess,time,unittest
from contextlib import closing
from runtime_v2_http import RuntimeHttp,BIN

class ForumFlow(RuntimeHttp):
    def test_separate_roles_claim_review_test_and_read_back(self):
        owner=(pathlib.Path(self.state_dir)/'v2-owner.token').read_text()
        _,created=self.post({'request_id':'space','tool':'spaces.create','args':{'name':'role handoff'}},owner)
        sid=created['space']['id'];admin=created['administrator']['credential']
        def call(who,tool,args,rid=None):
            return self.post({'request_id':rid or str(time.time_ns()),'tool':tool,'space_id':sid,'args':args},who)
        def member(role,name):
            status,invite=call(admin,'members.invite',{'roles':[role]});self.assertEqual(status,200,invite)
            status,value=call(invite['invite_token'],'members.register',{'name':name});self.assertEqual(status,200,value)
            return value
        a=member('implementer','worker A');b=member('implementer','worker B')
        reviewer=member('reviewer','independent reviewer');tester=member('tester','tester')
        _,topic=call(admin,'topics.create',{'title':'Deliver evidence','body':'Claim, submit, review, then test.','kind':'task'})
        def claim(m):return m,call(m['credential'],'task.act',{'topic_id':topic['id'],'action':'claim','comment':'Taking responsibility','expected_revision':0})
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:results=list(pool.map(claim,[a,b]))
        self.assertEqual(sorted(r[1][0] for r in results),[200,409])
        winner=next(m for m,r in results if r[0]==200)
        status,rejected=call(winner['credential'],'task.act',{'topic_id':topic['id'],'action':'approve_review','comment':'must not self elevate','expected_revision':1,'expected_submission':'none'})
        self.assertEqual(status,403,rejected)
        _,submitted=call(winner['credential'],'task.act',{'topic_id':topic['id'],'action':'submit_review','comment':'Deliver exact artifact','expected_revision':1,'artifact_ref':'git:handoff-proof'})
        submission=submitted['topic']['current_submission']['id']
        status,reviewed=call(reviewer['credential'],'task.act',{'topic_id':topic['id'],'action':'approve_review','comment':'Checked this submission','expected_revision':2,'expected_submission':submission})
        self.assertEqual(status,200,reviewed);self.assertFalse(reviewed['event']['self_review'])
        status,done=call(tester['credential'],'task.act',{'topic_id':topic['id'],'action':'pass_test','comment':'Verified this submission','expected_revision':3,'expected_submission':submission})
        self.assertEqual(status,200,done);self.assertEqual(done['topic']['state'],'done')
        tokenfile=pathlib.Path(self.state_dir)/'tester.token';tokenfile.write_text(tester['credential'])
        request={'request_id':'cli-readback','tool':'topics.read','space_id':sid,'args':{'topic_id':topic['id']}}
        output=subprocess.run([str(BIN),'tool','--state-dir',self.state_dir,'--credential-file',str(tokenfile)],input=json.dumps(request),text=True,capture_output=True,timeout=10)
        self.assertEqual(output.returncode,0,output.stderr)
        view=json.loads(output.stdout);self.assertEqual(view['topic']['state'],'done')
        self.assertEqual([c['action'] for c in view['comments']],['claim','submit_review','approve_review','pass_test'])
        self.assertEqual(len({c['author'] for c in view['comments']}),3)
        with closing(sqlite3.connect(pathlib.Path(self.state_dir)/'v2.sqlite3')) as db:
            durable=json.loads(db.execute('SELECT json FROM core_state WHERE id=1').fetchone()[0])
        stored=next(t for t in durable['topics'] if t['id']==topic['id'])
        self.assertEqual(stored['state'],'done');self.assertEqual(stored['assignee'],winner['id'])

if __name__=='__main__':unittest.main()
