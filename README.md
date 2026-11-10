# Runtime Gatekeeper · Rust MVP

多工 AI 工作站的 runtime registry + supervisor。Agent 宣告需要的服務，daemon 準備環境並回覆 READY 或明確阻擋原因；人類透過簡潔工作台查看 Process → Service → Job → Agent。

發布前驗收結果請見 [RELEASE_REVIEW.md](RELEASE_REVIEW.md)。目前為 experimental MVP；附帶執行檔尚未經 Developer ID 簽章與 Apple 公證，Gatekeeper 評估為 rejected。

## 啟動

已於 macOS Apple Silicon 原生編譯與測試。需要 Rust stable（本次 1.98.1）、macOS Command Line Tools；不需要 Node、Python、Docker 或任何模型服務。首次編譯需下載 crates。`Cargo.lock` 固定依賴版本。

```sh
cargo build --release --locked
cargo test --locked
./target/release/runtime-gatekeeper check runtime.yaml
./target/release/runtime-gatekeeper serve runtime.yaml
```

daemon 在前景運行，只監聽 `127.0.0.1:47831`。開啟它在終端顯示的完整網址（含 `#` 後的存取權杖）。設定檔路徑可為絕對路徑；`state_dir` 相對於設定檔，不受呼叫端工作目錄影響。

範例只註冊一個內建 `demo-worker`，以及 oMLX / LM Studio / ComfyUI 的唯讀 port 觀察。啟動 daemon **不會自動啟動任何服務**。可以在面板啟動 demo，或透過下列 MCP request 啟動。既有本機 AI 服務不會被修改、切換 provider 或終止。

本次交付另有 `bin/runtime-gatekeeper` 原生 arm64 執行檔，無須先安裝 Rust 即可執行：

```sh
./bin/runtime-gatekeeper serve runtime.yaml
```

關閉 daemon 前，先完成工作、release 租約，再從面板停止 managed 服務或 cleanup。daemon 終止不會強制殺掉正在執行的工作；重啟後不重新取得舊程序的控制權。

本機工具鏈已隔離安裝在本次工作的 `work/toolchain`，沒有修改全域 PATH。此工作目錄可用 `./dev.sh test --locked`、`./dev.sh build --release --locked` 重建；移到別處則使用 PATH 上的 Rust。

## macOS 27 相容性

本版保留 Rust daemon、MCP bridge 與 loopback Dashboard 的架構，沒有依賴 macOS 27 專屬 UI 或私有 framework。Apple Silicon 的程序用量由獨立的 macOS platform module 讀取 `phys_footprint`，失敗時退回 RSS；這讓 Metal / IOAccelerator 服務仍能被觀察，也保留較舊 macOS 的相容路徑。

macOS 27 的 launchd 不再載入帶有 `com.apple.quarantine` extended attribute 的 plist。從 GitHub 下載後若要安裝 user LaunchAgent，請先確認檔案來源，再清除該 plist 的 quarantine；本機 `open-panel.sh` 會在載入前檢查並給出指令，不會靜默繞過 Gatekeeper。現有部署可用以下唯讀檢查驗證 launchd、HTTP、token 與 `/api/status`：

```sh
./scripts/macos-smoke.sh
```

可用 `RUNTIME_GATEKEEPER_DIR`、`RUNTIME_GATEKEEPER_LABEL` 與 `RUNTIME_GATEKEEPER_PORT` 覆寫預設的 user deployment 路徑、label 和 port。

## MCP 接法

Agent 的設定方式、request / release 範本、READY 與阻擋回覆處理、安全界線，請先讀 [Agent MCP 操作指南](docs/AGENT_MCP_GUIDE.md)。

每個 Agent 啟動輕量 **stdio bridge**，連到同一 daemon。所有 Agent 和 Dashboard 共用同一份序列化狀態與 lease，避免多個 MCP server 各自啟動相同服務。MCP 使用逐行 JSON-RPC；stdout 只有協定訊息，錯誤寫 stderr。

在支援 MCP 的客戶端填入以下設定（替換兩個絕對路徑）：

```json
{
  "mcpServers": {
    "runtime-gatekeeper": {
      "command": "/absolute/path/runtime-gatekeeper/bin/runtime-gatekeeper",
      "args": ["mcp", "/absolute/path/runtime-gatekeeper/runtime.yaml"]
    }
  }
}
```

```json
{
  "name": "runtime.request",
  "arguments": {
    "job_id": "research-001",
    "agent": "hermes",
    "requires": ["demo-worker"],
    "memory_bytes": 134217728
  }
}
```

