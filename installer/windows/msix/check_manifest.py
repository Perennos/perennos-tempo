#!/usr/bin/env python3
"""Checks the MSIX manifest template without Windows: renders it the way build-msix.ps1 does (both editions, with
and without the virtualization block), then checks it is well-formed, that every picture it names exists at its
size, and the rules Windows enforces that can be seen from the text. Standard library, plus Pillow for sizes.

    python3 check_manifest.py            exit 0 = fine

It cannot replace makeappx and Microsoft's own validation (those need Windows): see README.md, "What was not tested".
"""
import os
import re
import sys
from xml.dom import minidom

HERE = os.path.dirname(os.path.abspath(__file__))
try:
    from PIL import Image
except ImportError:
    Image = None

NS = {
    "uap": "http://schemas.microsoft.com/appx/manifest/uap/windows10",
    "uap3": "http://schemas.microsoft.com/appx/manifest/uap/windows10/3",
    "uap10": "http://schemas.microsoft.com/appx/manifest/uap/windows10/10",
    "desktop": "http://schemas.microsoft.com/appx/manifest/desktop/windows10",
    "desktop6": "http://schemas.microsoft.com/appx/manifest/desktop/windows10/6",
    "rescap": "http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities",
}
FOUNDATION = "http://schemas.microsoft.com/appx/manifest/foundation/windows10"
SIZES = {"Square44x44Logo": (44, 44), "Square71x71Logo": (71, 71), "Square150x150Logo": (150, 150),
         "Wide310x150Logo": (310, 150), "Square310x310Logo": (310, 310), "StoreLogo": (50, 50)}
SCALES = [100, 125, 150, 200, 400]
TARGETS = [16, 24, 32, 48, 256]


def render(text, apps, virt):
    """The same rule as build-msix.ps1: drop blocks of apps that are not in the package, and the virt block if not wanted."""
    text = text.replace("@@IDENTITY_NAME@@", "Perennos.PerennosTempo").replace("@@PUBLISHER@@", "CN=Perennos")
    text = text.replace("@@VERSION@@", "0.1.0.0").replace("@@PUBLISHER_DISPLAY_NAME@@", "Perennos")

    def block(m):
        key = m.group(1)
        keep = virt if key == "virt" else key[4:] in apps
        return m.group(2) if keep else ""

    text = re.sub(r"[ \t]*<!--@(app:\w+|virt)-->\r?\n(.*?)[ \t]*<!--@/\1-->[ \t]*\r?\n", block, text, flags=re.S)
    return text


