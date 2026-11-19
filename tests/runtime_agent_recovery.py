"""Real crash -> daemon HTTP/CLI recovery. Fixtures use Engine commits, never seeded IDs.

Requires an already-built linkstart binary and its lib-test binary. The latter only
supplies the controlled model/timing seam; recovery uses the production daemon.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import shutil
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from contextlib import closing, nullcontext


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bin', required=True)
    parser.add_argument('--unit-test-bin', required=True)
    parser.add_argument('--evidence', required=True)
    parser.add_argument('--browser-node')
    parser.add_argument('--browser-helper')
    parser.add_argument('--keep-state', action='store_true', help='Retain private fixture and SQLite backup snapshots for review')
    args = parser.parse_args()
    binary = str(Path(args.bin).resolve())
    evidence = Path(args.evidence).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    private_fixture = nullcontext(tempfile.mkdtemp(prefix='linkstart-agent-recovery-')) if args.keep_state else tempfile.TemporaryDirectory(prefix='linkstart-agent-recovery-')
    with private_fixture as temporary:
        root = Path(temporary) / 'state'
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        processes = []
        log = open(evidence / 'transport-daemon.log', 'w', encoding='utf-8')
        # Production CodexProcess cannot find any provider executable in this test.
        provider_free_path = Path(temporary) / 'empty-provider-path'
        provider_free_path.mkdir()
        safe_path = [str(provider_free_path)]
        if os.name == 'nt':
            # Windows storage initialization invokes the OS ACL/SID tools.
            system = Path(os.environ['SystemRoot']) / 'System32'
            safe_path.extend([str(system), str(system / 'WindowsPowerShell' / 'v1.0')])
        provider_free_search = os.pathsep.join(safe_path)
        assert shutil.which('codex', path=provider_free_search) is None
        daemon_env = dict(os.environ, PATH=provider_free_search)

        def post(request, token):
            req = urllib.request.Request(base + '/v2/tools', data=json.dumps(request).encode(),
                headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + token})
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                return error.code, json.load(error)

        def start():
            child = subprocess.Popen([binary, 'daemon', 'run', '--state-dir', str(root), '--port', str(port)],
                env=daemon_env, stdout=log, stderr=log)
            processes.append(child)
            for _ in range(100):
                if child.poll() is not None:
                    raise AssertionError('daemon exited: ' + str(child.returncode))
                try:
                    with urllib.request.urlopen(base + '/v1/health', timeout=.2):
                        return child
                except OSError:
                    time.sleep(.05)
            raise AssertionError('daemon startup timed out')

        def call(tool, data, token, rid=None, scope=None):
            return post({'request_id': rid or str(time.time_ns()), 'tool': tool,
                'space_id': scope or sid, 'args': data}, token)

        def ledger_snapshot(label):
            # Online backup API yields coherent copies even while the original
            # worker is paused between its Engine commit and agent publication.
            private = root / 'private-snapshots'
            private.mkdir(exist_ok=True)
            copies = {}
            for name, original in [('core', root / 'v2.sqlite3'), ('agent', root / 'agent-jobs' / 'agent-jobs.sqlite3')]:
                snapshot = private / f'{label}-{name}.sqlite3'
                with closing(sqlite3.connect(original.resolve().as_uri() + '?mode=ro', uri=True)) as source:
                    with closing(sqlite3.connect(snapshot)) as destination:
                        source.backup(destination)
                copies[name] = snapshot
            with closing(sqlite3.connect(copies['core'])) as db:
                core = json.loads(db.execute('SELECT json FROM core_state WHERE id=1').fetchone()[0])
            with closing(sqlite3.connect(copies['agent'])) as db:
                agent = json.loads(db.execute('SELECT record FROM agent_jobs WHERE id=?', (job['job_id'],)).fetchone()[0])
            prefix = 'agent:' + hashlib.sha256(job['job_id'].encode()).hexdigest() + ':'
            def fields(value, names):
                return {name: value.get(name) for name in names}
            receipts = [r for r in core['receipts'] if r['request_id'].startswith(prefix)]
            allowed = {
                'agent': fields(agent, ['request', 'identity_id', 'status', 'topic_ids', 'error', 'schema_version',
                    'generation', 'renderer_version', 'plan_hash', 'attempt_id', 'recovery_reason', 'publication_pending']),
                'receipts': [dict(fields(r, ['identity', 'request_id', 'operation_scope']),
                    topic_id=r['result']['id'], fingerprint_sha256=hashlib.sha256(r['fingerprint'].encode()).hexdigest()) for r in receipts],
                'topics': [fields(t, ['id', 'space_id', 'parent_topic_id', 'dependencies', 'kind', 'revision'])
                    for t in core['topics'] if t.get('parent_topic_id') == mission['id']],
                'events': [fields(e, ['id', 'topic_id', 'request_id', 'action', 'author', 'revision_before', 'revision_after'])
                    for e in core['events'] if e.get('request_id', '').startswith(prefix)],
            }
            allowed['agent']['prior_error_sha256'] = [hashlib.sha256(error.encode()).hexdigest() for error in agent.get('prior_errors', [])]
            (evidence / f'ledger-{label}.json').write_text(json.dumps(allowed, indent=2))
            return allowed

        try:
            daemon = start()
            owner = (root / 'v2-owner.token').read_text()
            status, space = post({'request_id': 'space', 'tool': 'spaces.create', 'args': {'name': 'Saved plan recovery'}}, owner)
            assert status == 200, space
            sid, admin = space['space']['id'], space['administrator']['credential']
            _, invite = call('members.invite', {'roles': ['task_decomposer']}, admin)
            _, member = call('members.register', {'name': 'Recovery author'}, invite['invite_token'])
            token = member['credential']
            _, other_invite = call('members.invite', {'roles': ['task_decomposer']}, admin)
            _, other = call('members.register', {'name': 'Other identity'}, other_invite['invite_token'])
            _, mission = call('topics.create', {'title': 'Recover saved decomposition', 'body': 'Three ordered tasks', 'kind': 'plan'}, admin)
            job = {'job_id': 'public-recovery', 'space_id': sid, 'mission_topic_id': mission['id']}
            intent = {'job_id': job['job_id'], 'mission_topic_id': mission['id']}
            daemon.terminate()
            daemon.wait(timeout=10)
            (root / 'process-input.json').write_text(json.dumps({'job': job, 'credential': token, 'agent_dir': 'agent-jobs'}))
            helper_env = dict(os.environ, LINKSTART_RECOVERY_TEST_DIR=str(root), LINKSTART_RECOVERY_TEST_MODE='original')
            original = subprocess.Popen([str(Path(args.unit_test_bin).resolve()), '--ignored', '--exact',
                'runtime_agent::tests::recovery_process_helper', '--nocapture'], env=helper_env, stdout=log, stderr=log)
            processes.append(original)
            until = time.monotonic() + 15
            while not (root / 'second-engine-commit').exists():
                assert original.poll() is None, 'fixture original exited'
                assert time.monotonic() < until, 'fixture did not reach second commit'
                time.sleep(.02)
            before_ledger = ledger_snapshot('before')
            assert len(before_ledger['agent']['topic_ids']) == 1
            assert len(before_ledger['receipts']) == len(before_ledger['events']) == 2
            original.kill()
            original.wait(timeout=10)
            assert original.returncode != 0
            daemon = start()
            status, before = call('agent.get', intent, token)
            assert status == 200 and before['status'] == 'unknown' and len(before['topic_ids']) == 2, before
            (evidence / 'transport-before.json').write_text(json.dumps(before, indent=2))
            for invalid in [dict(intent, plan={}), dict(intent, topic_ids=[]), dict(intent, identity='owner'),
                    dict(intent, endpoint='arbitrary'), dict(intent, command='arbitrary'), dict(intent, generation=9),
                    dict(intent, job_id=''), {'job_id': job['job_id']}]:
                assert call('agent.recover', invalid, token)[0] == 400
            assert post({'request_id': 'no-space', 'tool': 'agent.recover', 'args': intent}, token)[0] == 400
            assert call('agent.get', intent, token, scope='wrong-space')[0] == 403
            assert call('agent.get', dict(intent, mission_topic_id='wrong'), token)[0] == 409
            assert call('agent.recover', intent, other['credential'])[0] == 403
            assert call('agent.recover', intent, owner)[0] == 403
            assert call('agent.recover', intent, admin)[0] == 403
            call('members.set_roles', {'member_id': member['id'], 'roles': ['observer']}, admin)
            assert call('agent.recover', intent, token)[0] == 403
            assert call('agent.get', intent, token)[0] == 403
            call('members.set_roles', {'member_id': member['id'], 'roles': ['task_decomposer']}, admin)
            _, rotated = call('members.rotate_credential', {'member_id': member['id'], 'expected_epoch': 0}, admin)
            assert call('agent.recover', intent, token)[0] == 401
            token = rotated['credential']
            credential_file = root / 'recovery-member.token'
            credential_file.write_text(token)
            admin_file = root / 'recovery-admin.token'
            admin_file.write_text(admin)
            session = root / 'browser-session.json'
            session.write_text(json.dumps({'base': base, 'space_id': sid, 'mission_topic_id': mission['id'],
                'job_id': job['job_id'], 'credential_file': str(credential_file), 'admin_file': str(admin_file),
                'member_id': member['id'], 'evidence': str(evidence)}))
            if args.browser_node:
                assert args.browser_helper, '--browser-helper must name the seeded draw/browser.mjs'
                subprocess.run([args.browser_node, str(Path(__file__).with_name('runtime_agent_recovery_browser.mjs')),
                    str(session), str(Path(args.browser_helper).resolve())], check=True, timeout=90)
            else:
                assert call('agent.recover', intent, token, rid='http-explicit-recovery')[0] == 200
            for _ in range(100):
                status, after = call('agent.get', intent, token)
                if status == 200 and after['status'] == 'completed':
                    break
                time.sleep(.03)
            assert after['status'] == 'completed' and len(after['topic_ids']) == 3, after
            assert after['error'] is None, 'completed public result must not carry a historical current error'
            assert 'prior_errors' not in after, 'private history must not be exposed by public snapshot'
            attempt = after['recovery']['attempt_id']
            assert call('agent.recover', intent, token, rid=attempt)[1]['topic_ids'] == after['topic_ids']
            for tool in ['agent.recover', 'agent.get', 'topics.list']:
                request = {'request_id': attempt if tool == 'agent.recover' else 'cli-' + tool, 'tool': tool,
                    'space_id': sid, 'args': {} if tool == 'topics.list' else intent}
                command = [binary, 'tool', '--state-dir', str(root), '--credential-file', str(credential_file)]
                assert token not in ' '.join(command)
                result = subprocess.run(command, input=json.dumps(request), text=True, capture_output=True, timeout=15)
                assert result.returncode == 0, result.stderr
                value = json.loads(result.stdout)
                if tool != 'topics.list':
                    assert value['topic_ids'] == after['topic_ids']
                else:
                    assert set(after['topic_ids']).issubset({t['id'] for t in value['items']})
            with closing(sqlite3.connect(root / 'v2.sqlite3')) as db:
                durable = json.loads(db.execute('SELECT json FROM core_state WHERE id=1').fetchone()[0])
            prefix = 'agent:' + hashlib.sha256(job['job_id'].encode()).hexdigest() + ':'
            receipts = [r for r in durable['receipts'] if r['request_id'].startswith(prefix)]
            assert len(receipts) == 3
            assert {r['result']['id'] for r in receipts} == set(after['topic_ids'])
            assert (root / 'model-called').read_text() == '1'
            assert not list((root / 'agent-jobs' / 'agent-work').glob('**/last-message.json'))
            (evidence / 'transport-after.json').write_text(json.dumps(after, indent=2))
            after_ledger = ledger_snapshot('after')
            assert len(after_ledger['agent']['topic_ids']) == len(after_ledger['receipts']) == len(after_ledger['events']) == 3
            assert after_ledger['agent']['status'] == 'completed'
            assert after_ledger['agent']['error'] is None
            assert hashlib.sha256(before['error'].encode()).hexdigest() in after_ledger['agent']['prior_error_sha256']
            (evidence / 'transport-proof.json').write_text(json.dumps({'status': 'PASS', 'original_exit': original.returncode,
                'engine_receipts': len(receipts), 'unique_children': len(set(after['topic_ids'])),
                'model_generations': 1, 'recovery_model_generations': 0, 'http_cli_ids_match': True,
                'browser_executed': bool(args.browser_node), 'private_state_retained': args.keep_state,
                'private_state_path': str(root) if args.keep_state else None}, indent=2))
            print('agent recovery HTTP/CLI/crash PASS; browser=' + str(bool(args.browser_node)))
        finally:
            for child in reversed(processes):
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=10)
            (evidence / 'process-exits.json').write_text(json.dumps([
                {'pid': child.pid, 'returncode': child.returncode} for child in processes], indent=2))
            log.close()


if __name__ == '__main__':
    main()
