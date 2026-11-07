#!/usr/bin/env python3
"""The wizard's license page: an introduction, then parts, each a heading and its text files,
as one RTF document (Inno Setup shows .rtf with its formatting).

usage: python3 license_page.py OUT.rtf [--intro TEXT] --part HEADING FILE [FILE...] [--part ...]

Text with long lines (one line per paragraph, Markdown headings) is set as prose; text wrapped
by hand (most license files) keeps its lines, in a monospaced font.
"""
import sys


def esc(s):
    """RTF text: braces and backslashes escaped, everything past ASCII as \\uN?."""
    out = []
    for ch in s:
        o = ord(ch)
        if ch in "\\{}":
            out.append("\\" + ch)
        elif ch == "\t":
            out.append("\\tab ")
        elif 32 <= o < 127:
            out.append(ch)
        elif o < 32:
            continue
        elif o < 0x10000:
            out.append("\\u%d?" % (o - 65536 if o > 32767 else o))
        else:
            o -= 0x10000
            for u in (0xD800 + (o >> 10), 0xDC00 + (o & 0x3FF)):
                out.append("\\u%d?" % (u - 65536))
    return "".join(out)


def prose(text):
    """One line per paragraph (a fifth or more of the lines are long), not wrapped by hand."""
    lines = [x for x in text.splitlines() if x.strip()]
    return bool(lines) and sum(len(x) > 100 for x in lines) >= len(lines) / 5


def body(text):
    """RTF paragraphs for one text."""
    out = []
    if prose(text):
        for line in text.splitlines():
            t = line.strip()
            if not t:
                continue
            if t.startswith("#"):
                out.append("\\pard\\sb120\\sa60\\keepn\\f0\\fs18\\b\\cf1 %s\\b0\\par" % esc(t.lstrip("#").strip()))
            else:
                t = t.lstrip(">").strip().replace("`", "")
                if t.startswith("<") and t.endswith(">"):
                    t = t[1:-1]
                out.append("\\pard\\sa80\\f0\\fs17\\cf1 %s\\par" % esc(t))
    else:
        # one paragraph for the whole text, its lines kept with \\line: rich-text boxes load
        # this far faster than a paragraph a line
        lines = [esc(line.rstrip()) for line in text.splitlines()]
        out.append("\\pard\\sa0\\f1\\fs15\\cf1 " + "\\line\n".join(lines) + "\\par")
    return out


def page(intro, parts):
    doc = [
        "{\\rtf1\\ansi\\ansicpg1252\\deff0\\uc1",
        "{\\fonttbl{\\f0\\fswiss\\fcharset0 Segoe UI;}{\\f1\\fmodern\\fcharset0 Consolas;}}",
        "{\\colortbl ;\\red27\\green26\\blue27;\\red95\\green96\\blue100;\\red216\\green67\\blue26;}",
    ]
    if intro:
        doc.append("\\pard\\sa160\\f0\\fs18\\cf2 %s\\par" % esc(intro))
    for i, (heading, files) in enumerate(parts):
        before = "\\sb60" if i == 0 and not intro else "\\sb280"
        doc.append("\\pard%s\\sa120\\keepn\\f0\\fs22\\b\\cf3 \\u9632?\\cf1  %s\\b0\\par" % (before, esc(heading)))
        for f in files:
            with open(f, encoding="utf-8") as fh:
                doc.extend(body(fh.read().replace("\r\n", "\n").strip("\n")))
            doc.append("\\pard\\sa80\\par")
    doc.append("}")
    return "\n".join(doc) + "\n"


def main(argv):
    if len(argv) < 4 or "--part" not in argv:
        sys.exit(__doc__.strip())
    out, args = argv[1], argv[2:]
    intro, parts = "", []
    while args:
        a = args.pop(0)
        if a == "--intro":
            intro = args.pop(0)
        elif a == "--part":
            parts.append((args.pop(0), []))
        elif parts:
            parts[-1][1].append(a)
        else:
            sys.exit(f"{a}: a file needs a --part before it")
    if any(not files for _, files in parts):
        sys.exit("every --part needs at least one file")
    with open(out, "w", encoding="ascii", newline="\r\n") as f:
        f.write(page(intro, parts))
    print(f"{len(parts)} parts -> {out}")


if __name__ == "__main__":
    main(sys.argv)