一次 request 會展開服務相依，檢查監聽程序身分、可選的 HTTP 健康路徑、同組工作租約與已量測的記憶體需求，最後回傳 `runtime_id`。使用相同 job_id 與相同內容重試具冪等性，不新增租約、不重複啟動。不同內容使用同一 job_id 回 `JOB_CONFLICT`。BLOCKED 可用原 request 重試；RELEASED 的工作需使用新 job_id。READY 重試會再檢查服務健康，不會掩蓋服務已退出的情況。

工作完成後：

```json
{"name":"runtime.release","arguments":{"runtime_id":"rt-回傳的識別碼"}}
```

release 只解除租約，不停止共享服務。若要回收可丟棄的閒置服務，另呼叫 `runtime.cleanup`（也有面板按鈕）。

| 工具 | 參數 | 行為 |
|---|---|---|
| runtime.status | 無 | CPU / RAM / swap / services / jobs / unknown |
| runtime.observe | service_id? | 讀取主機或指定服務的資源、預留與原生佇列樣本 |
| runtime.request | job_id, agent, requires, memory_bytes? | 取得工作租約或回傳 BLOCKED |
| runtime.release | runtime_id | 冪等釋放租約 |
| services.list | 無 | 全部服務與用量、控制能力 |
| services.get | service_id | 單一服務、程序與租約 |
| services.start | service_id | 啟動允許且未被外部占用的 managed 定義 |
| services.stop | service_id | 停止 daemon 自己啟動且無租約的服務 |
| services.restart | service_id | 需同時允許 start / stop / restart，且無租約 |
| jobs.list | 無 | 所有工作含阻擋、已釋放工作 |
| jobs.get | job_id | 工作、需求、runtime_id 與判定結果 |
| runtime.cleanup | 無 | 只停止 owned + disposable + 無租約 + 允許 stop |

MCP 提供 initialize、ping、tools/list、tools/call，支援 2024-11-05、2025-06-18、2025-11-25 協定版本；未知版本回覆 2025-06-18，由客戶端決定是否相容。不提供 resources、prompts、訂閱、串流通知或 HTTP MCP transport。`/rpc` 是內部受保護的 bridge API，不宣稱為 MCP Streamable HTTP。

