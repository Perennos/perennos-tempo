# Controls bundle

The controls bundle carries only `tempo-launch`, `tempo-local-agent`, version and target metadata, and the three release notices. It supplies those files as inputs to an approved node bootstrap or maintenance procedure. Extracting it alone does not enroll a node, install files, or persist changes.

It does not include the full installer, models, settings, startup scripts, secrets, or node data. The packager reads the staged files, never runs them, and leaves them unchanged.

## Files

Each release has one archive and a SHA-256 sidecar:

```text
perennos-tempo-controls-VERSION-TARGET.tar.gz
perennos-tempo-controls-VERSION-TARGET.tar.gz.sha256
```

The archive contains one folder with this layout:

```text
VERSION
TARGET
EDITION
CONTROLS.json
bin/tempo-launch[.exe]
bin/tempo-local-agent[.exe]
legal/LICENSE-PolyForm-Shield-1.0.0.txt
legal/NOTICE.txt
legal/THIRD-PARTY-NOTICES.txt
```

`EDITION` is `source-available`. `CONTROLS.json` uses schema `1` and lists each of the eight other files by relative path, byte count, and SHA-256 digest. It does not list itself. Members are sorted and use fixed timestamps and ownership so repeated builds from identical inputs produce identical archives.

Current release targets are `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`. `x86_64-pc-windows-gnu` is accepted for controlled builds.

## Check and extract

Linux:

```sh
sha256sum -c perennos-tempo-controls-VERSION-TARGET.tar.gz.sha256
tar -xzf perennos-tempo-controls-VERSION-TARGET.tar.gz
```

macOS:

```sh
shasum -a 256 -c perennos-tempo-controls-VERSION-TARGET.tar.gz.sha256
tar -xzf perennos-tempo-controls-VERSION-TARGET.tar.gz
```

Windows PowerShell:

```powershell
$archive = 'perennos-tempo-controls-VERSION-TARGET.tar.gz'
$expected = (Get-Content "$archive.sha256" -Raw).Trim().Split(' ')[0]
$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw 'The controls archive checksum does not match.' }
tar.exe -xzf $archive
```

These checks use the system's checksum tools and `tar`; the archive does not need Python or another unpacker on the node. After extraction, use the approved node bootstrap or maintenance procedure to place the two controls in their intended locations.
