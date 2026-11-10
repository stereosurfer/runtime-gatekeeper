#!/usr/bin/env python3
"""Isolated MCP contract check. Requires jsonschema; never reads live config."""
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from jsonschema import Draft202012Validator

binary = str(Path(sys.argv[1]).resolve())
def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]
with tempfile.TemporaryDirectory(prefix='gatekeeper-schema-') as root:
    root = Path(root)
    config = root / 'runtime.yaml'
    config.write_text(f'''port: {port()}
state_dir: state
safety_margin_bytes: 0
services:
  demo:
    mode: managed
    command: ['@self', fixture, '{port()}']
    memory_bytes: 1048576
    disposable: true
    allowed_actions: [start, stop, restart]
''')
    daemon = subprocess.Popen([binary, 'serve', str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    bridge = None
    try:
        for _ in range(200):
            if (root/'state/token').exists(): break
            time.sleep(.05)
        bridge = subprocess.Popen([binary, 'mcp', str(config)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        def rpc(method, params):
            bridge.stdin.write(json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params})+'\n')
            bridge.stdin.flush()
            result = json.loads(bridge.stdout.readline())
            assert 'error' not in result, result
            return result['result']
        rpc('initialize', {'protocolVersion':'2025-06-18','clientInfo':{'name':'schema-check','version':'1'}})
        schemas = {t['name']: t['outputSchema'] for t in rpc('tools/list', {})['tools']}
        for schema in schemas.values(): Draft202012Validator.check_schema(schema)
        seen = set()
        def call(name, args):
            r = rpc('tools/call', {'name': name, 'arguments': args})
            assert not r['isError'], r
            Draft202012Validator(schemas[name]).validate(r['structuredContent'])
            seen.add(name)
            return r['structuredContent']
        call('services.start', {'service_id':'demo'})
        call('services.restart', {'service_id':'demo'})
        call('services.stop', {'service_id':'demo'})
        blocked = call('runtime.request', {'job_id':'large','agent':'schema','requires':['demo'],'memory_bytes':2**63})
        assert blocked['status']=='BLOCKED_RESOURCE'
        lease = call('runtime.request', {'job_id':'small','agent':'schema','requires':['demo']})
        assert lease['status']=='READY'
        call('runtime.observe', {'service_id':'demo'})
        call('runtime.status', {})
        call('services.list', {})
        call('services.get', {'service_id':'demo'})
        call('jobs.list', {})
        call('jobs.get', {'job_id':'small'})
        call('runtime.release', {'runtime_id':lease['runtime_id']})
        call('runtime.cleanup', {})
        assert seen == set(schemas), seen
        print('PASS: 12 tool schemas; actual success outputs, READY and BLOCKED_RESOURCE validated in isolated daemon')
    finally:
        # Best-effort cleanup of only this harness's managed child, even on assertion failure.
        if bridge is not None:
            try:
                for j in rpc('tools/call', {'name':'jobs.list','arguments':{}})['structuredContent']['jobs']:
                    rpc('tools/call', {'name':'runtime.release','arguments':{'runtime_id':j['runtime_id']}})
                rpc('tools/call', {'name':'runtime.cleanup','arguments':{}})
            finally:
                bridge.terminate(); bridge.wait(timeout=5)
        daemon.terminate(); daemon.wait(timeout=5)
