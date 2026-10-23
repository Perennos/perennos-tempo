# Deployment options

The public core runs under one user on one local machine. Check the matching GitHub Release for its tested Windows environment and acceptance results. Multi-user, office-server and LAN deployments are outside this release.

## Local deployment

Run the desktop application or headless API under one OS account with its own `TEMPO_HOME`, local engine, and ledger. The HTTP gateway binds to `127.0.0.1`. The MCP adapter is a local stdio process that calls that loopback gateway.

For separate owners, use separate OS accounts or machines. Separate `TEMPO_HOME` folders alone do not isolate OS processes or files. Keep application, data, and model folders separate.

## Not included

This release has no shared desk, tenant authorization service, approvals UI, office/customer profile packs, vertical-specific workspaces, or LAN authentication. Do not expose the single-user bearer-token API to a network interface or put it behind a public reverse proxy. Do not add firewall rules, port forwarding, public listeners, or automatic discovery.

A shared-office service would need separately implemented and independently reviewed authentication, tenant isolation, session revocation, concurrency/resource admission, backup recovery, and token handling. None of those controls are established by this source package.
