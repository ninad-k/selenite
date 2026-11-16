# Headless server

`selenite-server` serves a vault to AI agents over MCP (Streamable HTTP) on a machine with no
desktop session: a home server, a VPS, a container. It exposes the same tools as the local stdio
server (ADR-007): everything the app can do except delete, with an audit log. It also keeps the
vault in sync with its cloud remotes in the background.

## Run

```bash
selenite-server --vault /srv/vault                    # 127.0.0.1:8787, sync every 300 s
selenite-server --vault /srv/vault --listen 127.0.0.1:9000 --sync-interval 600
selenite-server --vault /srv/vault --no-sync
selenite-server --print-token                         # show the access token and exit
```

| Flag | Meaning |
|---|---|
| `--vault PATH` | Vault folder (required except with `--print-token`). |
| `--listen ADDR` | Default `127.0.0.1:8787`. A non-loopback address logs a warning (see below). |
| `--sync-interval SECS` | Default 300, minimum 10. |
| `--no-sync` | Never sync, even if the vault has remotes. |
| `--allowed-origin ORIGIN` | Extra accepted `Origin`, repeatable. Loopback origins are always accepted. |
| `--allowed-host HOST` | Extra accepted `Host`, repeatable (see "Host and Origin checks"). |
| `--print-token` | Print the token to stdout and exit. |

Logs go to stderr; `RUST_LOG` sets the level (default `info`). They contain paths, remote ids and
counts, never the token, the `Authorization` header or vault content. Ctrl+C and SIGTERM stop
the server gracefully.

Endpoints: `POST/GET/DELETE /mcp` (MCP, token required) and `GET /healthz` (returns `ok`, no
token, nothing about the vault).

## Access token

Every request to `/mcp` must carry `Authorization: Bearer <token>`. The token is compared in
constant time (both sides are hashed with SHA-256 first). A missing or wrong token gets a bare
`401` with no body.

The token comes from, in order:

1. the `SELENITE_SERVER_TOKEN` environment variable (16 or more characters, no whitespace);
2. a token file outside the vault: `%APPDATA%\Selenite\server-token` on Windows,
   `$XDG_CONFIG_HOME/selenite/server-token` or `~/.config/selenite/server-token` elsewhere.

On first run the file is created with 32 random bytes (base64url) and mode `0600` on Unix. The
server prints the file's path, not the token. Run `selenite-server --print-token` to read it,
and delete the file to rotate (the next start creates a new one). The token never lives in the
vault, so syncing the vault never copies it (ADR-004).

## Host and Origin checks

Browsers can be tricked into calling a local server (DNS rebinding). The server rejects with
`403`:

- a request with an `Origin` header that is not loopback (`localhost`, `127.0.0.1`, `[::1]`)
  and not listed with `--allowed-origin`. Requests without `Origin` (agents, curl) are fine;
- a request whose `Host` header is not loopback or listed with `--allowed-host`, when the
  server listens on loopback. When it listens on a non-loopback address and no `--allowed-host`
  is given, the `Host` check is off (the token is the gate); give `--allowed-host
  notes.example.com` to enforce it.

## TLS and reverse proxy

The server speaks plain HTTP. Binding a non-loopback address prints a warning: put a
TLS-terminating reverse proxy in front, or the bearer token crosses the network in clear text.
Keep the server itself on loopback (or an internal network) and let the proxy face the internet.

Caddy:

```caddyfile
notes.example.com {
    reverse_proxy 127.0.0.1:8787 {
        flush_interval -1     # do not buffer server-sent events
    }
}
```

Run with `--allowed-host notes.example.com --allowed-origin https://notes.example.com` if you
want the proxy's name enforced. Caddy passes `Host` through unchanged.

## Docker

```bash
docker build -t selenite-server .
docker run -d --name selenite -p 127.0.0.1:8787:8787 \
  -v /srv/vault:/vault -e SELENITE_SERVER_TOKEN="$(openssl rand -base64 32)" \
  selenite-server
```

The image runs as a non-root user (uid 10001; make the mounted folder writable by it), binds
`0.0.0.0:8787` inside the container and stores the vault on the `/vault` volume. Publish the
port to loopback only and terminate TLS in a proxy as above. Pass extra flags after the image
name, for example `--no-sync`.

## Client configuration

An MCP client that supports Streamable HTTP with headers:

```json
{
  "mcpServers": {
    "selenite": {
      "url": "https://notes.example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Locally, without a proxy: `http://127.0.0.1:8787/mcp`.

The tools, resources and prompts are the ones in `docs/mcp.md`. Audit entries
(`.selenite/audit/mcp-YYYY-MM.jsonl`) gain an optional `transport` field: `http` for calls that
came through this server, `stdio` for the local server. Older entries without the field remain
valid.

## Sync

If `.selenite/remotes.yaml` lists remotes, the server syncs each one at start and then every
`--sync-interval` seconds (the file is re-read each round, so remotes added later are picked
up). After a round that reached a remote it refreshes the index; the engine lock is held only
for that refresh, so MCP calls are not blocked by network transfers. Logs hold remote ids and
counts only.

Refresh tokens are read from the OS keychain, as in the desktop app. A headless Linux host
usually has no keychain, so the server logs one warning per remote (it does not repeat every
round) and keeps serving MCP:

> sign in on a desktop machine and copy the refresh token into the keychain, or run without sync

Sign-in needs a browser, so it always happens on a desktop machine (`selenite remote add ...`).
If no keychain is available on the server, run with `--no-sync` and sync from the desktop.

## Limits

- One vault per server process, one token for all clients.
- Plain HTTP only; TLS, rate limiting and IP allow-lists belong to the reverse proxy.
- Reminders are not delivered: a headless host has no OS notifications. Agents can still read
  what is due with the `get_agenda` tool.
- No delete over MCP (by design); `archive_page` and `archive_task` move items to the vault
  trash.
- Sessions are held in memory; a server restart ends them and clients re-initialise.
