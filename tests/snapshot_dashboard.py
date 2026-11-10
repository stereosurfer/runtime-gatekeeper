"""Run the real stdio bridge, HTTP handler and headless Chrome against identical frozen samples.
Usage: python3 tests/snapshot_dashboard.py BINARY RUST_UNIT_TEST_BINARY CHROME OUTPUT_DIR
No packages, production config, credentials or AI service required.
"""
from concurrent.futures import ThreadPoolExecutor
import json
import re
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from html.parser import HTMLParser
from pathlib import Path

BINARY, UNIT_TEST, CHROME, OUTPUT = map(lambda arg: Path(arg).resolve(), sys.argv[1:5])
GIB = 2**30
TOKEN = "fixture-observer"
CASES = ("normal", "zero", "empty", "missing", "partial", "stale", "busy")


def process(pid, owner, category, ram, cpu, basis="phys_footprint"):
    return dict(pid=pid, start_time=1000+pid, parent=1, name="fixture-"+str(pid),
                service=owner, **{"class":category}, ram_bytes=ram, cpu_percent=cpu, memory_basis=basis)


def fixture(case):
    managed = [process(101, "worker", "managed", 2*GIB, 8.0)]
    discovered = [process(201, "external", "discovered", GIB, 1.5)]
    other = [process(301, None, "unknown", GIB//2, 0.0)]
    mem = dict(used=6*GIB,total=16*GIB,available=10*GIB,swap_used=GIB,swap_total=4*GIB,
               pressure="normal",safety_margin=0,process_basis="test fixture")
    cpu = 25.0
    if case == "zero":
        cpu = 0.0
        mem.update(used=0,available=16*GIB,swap_used=0,swap_total=0)
        for p in managed+discovered+other:
            p.update(ram_bytes=0,cpu_percent=0.0)
    if case == "missing":
        cpu = None
        for k in ["used","total","available","swap_used","swap_total","pressure"]:
            mem[k]=None
        for p in managed+discovered+other:
            p.update(ram_bytes=0,cpu_percent=None,memory_basis="rss_fallback")
    if case == "partial":
        managed[0].update(ram_bytes=1)
        managed.append(process(102,"worker","managed",0,None,"rss_fallback"))
        discovered.clear()
        other[0].update(ram_bytes=0)
    def service(name, category, ps):
        return dict(id=name, **{"class":category},state="running" if ps else "stopped", processes=ps,
                    leases=[],actions=[], identity_verified=bool(ps),memory_estimate_bytes=None)
    services=[service("worker","managed",managed),service("external","discovered",discovered)]
    if case == "empty":
        services,other=[],[]
    mem["host_available"] = mem["available"]
    mem["available"] = 123456  # Independent admission input, including when the host probe is missing.
    return dict(sample_id="p0-"+case,sampled_at=1794280000,age_ms=12000 if case=="stale" else 0,
                busy="services.start" if case=="busy" else None,
                raw=dict(cpu_percent=cpu,memory=mem,unknown=other,services=services,jobs=[],storage_failed=False))


class Element:
    def __init__(self, tag, attrs):
        self.tag, self.attrs, self.children = tag, dict(attrs), []

    def text(self):
        return "".join(x if isinstance(x,str) else x.text() for x in self.children)


class DOM(HTMLParser):
    def __init__(self, html):
        super().__init__()
        self.nodes=[]
        self.stack=[]
        self.feed(html)

    def handle_starttag(self,tag,attrs):
        n=Element(tag,attrs)
        if self.stack:
            self.stack[-1].children.append(n)
        self.nodes.append(n)
        if tag not in {"meta","link","input","br","img","hr","source","base"}:
            self.stack.append(n)

    def handle_endtag(self,tag):
        for i in reversed(range(len(self.stack))):
            if self.stack[i].tag==tag:
                del self.stack[i:]
                return

    def handle_data(self,data):
        if self.stack:
            self.stack[-1].children.append(data)

    def by_id(self,key):
        return next(n for n in self.nodes if n.attrs.get("id")==key)


def bridge(cfg, names):
    requests = [dict(jsonrpc="2.0",id=i,method="tools/call",params=dict(name=n,arguments={}))
                for i,n in enumerate(names,1)]
    run=subprocess.run([BINARY,"mcp-observe",cfg],input="".join(json.dumps(x)+"\n" for x in requests),
                       text=True,capture_output=True,timeout=20,check=True)
    responses=[json.loads(line) for line in run.stdout.splitlines()]
    assert [r["id"] for r in responses]==list(range(1,len(names)+1))
    return [r["result"]["structuredContent"] for r in responses]


def browser(url, root, case):
    cmd=[CHROME,"--headless","--disable-gpu","--no-first-run","--no-default-browser-check",
         "--disable-background-networking","--disable-sync","--disable-extensions",
         "--user-data-dir="+str(root/"chrome"),"--window-size=1180,1050","--virtual-time-budget=1500",
         "--dump-dom"]
    if case=="partial":
        cmd.append("--screenshot="+str(OUTPUT/"partial-dashboard.png"))
    run=subprocess.run(cmd+[url],text=True,capture_output=True,timeout=35,check=True)
    return DOM(run.stdout)


def expected_groups(raw):
    buckets=[[p for s in raw["services"] for p in s["processes"]],raw["unknown"]]
    result={}
    for name,ps in zip(("attributed","unattributed","all"),[*buckets,buckets[0]+buckets[1]]):
        known=[p["ram_bytes"] for p in ps if p["memory_basis"]=="phys_footprint" or p["ram_bytes"]>0]
        result[name]=(len(ps),len(ps)-len(known),sum(known) if known or not ps else None)
    return result


def display(value,unit,case):
    if value is None:
        return "未取得"
    if unit=="percent":
        shown=f"{value:.1f}%"
    else:
        shown=f"{value} B"
        for size,suffix in [(GIB,"GiB"),(2**20,"MiB"),(1024,"KiB")]:
            if abs(value)>=size:
                shown=f"{value/size:.2f} {suffix}"
                break
    return shown+{"stale":"（過期）","busy":"（控制忙碌，上次取樣）"}.get(case,"")


def verify(case, fx, status, observe, http, dom):
    raw=fx["raw"]; sid=fx["sample_id"]
    assert {s["snapshot"]["id"] for s in (status,observe,http)}=={sid}
    assert all(s["snapshot"]["sampled_at"]==fx["sampled_at"] for s in (status,observe,http))
    assert status["services"]==http["services"] and status["unknown"]==http["unknown"]
    assert status["resources"]==http["resources"]
    assert observe["system"]==status["resources"]["system"]
    assert observe["process_accounting"]==status["resources"]["process_accounting"]
    assert sid in dom.by_id("updated").text()
    assert "未連線" not in dom.by_id("connection").text()
    assert "disabled" in dom.by_id("cleanup").attrs
    assert http["read_only"] is True and status["memory"]["available"] == 123456
    def measure(m,value,unit):
        state="unavailable" if value is None else "stale" if case in {"stale","busy"} else "available"
        assert (m["value"],m["availability"],m["unit"],m["timestamp"])==(value,state,unit,fx["sampled_at"])
        assert m["source"]
    readings={}
    for key in ["cpu_percent","total","used","available","swap_used","swap_total"]:
        n=raw["cpu_percent"] if key=="cpu_percent" else raw["memory"]["host_available" if key == "available" else key]
        unit="percent" if key=="cpu_percent" else "bytes"
        measure(observe["system"][key],n,unit)
        assert (status["cpu_percent"] if key=="cpu_percent" else status["memory"]["host_available" if key == "available" else key])==n
        readings[key]=display(n,unit,case)
    cards={n.attrs["data-metric"]:next(c.text() for c in n.children if isinstance(c,Element) and c.tag=="strong")
           for n in dom.nodes if "data-metric" in n.attrs}
    assert cards["ram"]==readings["used"]+" / "+readings["total"]
    assert cards["swap"]==readings["swap_used"]+" / "+readings["swap_total"]
    assert cards["cpu"]==readings["cpu_percent"] and cards["available"]==readings["available"]
    for key,(count,missing,amount) in expected_groups(raw).items():
        group=observe["process_accounting"][key]
        assert (group["count"],group["unmeasured_count"])==(count,missing)
        measure(group["known_bytes"],amount,"bytes")
        assert cards[key]==display(amount,"bytes",case)+(f" · {missing} 個程序未取得" if missing else "")
    srcrows=[p for s in raw["services"] for p in s["processes"]]+raw["unknown"]
    actualrows=[p for s in observe["services"] for p in s["processes"]]+observe["unattributed_processes"]["top"]
    domrows={int(n.attrs["data-pid"]):n.text() for n in dom.nodes if "data-pid" in n.attrs}
    assert len(srcrows)==len(actualrows)==len(domrows)
    groups = {n.attrs["data-key"]:n for n in dom.nodes if n.tag=="details" and "data-key" in n.attrs}
    for orig,p in zip(srcrows,actualrows):
        assert [p[k] for k in ("pid","start_time","class","service")]==[orig[k] for k in ("pid","start_time","class","service")]
        n=None if orig["memory_basis"]=="rss_fallback" and orig["ram_bytes"]==0 else orig["ram_bytes"]
        measure(p["memory"],n,"bytes");measure(p["cpu"],orig["cpu_percent"],"percent")
        group_nodes = [groups["service-"+orig["service"]]] if orig["service"] else [v for k,v in groups.items() if k.startswith("unknown-group-")]
        assert any(str(orig["pid"])+" / "+orig["name"] in group.text() for group in group_nodes)
        assert display(n,"bytes",case) in domrows[p["pid"]]
        assert display(orig["cpu_percent"],"percent",case) in domrows[p["pid"]]
    for raw_s, report in zip(status["services"],observe["services"]):
        assert raw_s["id"]==report["id"] and raw_s["class"]==report["class"]
        measure(report["attributed_memory"],raw_s["ram_bytes"],"bytes")
        measure(report["cpu"],raw_s["cpu_percent"],"percent")
    return {"sample_id":sid,"system":observe["system"],"process_accounting":observe["process_accounting"],
            "dashboard":cards,"pid_rows":domrows,"passed":True}


def run(case):
    fx=fixture(case)
    with tempfile.TemporaryDirectory(prefix="gk-p0-") as tmp:
        root=Path(tmp); (root/"fixture.json").write_text(json.dumps(fx))
        (root/"observer-token").write_text(TOKEN)
        with subprocess.Popen([UNIT_TEST,"--exact","server::serve_fixed_snapshot","--ignored"],env=dict(os.environ,GK_FIXTURE_DIR=tmp),
                              stdout=subprocess.DEVNULL) as daemon:
            try:
                for _ in range(200):
                    if (root/"port").exists():break
                    if daemon.poll() is not None:raise RuntimeError("fixture server exited")
                    time.sleep(.03)
                port=int((root/"port").read_text())
                cfg=root/"runtime.yaml";cfg.write_text(json.dumps(dict(port=port,state_dir=tmp,safety_margin_bytes=0,services={})))
                status,observe=bridge(cfg,["runtime.status","runtime.observe"])
                url=f"http://127.0.0.1:{port}/"
                req=urllib.request.Request(url+"api/status",headers={"Authorization":"Bearer "+TOKEN})
                with urllib.request.urlopen(req,timeout=5) as response:http=json.load(response)
                dom=browser(url+"#"+TOKEN,root,case)
                if case in {"normal", "stale", "busy"}:
                    operator = browser(url+"#fixture-admin", root, "operator")
                    assert ("disabled" in operator.by_id("cleanup").attrs) == (case != "normal")
                return verify(case,fx,status,observe,http,dom)
            finally:
                daemon.terminate()


def live():
    with tempfile.TemporaryDirectory(prefix="gk-p0-live-") as tmp:
        root=Path(tmp)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1",0))
            port=sock.getsockname()[1]
        assert port!=47831
        cfg=root/"runtime.yaml"
        cfg.write_text(json.dumps(dict(port=port,state_dir=tmp,safety_margin_bytes=0,services={})))
        with subprocess.Popen([BINARY,"serve",cfg],stderr=subprocess.DEVNULL) as daemon:
            try:
                deadline=time.monotonic()+20
                while not (root/"token").exists():
                    assert daemon.poll() is None and time.monotonic()<deadline
                    time.sleep(.05)
                # CPU load is a delta. The first sample has no prior interval.
                before={n:(root/n).read_bytes() for n in ["state.json","events.jsonl"]}
                first,=bridge(cfg,["runtime.status"])
                assert first["resources"]["system"]["cpu_percent"]["availability"]=="unavailable"
                assert all(p["cpu_percent"] is None for p in first["unknown"])
                while True:
                    probe,=bridge(cfg,["runtime.status"])
                    if probe["cpu_percent"] is not None:break
                    assert time.monotonic()<deadline
                    time.sleep(.25)
                assert first["snapshot"]["id"]!=probe["snapshot"]["id"]
                token=(root/"observer-token").read_text()
                url=f"http://127.0.0.1:{port}/"
                req=urllib.request.Request(url+"api/status",headers={"Authorization":"Bearer "+token})
                samples={}
                with ThreadPoolExecutor(max_workers=1) as pool:
                    drawing=pool.submit(browser,url+"#"+token,root,"live")
                    while True:
                        status,obs=bridge(cfg,["runtime.status","runtime.observe"])
                        with urllib.request.urlopen(req,timeout=5) as response:http=json.load(response)
                        sid=status["snapshot"]["id"]
                        if sid==obs["snapshot"]["id"]==http["snapshot"]["id"]:
                            samples[sid]=(status,obs,http)
                        if drawing.done():break
                        assert time.monotonic()<deadline+30
                        time.sleep(.12)
                    dom=drawing.result()
                sid=re.search(r"快照 ([0-9a-f-]{36})",dom.by_id("updated").text())[1]
                status,obs,http=samples[sid]
                assert obs["system"]==http["resources"]["system"]==status["resources"]["system"]
                assert obs["process_accounting"]==http["resources"]["process_accounting"]==status["resources"]["process_accounting"]
                assert status["services"]==obs["services"]==[]
                assert obs["unattributed_processes"]["total_count"]==len(status["unknown"])
                assert [p["pid"] for p in obs["unattributed_processes"]["top"]]==[p["pid"] for p in status["unknown"][:5]]
                cards={n.attrs["data-metric"]:next(c.text() for c in n.children if isinstance(c,Element) and c.tag=="strong")
                       for n in dom.nodes if "data-metric" in n.attrs}
                system=obs["system"]
                def fmt(key):
                    m=system[key]
                    return display(m["value"],m["unit"],"live")
                assert cards["ram"]==fmt("used")+" / "+fmt("total") and cards["available"]==fmt("available")
                assert cards["swap"]==fmt("swap_used")+" / "+fmt("swap_total") and cards["cpu"]==fmt("cpu_percent")
                for key, group in obs["process_accounting"].items():
                    missing=group["unmeasured_count"]
                    assert cards[key]==display(group["known_bytes"]["value"],"bytes","live")+(f" · {missing} 個程序未取得" if missing else "")
                assert len([n for n in dom.nodes if "data-pid" in n.attrs])==len(status["unknown"])
                assert {n:(root/n).read_bytes() for n in before}==before
                pid_cpu=[p["cpu_percent"] for p in status["unknown"]]
                assert any(n is not None for n in pid_cpu)
                for row,p in zip(obs["unattributed_processes"]["top"],status["unknown"]):
                    assert row["cpu"]["value"]==p["cpu_percent"] and row["memory"]["value"]==p["ram_bytes"]
                result={"sample_id":sid,"state_and_events_unchanged":True,
                        "process_cpu":{"unavailable":pid_cpu.count(None),"zero":pid_cpu.count(0),"positive":sum(n is not None and n>0 for n in pid_cpu)},"sampled_at":status["snapshot"]["sampled_at"],"registered_services":0,
                        "first_cpu_availability":first["resources"]["system"]["cpu_percent"]["availability"],
                        "system":system,"process_accounting":obs["process_accounting"],"dashboard":cards,"passed":True}
                (OUTPUT/"live.json").write_text(json.dumps(result,indent=2,ensure_ascii=False)+"\n")
                print("PASS live",sid,flush=True)
            finally:
                daemon.terminate()


if __name__=="__main__":
    OUTPUT.mkdir(parents=True,exist_ok=True)
    if sys.argv[5:]==["--live"]:
        live()
        sys.exit(0)
    for case in CASES:
        data=run(case)
        (OUTPUT/(case+".json")).write_text(json.dumps(data,indent=2,ensure_ascii=False)+"\n")
        print("PASS",case,flush=True)
