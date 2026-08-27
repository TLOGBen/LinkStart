# common-dev Integration Plugin mirror

This directory mirrors the released `linkstart` Integration Plugin for repository-local discovery:

- `plugins/linkstart/` — Claude Code canonical package copy
- `codex/plugins/linkstart/` — Codex package generated from that Claude source

The marketplace source of truth is the sibling `common-dev-plugin` repository. Never edit this mirror independently. Update common-dev first, regenerate its Codex package, then replace both mirror trees and run `python3 integrations/common-dev/check-parity.py` from the LinkStart repository root.
