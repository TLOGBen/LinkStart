"""Flag a file whose line endings no longer match what git itself would write for it (LF became CRLF, or the
reverse).

  python .claude/lint/scripts/eol-flip.py <file>...

The baseline is `git cat-file --filters HEAD:<file>`: the bytes a checkout of the committed version would
put on disk, after `core.autocrlf` and `.gitattributes` eol rules. Comparing with the raw blob instead would
report every file of a repo with `core.autocrlf=true` and no attributes, where a checkout writes CRLF over an
LF blob. Raw bytes on both sides, since git hides a flip when it normalises line endings before diffing.
A file not in HEAD (new, or outside a repo) is skipped; a baseline that mixes both endings is skipped.
Exit 1 when any file flipped.
"""
import subprocess
import sys
from pathlib import Path


def style(b: bytes) -> str:
    crlf = b.count(b"\r\n")
    lf = b.count(b"\n")
    if lf == 0:
        return "none"
    if crlf == 0:
        return "LF"
    return "CRLF" if crlf == lf else "mixed"


def main() -> int:
    # The finding is Chinese; a Windows console's cp950 cannot print it.
    sys.stdout.reconfigure(encoding="utf-8")
    flipped = []
    for arg in sys.argv[1:]:
        f = Path(arg)
        if not f.is_file():
            continue
        r = subprocess.run(["git", "cat-file", "--filters", f"HEAD:./{f.name}"], cwd=f.parent if str(f.parent) else ".", capture_output=True)
        if r.returncode != 0:
            continue
        was, now = style(r.stdout), style(f.read_bytes())
        if was in ("LF", "CRLF") and now != "none" and now != was:
            flipped.append(f"{arg}：換行從 {was} 變成 {now}")
    for line in flipped:
        print(line)
    return 1 if flipped else 0


if __name__ == "__main__":
    sys.exit(main())
