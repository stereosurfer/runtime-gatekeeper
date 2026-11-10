# 服務登錄與准入更新

此文件供管理員更新單機 `runtime.yaml`。Agent 仍只使用一次 `runtime.request` 和一次 `runtime.release`；新增服務、重啟 Gatekeeper 與資源取捨由管理員決定。

## 登錄前

1. 讀取原服務的啟動器，核對執行檔、完整參數、監聽埠及其目前的 PID／父程序。只看名稱或埠不足以認定服務歸屬。
2. 使用現有程序的實際執行檔與專屬參數填入 `match_executable`、`match_args`。`discovered` 保持唯讀，不填啟停命令。
3. 確認可在 loopback 上取得 HTTP 2xx 的健康路徑，再填入 `http_health_path`。若路徑只證明 Web 介面存在，專項技能仍須核對模型與工作流。
4. 重型互斥服務填入相同 `exclusive_group`。服務固有相依填入 `requires`，例如 Director 引擎需要其 UI。不要用相同組別代表未知記憶體峰值已量得。

## 埠衝突與尚未登錄的服務

Gatekeeper 只管理 `runtime.yaml` 已定義的 service ID。外部安裝程式仍可嘗試使用任何埠；Gatekeeper 不會阻止安裝、不會自動改埠，也不會停止或接管占埠程序。

| 情況 | 可觀察結果與處理 |
| --- | --- |
| 新服務嘗試綁定已由原服務占用的埠 | 作業系統通常讓後啟動者綁定失敗；檢查原服務與新服務的啟動輸出，改用不同埠或由管理員決定啟動順序。Gatekeeper 不會代替兩者裁決。 |
| 原服務已退出，未登錄程序接手它的埠 | 若埠可連而監聽 PID 不屬於登錄服務，該服務顯示 `unhealthy`，`runtime.request` 回 `BLOCKED_SERVICE`；不會只因埠可連就回 `READY`。 |
| managed 服務要啟動，但已有外部監聽者 | 不接管外部程序，也不強制清空埠。工作申請可能先以 `BLOCKED_SERVICE` 阻擋；直接啟動也會在前置檢查被拒，依程序歸屬可能回 `EXTERNAL_SERVICE` 或 `PORT_CONFLICT`。若啟動記憶體尚未量測，還會先遇到資源阻擋。 |
| 兩個服務定義填入同一個埠，或與 Gatekeeper 自身埠相同 | `check` 回 `duplicate/invalid port`。先修正設定再重啟；若略過 `check`，新 daemon 會因無效設定而無法啟動。 |
| 未登錄服務使用空閒埠並正常運行 | 它不會自動取得 service ID、租約或 `exclusive_group` 保護；若與其他登錄服務沒有明確父子歸屬，會列為未歸屬程序。OS 可用記憶體仍會反映它的占用，但 Gatekeeper 無法替它作工作級協調。 |

新增服務前，先核對目前監聽者，例如 `lsof -nP -iTCP:<port> -sTCP:LISTEN`，再核對 PID 的執行檔、參數與父程序。設定變更不會熱更新；以暫存設定執行 `runtime-gatekeeper check`，保留差異及備份，再受控重啟。若檢查與實際綁定之間有人搶先占埠，作業系統仍可能拒絕啟動；收到阻擋後重新核對，不要自行終止未知程序。

## 套用與回復

1. 先看是否有 `READY` 或 `INTERRUPTED` 租約；有租約時不要重啟 daemon。原設定及執行檔各留一份權限受限的備份。
2. 在暫存檔上修改設定，查看與原設定的差異，使用新執行檔的 `check` 命令核對 schema，再原子替換正式檔案。記下前後雜湊與備份位置；不要把 token、原始狀態檔或私人完整設定附在公開文件中。
3. 受控重啟後讀取 `services.list`：身分探針可用、監聽者屬於登錄程序、HTTP 健康路徑成功，才會顯示 `running`。`unhealthy` 表示須由管理員檢查原服務及設定；不要用另一個程序占同埠來滿足健康檢查。
4. 若新 daemon 無法啟動或預期服務遭誤判，回復執行檔與設定備份，再重啟。保留已存在的租約與原服務程序，不以重建狀態檔作為回復方法。

## 回覆語意

- `READY` 加上 `memory_assurance: unknown`：服務就緒及同組租約條件成立；額外工作記憶體未量測，不能宣稱容量充足。
- `BLOCKED_RESOURCE`、`resource: exclusive_group`：同組工作已有 `READY` 或 `INTERRUPTED` 租約；回覆包含受保護工作。
- `BLOCKED_RESOURCE`、`resource: unknown_service_startup_memory`：未量測的 managed 服務不自動啟動。
- `BLOCKED_SERVICE`：所需服務沒有通過身分與健康檢查；`expanded_requires` 列出服務相依展開結果。

監聽者歸屬使用 macOS 內建 `lsof` 讀取；探針不可用時採保守阻擋。它只確認觀察時的 socket 擁有者，並非模型載入或未來生成成功的保證。

## 原生佇列的唯讀來源

在已核准的服務項目中指定來源。以下是設定片段；替換服務 ID、連接埠、路徑及身分比對資料，就能用於另一個相容服務。

```yaml
services:
  example-worker:
    mode: discovered
    match_executable: /absolute/path/to/worker
    match_args: ["--instance=example"]
    port: 49321
    queue_source:
      path: /v1/metrics/queue
      format: counts
```

- `counts` 接受 running、pending 兩個非負整數；`comfy_queue` 從 queue_running、queue_pending 陣列取長度。原始內容與回傳的其他欄位不會進入觀察回覆。工作歸屬固定為 unknown。要關聯 attempt，需由執行器另提供可核對的原生工作 ID。
- 管理員需確認指定 GET 路徑具唯讀語意。來源只可使用登錄服務在 127.0.0.1 的連接埠與設定檔中的路徑；不接受任意 URL、查詢字串、代理、重新導向、其他 HTTP 方法或呼叫端覆寫。省略 queue_source 即關閉探測。先前使用固定服務名稱及埠的候選設定，需明確補上來源。
- 使用 `runtime.observe` 並指定 service_id 才讀原生佇列。一般狀態、Dashboard、全機觀察都不發佇列 GET。有來源但讀不到時回 null、unavailable 及原因；成功讀到 0/0 才表示該次端點樣本為空。
- macOS 需要健康狀態、可驗證的唯一監聽程序、與登錄服務一致的 PID 及含微秒的啟動時間。每次在發 GET 前及取得回覆後都核對；不一致丟棄此次結果。單次 HTTP 限時 600 ms、內容上限 256 KiB；監聽者查詢另有時限。單一讀取執行緒忙時回未知，不排隊。
- 回應的 instance 給出 PID、啟動時間（Unix 秒）及 birth_marker（該秒內的微秒）。timestamp 是此次端點讀取時間，與主機快照各自記錄。last_success_at 僅供當前可驗證實例參考，失敗時仍不回填舊數量。沒有保存前次佇列內容。其他平台缺少相同的實例驗證時會回未知。
- 程序及監聽者核對是取樣邊界，不是端點認證。同一 PID 內的模型熱載入、遠端代理的後端切換，以及兩次取樣間已完成的短工作，需要服務另提供原生身分或事件才能辨認。
