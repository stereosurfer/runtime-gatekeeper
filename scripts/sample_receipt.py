#!/usr/bin/env python3
"""Join trusted executor evidence with a recorded Gatekeeper observation window, offline."""
import argparse
from collections import Counter
from datetime import datetime
import hashlib
import json
from pathlib import Path
import sys

TERMINAL = {"completed", "failed", "cancelled"}
KEY_FIELDS = ("ticket_id", "request_ref", "attempt_id", "native_job_id", "host_ref", "service_instance_ref")


def identifier(value):
    return isinstance(value, str) and bool(value.strip())


def instant(value):
    if not isinstance(value, str):
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return dt.timestamp() if dt.utcoffset() is not None else None
    except (ValueError, OverflowError, OSError):
        return None


def read_file(path):
    content = path.read_bytes()
    doc = json.loads(content)
    if not isinstance(doc, dict):
        raise ValueError("expected a JSON object")
    return doc, {"file": path.name, "sha256": hashlib.sha256(content).hexdigest()}


def objects(doc, field):
    rows = doc.get(field)
    if rows is None:
        return []
    if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
        raise ValueError(f"{field} must be an array of objects")
    return rows


def execution(history, attempt, attempt_ref, request):
    events = objects(history, "events")
    selected = [e for e in events if e.get("request_ref") == request]
    record = dict(status="unknown", reason="no_native_attempt_evidence", attempt_id=None,
                  native_job_id=None, runtime_id=None, started_at=None, ended_at=None,
                  duration_seconds=None, terminal_state=None, service_instance_ref=None)
    if attempt is not None:
        selected = [e for e in selected if e.get("attempt_ref") == attempt_ref
                    or (not e.get("attempt_ref") and attempt.get("runtime_id")
                        and e.get("runtime_id") == attempt["runtime_id"])]
    leases = {e["runtime_id"] for e in selected if identifier(e.get("runtime_id"))}
    admin = {
        "runtime_id": next(iter(leases)) if len(leases)==1 else None,
        "admission": next((e["admission"] for e in reversed(selected) if e.get("admission")), None),
        "release": next((e["release"] for e in reversed(selected) if e.get("release")), None),
        "events": [e["kind"] for e in selected],
        "reconciliation": next((e["reconciliation"] for e in reversed(selected) if e.get("reconciliation")), None),
        "other_undispatched_revisions": sorted({e["request_ref"] for e in events
            if e.get("request_ref") != request and e.get("kind") == "revision_recorded"
            and e.get("reconciliation") == "not_dispatched"}),
    }
    if attempt is None:
        return record, admin
    aid, native = attempt.get("attempt_id"), attempt.get("engine_run_id")
    if not all(map(identifier, (history.get("ticket_id"), request, aid, native, attempt_ref))):
        return record, admin
    if (attempt.get("ticket_id") != history["ticket_id"] or attempt.get("request_ref") != request
            or Path(attempt_ref).stem != aid):
        record["reason"] = "attempt_ticket_mismatch"
        return record, admin

    # Never use a take, lease, queue count or notification as an execution identity.
    related = [e for e in events if e.get("kind") == "engine_result"
               and (e.get("attempt_ref") == attempt_ref or e.get("native_job_id") == native)]
    if not related:
        return record, admin
    for event in related:
        if (event.get("request_ref"), event.get("attempt_ref"), event.get("native_job_id"),
            event.get("engine_result"), event.get("evidence_ref")) != (
                request, attempt_ref, native, attempt.get("outcome"), attempt_ref):
            record["reason"] = "conflicting_native_evidence"
            return record, admin
        if event.get("runtime_id") and attempt.get("runtime_id") and event["runtime_id"] != attempt["runtime_id"]:
            record["reason"] = "lease_evidence_mismatch"
            return record, admin

    instances = {e.get("service_instance_ref") for e in related}
    if len(instances) > 1:
        record["reason"] = "ambiguous_service_instance"
        return record, admin
    start, end = instant(attempt.get("started_at")), instant(attempt.get("ended_at"))
    if start is not None and end is not None and end < start:
        record["reason"] = "reversed_native_times"
        return record, admin
    record.update(status="linked", reason=None, attempt_id=aid, native_job_id=native,
                  runtime_id=attempt.get("runtime_id"), service_instance_ref=instances.pop(),
                  started_at=attempt["started_at"] if start is not None else None,
                  ended_at=attempt["ended_at"] if end is not None else None,
                  terminal_state=attempt.get("outcome") if attempt.get("outcome") in TERMINAL else None,
                  duration_seconds=end-start if start is not None and end is not None else None)
    return record, admin