def problems(xml, label):
    out = []
    if "@@" in xml:
        out.append(f"{label}: a placeholder is left")
    try:
        doc = minidom.parseString(xml.encode("utf-8"))
    except Exception as e:  # noqa: BLE001
        return [f"{label}: not well-formed: {e}"]
    pkg = doc.documentElement
    if pkg.namespaceURI != FOUNDATION or pkg.localName != "Package":
        out.append(f"{label}: root is not foundation Package")
    ignorable = pkg.getAttribute("IgnorableNamespaces").split()
    for prefix, uri in NS.items():
        if pkg.getAttribute("xmlns:" + prefix) != uri:
            out.append(f"{label}: xmlns:{prefix} is not {uri}")
        if prefix not in ignorable:
            out.append(f"{label}: {prefix} is not in IgnorableNamespaces")
    ident = doc.getElementsByTagName("Identity")[0]
    if not re.fullmatch(r"\d+\.\d+\.\d+\.\d+", ident.getAttribute("Version")):
        out.append(f"{label}: Version is not four numbers")
    if not re.fullmatch(r"[-A-Za-z0-9.]{3,50}", ident.getAttribute("Name")):
        out.append(f"{label}: Identity Name has characters Windows does not allow")
    # applications
    ids, aliases, exes = set(), set(), set()
    for app in doc.getElementsByTagName("Application"):
        i = app.getAttribute("Id")
        if not re.fullmatch(r"[A-Za-z][A-Za-z0-9.]{0,63}", i) or i in ids:
            out.append(f"{label}: Application Id {i!r} is bad or repeated")
        ids.add(i)
        exe = app.getAttribute("Executable")
        exes.add(exe)
        if app.hasAttribute("EntryPoint") and app.hasAttributeNS(NS["uap10"], "Parameters"):
            out.append(f"{label}: {i}: EntryPoint and uap10 attributes together")
        if app.hasAttributeNS(NS["uap10"], "Parameters") and not (
            app.getAttributeNS(NS["uap10"], "RuntimeBehavior") == "packagedClassicApp"
            and app.getAttributeNS(NS["uap10"], "TrustLevel") == "mediumIL"
        ):
            out.append(f"{label}: {i}: uap10:Parameters needs packagedClassicApp and mediumIL")
        for ext in app.getElementsByTagName("*"):
            if ext.localName == "Extension" and ext.getAttribute("Executable") not in ("", exe):
                out.append(f"{label}: {i}: an extension names another executable than the application")
        for a in app.getElementsByTagNameNS(NS["desktop"], "ExecutionAlias"):
            al = a.getAttribute("Alias")
            if not al.endswith(".exe") or al in aliases:
                out.append(f"{label}: alias {al!r} is not *.exe or is repeated")
            aliases.add(al)
        for st in app.getElementsByTagNameNS(NS["desktop"], "StartupTask"):
            if st.getAttribute("Enabled") != "false" or not st.getAttribute("TaskId"):
                out.append(f"{label}: the startup task must be off by default and have a TaskId")
    for p in doc.getElementsByTagNameNS(NS["uap"], "Protocol"):
        if not re.fullmatch(r"[a-z][a-z0-9.+-]*", p.getAttribute("Name")):
            out.append(f"{label}: protocol name {p.getAttribute('Name')!r} is not allowed")
    caps = {c.getAttribute("Name") for c in doc.getElementsByTagNameNS(NS["rescap"], "Capability")}
    if "runFullTrust" not in caps:
        out.append(f"{label}: runFullTrust is missing")
    virt = bool(doc.getElementsByTagNameNS(NS["desktop6"], "FileSystemWriteVirtualization"))
    if virt != ("unvirtualizedResources" in caps):
        out.append(f"{label}: write virtualization and the unvirtualizedResources capability must go together")
    for e in exes:
        if not e.startswith("bin\\") or not e.endswith(".exe"):
            out.append(f"{label}: executable {e!r} is not bin\\*.exe")
    # pictures
    paths = set()
    for el in doc.getElementsByTagName("*"):
        for attr in ("Square150x150Logo", "Square44x44Logo", "Wide310x150Logo", "Square310x310Logo", "Square71x71Logo"):
            if el.hasAttribute(attr):
                paths.add(el.getAttribute(attr))
    for logo in doc.getElementsByTagName("Logo"):
        paths.add(logo.firstChild.data.strip())
    for p in sorted(paths):
        m = re.fullmatch(r"Assets\\(\w+)\.png", p)
        if not m or m.group(1) not in SIZES:
            out.append(f"{label}: picture {p!r} is not an Assets\\<known name>.png")
            continue
        base = m.group(1)
        w, h = SIZES[base]
        want = [(f"{base}.png", (w, h))] + [(f"{base}.scale-{s}.png", (w * s // 100, h * s // 100)) for s in SCALES]
        if base == "Square44x44Logo":
            for t in TARGETS:
                for suf in ("", "_altform-unplated", "_altform-lightunplated"):
                    want.append((f"{base}.targetsize-{t}{suf}.png", (t, t)))
        for name, size in want:
            f = os.path.join(HERE, "Assets", name)
            if not os.path.isfile(f):
                out.append(f"{label}: missing {name}")
            elif Image is not None and Image.open(f).size != size:
                out.append(f"{label}: {name} is {Image.open(f).size}, want {size}")
    return out


def main():
    text = open(os.path.join(HERE, "AppxManifest.xml"), encoding="utf-8").read()
    bad = []
    full = {"vault", "room", "studio", "cli"}
    for label, apps, virt in (("full", full, True), ("source-available", full - {"cli"}, True),
                              ("no-virtualization", full, False), ("vault-only", {"vault"}, True)):
        bad += problems(render(text, apps, virt), label)
    for b in sorted(set(bad)):
        print("PROBLEM:", b)
    print("manifest ok (4 variants)" if not bad else f"{len(set(bad))} problem(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
