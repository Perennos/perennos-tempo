# Perennos Tempo as an MSIX package (free, no purchased certificate)

An MSIX is the modern Windows app package: one-click install, clean uninstall, and (from the Microsoft Store) automatic
updates. This folder builds one. It is free in both of its two uses:

| Who it is for | How it is signed | What people see |
| --- | --- | --- |
| **Everyone else** | The **Microsoft Store signs it** after certification. You upload it unsigned. | Install button, no SmartScreen warning, publisher shown, automatic updates |
| **Your own machines** | **Your own free certificate** "Perennos" (self-signed), trusted only on machines where you add it | Double-click to install (no Store needed); you copy updates by hand |

The old Inno Setup `.exe` (`../README.md`) stays as an unsigned direct-download fallback.

Why not a certificate from a company? A normal code-signing certificate costs money every year. The Microsoft Store
is the free way to get a package that every Windows PC trusts, because Microsoft signs it. (It signs MSIX packages only,
never `.exe` or `.msi` installers.) SignPath's free signing for open source is not available: Perennos Tempo is under
PolyForm Shield, which is not an OSI-approved open-source licence.

## What is in this folder

| File | What it is |
| --- | --- |
| `AppxManifest.xml` | The package's manifest, as a template (it has placeholders; the build fills them) |
| `Assets/` | Every picture the manifest names, at every size, as PNG, drawn from the Perennos mark |
| `make_assets.py` | Draws `Assets/`, `../store/art/` and the program icons (Pillow). Run again only if the mark changes |
| `check_manifest.py` | Checks the manifest and the pictures without Windows |
| `build-msix.ps1` | Builds the package. `-Store` or `-SignWithMyCertificate` |
| `new-perennos-cert.ps1` | Makes your free certificate (once, on your main machine) |
| `trust-perennos-cert.ps1` | Makes a machine trust it (once per machine, needs administrator) |
| `icons/` | Icons for the agent and Vault, to be placed in those crates (see the report) |

What the package contains: the programs in `bin\` (the launcher, the agent, and whichever of Vault, Studio, Room, and in
the full edition the Field and the `tempo` command are in the build), the legal texts, the pictures and the manifest.

What it gives the user:

- Start menu: **Perennos Tempo**, **Tempo Vault**, **Tempo Room** (each runs `tempo-launch.exe`, with a parameter for the last two).
- Command names that work in any terminal with no PATH change: `tempo-launch`, `tempo-local-agent`, `tempo-vault`,
  `tempo-room`, `tempo-studio` (and `tempo` in the full edition).
- A startup entry, **off** until the user turns "Perennos Tempo" on in Settings, Apps, Startup (or Task Manager, Startup).
  It starts Tempo in the background with nothing opened.
- A web link `perennos-tempo:` (open, agent, vault, room, field) for the setup page's buttons.
- Windows 10 version 2004 (build 19041) and up, 64-bit.

Everything the manifest uses is from Microsoft's published schemas (namespaces are listed at the top of
`AppxManifest.xml`; the reference is https://learn.microsoft.com/uwp/schemas/appxpackage/uapmanifestschema/schema-root).

### Where Tempo's data lives (important)

A normal MSIX app has its writes under AppData quietly redirected into a private copy that Windows deletes when the app
is uninstalled. Tempo must not work that way: its data folder `%LOCALAPPDATA%\Perennos Tempo` is shared with the
terminal commands and must survive an uninstall. So the manifest turns file and registry write virtualization off
(`desktop6:FileSystemWriteVirtualization` and `RegistryWriteVirtualization` set to `disabled`) and asks for the restricted
capability `unvirtualizedResources`. The notes app's folder (`Documents\Tempo Vault`) is not virtualized by Windows
anyway. The Store asks you to justify restricted capabilities: the text is in `../store/listing.md` ("Notes for
certification").

## One time: what you need on the Windows machine that builds

- Rust (https://rustup.rs) with the MSVC toolchain, as for the Inno build (`../README.md`, "On Windows"). Python 3 helps
  (the icon and the third-party notices) but is optional.
- The free **Windows SDK**: it holds `makeappx.exe` (makes the package), `makepri.exe` and `signtool.exe` (signs).
  Install it with `winget install Microsoft.WindowsSDK.10.0.26100` (check the exact id with `winget search Microsoft.WindowsSDK`)
  or from https://developer.microsoft.com/windows/downloads/windows-sdk/ (keep "Windows SDK Signing Tools for Desktop
  Apps" ticked). The build script says exactly this if it cannot find them.

## Path 1: for everyone, through the Microsoft Store

You do the Partner Center steps yourself (they need your login). The whole list, with the text to paste, is in
`../store/checklist.md`. In short:

1. Create the free individual developer account at https://partner.microsoft.com/dashboard.
2. Reserve the name **Perennos Tempo**. Open Product management, Product identity, and copy three values:
   `Package/Identity/Name`, `Package/Identity/Publisher` (looks like `CN=...`) and `Package/Properties/PublisherDisplayName`.
3. Build the package, in PowerShell from the repository (the Store gets the source-available edition):

   ```powershell
   .\installer\windows\msix\build-msix.ps1 -Store `
     -IdentityName "<the Name>" -Publisher "<the Publisher>" -PublisherDisplayName "<the PublisherDisplayName>" `
     -Apps vault,studio,room
   ```

   It makes `installer\windows\build\msix-out\PerennosTempo-<version>-x64-source-available.msix` and a `.msixupload`.
   Both are **unsigned on purpose**: do not try to install them on a PC. If Partner Center complains that the version starts
   with 0, add `-Version 1.0.0.0`.
4. Upload one of those files on the app's Packages page, fill the listing from `../store/listing.md`, host
   `../store/privacy.html` publicly, and submit (`../store/checklist.md`).
5. After certification, Microsoft signs it and publishes it. Every new version: raise the version, build, upload again.

## Path 2: for your own machines, with your free certificate

Do this on the machine you will build and sign on (your main one).

1. Make the certificate, once:

   ```powershell
   .\installer\windows\msix\new-perennos-cert.ps1
   ```

   It makes a certificate named `CN=Perennos`, valid 3 years, in your own certificate store, and writes only the
   **public** half to `installer\windows\build\perennos-tempo-signing.cer`.

   *The trade-off, in plain words:* this certificate is not known to the rest of the world; only machines where you
   import it trust it. Its secret key is made non-exportable: it stays in this machine's store and can never be copied
   out, even by you. That is the safe choice. The price is that **this one machine does all the signing**: build here,
   then copy the finished signed `.msix` to your other machines. If this machine is wiped, the key is gone; you make a
   new certificate and trust it again on each machine. Never copy a `.pfx` or the key anywhere: whoever has it can make
   packages that install on your machines without any warning.

2. Trust it on this machine (once; needs **administrator**: right-click PowerShell, "Run as administrator"):

   ```powershell
   .\installer\windows\msix\trust-perennos-cert.ps1
   ```

3. Build and sign (the full edition by default: the Field and `tempo` included):

   ```powershell
   .\installer\windows\msix\build-msix.ps1 -SignWithMyCertificate
   ```

   The result is `installer\windows\build\msix-out\PerennosTempo-<version>-x64-signed.msix`. The package's publisher is
   taken from the certificate, so they always match. It is timestamped with a free timestamp server
   (`http://timestamp.digicert.com`, then two others if that fails), so the signature keeps working after the
   certificate expires. **Do not upload this package anywhere public**: it holds the private parts.