def valid_instance(value):
    return (isinstance(value, dict)
            and all(type(value.get(k)) is int for k in ("pid", "start_time", "birth_marker"))
            and value["pid"] > 0 and value["start_time"] > 0 and value["birth_marker"] >= 0)


def select_sample(row, key, binding):
    context = row.get("context")
    if not isinstance(context, dict) or any(context.get(k) != key[k] for k in KEY_FIELDS):
        return None, "different_attempt_or_origin"
    return footprint_point(row.get("observation", {}), binding)


def footprint_point(obs, binding):
    if not isinstance(obs, dict) or not isinstance(obs.get("snapshot"), dict):
        return None, "invalid_snapshot"
    snap = obs["snapshot"]
    sid, at = snap.get("id"), snap.get("sampled_at")
    if (not identifier(sid) or type(at) is not int or obs.get("observation_only") is not True
            or type(obs.get("version")) is not int or obs["version"] != 1):
        return None, "invalid_snapshot"
    age = snap.get("age_ms")
    if (type(age) is not int or not 0 <= age <= 10000 or snap.get("stale") is not False
            or "control_operation" not in snap or snap["control_operation"] is not None):
        return None, "stale_or_busy"
    services = [s for s in objects(obs, "services") if s.get("id") == binding["service_id"]]
    if len(services) != 1:
        return None, "service_missing_or_ambiguous"
    service = services[0]
    instance = service.get("service_instance")
    if (not valid_instance(instance) or instance != binding["instance"]
            or service.get("identity_verified") is not True):
        return None, "service_instance_unverified"
    metric = service.get("attributed_memory", {})
    if not isinstance(metric, dict):
        return None, "footprint_unavailable"
    value = metric.get("value")
    if (metric.get("source") != "sum_of_attributed_processes" or metric.get("unit") != "bytes"
            or metric.get("confidence") != "aggregate" or metric.get("availability") != "available" or type(metric.get("timestamp")) is not int
            or metric["timestamp"] != at or type(value) is not int or value < 0):
        return None, "footprint_unavailable"
    return dict(snapshot_id=sid, sampled_at=at, bytes=value), None


def boundary_sample(points, at):
    # Snapshot IDs identify samples, but do not order conflicting values within one Unix second.
    candidates = [p for p in points if p["sampled_at"] == at]
    if not candidates or len({p["bytes"] for p in candidates}) != 1:
        return None
    return candidates[0]


