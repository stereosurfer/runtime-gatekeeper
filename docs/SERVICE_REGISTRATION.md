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
