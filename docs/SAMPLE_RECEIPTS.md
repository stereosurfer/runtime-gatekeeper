# 工單採樣收據

`scripts/sample_receipt.py` 是離線唯讀入口，使用 Python 3 標準函式庫。它把執行器的原生結果、Local Admin 的事件及 Gatekeeper 的資源樣本接在同一次工作嘗試上。執行期間由獲准的工作流保存樣本；此工具讀取已保存的檔案，將一張 JSON 收據輸出至 stdout。

```sh
python3 -I -B scripts/sample_receipt.py \
  --history TICKET/admin/history.json \
  --attempt TICKET/attempts/ATTEMPT_ID.json \
  --samples capture.json
```

只有管理記錄時可省略 attempt、samples。history 含 dispatch_hold_request_ref 時優先核對該次派送；也可用 `--request-ref` 明確選定修訂。測例使用 `--example` 標明身分。輸入檔案雜湊附在收據中，可檢查重播所用版本。

## 原生證據

attempt 提供 ticket_id、request_ref、attempt_id、engine_run_id、原生起訖時間及 outcome。history 的 engine_result 以同一修訂、attempt_ref、evidence_ref、native_job_id 與終態核對。若兩處均提供 runtime_id 也需一致。attempt 檔名為該 ID；參照相對於工單目錄。ID 衝突或無對應原生證據時，execution 以 unknown 表示，時間和終態留 null。管理事件仍可獨立列出准入、最後的對帳狀態及釋放回條。通知重送和修訂登記都不產生新 attempt。

## 樣本與收據欄位

capture JSON 由受信任的採集端提供，origin 選 `isolated_replay` 或 `recorded_observation`。interval_seconds 為預先設定的正整數。binding 包含以下資料：

- ticket_id、request_ref、attempt_id、native_job_id、host_ref、service_instance_ref。這六欄逐項與執行器／工單記錄核對。
- 核准工作流提供的 service_id，及該服務實例的 instance（pid、start_time、birth_marker）。basis 為 `executor_native_binding`，evidence_ref 指向執行器確認該映射的證據。測試可使用明確標記的 fixture 映射。簽發者與證據參照的真實性由提供者核對；這個離線工具只驗內容一致性。
- samples 是陣列，每筆含 context（相同六欄）及 observation（`runtime.observe` 的 structuredContent）。觀察必須來自同一個可辨識的服務實例。主機樣本中的 service_instance 與資源數字共用取樣時間，獨立於原生佇列讀取。來源為 `sum_of_attributed_processes`、單位 bytes、可用且未過期的 attributed_memory 才參與計算。

baseline 取開始之前一個間隔以內的最後一筆；start_sample 與 end_sample 要對到原生開始及結束的取樣秒。sampled_peak 是從原生開始（含）到結束（不含）之間最高的有效觀測值，另記觀測時間、快照 ID、有效樣本數及間隔。若第一筆缺失或相鄰觀測跨過預設間隔，主峰值留 null，並列出時間缺口。已確認的原生終態仍可保留。對時需有共用時基。跨越原生小數秒邊界的整秒樣本不參與歸類；同一邊界秒若有互相矛盾的數值，該邊界值留 null。開始／結束對不到整秒的樣本時不插值。沒有可對帳的時窗，gaps 留 null。相同快照重送只計一次，衝突副本排除。量到零時保留數字 0。

`window_link` 表示工作與觀察時段的核對結果。資源值的 scope 為 service_instance，共用模型與其他工單可能同時使用該服務。`actual_job_peak_bytes` 保留 null；要驗證工單自己的用量需另有工作專屬的量測來源。採樣點之間的瞬時峰值也無法由這份收據補出。

若採集端使用 `baseline_protocol: before_submit`，開始前樣本須標記 `phase: before_submit`。工具核對回條時間 `pre_submit_ack_at` 介於取樣與原生啟動之間。來不及對時或重送同一取樣時，缺少的開始或峰值維持未知。