def sampling(record, key, capture, origin):
    out = dict(origin=origin, scope="service_instance", metric="service_footprint", unit="bytes",
               source="runtime.observe/services[].attributed_memory", window_link="unknown", status="unknown", reason=None,
               baseline=None, start_sample=None, sampled_peak=None, end_sample=None,
               peak_minus_baseline_bytes=None, actual_job_peak_bytes=None,
               job_attribution="unknown", job_attribution_reason="no_job_scoped_resource_measurement",
               interval_seconds=None, valid_running_samples=0, gaps=None, rejected={},
               unknown_fields=["baseline","start_sample","sampled_peak","end_sample"])
    if isinstance(capture, dict) and capture.get("collection_method") == "registered-process":
        out["source"] = "Gatekeeper local registered-process observation/services[].attributed_memory"
    if record["status"] != "linked":
        out["reason"] = "no_confirmed_native_attempt"
        return out
    if capture is None:
        out["reason"] = "no_samples_provided"
        return out
    binding = capture.get("binding", {})
    if (not isinstance(binding, dict) or not all(identifier(key[k]) and binding.get(k) == key[k] for k in KEY_FIELDS)
            or not valid_instance(binding.get("instance")) or not identifier(binding.get("service_id"))
            or binding.get("basis") != "executor_native_binding" or not identifier(binding.get("evidence_ref"))):
        out["reason"] = "no_verified_capture_binding"
        return out
    start, end = instant(record["started_at"]), instant(record["ended_at"])
    if start is None or end is None or record["terminal_state"] is None:
        out["reason"] = "incomplete_native_window"
        return out
    cadence = capture.get("interval_seconds")
    if type(cadence) is not int or cadence < 1:
        raise ValueError("interval_seconds must be a positive integer")
    out.update(window_link="matched_attempt_and_native_job", host_ref=key["host_ref"],
               interval_seconds=cadence, service_id=binding["service_id"], instance=binding["instance"],
               binding_evidence_ref=binding["evidence_ref"])

    rejected = Counter()
    samples = {}
    conflicts = set()
    points = []
    pre_submit_ids = set()
    for row in objects(capture, "samples"):
        point, reason = select_sample(row, key, binding)
        if reason:
            rejected[reason] += 1
            continue
        points.append(point)
        if row.get("phase") == "before_submit":
            pre_submit_ids.add(point["snapshot_id"])

    ack_at = instant(capture.get("pre_submit_ack_at"))
    for point in points:
        at = point["sampled_at"]
        if point["snapshot_id"] in pre_submit_ids and (ack_at is None or not at <= ack_at < start):
            rejected["pre_submit_not_before_native_start"] += 1
            continue
        if capture.get("baseline_protocol") == "before_submit" and at < start and point["snapshot_id"] not in pre_submit_ids:
            rejected["not_the_pre_submit_baseline"] += 1
            continue
        if at < start < at+1 or at < end < at+1:
            rejected["ambiguous_subsecond_boundary"] += 1
            continue
        if not start-cadence <= at <= end:
            rejected["outside_window"] += 1
            continue
        sid = point["snapshot_id"]
        if sid in samples and samples[sid] != point:
            conflicts.add(sid)
        samples[sid] = point
    for sid in conflicts:
        del samples[sid]
    if conflicts:
        rejected["conflicting_snapshot_id"] = len(conflicts)
    out["rejected"] = dict(rejected)
    points = sorted(samples.values(), key=lambda p:(p["sampled_at"], p["snapshot_id"]))
    before = [p for p in points if p["sampled_at"] < start]
    running = [p for p in points if start <= p["sampled_at"] < end]
    out["baseline"] = boundary_sample(before, before[-1]["sampled_at"]) if before else None
    out["end_sample"] = boundary_sample(points, end)
    out["valid_running_samples"] = len(running)
    out["start_sample"] = boundary_sample(running, start)
    times = [start] + [p["sampled_at"] for p in running] + [end]
    out["gaps"] = [{"from":a,"to":b,"seconds":b-a} for a,b in zip(times,times[1:]) if b-a > cadence]
    if out["start_sample"] is not None and not out["gaps"]:
        out["sampled_peak"] = max(running,key=lambda p:p["bytes"])
        if out["baseline"] is not None:
            out["peak_minus_baseline_bytes"] = out["sampled_peak"]["bytes"]-out["baseline"]["bytes"]
    missing = [field for field in ("baseline","start_sample","sampled_peak","end_sample") if out[field] is None]
    out["status"] = "complete" if not missing else ("partial" if points else "unknown")
    out["reason"] = None if not missing else "missing_or_unusable_samples"
    out["unknown_fields"] = missing
    return out


def receipt(history, attempt, attempt_ref, capture, request, example):
    if not identifier(history.get("ticket_id")) or not identifier(request):
        raise ValueError("ticket_id and request_ref are required")
    record, admin = execution(history, attempt, attempt_ref, request)
    origin = capture.get("origin") if capture is not None else "no_capture"
    if origin not in {"isolated_replay","recorded_observation","no_capture"}:
        raise ValueError("unrecognized sampling provenance")
    if capture is not None and origin == "no_capture":
        raise ValueError("capture bundle must declare its provenance")
    key = dict(ticket_id=history["ticket_id"], request_ref=request, attempt_id=record["attempt_id"],
               native_job_id=record["native_job_id"], host_ref=history.get("host_ref"),
               service_instance_ref=record["service_instance_ref"])
    return dict(schema_version=1, example_only=example, ticket_id=key["ticket_id"],request_ref=request,
                execution=record,administrative=admin,sampling=sampling(record,key,capture,origin))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history",required=True,type=Path)
    parser.add_argument("--attempt",type=Path)
    parser.add_argument("--request-ref",help="revision to reconcile; defaults to attempt or held dispatch")
    parser.add_argument("--samples",type=Path)
    parser.add_argument("--example",action="store_true",help="mark source as a deidentified example")
    args=parser.parse_args()
    history,h=read_file(args.history)
    sources={"history":h}
    attempt=ref=capture=None
    if args.attempt:
        attempt,sources["attempt"]=read_file(args.attempt)
        ref=args.attempt.resolve().relative_to(args.history.resolve().parent.parent).as_posix()
    if args.samples:
        capture,sources["samples"]=read_file(args.samples)
    request=args.request_ref or ((attempt or {}).get("request_ref") or history.get("dispatch_hold_request_ref")
                                 or history.get("request_ref"))
    result=receipt(history,attempt,ref,capture,request,args.example)
    result["sources"]=sources
    print(json.dumps(result,ensure_ascii=False,indent=2,allow_nan=False))


if __name__=="__main__":
    try:
        main()
    except (ValueError,TypeError,KeyError,OSError) as exc:
        sys.exit(f"Invalid receipt input: {exc}")
