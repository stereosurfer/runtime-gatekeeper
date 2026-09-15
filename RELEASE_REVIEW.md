# Runtime Gatekeeper · 發布前驗收

日期：2026-09-16。結論：**本機 MVP 功能驗收通過；公開發布準備仍為 PARTIAL**。

## 本次修正

1. 將已停止維護的 serde_yaml / unsafe-libyaml 換為 serde-saphyr 1.2.0，既有 YAML 設定與整合測試通過。
2. ureq 關閉不需要的 TLS / gzip 預設功能；sysinfo 只啟用 system，限制到本產品使用的觀察功能。
3. 面板在操作中、斷線、權杖失效或 state 寫入失敗時停用控制，拒絕重複提交。
4. 斷線資料明確標示為上次快照；重新連線成功時清除過期錯誤提示。
5. 清理部分失敗顯示未停止的服務與原因，不再以處理數掩蓋失敗。
6. macOS Apple Silicon 程序記憶體改用 `phys_footprint`，把 Metal / IOAccelerator 圖形記憶體納入服務聚合；讀取失敗時退回 RSS。
7. macOS 記憶體讀取移至獨立 platform module；加入 macOS 27 launchd quarantine preflight 與 user deployment smoke test。

## 自動驗證

- Rust：14 tests passed / 0 failed（3 unit + 11 integration）。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、release build 通過。
- `node tests/dashboard.mjs` 通過：真實嵌入 JavaScript 搭配受控 transport，驗證 pending/duplicate、partial cleanup failure、offline、expired token、storage fault、reconnect。這是 UI 邏輯測試，不冒充真實瀏覽器。
- `scripts/macos-smoke.sh`：**PASS**，macOS 27.0 build 26A428；launchd、HTTP、token、`/api/status` 與 quarantine 檢查通過。
- Rust 實際啟停測試含共享租約、並行 request 僅啟動一次、部分失敗不停止既有共享服務、健康 timeout、rollback 寫入失敗、程序意外退出、重複 daemon、state 損毀、Host/Origin/token、MCP stdio。
- rollback 寫入失敗測試確認：不回 READY；存活程序仍可觀察；後續控制停用。測試自己的殘留 fixture 經確認後清理。

## 依賴與授權

Cargo audit 0.22.2 使用 RustSec advisory database commit `b50980aad8b8f14f77e25a97b32dd94bf008b0af`（2026-09-09 更新，1,243 advisories），掃描完整 Cargo.lock：**0 vulnerabilities、0 warnings、0 ignored advisories**。見 `evidence/dependency-audit.json`。這是已知公告比對，不等於所有依賴都沒有未知漏洞。[RustSec](https://rustsec.org/)

完整 lock 有 109 個第三方 package；macOS 目標及開發依賴的解析圖為 76 個。`third-party/inventory.json` 保存套件版本、SPDX 授權表達式與授權檔，包含其他平台的保守超集。直接依賴上游 repository 狀態另存 `evidence/direct-dependency-maintenance.json`；repository 未封存不代表舊 major 版本仍獲安全維護。

部分 crates 未隨套件附帶獨立 LICENSE，已保留其原始 copyright / AUTHORS / README，補上其授權允許的 Apache-2.0 文本；objc2 的聲明取自套件對應的上游 commit。**objc2 上游自行記載 Apple SDK 衍生授權的不確定性，本次原文保留，沒有宣稱已解決該法律問題。** 詳見 `third-party/objc2-*/UPSTREAM-LICENSE.md`。

**本專案 LICENSE 尚待使用者決定**。第三方清單不授予 Runtime Gatekeeper 本身的開源權利；目前未代選授權，已建立 [stereosurfer/runtime-gatekeeper](https://github.com/stereosurfer/runtime-gatekeeper) 公開原始碼 repository，但尚未發布 Release。

## 真實瀏覽器驗收

使用 Codex 獨立 in-app browser，從解壓包的執行檔啟動，沒有編譯或借用工具鏈。

| 情境 | 結果 |
|---|---|
| 首次載入、CPU/RAM/swap、分類 | PASS |
| 啟動 → 取消 | PASS，事件沒有新增啟動 |
| 啟動確認雙擊 | PASS，事件僅一筆 service_started |
| restart | PASS，停止後重新啟動，服務恢復 healthy |
| 停止確認框打開後另有 Agent 取得 lease | PASS，後端 PROTECTED 拒絕，程序保持運行 |
| release | PASS，工作消失、服務保留，控制重新可用 |
| cleanup | PASS，明確顯示已停止 1 個服務，程序與 port 清空 |
| daemon 中斷 | PASS，上次快照標示、控制停用 |
| daemon 重啟使舊 token 失效 | PASS，要求重新認證、控制停用 |
| 新 token 登入 | PASS，恢復控制；過期提示已修正並加入回歸測試 |

先前 MVP 已做過一般 stop 按鈕、READY/BLOCKED_RESOURCE 工作顯示及程序詳情；本輪加強上述邊界。尚未做手機 viewport、無障礙完整審查、長時間 soak、另一台 Mac 的網路下載試跑。

## macOS 簽章與分發

- 執行檔：Mach-O arm64；ad-hoc / linker-signed，沒有 Developer ID。
- `codesign --verify --strict`：PASS，檔案完整性通過。
- 主機環境 `spctl --assess --type execute`：**rejected**。受限環境第一次出現 subsystem error，已在主機重新確認，未將環境錯誤當成產品結果。
- 未經 Apple notarization；不得宣稱「任何 Mac 下載即可用」。沒有停用 Gatekeeper、沒有移除 quarantine 作為通過證據。
- 真正正式下載體驗仍需 Developer ID 簽章、公證，以及另一台/乾淨 Mac 的下載測試。

## 發布邊界

本次只準備可審查的交付物。尚待：專案 LICENSE 選擇；第三方上游 SDK 授權 caveat 的發布判斷；若提供一般使用者 binary，完成 Apple 分發流程。原始碼可作為實驗性 MVP 分享的技術基礎，但尚未替使用者授權或發布。

最終包的 checksum 見 `bin/SHA256SUMS`。不包含 token、state、私有路徑設定、測試 log 或本機工具鏈；測試過程的原始工作檔保留在交付目錄外。

## 最終交付與清理

最終 ZIP 已驗證壓縮完整性、解壓後執行權限、binary byte identity、設定驗證及 HTTP 面板載入；新版重新認證成功後已在真實瀏覽器確認舊錯誤提示消失。個人絕對路徑與 state/token/target 排除掃描通過。公開 repository 不包含本機 state、token、LaunchAgent 或私有 runtime.yaml；本機 user deployment 的目前狀態由 `scripts/macos-smoke.sh` 另行驗證。