參照：[MCP tools 規格](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)、[sysinfo API](https://docs.rs/sysinfo/0.37.2/sysinfo/struct.System.html)。新增服務前先讀 [服務登錄與回復流程](docs/SERVICE_REGISTRATION.md)。

## 靜態設定

`runtime.yaml` 是服務定義的唯一來源；不接受 MCP 傳任意 shell、命令、環境變數、PID 或路徑。修改設定後重啟 daemon 生效，沒有熱更新。

```yaml
port: 47831
state_dir: .runtime
safety_margin_bytes: 2147483648
services:
  worker:
    mode: managed
    command: [/absolute/venv/bin/python, /absolute/service/server.py]
    cwd: /absolute/service
    match_executable: /absolute/venv/bin/python
    match_args: [/absolute/service/server.py]
    port: 47991
    http_health_path: /health
    memory_bytes: 4294967296
    exclusive_group: media-heavy
    requires: []
    disposable: true
    allowed_actions: [start, stop, restart]
    startup_timeout_ms: 10000
```

- `command` 是 executable + argv，直接 spawn，不經 shell；executable 必須絕對路徑，`@self` 是內建 demo 用的目前執行檔。
- 服務必須以前景模式執行，子程序留在專屬 process group。不要用 daemonize、`launchctl`、`brew services` 或會背景化的 wrapper；本版不接管這類程序。
- `match_executable` 是 OS 回報的完整 executable path；virtualenv symlink 可能回報實體 interpreter，需以實際狀態確認。`match_args` 每一項都需精確匹配某個 argv。多個定義同時匹配時不猜測歸屬。
- `mode: discovered` 不可設 command、allowed_actions 或 disposable。帶有 port 的服務必須由精確的執行檔／參數辨識到程序，且該程序（或其明確子程序）確實持有監聽 socket；只知道 port 可連時狀態為 unhealthy，不能取得 READY。socket 歸屬探針失敗也採保守阻擋。
- `http_health_path` 可選；指定後，Gatekeeper 還會對 loopback 端點要求 HTTP 2xx。未指定時只有 socket 身分和 TCP 判定。這不證明模型、節點或整個工作流可用，專項工作仍須檢查。
- `exclusive_group` 讓同組服務的不同工作一次只能有一筆 READY／INTERRUPTED 租約；不停止既有工作。`requires` 是服務相依，request 會自動展開並保存在工作收據中，例如 Director 引擎可要求 Director UI。設定須避免循環。
- `port` 可省略，此時 READY 只表示已辨識程序通過短暫存活檢查；沒有端點就不能做 HTTP 就緒檢查。
- timeout 為 100–60000 ms；port 不可重複；服務 id 限英數字、`-`、`_`。

尚未登錄的程式若占用既有服務的埠，不會被自動接管或關閉。原服務已退出而別的程序占埠時，身分檢查會阻擋 READY；設定內重複埠會在 `check` 階段被拒絕。未登錄服務即使使用空閒埠，也沒有租約及互斥組保護。安裝前的核對與回復步驟見[服務登錄與回復流程](docs/SERVICE_REGISTRATION.md#埠衝突與尚未登錄的服務)。

## 記憶體判定

全程使用整數 bytes，依固定服務 id 順序處理，同一量測快照與狀態必定得到同一結果：

```text
required = 本次 job 額外 memory_bytes + 尚未運行服務的 memory_bytes
outstanding = 現有 READY/INTERRUPTED job 額外 memory_bytes
              + 運行服務 max(估計 RAM - 已量測 RAM, 0)
available = max(OS available - safety_margin - outstanding, 0)
shortfall = max(required - available, 0)
```

macOS 上若 sysinfo 的 `available_memory()` 因壓縮頁扣除而飽和為 0，准入改用同次系統快照的 `free_memory()` 作保守下界；它只計真正空閒的實體頁，不把 inactive／purgeable 頁或記憶體壓力百分比換算成可用 bytes。兩者皆為 0 時仍阻擋，且安全保留量與既有工作估計照常扣除。`runtime.status` 與記憶體阻擋回覆會列出 `available_basis`、原始 `sysinfo_available` 與 `free_memory`，方便追查判定來源。這仍可能錯拒可運行的工作；任何 READY 都不是 OS 保留記憶體的保證。

`memory_bytes` 是**額外工作需求**，不是包括共享服務在內的總 RAM。未填值保持 `null`，回覆的 `memory_assurance: unknown` 與 `unmeasured_job_memory: true` 明示容量未知；即使其他條件得到 READY，也不能將 0 當作量測結果。未量測的 managed 服務啟動會以 `unknown_service_startup_memory` 阻擋。運行中的共享服務不重複計算啟動 RAM。現有已量測 job 額外需求持續保守扣除至 release。這是簡單 admission accounting，沒有排程、優先權、資源搶占或 OS 記憶體保留；外部程序仍可能在 READY 後消耗 RAM。

```json
{
  "status": "BLOCKED_RESOURCE",
  "resource": "memory",
  "units": "bytes",
  "required": 40000000000,
  "available": 22000000000,
  "shortfall": 18000000000,
  "reclaimable": [],
  "protected": [],
  "runtime_id": "rt-...",
  "job_id": "research-001"
}
```

reclaimable 列出 daemon-owned、無租約且可手動 stop 的服務；protected 列出其他運行中的已登錄服務及 consumers。未識別程序在 status / Dashboard 的 Unknown 中，永遠不可控制。reclaimable 的 RAM 是估計可釋放量，並非系統保證。資源不足時不啟動服務、不釋放租約、不殺程序。人類裁決後，Agent 重送同一 request。

## 架構與安全模型

```text
Agent ─ stdio MCP bridge ─┐
                         ├─ loopback HTTP ─ 單一序列化 daemon
Human ─ Web Dashboard ───┘                      │
                         registry / leases / memory gate
                            │                 │
                    sysinfo + macOS       supervisor
                  phys_footprint API     owned Child + process group
                            │                 │
                         runtime.yaml / state.json / events.jsonl
```

- **Managed**：可啟動的靜態定義；運行時只有本 daemon 持有 live Child handle 的程序可控制。
- **Discovered**：外部啟動、由登錄的執行檔與參數辨識。若設定了 port，監聽者還必須屬於該程序或其明確子程序；只看見 port 不代表服務健康。即使定義為 managed，已有外部 listener 也只觀察，拒絕接管或重複啟動。
- **Unknown**：未歸屬程序，僅查看。看不到的 OS 程序、受權限限制的資訊可能缺漏。
- Dashboard 會把 Unknown 依顯示用的應用程式群組彙總（例如 Google Chrome、ChatGPT / Codex Desktop、WebKit / In-app Browser、Python / Node），並保留可展開的 PID 明細。這是閱讀用的啟發式分組，不改變服務所有權；真正的服務歸屬仍只由 `runtime.yaml` 的精確程序匹配決定。
- Group membership 用於 daemon-owned 服務聚合，外部精確匹配後向子程序繼承歸屬；每個 PID 只計入一個服務。只顯示程序名稱、PID 與用量，不把完整 argv 送到面板。
- 有 READY 或 INTERRUPTED lease 時一律拒絕 stop / restart，沒有 force 選項。Agent 名稱是 caller 提供的標籤，不是身分認證；本版信任同一使用者的 MCP 客戶端。
- 停止先向已驗證、live root 的專屬 group 發 SIGTERM，等待 2 秒，再針對仍符合 PID + start time + group 的成員發 SIGKILL，最後驗證退出。沒有 kill arbitrary PID 工具。
- 新 request 的部分啟動失敗會回復本次新啟動的服務；不碰先前已運行服務。rollback 結果會回傳，失敗不宣稱 READY。服務日誌位於 state_dir。
- daemon 重啟時，舊 READY 工作標為 INTERRUPTED 並保留租約。舊 PID 不恢復控制權；需確認工作後 release，再決定如何處理外部程序。程序自行退出不會自動重啟。
- state directory 使用 0700，token、state、事件與 log 使用 0600；state.json 以暫存檔 + rename 更新，事件 append JSONL。單一 state_dir 的檔案鎖阻擋第二個 daemon。
- HTTP 只綁 loopback，API 需 bearer token，檢查 Host / Origin，不開 CORS。Dashboard token 由 URL fragment 讀入 sessionStorage，隨即移除 fragment。UI 控制需人類確認。資料使用 textContent 呈現，無外部資產。
- 本版為受信任的單一 macOS 帳號使用；不是惡意本機使用者隔離、安全沙箱或高可用 supervisor。監聽程序身分及 HTTP 健康檢查與實際使用之間仍有時間差，不能證明某一模型已載入。不要暴露 port、轉發代理或共用 token。

## 已知 MVP 限制

- macOS 主機總量與可用 RAM 分別採自 sysctl 與 Mach；准入讀取 sysinfo，必要時採同次快照的空閒頁下界。macOS 程序用量優先讀 `phys_footprint`，包含 Metal / IOAccelerator 圖形記憶體，讀取失敗時退回 RSS。程序 footprint 聚合仍可能因共享資源重疊而高於系統已用 RAM，也不能推論 per-process swap。CPU 是兩次更新的差值，第一次不代表穩態。
- 無 GPU / SMC、歷史圖表、多機、scheduler、自動回收排程、自動犧牲工作、模型載入驗證、跨工作排程 DAG、輸出目錄準備。服務相依關係在單筆 request 內展開，面板每 5 秒讀取已發佈的快照。
- 控制操作由單一 engine 執行；讀取由約每 2 秒更新的快照提供。啟動 health 等待期間快照暫停更新，但面板與唯讀 MCP 仍可回應，並附上取樣時間、年齡與目前控制操作。超過 10 秒或控制忙碌時面板停用控制；准入仍由 engine 重新檢查即時資源。HTTP 尚未提供抗惡意慢連線或長時間不讀取回應的服務品質保證。
- 不支援服務逃離 process group、root 退出後持續工作的 daemonized 子程序。此類程序退為觀察，需人類處理。macOS PID/start-time 檢查可降低誤殺風險，不能提供 kernel pidfd 等級的無競態保證。
- JSON state 與 JSONL event 不是單一資料庫 transaction，崩潰時事件可能少最後一筆；損毀 state 會拒絕啟動，不清空租約。寫入失敗會停用後續控制操作，需修復後重啟。events / logs 尚無 rotation。
- 未自動安裝到 launchd，也未替 Hermes / Codex 寫入 MCP 設定。

## 驗證

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
node tests/dashboard.mjs # 可選 Node.js，僅驗證面板；執行服務不需要 Node
./scripts/macos-smoke.sh # macOS user LaunchAgent 的唯讀相容性檢查
```

整合測試使用獨立暫存目錄與隨機 loopback ports，實際啟停內建 fixture，不觸碰本機 AI 服務。包含：記憶體邊界、discovered 設定拒絕、共享租約、request 冪等、受保護 stop/restart、release/cleanup、外部 port 不接管、部分啟動 rollback、stdio MCP handshake、Host/Origin/token 邊界、daemon 重啟保留租約、健康 timeout、job_id 衝突。

依賴使用 Cargo.lock 固定版本。YAML 解析已改為 serde-saphyr；HTTP bridge 只保留 JSON 功能，sysinfo 只啟用 system。第三方授權清單與上游聲明位於 `third-party/`，其授權不等於本專案授權。

## MCP 診斷與輸出契約

面板顯示 bridge 識別、client 名稱/版本、initialize、tools/list、最近成功工具呼叫與最近結果。這是本次 daemon 啟動後的觀察紀錄，最多保留 64 個 bridge；不是心跳或在線保證，也不能證明另一串對話已曝光工具。未帶 bridge 標頭的舊 client 不會出現在這個清單；重新啟動該 client 的 bridge 後才使用新版診斷。

每個 stdio bridge 獨立存在，斷線不會釋放工作 lease 或停止其他 client 的共用服務。bridge 每次呼叫重新讀取 token，daemon 重啟後可在下次呼叫恢復；傳輸失敗回傳 JSON-RPC error，不自動重送可能已執行的變更。先讀取工作狀態再決定是否重試。Dashboard 在 daemon token 輪替後仍需新的存取網址。

12 個工具提供 object outputSchema 與 structuredContent；業務阻擋仍是成功傳遞的判定，Agent 必須讀 status。錯誤回覆的 isError 與 JSON-RPC errors 不套成功輸出 schema。支援協定版本不變。所有唯讀工具都附 snapshot metadata；它們不是即時准入保證。程序列附 memory_basis：phys_footprint、rss_fallback 或 rss；取樣時間在 snapshot.sampled_at。聚合仍可能重疊，不能反推個別程序 swap。

隔離驗證使用測試專用設定、state 及閒置 loopback ports，不載入正式 runtime.yaml。正式更新前需確認沒有 READY/INTERRUPTED lease 及 daemon-owned 服務，備份執行檔與設定，才可切換及重啟。不得為了更新自行釋放別人的租約。

## 資源觀察（需由原始碼建置）

隨附的舊執行檔不含 `runtime.observe`。不帶參數時可看系統壓力、Swap、已知與未知預留；指定已登錄的服務可另看程序歸屬。登錄時可為任意服務設定 `queue_source`。指定服務的觀察查詢會由獨立讀取執行緒，以唯讀 GET 讀取經核對實例的佇列數量；讀取忙碌回 `sampler_busy`，無來源回 `no_configured_source`。服務歸屬與實例識別規則見[服務登錄指南](docs/SERVICE_REGISTRATION.md)。觀察不會核發租約，外部工具的圖形記憶體不能再加到程序 footprint。

設定唯讀 MCP 時，將 stdio 指令設為 `runtime-gatekeeper mcp-observe /absolute/path/runtime.yaml`。daemon 以獨立的 observer-token 驗證六項查詢。控制命令沿用原有管理入口。

## 同一份主機快照

在 runtime.status 的 `resources.system`、runtime.observe 的 `system` 和面板上比對 CPU、RAM、swap 時，先比對 `snapshot.id`。每次發佈新取樣都會產生新 ID；同一份樣本的 `sampled_at` 固定，`age_ms` 隨讀取時間增加。來源、單位、原值及可用性保存在每項量測中。定向查詢的原生佇列有自己的取樣時間。

`memory.available` 與 `available_basis` 保留准入採用的值及來源。主機量測在 `resources.system.available`，與 observe 和面板共用；macOS 核心讀值失敗時標示未取得。

`resources.process_accounting`（observe 中為 `process_accounting`）分列已歸屬、未歸屬和全部程序的筆數、未量得 RAM 的筆數及已量得的 bytes 小計。確實空的清單其小計為零；非空但全數讀不到時為 null。部分讀取成功會保留已知小計，同時揭露缺幾個程序。單一服務顯示完整合計，有缺值就顯示未取得。程序加總與系統已用量口徑不同。observe 保留未歸屬程序的前五筆，總量仍涵蓋本次整份清單。

macOS 觀察取樣直接檢查 Mach、sysctl 與程序計數器讀取結果。主機 CPU 是整機忙碌比例；程序 CPU 是單核心百分比，可高於 100%。首次取樣、程序換實例或缺計數器基線時，CPU 回報未取得。有效的兩筆讀數沒有增加，才得到 0%。程序記憶體保留 phys_footprint／RSS 回退的來源。超過 10 秒的樣本標過期；控制執行中則保留忙碌原因與前次樣本的時間。

`tests/snapshot_dashboard.py` 使用 Rust 測試執行檔中的固定採樣器、正式 stdio bridge、HTTP 路由和背景 Chrome 驗證面板。`--live` 另外使用零登錄服務的獨立 daemon，從三個入口比對同一筆實際 Mac 快照。

## 工單採樣收據

可用[離線收據工具](docs/SAMPLE_RECEIPTS.md)把執行器的 attempt 與原生工作 ID 對上基線、執行中服務 footprint 的採樣峰值、結束樣本與終態。收據分開記錄證據來源、缺樣及歸屬範圍。
