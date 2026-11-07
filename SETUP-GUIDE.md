# Perennos Tempo setup guide

## Install the apps (version 2.0.0)

Open the [latest release](https://github.com/Perennos/perennos-tempo/releases/latest) and download the file for your computer:

| Computer | File |
|---|---|
| Windows 10 and 11 (x64) | `PerennosTempo-Setup-2.0.0-x64-source-available.exe` |
| macOS, Apple silicon | `perennos-tempo-source-available-2.0.0-aarch64-apple-darwin.tar.gz` |
| Linux (x86_64) | `perennos-tempo-source-available-2.0.0-x86_64-unknown-linux-gnu.tar.gz` |
| Debian and Ubuntu (x86_64) | `perennos-tempo-source-available_2.0.0-1_amd64.deb` |

**Windows.** Download the Setup file and double-click it. The installer is not code-signed, so Windows may show "Windows protected your PC": click **More info**, then **Run anyway**. Keep SmartScreen, antivirus and UAC enabled; this is the one prompt to click through. Follow the installer's steps. It adds Start-menu entries and `tempo-launch` to your PATH. The installer's optional "Set up models" step downloads several gigabytes, so leave it unticked if you would rather choose later with `tempo-launch setup`.

**macOS and Linux.** Unpack the archive and run the installer:

```sh
tar xzf perennos-tempo-source-available-*.tar.gz && cd perennos-tempo-source-available-*/ && ./install.sh
```

On Debian and Ubuntu, `sudo apt install ./perennos-tempo-source-available_2.0.0-1_amd64.deb` works too.

**Check a download** (optional): compare its SHA-256 with `SHA256SUMS.txt` on the release page, or run `gh attestation verify <file> --repo Perennos/perennos-tempo`, which checks that this repository's release workflow built the file.

## First start and network use

Run `tempo-launch setup`. It checks the memory, graphics card and disk of this computer, recommends models that fit, and shows the exact plan with the free space before anything large is fetched. Nothing is downloaded until you say yes. When no Ollama is installed it can fetch a pinned one, checked against its published SHA-256. Models and Ollama need network access and several gigabytes of free storage.

## Prepare a private data folder

For manual source use, run from the extracted package folder with Python 3.12 or newer. Use a data folder owned by the current OS user and separate it from the application and model folders. The bootstrap refuses non-empty unowned folders.

If you installed the gateway with its own Windows desktop installer (`PerennosTempo-Setup-unsigned.exe`, in the [v1.0.0 release](https://github.com/Perennos/perennos-tempo/releases/tag/v1.0.0)), use its native setup and review the exact download-size and free-space plan before consenting. The default setup is local; do not pass hosted-provider options unless you have deliberately reviewed the provider, privacy policy, credentials, and cost cap.

The gateway's own Windows installer (v1.0.0) does not put Python on PATH. For the commands below, replace `python tempo.py` with `& "<app-folder>\runtime\venv\Scripts\python.exe" -I -B "<app-folder>\tempo.py"` in PowerShell. Use the `data_home` and `gateway_port` values from `<app-folder>\desktop-install-receipt.json`, rather than assuming port 8765 is free. Closing the chat leaves its local API available; a second gateway on the same port is unnecessary.

## Desktop chat

With the local gateway configured and running, open the desktop client:

```powershell
python tempo.py gui --home "<data-folder>" --port 8765
```

The GUI does not install a model. Closing the window does not necessarily stop separately running services.

## Headless API

Start the local API in a foreground terminal:

```powershell
python tempo.py headless --home "<data-folder>" --host 127.0.0.1 --port 8765 --device cpu
```

Keep the host at `127.0.0.1`. Stop the process with Ctrl+C. `--device auto` is the normal resource-selection mode; `--device cpu` explicitly requests CPU execution. GPU use requires a correctly configured lease gate. A configured booking or capacity value is not itself a reservation.

The API exposes `GET /health`, authenticated `GET /v1/models`, and authenticated `POST /v1/chat/completions` at `http://127.0.0.1:8765`. A chat response includes an `x-tempo-receipt-id` header when a receipt is written. A health response alone is not a successful model-response check.

### API token

The gateway stores its token at `<data-folder>/ledger/gateway-token` (the folder keeps this older name; it holds your run records). Keep the data folder under the same OS account that runs the gateway. Never print, paste, screenshot, commit, or place the token in a command-line argument or shared client configuration. A compatible local client should read the file inside its own process and set the bearer header without displaying the value. The MCP adapter does this itself.

Example for a local Python client, using a path you choose and without printing the token:

```python
import json
import os
from pathlib import Path
from urllib.request import Request, urlopen

home = Path(os.environ["TEMPO_HOME"])
token = (home / "ledger" / "gateway-token").read_text(encoding="utf-8").strip()
body = json.dumps({
    "model": "tempo/private",
    "messages": [{"role": "user", "content": "Draft a short note about a fictional event."}],
    "metadata": {"task_class": "draft", "privacy_class": "private"},
}).encode("utf-8")
request = Request(
    "http://127.0.0.1:8765/v1/chat/completions",
    data=body,
    headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    method="POST",
)
with urlopen(request, timeout=120) as response:
    result = json.load(response)
```

The request example is documentation, not an acceptance result. Do not use real private or customer data during an unqualified evaluation.

## MCP client

The bridge uses stdio JSON-RPC and the authenticated loopback API. It exposes `ask`, `listmodels`, and `status`; it does not open another network listener or copy the token into client configuration.

Generate a secret-free client entry after the package and data folder are in their final locations:

```powershell
python tempo.py mcp-config --home "<data-folder>" --port 8765
```

The printed JSON contains absolute local paths, not the bearer token. Keep it local, paste it into the MCP client's user-level configuration, then restart or reload that client. Regenerate it if the package moves. Start the headless API before using `ask`.

For direct launch:

```powershell
python tempo.py mcp --home "<data-folder>" --host 127.0.0.1 --port 8765
```

## Linux source path

The package has a POSIX wrapper, but Linux setup and desktop behavior are **Untested**. It is not a Linux installer or GUI release. The headless command requires Python, pinned dependencies, a prepared data folder, and a local engine endpoint.

```sh
sh tempo.sh headless --home "<data-folder>" --host 127.0.0.1 --port 8765 --device cpu
```

Keep the endpoint on loopback. Do not expose it through Docker, a firewall rule, reverse proxy, port forwarding, or LAN listener.

## Data and removal

Conversation data, setup settings, profiles, receipts, and run records are stored under the selected data folder. That folder is a namespace, not an OS security boundary. This core has no automated uninstaller or verified recovery path; back up the data folder before manual removal. Keep application files, private data, and model files in separate folders.

For current profile behavior, see [PROFILES-README.md](PROFILES-README.md). For deployment limits, see [DEPLOYMENT-OPTIONS.md](DEPLOYMENT-OPTIONS.md). The public core does not include office/customer packs, desks, approvals UX, Cayman verticals, or the private resident adapter.
