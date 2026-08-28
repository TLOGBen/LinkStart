# GitHub Actions Release 與跨 Repo 同步

Type: grilling
Status: resolved
Blocked by: 09

## Question

LinkStart tag 到 GitHub Release、三平台 artifacts、SHA-256、macOS universal 組裝、MIT source release，再到 common-dev 自動同步 PR、plugin version bump、Claude→Codex regeneration 與 install verification 的 release contract 應如何設計，才能保持 LinkStart artifact canonical 且不讓 workflow 直接改 common-dev main？

## Answer

### LinkStart release

- Runtime tag `vMAJOR.MINOR.PATCH` 觸發 GitHub Actions；tag 必須指向 main 的 clean、測試通過 commit。
- Native matrix：Windows x64 MSVC、Linux x64 musl、macOS arm64、macOS Intel；每個 slice先 test + executable smoke + linkage inspection，macOS 再以 `lipo` 合成並驗證 universal。
- Release assets 固定為三個 target binary archives/raw artifacts、`SHA256SUMS`、`release-manifest.json`、SPDX JSON SBOM、MIT LICENSE 與 source archive。`release-manifest.json` 記錄 Runtime SemVer、protocol major、commit、target、size、SHA-256、runner image與 build timestamp。
- v1 要求 deterministic inputs／locked dependencies／documented toolchain，但不以跨 runner bit-for-bit reproducibility 作 gate；差異以 provenance 顯示，不冒充 reproducible build。
- 只有 all-target gates 綠才建立 GitHub Release；Preview Adapter MOE gate由 ticket 11 管理，未通過不得 tag。

### common-dev 同步 PR

- Release publish 成功後，以最小權限 GitHub App token（fallback fine-grained PAT）建立／更新 `linkstart/runtime-v<version>` branch 與 PR；token 只允許 common-dev contents/pull-request必要權限，不得直接 push main。
- Workflow 從剛發布的 exact release下載 artifacts，先驗 `SHA256SUMS`／manifest，再更新 `plugins/linkstart/skills/open-connect/assets/bin/<target>/` 與 `checksums.json`。
- 同步 PR 同時 bump `linkstart` plugin version、common-dev marketplace/catalog version，更新兩個 marketplace catalogs、README、CHANGELOG、AGENTS binary/transfer規則。
- 以 pinned Baransu transfer 版本從 Claude source 生成 `codex/plugins/linkstart/`；不得手改 generated subtree。
- PR gate：全部 JSON parse、Claude/Codex manifest shape、source/generated binary SHA/size/mode parity、transfer closure、temporary Codex HOME Layout A install/list/add、README/CHANGELOG/version consistency。
- Branch/PR creation 必須 idempotent；同 Runtime tag 重跑只更新同一 branch／PR，不建立重複 PR。PR body 列 source tag/commit、artifact checksums、protocol major、Preview maturity 與完整 validation receipts。
- common-dev reviewer/CI 合併後才算 Integration Plugin 發布；LinkStart Release 存在只算 Runtime MOP，不算雙 repo release MOE。

### Recovery

- LinkStart release 失敗：不建立 release、不觸發 sync。
- Sync 驗證失敗：保留 LinkStart release，PR 標 failed/blocked，不推 main、不回滾 canonical assets。
- common-dev 拒絕／延後：Runtime release仍有效，但不得宣稱該版本已內嵌於 Integration Plugin。
