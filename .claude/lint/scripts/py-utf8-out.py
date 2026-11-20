"""Flag a Python script that prints non-ASCII text without making its output UTF-8.

  python .claude/lint/scripts/py-utf8-out.py <file.py>...

On Windows a console or pipe defaults to cp950 / cp1252: printing 中文 raises UnicodeEncodeError or comes out
garbled. The text is often built elsewhere (a list of findings joined later), so the check is per file: a
string literal with a non-ASCII letter, a print() or sys.stdout.write(), and no
`sys.stdout.reconfigure(encoding="utf-8")` (or the same on stderr) → flagged. Exit 1 when any file is.
"""
import ast
import sys


def flagged(path: str) -> str | None:
    try:
        src = open(path, encoding="utf-8").read()
        tree = ast.parse(src)
    except (OSError, SyntaxError, UnicodeDecodeError):
        return None
    # Docstrings are never printed: a string that is a whole statement is left out.
    docs = {id(n.value) for n in ast.walk(tree) if isinstance(n, ast.Expr) and isinstance(n.value, ast.Constant)}
    non_ascii = any(
        isinstance(n, ast.Constant) and isinstance(n.value, str) and id(n) not in docs and any(ord(c) > 127 for c in n.value)
        for n in ast.walk(tree)
    )
    utf8 = lambda node: isinstance(node, ast.Constant) and str(node.value).lower().replace("-", "").replace("_", "") == "utf8"
    prints = False
    fixed = False
    for n in ast.walk(tree):
        if not isinstance(n, ast.Call):
            continue
        f = n.func
        if isinstance(f, ast.Name) and f.id == "print":
            prints = True
        if isinstance(f, ast.Attribute):
            target = ast.unparse(f.value)
            if f.attr == "write" and target in ("sys.stdout", "sys.stderr"):
                prints = True
            # sys.stdout.reconfigure(encoding="utf-8"), or on a loop variable over the streams
            if f.attr == "reconfigure" and any(k.arg == "encoding" and utf8(k.value) for k in n.keywords):
                fixed = True
        # sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
        name = ast.unparse(f)
        if name.endswith("TextIOWrapper") and any(k.arg == "encoding" and utf8(k.value) for k in n.keywords):
            fixed = True
    if non_ascii and prints and not fixed:
        return f"{path}：印出非 ASCII 的文字，卻沒有把輸出設成 UTF-8"
    return None


def main() -> int:
    sys.stdout.reconfigure(encoding="utf-8")
    found = [m for p in sys.argv[1:] if p.endswith(".py") and (m := flagged(p))]
    for m in found:
        print(m)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
