"""Isolated macOS P1 acceptance: real HTTP listeners and the real read-only MCP bridge.

python3 tests/native_queue_e2e.py PATH_TO_BUILT_BINARY RECEIPT_JSON
"""
import ctypes
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def save(path, value):
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(value))
    tmp.replace(path)


def wait_for(fn, description, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = fn()
        if result:
            return result
        time.sleep(.02)
    raise AssertionError("timeout: " + description)


def executable(pid):
    buf = ctypes.create_string_buffer(4096)
    n = ctypes.CDLL("/usr/lib/libproc.dylib").proc_pidpath(pid, buf, len(buf))
    assert n > 0
    return os.fsdecode(buf.value)


def fixture(root, port):
    finished = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            with (root/"requests").open("a") as log:
                log.write("GET " + self.path + "\n")
            state = json.loads((root/"reply.json").read_text())
            mode = state.get("mode")
            status = 200
            if mode == "handoff":
                self.server.shutdown()
                wait_for(lambda: (root/"deliver").exists(), "release original response", 4)
            if mode == "pause":
                (root/"entered").touch()
                wait_for(lambda: (root/"deliver").exists(), "release pending read", 4)
            if mode == "timeout":
                time.sleep(1.0)
            if mode == "redirect":
                status = 302
            body = json.dumps(state.get("body", {})).encode()
            if mode == "oversize":
                body = b"x" * (256*1024+1)
            if mode == "bad_json":
                body = b"{"
            self.send_response(status)
            if mode == "redirect":
                self.send_header("Location", "/unexpected-target")
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass
            if mode == "handoff":
                finished.set()

    with ThreadingHTTPServer(("127.0.0.1", port), Handler) as server:
        save(root/"started.json", {"port":server.server_port, "pid":os.getpid()})
        server.serve_forever(poll_interval=.005)
    (root/"closed").touch()
    finished.wait(timeout=5)


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run(binary, output):
    data = {}
    with tempfile.TemporaryDirectory(prefix="gatekeeper-p1-") as tmp, ExitStack() as scope:
        root = Path(tmp)
        def start(name, marker, body, port=0):
            directory = root/name
            directory.mkdir()
            save(directory/"reply.json", {"body":body})
            proc = subprocess.Popen([sys.executable, "-I", "-B", str(Path(__file__).resolve()),
                                     "--fixture", str(directory), str(port), marker])
            scope.callback(stop, proc)
            wait_for(lambda:(directory/"started.json").exists(), "listen "+name)
            info = json.loads((directory/"started.json").read_text())
            return directory, info
        count_body = {"running":3, "pending":1, "native_id":"PRIVATE-NATIVE-ID","owner":"FAKE-OWNER"}
        a, ai = start("alpha", "fixture-A", count_body)
        b, bi = start("bravo", "fixture-B", {"queue_running":[{"prompt":"PRIVATE-PROMPT"}],
                                            "queue_pending":[1,2]})
        c, ci = start("silent", "fixture-C", {})
        d, di = start("foreign", "fixture-D", {})
        def service(info, marker, source=None):
            row = dict(mode="discovered", port=info["port"], match_executable=executable(info["pid"]),
                       match_args=[marker])
            if source is not None:
                row["queue_source"]=source
            return row
        registry = {
            "queue-alpha":service(ai,"fixture-A",{"path":"/v2/metrics/queue","format":"counts"}),
            "other-engine":service(bi,"fixture-B",{"path":"/fixture/snapshot.json","format":"comfy_queue"}),
            "no-source":service(ci,"fixture-C"),
            "wrong-owner":service(di,"not-the-fixture",{"path":"/queue","format":"counts"}),
        }
        with socket.socket() as sock:
            sock.bind(("127.0.0.1",0))
            port=sock.getsockname()[1]
        assert port!=47831
        state=root/"state"
        state.mkdir()
        # Seed only this temporary ledger: one lease is a stronger attribution trap than no lease.
        job={"runtime_id":"rt-fixture","status":"INTERRUPTED","result":{},
             "request":{"job_id":"order-fixture","agent":"test","requires":["queue-alpha"]}}
        save(state/"state.json", {"jobs":{"order-fixture":job}})
        cfg=root/"runtime.yaml"
        conf={"port":port,"state_dir":str(state),"safety_margin_bytes":0,"services":registry}
        save(cfg,conf)
        subprocess.run([binary,"check",cfg],check=True,stdout=subprocess.DEVNULL)
        daemon=subprocess.Popen([binary,"serve",cfg],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        scope.callback(stop,daemon)
        wait_for(lambda:(state/"token").exists() and (state/"token").stat().st_size, "daemon ready")

        def rpc(name,args):
            request={"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}
            res=subprocess.run([binary,"mcp-observe",cfg],input=json.dumps(request)+"\n", text=True,
                               capture_output=True,check=True,timeout=6)
            packet=json.loads(res.stdout)
            assert not packet.get("error") and not packet["result"]["isError"], packet
            return packet["result"]["structuredContent"]
        def observe(name):
            return rpc("runtime.observe",{"service_id":name})["services"][0]
        def queue(name):
            return observe(name)["native_queue"]
        def expected(q,value,reason=None):
            assert q["value"]==value and q["work_order_attribution"]=="unknown" and q["scope"]=="service_instance",q
            assert q["availability"]==("unavailable" if value is None else "available"),q
            if value is not None:
                assert q["attribution_reason"]=="counts_only"
            if reason:
                assert q.get("reason")==reason,q
            assert not any(s in json.dumps(q) for s in ["PRIVATE-", "FAKE-OWNER", "order-fixture", "rt-fixture"])
        def record(case,q):
            data[case]=q
            print("PASS",case,flush=True)
        baseline={n:(state/n).read_bytes() for n in ("state.json","events.jsonl")}
        listing=rpc("runtime.observe",{})
        assert all(s["native_queue"]["value"] is None for s in listing["services"])
        assert not any((p/"requests").exists() for p in [a,b,c,d])
        view=observe("queue-alpha")
        assert [j["job_id"] for j in view["leases"]]==["order-fixture"]
        qa=view["native_queue"]
        expected(qa,{"running":3,"pending":1})
        assert qa["instance"]["pid"]==ai["pid"] and qa["last_success_at"]==qa["timestamp"]
        record("counts_with_single_registered_lease",qa)
        qb=queue("other-engine")
        expected(qb,{"running":1,"pending":2})
        assert qb["instance"]["pid"]==bi["pid"]
        record("replacement_format_and_path",qb)
        without_queue=observe("no-source")
        assert without_queue["service_instance"]["pid"] == ci["pid"]
        no_source=without_queue["native_queue"]
        expected(no_source,None,"no_configured_source")
        record("no_source",no_source)
        foreign=queue("wrong-owner")
        expected(foreign,None,"unverified_service")
        record("foreign_listener",foreign)
        assert not (c/"requests").exists() and not (d/"requests").exists()

        save(a/"reply.json",{"body":{"running":0,"pending":0}})
        empty=queue("queue-alpha")
        expected(empty,{"running":0,"pending":0})
        record("measured_zero",empty)
        for mode,body,reason in (
            ("shape",{"running":0},"invalid_queue_shape"),("bad_json",{},"invalid_json"),
            ("redirect",{},"unexpected_http_status"),("oversize",{},"response_too_large"),
            ("timeout",{},"transport_error")):
            save(a/"reply.json",{"mode":mode,"body":body})
            result=queue("queue-alpha")
            expected(result,None,reason)
            assert result["last_success_at"]==empty["timestamp"]
            record(mode,result)
        save(a/"reply.json",{"mode":"pause","body":count_body})
        with ThreadPoolExecutor(max_workers=1) as pool:
            pending=pool.submit(queue,"queue-alpha")
            wait_for(lambda:(a/"entered").exists(),"pending native read")
            blocked=queue("other-engine")
            expected(blocked,None,"sampler_busy")
            assert rpc("jobs.list",{})["jobs"][0]["request"]["job_id"]=="order-fixture"
            (a/"deliver").touch()
            expected(pending.result(),{"running":3,"pending":1})
        record("concurrent_read",blocked)
        (a/"deliver").unlink()

        # The old process keeps its accepted socket alive while a NEW PID binds the same port.
        save(a/"reply.json",{"mode":"handoff","body":{"running":33,"pending":7}})
        with ThreadPoolExecutor(max_workers=1) as pool:
            delayed=pool.submit(queue,"queue-alpha")
            wait_for(lambda:(a/"closed").exists(),"old listener relinquished port")
            new,ni=start("alpha-new","fixture-A",{"pending":2},ai["port"])
            assert ni["port"]==ai["port"] and ni["pid"]!=ai["pid"]
            (a/"deliver").touch()
            inflight=delayed.result()
        expected(inflight,None,"listener_instance_changed")
        assert inflight["instance"] is None and inflight["last_success_at"] is None
        record("discard_inflight_old_body",inflight)
        results=[]
        def new_failure():
            q=queue("queue-alpha")
            assert q["value"] is None and q["last_success_at"] is None,q
            results.append(q["reason"])
            return q if q["reason"]=="invalid_queue_shape" else None
        fresh_fail=wait_for(new_failure,"new instance in host snapshot")
        assert fresh_fail["instance"]["pid"]==ni["pid"]
        record("restart_without_success_history",fresh_fail)
        save(new/"reply.json",{"body":{"running":0,"pending":2}})
        fresh=queue("queue-alpha")
        expected(fresh,{"running":0,"pending":2})
        assert fresh["instance"]!=qa["instance"] and fresh["registered_source"]==qa["registered_source"]
        record("same_port_new_instance",fresh)
        for path,allowed in ((a,"GET /v2/metrics/queue"),(new,"GET /v2/metrics/queue"),
                             (b,"GET /fixture/snapshot.json")):
            assert set((path/"requests").read_text().splitlines())=={allowed}
        assert baseline=={n:(state/n).read_bytes() for n in baseline}
        data["checks"]={"state_and_events_unchanged":True,"unconfigured_and_wrong_owner_http_reads":0,
                        "redirect_followed":False,"handoff_observations":results}
    output.parent.mkdir(parents=True,exist_ok=True)
    output.write_text(json.dumps(data,indent=2)+"\n")


if __name__=="__main__":
    if sys.argv[1]=="--fixture":
        fixture(Path(sys.argv[2]),int(sys.argv[3]))
    else:
        run(Path(sys.argv[1]).resolve(),Path(sys.argv[2]).resolve())
