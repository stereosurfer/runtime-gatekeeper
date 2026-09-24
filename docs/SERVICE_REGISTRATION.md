# 服務登錄與准入更新

此文件供管理員更新單機 `runtime.yaml`。Agent 仍只使用一次 `runtime.request` 和一次 `runtime.release`；新增服務、重啟 Gatekeeper 與資源取捨由管理員決定。

## 登錄前

1. 讀取原服務的啟動器，核對執行檔、完整參數、監聽埠及其目前的 PID／父程序。只看名稱或埠不足以認定服務歸屬。
2. 使用現有程序的實際執行檔與專屬參數填入 `match_executable`、`match_args`。`discovered` 保持唯讀，不填啟停命令。
3. 確認可在 loopback 上取得 HTTP 2xx 的健康路徑，再填入 `http_health_path`。若路徑只證明 Web 介面存在，專項技能仍須核對模型與工作流。
4. 重型互斥服務填入相同 `exclusive_group`。服務固有相依填入 `requires`，例如 Director 引擎需要其 UI。不要用相同組別代表未知記憶體峰值已量得。

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
