# Runtime Gatekeeper：Agent MCP 操作指南

這份指南給會呼叫 Runtime Gatekeeper MCP 的 Agent。目標是讓 Agent 宣告工作需要的服務，接收准入結果，完成後釋放工作租約；環境檢查、程序辨識和服務生命週期由 Runtime Gatekeeper 管理。

## Agent 必須遵守的流程

1. **每個新工作只送一次 `runtime.request`。** 在同一份 request 中列出這項工作必需的所有服務，不要逐項試服務，也不要自行查程序、port、RAM 或執行環境。
2. **只有收到 `READY` 才開始依賴這些服務的工作。** 記下回傳的 `runtime_id`。
3. 收到 `BLOCKED_RESOURCE` 時，停止需要該環境的工作，把下方列出的資源數字和服務名稱原樣回報使用者，等待使用者裁決。不得自行停止服務、呼叫 cleanup、殺程序或降低工作需求。
4. 使用者處理資源後若仍要繼續，使用**相同 `job_id` 和完全相同的 request** 重試。不要在等待重試時 release；blocked 工作不持有服務 lease。若使用者取消且不再重試，可用回傳的 `runtime_id` 呼叫 `runtime.release` 關閉工作紀錄。
5. 收到 `BLOCKED_SERVICE` 時，回報 `unavailable` 服務及錯誤原因。不要假設服務已就緒；由使用者或管理員處理後，才用相同 request 重試。
6. 工作完成、失敗或取消後，若曾取得 `READY`，以該次回傳的 `runtime_id` 呼叫一次 `runtime.release`。Release 只解除工作租約，**不會停止共享服務**。

不可自行呼叫 `services.start`、`services.stop`、`services.restart` 或 `runtime.cleanup`。`runtime.request` 會按設定啟動所需的 managed 服務；其餘服務控制保留給使用者。也不要把 Agent 名稱當成已驗證的使用者身分。

## 開始前：管理員設定一次

Runtime Gatekeeper daemon 必須先運行。MCP stdio bridge 是客戶端啟動的連線程式，不會代替 daemon；若連線失敗，回報 MCP 服務目前不可用，不要自行啟動第二個 daemon 或改查系統環境。

若尚未部署 daemon，可在專案目錄前景啟動：

```sh
./bin/runtime-gatekeeper serve runtime.yaml
```

如果已由 LaunchAgent 或其他管理方式運行，不要再啟動第二份。設定檔中只有已登錄的 service ID 能放進 `requires`。範例設定的 `demo-worker` 是安全的內建 HTTP fixture；`omlx`、`lmstudio`、`comfyui` 是唯讀 discovered 項目，Gatekeeper 不會替它們啟動程序。個人設定中的 service ID 以該機器的 `runtime.yaml` 為準，不要猜名稱，也不要把尚未登錄的程序冒認為服務。

GitHub 附帶的 macOS 執行檔目前未經 Developer ID 簽章與 Apple 公證，系統可能會阻擋執行。若遇到此情況，請依 README 的建置方式從可信任的原始碼編譯；不要為了讓 Agent 連線而停用 Gatekeeper。

### Hermes Agent

在 `~/.hermes/config.yaml` 加入以下區段，將兩個路徑換成此機器上的絕對路徑：

```yaml
mcp_servers:
  runtime-gatekeeper:
    command: "/absolute/path/runtime-gatekeeper/bin/runtime-gatekeeper"
    args:
      - mcp
      - "/absolute/path/runtime-gatekeeper/runtime.yaml"
    enabled: true
```

保留既有 `config.yaml` 內容，將 `mcp_servers` 合併到原有設定，不要整份覆寫。重新啟動 Hermes 或依 Hermes 文件重新載入 MCP。管理員可用下列方式確認連線及工具探索：

```sh
hermes mcp test runtime-gatekeeper
```

Hermes 的 MCP 設定只會讓 Hermes 看見這些工具；每個 MCP 客戶端都要各自設定。設定 Hermes 不會自動把工具加到 Codex 或 ChatGPT。詳見 [Hermes MCP 官方文件](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/mcp.md)。

### 其他支援 MCP 的客戶端

若客戶端使用 JSON `mcpServers` 設定，可填入：

```json
{
  "mcpServers": {
    "runtime-gatekeeper": {
      "command": "/absolute/path/runtime-gatekeeper/bin/runtime-gatekeeper",
      "args": [
        "mcp",
        "/absolute/path/runtime-gatekeeper/runtime.yaml"
      ]
    }
  }
}
```

請依該客戶端的 MCP 設定格式放置這段設定。不要把私人的絕對路徑或使用者設定檔提交到 GitHub。

## Agent 呼叫範本

先取得工作流程已核准的 service ID、Agent 名稱和唯一 `job_id`。`requires` 必填且不可為空；`memory_bytes` 可省略，若提供，代表**額外工作記憶體**，單位是 bytes，不是服務總 RAM。不要自行估算或填入未經工作定義提供的需求。

```json
{
  "job_id": "research-20260923-01",
  "agent": "hermes",
  "requires": ["demo-worker"]
}
```

在 MCP 客戶端選擇 `runtime.request` 工具，將上面的物件作為 arguments 傳入。實際工作應使用該工作流程在本機 `runtime.yaml` 登錄的 service ID；`demo-worker` 僅供範例測試。

## 判讀回覆

### `READY`

工作可以開始。保存 `runtime_id`，不要因每個後續子步驟或工具呼叫再次 request。Agent 不必再逐一確認 service、process 或 port。

### `BLOCKED_RESOURCE`

以結果內的原始值回報使用者：

- `required`：本次額外需求，bytes。
- `available`：扣除安全保留量及現有估算後可供本次准入的量，bytes。
- `shortfall`：仍不足的量，bytes。
- `reclaimable`：daemon 擁有、沒有工作 lease 且允許手動停止的服務候選；只供使用者裁決，不代表 Agent 可停止它。
- `protected`：目前不應停止的已登錄服務及其工作使用者。

可將 bytes 換算成易讀單位，但同時保留原始數值與 service ID。不要只說「記憶體不夠」，也不要推測某個服務可以犧牲。資源狀態變更由使用者在 Dashboard 或自行管理後，Agent 才以同一 `job_id`、同一 arguments 重試。

### `BLOCKED_SERVICE`

回報 `unavailable`、`service`、`reason` 及 `rollback`（若回傳）。常見原因是 discovered 服務尚未運行、埠口健康檢查失敗，或 managed 服務啟動失敗。discovered 服務只能觀察，Gatekeeper 不會接管或啟動外部程序。

### MCP 工具錯誤

如 service ID 未登錄、arguments 不合法或 daemon 無法連線，原樣回報錯誤及使用的 `job_id`。不要以 shell、PID、port 掃描或更改客戶端 provider 作為替代流程。

## Release 範本

只在 `READY` 工作結束後使用，填入 `runtime.request` 的回傳值：

```json
{
  "runtime_id": "rt-回傳的識別碼"
}
```

在 MCP 客戶端呼叫 `runtime.release`。如果工作因資源或服務阻擋而取消，且不打算重試，可用該 blocked 回覆的 `runtime_id` 關閉工作紀錄；這不會停止任何服務。若要稍後重試，保留 blocked 紀錄並重送原 request。

## 可用工具與 Agent 使用範圍

Runtime Gatekeeper 也提供唯讀工具 `runtime.status`、`services.list/get`、`jobs.list/get`，供使用者或明確要求檢查狀態的工作查閱。正常工作路徑只需 `runtime.request` 和 `runtime.release`。服務控制與清理工具由管理員按明確決策操作，不屬於 Agent 自動排障流程。
