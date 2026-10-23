# Perennos Tempo setup guide

Installation results belong to the matching [GitHub Release](https://github.com/Perennos/perennos-tempo/releases): check its tested Windows image, installer checksum, and individual acceptance results. An extracted folder or running process alone is not acceptance. Consumer Windows, removal, recovery and external-client reconnect remain unverified unless that release records their results.

## Requirements and network use

Download `PerennosTempo-Setup-unsigned.exe` from the matching [GitHub Release](https://github.com/Perennos/perennos-tempo/releases), verify its published SHA-256, open it, and click **Install and open**. Setup extracts its bundled app-local Python/Tk runtime; no existing Python installation is required or modified. The EXE is unsigned. Keep SmartScreen, antivirus, and UAC enabled.

First setup needs network access and several gigabytes of disk space for pinned packages, the app-owned Ollama runtime, a local `qwen3:4b` model, and pinned decision-head files. The source catalog indicates about 2.5 GB for the default engine; the decision-head fetcher describes about 1.7 GB for its model files. Package wheels and the Ollama archive add more. Exact bytes vary; setup shows the resolved plan and free-space checks. Model weights are not bundled.

Setup has two download-related confirmations. The first authorizes dependency resolution, which can contact package indexes and transfer candidate wheel data before exact totals are known (15-minute preflight and 2 GiB of app-local temporary data). The second shows resolved dependency, model and Ollama download sizes and available space before those downloads. The second prompt is not the first network contact.

## Prepare a private data folder

For manual source use, run from the extracted package folder with Python 3.12 or newer. Use a data folder owned by the current OS user and separate it from the application and model folders. The bootstrap refuses non-empty unowned folders.

For the Windows download, use the native setup above and review the exact download-size and free-space plan before consenting. The default setup is local; do not pass hosted-provider options unless you have deliberately reviewed the provider, privacy policy, credentials, and cost cap.

The Windows installer does not put Python on PATH. For the commands below, replace `python tempo.py` with `& "<app-folder>\runtime\venv\Scripts\python.exe" -I -B "<app-folder>\tempo.py"` in PowerShell. Use the `data_home` and `gateway_port` values from `<app-folder>\desktop-install-receipt.json`, rather than assuming port 8765 is free. Closing the chat leaves its local API available; a second gateway on the same port is unnecessary.

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

The gateway stores its token at `<data-folder>/ledger/gateway-token`. Keep the data folder under the same OS account that runs the gateway. Never print, paste, screenshot, commit, or place the token in a command-line argument or shared client configuration. A compatible local client should read the file inside its own process and set the bearer header without displaying the value. The MCP adapter does this itself.

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

Conversation data, setup settings, profiles, receipts, and ledgers are stored under the selected data folder. That folder is a namespace, not an OS security boundary. This core has no automated uninstaller or verified recovery path; back up the data folder before manual removal. Keep application files, private data, and model files in separate folders.

For current profile behavior, see [PROFILES-README.md](PROFILES-README.md). For deployment limits, see [DEPLOYMENT-OPTIONS.md](DEPLOYMENT-OPTIONS.md). The public core does not include office/customer packs, desks, approvals UX, Cayman verticals, or the private resident adapter.