4. Install it here: double-click the file, or `Add-AppxPackage <path>`.
5. On each other machine: copy the signed `.msix` and the public `.cer` there, run
   `trust-perennos-cert.ps1 -Cer <path to the .cer>` once as administrator, then install the `.msix`.
   An update is the same file with a higher version (`Add-AppxPackage` on a newer version updates in place; your data stays).

Windows in S mode cannot install packages from outside the Store. Everything else can (Settings, Apps, Advanced app
settings should not say "Microsoft Store only").

If you already installed Perennos Tempo with the Inno `.exe`, uninstall that first (your notes and data stay), so the
two do not fight over the command names and ports. Do not put the Store version and your own signed version on the same
machine either.

To remove it: Settings, Apps, Perennos Tempo, Uninstall, or `Get-AppxPackage *PerennosTempo* | Remove-AppxPackage`.
Your data folder and notes are left in place. To stop trusting the certificate: `trust-perennos-cert.ps1 -Cer <the .cer> -Remove`.

## If the Store says no

The Store may refuse `unvirtualizedResources` (a restricted capability) or ask for more detail. Plans, in order:

1. **Answer with the justification** in `../store/listing.md`. It is true and specific, and restricted capabilities are approved
   routinely when the reason is clear.
2. **Narrow the request.** Microsoft's manifest schema also allows excluding one named folder from virtualization instead
   of switching it off for everything (the "virtualization" extension with `ExcludedDirectories`, which also needs
   `unvirtualizedResources`). That is a smaller ask: only `%LOCALAPPDATA%\Perennos Tempo`. Not built here: check the exact
   element names in Microsoft's docs ("Package a desktop app: flexible virtualization") before using them. Untested.
3. **Give up the capability** (it still ships): `build-msix.ps1 -Store -WithoutUnvirtualized ...`. What changes:
   Windows then redirects Tempo's writes under AppData into a private folder inside the package's own storage. Tempo
   works, but its data (conversations, memory, rooms) is deleted when the app is uninstalled, and programs started from a
   terminal outside the package do not see the same folder. To avoid both, pin the home outside AppData yourself (the app does not do it for you): `setx TEMPO_HOME "%USERPROFILE%\Perennos Tempo"`. Every Tempo program already obeys
   `TEMPO_HOME` (`docs/TEMPO-HOME.md`). Notes in `Documents\Tempo Vault` are never affected.
4. The Inno `.exe` stays available as a download either way.

## Problems

| What you see | What it means |
| --- | --- |
| "makeappx.exe was not found" | Install the Windows SDK (see above) |
| Install fails, 0x800B0109 ("root certificate not trusted") | This machine has not run `trust-perennos-cert.ps1` yet, or imported the wrong `.cer` |
| Install fails, 0x8007000B | The package's Publisher does not match the certificate's Subject. Use `-SignWithMyCertificate`, which copies it for you |
| `signtool verify /pa` warns after signing | Expected until the machine trusts the certificate (see above) |
| Install says the package is not signed | You built with `-Store`; that package is for upload only |
| Terminal does not know `tempo-launch` | Open a new terminal window after installing. Aliases are in `%LOCALAPPDATA%\Microsoft\WindowsApps` |

## What was not tested (needs Windows)

Everything in this folder was checked for syntax and well-formedness on Linux only. Not run anywhere yet:
`makeappx pack`, `makepri`, `signtool sign` and `verify`, the certificate scripts, installing the package, the aliases,
the startup task, the `perennos-tempo:` link, the Start tiles with parameters, and Store certification. The first Windows
run may need small fixes to `build-msix.ps1`; report the message it prints. The launcher already has what the package
depends on (sign-in start, the web link, the scheduled task inside the package).
