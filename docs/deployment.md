# Deployment

This is the personal self-hosted path: the bridge runs next to the MeshCore
sidecar (or reaches it through a tunnel), and `bbctl proxy` carries Beeper's
appservice websocket to the local HTTP server.

## Files

```
~/.config/meshcore-mautrix/config.toml
~/.config/meshcore-mautrix/secrets/registration.json   # includes local url
~/.config/meshcore-mautrix/secrets/database-key
~/.config/meshcore-mautrix/secrets/meshcore-token
```

All files mode `0600`. The database is created `0600` and locked against a
second process.

## systemd user units

`meshcore-mautrix.service`:

```ini
[Unit]
Description=MeshCore to Matrix bridge
After=network-online.target

[Service]
Type=simple
WorkingDirectory=%h/.config/meshcore-mautrix
Environment=MESHCORE_MAUTRIX_KEY_FILE=%h/.config/meshcore-mautrix/secrets/database-key
Environment=MESHCORE_TOKEN_FILE=%h/.config/meshcore-mautrix/secrets/meshcore-token
ExecStart=%h/.local/bin/meshcore-mautrix --config %h/.config/meshcore-mautrix/config.toml run
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

`bbctl-proxy.service`:

```ini
[Unit]
Description=Beeper appservice websocket proxy
After=network-online.target meshcore-mautrix.service

[Service]
Type=simple
ExecStart=/path/to/bbctl proxy -r %h/.config/meshcore-mautrix/secrets/registration.json
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

## Remote sidecar

If the sidecar is on another host, bind an SSH tunnel locally:

```sh
ssh -N -L 8787:127.0.0.1:8787 radioman-pi
```

and keep `meshcore.url = "http://127.0.0.1:8787"`. Do not expose the sidecar's
bearer-token API on a public interface.

## Failure modes

- `401` from the sidecar: wrong or rotated token.
- `403` from the sidecar: channel/contact is not in its `allowed_channels` /
  `allowed_contacts`.
- `/readyz` false: the poller has not completed a successful page yet.
- Beeper sees no rooms: check `bbctl proxy` is connected and the registration
  `url` points at the bridge's `listen` address.
- Messages send but no replies: the sidecar's radio may be offline; `check`
  reports the sidecar health and radio name.

## Upgrade

Replace the binary and restart `meshcore-mautrix`; the cursor and bindings are
durable. Keep the same database key and registration. Never re-register just to
fix a websocket issue: deletion removes the bridge's rooms.
