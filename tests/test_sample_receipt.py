import copy
import unittest
from pathlib import Path
import sys

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"scripts"))
from sample_receipt import receipt


class ReceiptTests(unittest.TestCase):
    def setUp(self):
        self.ref="attempts/a2.json"
        self.attempt=dict(ticket_id="ticket",request_ref="v2",attempt_id="a2",engine_run_id="native",
                          runtime_id="lease",started_at="1970-01-01T00:01:40Z",
                          ended_at="1970-01-01T00:01:50Z",outcome="completed")
        self.event=dict(request_ref="v2",attempt_ref=self.ref,kind="engine_result",engine_result="completed",
                        native_job_id="native",service_instance_ref="inst",evidence_ref=self.ref,runtime_id="lease")
        self.history=dict(ticket_id="ticket",request_ref="v2",host_ref="host",events=[self.event])
        self.key=dict(ticket_id="ticket",request_ref="v2",attempt_id="a2",native_job_id="native",
                      host_ref="host",service_instance_ref="inst")
        self.instance=dict(pid=12,start_time=1,birth_marker=0)
        binding=dict(self.key,service_id="svc",instance=self.instance,
                     basis="executor_native_binding",evidence_ref="test:binding")
        self.capture=dict(origin="isolated_replay",binding=binding,interval_seconds=2,
                          samples=[self.row(t,v) for t,v in [(98,10),(100,15),(102,20),
                                                              (104,25),(106,120),(108,50),(110,150)]])

    def row(self,t,n):
        svc=dict(id="svc",identity_verified=True,service_instance=dict(self.instance),
                 attributed_memory=dict(value=n,unit="bytes",timestamp=t,availability="available",
                                        source="sum_of_attributed_processes",confidence="aggregate"))
        obs=dict(version=1,observation_only=True,snapshot=dict(id=f"s{t}",sampled_at=t,stale=False,
                                                      age_ms=1,control_operation=None),services=[svc])
        return dict(context=dict(self.key),observation=obs)

    def result(self):
        return receipt(self.history,self.attempt,self.ref,self.capture,"v2",True)

    def sampling(self):
        return self.result()["sampling"]

    def test_native_window_and_peak_scope(self):
        r=self.result()
        self.assertEqual(r["execution"]["duration_seconds"],10)
        s=r["sampling"]
        self.assertEqual(s["status"],"complete")
        self.assertEqual(s["valid_running_samples"],5)
        self.assertEqual([s[k]["bytes"] for k in ("baseline","start_sample","sampled_peak","end_sample")],
                         [10,15,120,150])
        self.assertEqual(s["sampled_peak"]["sampled_at"],106)
        self.assertEqual(s["peak_minus_baseline_bytes"],110)
        self.assertIsNone(s["actual_job_peak_bytes"])
        self.assertEqual(s["job_attribution"],"unknown")

    def test_pre_submit_ack_bounds_and_same_second_replay(self):
        self.capture["baseline_protocol"] = "before_submit"
        self.capture["samples"][0]["phase"] = "before_submit"
        for ack, valid in (("00:01:37", False), ("00:01:39", True), ("00:01:40", False)):
            with self.subTest(ack=ack):
                self.capture["pre_submit_ack_at"] = f"1970-01-01T{ack}Z"
                self.assertEqual(self.sampling()["baseline"] is not None, valid)
        self.capture["samples"][1]["phase"] = "before_submit"
        self.capture["samples"].append(self.row(100,15))
        self.assertIsNone(self.sampling()["start_sample"])

    def test_gap_preserves_lifecycle_and_endpoint_samples(self):
        del self.capture["samples"][4]
        s=self.sampling()
        self.assertEqual(s["gaps"],[dict(seconds=4,**{"from":104,"to":108})])
        self.assertIsNone(s["sampled_peak"])
        self.assertIsNone(s["peak_minus_baseline_bytes"])
        self.assertEqual(s["end_sample"]["bytes"],150)
        self.assertEqual(self.result()["execution"]["terminal_state"],"completed")

    def test_missing_baseline_and_end_do_not_create_bytes(self):
        self.capture["samples"]=self.capture["samples"][1:-1]
        s=self.sampling()
        self.assertIsNone(s["end_sample"])
        self.assertIsNone(s["baseline"])
        self.assertIsNone(s["peak_minus_baseline_bytes"])
        self.assertEqual(s["sampled_peak"]["bytes"],120)

    def test_missing_start_has_no_start_value_and_no_complete_peak(self):
        del self.capture["samples"][1]
        s=self.sampling()
        self.assertIsNone(s["start_sample"])
        self.assertIsNone(s["sampled_peak"])

    def test_no_samples_preserves_known_terminal(self):
        self.capture=None
        r=self.result()
        self.assertEqual(r["execution"]["terminal_state"],"completed")
        self.assertEqual(r["sampling"]["reason"],"no_samples_provided")
        self.assertIsNone(r["sampling"]["sampled_peak"])

    def test_real_zero_survives(self):
        for row in self.capture["samples"]:
            row["observation"]["services"][0]["attributed_memory"]["value"]=0
        s=self.sampling()
        self.assertEqual(s["sampled_peak"]["bytes"],0)
        self.assertEqual(s["peak_minus_baseline_bytes"],0)

    def test_wrong_ticket_attempt_native_job_revision_host_and_instance(self):
        for key in self.key:
            with self.subTest(key=key):
                row=self.capture["samples"][4]
                row["context"][key]="other"
                self.assertIsNone(self.sampling()["sampled_peak"])
                row["context"][key]=self.key[key]

    def test_instance_rollover_never_contributes(self):
        peak=self.capture["samples"][4]["observation"]["services"][0]
        peak["service_instance"]["birth_marker"]=1
        s=self.sampling()
        self.assertEqual(s["rejected"],{"service_instance_unverified":1})
        self.assertIsNone(s["sampled_peak"])

    def test_single_service_lease_or_queue_count_cannot_substitute_for_binding(self):
        for basis in ["sole_lease","queue_counts","time_overlap",None]:
            self.capture["binding"]["basis"]=basis
            s=self.sampling()
            self.assertEqual(s["reason"],"no_verified_capture_binding")
            self.assertIsNone(s["sampled_peak"])

    def test_damaged_stale_and_busy_points_never_raise_peak(self):
        saved=copy.deepcopy(self.capture["samples"][4])
        for field,bad in [("availability","stale"),("unit","MiB"),("timestamp",105),
                          ("source","queue_counts"),("value",False),("value",None),("confidence","unknown")]:
            with self.subTest(field=field):
                row=copy.deepcopy(saved)
                row["observation"]["services"][0]["attributed_memory"][field]=bad
                self.capture["samples"][4]=row
                self.assertIsNone(self.sampling()["sampled_peak"])
        for field,bad in [("age_ms",12000),("stale",True),("control_operation","restart")]:
            row=copy.deepcopy(saved)
            row["observation"]["snapshot"][field]=bad
            self.capture["samples"][4]=row
            self.assertIsNone(self.sampling()["sampled_peak"])

    def test_repeated_notifications_and_samples_are_idempotent(self):
        expected=self.result()
        self.capture["samples"].extend(copy.deepcopy(self.capture["samples"]))
        self.capture["samples"].reverse()
        self.history["events"].append(dict(self.event,kind="notification_resent"))
        r=self.result()
        self.assertEqual(r["execution"],expected["execution"])
        self.assertEqual(r["sampling"],expected["sampling"])

    def test_contradictory_duplicate_snapshot_is_excluded(self):
        dup=copy.deepcopy(self.capture["samples"][4])
        dup["observation"]["services"][0]["attributed_memory"]["value"]=999
        self.capture["samples"].append(dup)
        self.assertEqual(self.sampling()["rejected"],{"conflicting_snapshot_id":1})
        self.assertIsNone(self.sampling()["sampled_peak"])

    def test_wrong_attempt_ref_result_and_id_collision(self):
        for field,bad in [("native_job_id","different"),("engine_result","failed"),("runtime_id","other"),
                          ("request_ref","v1"),("attempt_ref","attempts/other.json"),("evidence_ref","ack")]:
            with self.subTest(field=field):
                self.history["events"].append(dict(self.event,**{field:bad}))
                self.assertEqual(self.result()["execution"]["status"],"unknown")
                self.history["events"].pop()
        self.attempt["attempt_id"]="other"
        self.assertEqual(self.result()["execution"]["status"],"unknown")

    def test_ack_only_never_invents_an_attempt_even_with_a_capture(self):
        self.history["events"]=[dict(kind="prepared",admission="READY",request_ref="v2",runtime_id="lease"),
                                dict(kind="lookup_no_match",request_ref="v2",reconciliation="awaiting_reconciliation")]
        self.attempt=None
        r=self.result()
        self.assertIsNone(r["execution"]["attempt_id"])
        self.assertIsNone(r["execution"]["duration_seconds"])
        self.assertIsNone(r["sampling"]["sampled_peak"])
        self.assertEqual(r["administrative"]["admission"],"READY")

    def test_invalid_times_do_not_generate_duration(self):
        for ended in ["1970-01-01T00:01:00Z","1970-01-01T00:02:00","invalid",None]:
            self.attempt["ended_at"]=ended
            self.assertIsNone(self.result()["execution"]["duration_seconds"])
            self.assertIsNone(self.sampling()["sampled_peak"])

    def test_different_values_in_same_boundary_second_have_no_arbitrary_winner(self):
        extra=self.row(98,999)
        extra["observation"]["snapshot"]["id"]="another-snapshot"
        self.capture["samples"].append(extra)
        s=self.sampling()
        self.assertIsNone(s["baseline"])
        self.assertIsNone(s["peak_minus_baseline_bytes"])
        self.assertEqual(s["sampled_peak"]["bytes"],120)

    def test_subsecond_start_cannot_relabel_same_second_as_baseline(self):
        self.attempt["started_at"]="1970-01-01T00:01:40.500Z"
        s=self.sampling()
        self.assertIsNone(s["baseline"])
        self.assertIsNone(s["start_sample"])
        self.assertEqual(s["rejected"]["ambiguous_subsecond_boundary"],1)

    def test_failed_attempt_keeps_its_native_terminal(self):
        self.attempt["outcome"]=self.event["engine_result"]="failed"
        self.assertEqual(self.result()["execution"]["terminal_state"],"failed")


if __name__=="__main__":
    unittest.main()
