# Skill-local Binary 與雙平台 Port 契約

Type: grilling
Status: resolved
Blocked by: 01, 07, 08

## Question

`plugins/linkstart/skills/open-connect/assets/bin/<target>/` 應如何放置、選擇、驗 checksum 並直接執行各平台 binary，Claude source 如何引用 skill-local assets，Codex generated output 如何保證完整複製與 executable behavior，而不重複 binary 或手改 generated subtree？

## Answer

唯一 binary source layout：

```text
plugins/linkstart/skills/open-connect/assets/
├── bin/
│   ├── linux-x64-musl/linkstart
│   ├── windows-x64/linkstart.exe
│   └── macos-universal/linkstart
└── checksums.json
```

- Binary 來源必須是 exact LinkStart Runtime tag 的 GitHub Release assets；`checksums.json` 記錄 Runtime SemVer、protocol major、target、size、SHA-256 與 build provenance。Plugin 不自行重編 binary。
- Target resolution 依**執行環境**而非外層 OS：Linux/WSL → `linux-x64-musl`、Windows native → `windows-x64`、macOS → `macos-universal`；unsupported arch fail closed。
- 執行前以 target path + `checksums.json` 驗 SHA-256、Runtime exact version 與 protocol major；不符回 `runtime_binary_invalid`，不得找 PATH 或下載替代品。
- Unix/macOS binary 以 Git mode `100755` 追蹤；Windows `.exe` 不依賴 POSIX mode。若 plugin cache 無 executable permission或被 noexec/quarantine 阻擋，fail closed並回可診斷錯誤，不偷偷複製到別處。
- Claude source 透過 skill-local root 引用 `open-connect/assets/...`；Codex skill 由 transfer 產生其對應 root resolver，skills 不硬編 user cache/version path。
- `assets/` 是 transfer 的標準遞迴 copy surface；binary 不放 plugin-root assets。Transfer 後必須 machine-check source/generated 每個 binary SHA-256、size、相對 path 與 Unix mode；不得只信 closure existence。
- `link-in`、`register-app` 不攜帶 binary，只呼叫 `open-connect` 定義的同一 resolver／launcher contract。
- `codex/` 永遠由 Claude source transfer 生成；不得手改 generated binary subtree。兩個 marketplace catalogs 手動新增 `linkstart` entry並與生成 tree 對齊。
- common-dev `AGENTS.md` 應同步修正 transfer copy-surface drift（目前實作亦含 `evals/`），並新增 binary parity validation；README/CHANGELOG、plugin version 與 catalog version跟隨 distributed change 更新。
