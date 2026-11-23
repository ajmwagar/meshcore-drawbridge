# MeshCore → Matrix (Mautrix / Beeper)

A small Rust application-service bridge that turns a MeshCore companion radio
into Matrix rooms. It is built on [Drawbridge](https://github.com/ajmwagar/drawbridge),
the shared bridge core extracted from `rakazo-beeper`.

The bridge talks to the **Radioman MeshCore sidecar** over its loopback HTTP API,
not to the radio over BLE. The sidecar remains the single owner of the companion;
this process only polls its durable message log and sends through its bounded,
allowlisted `/v1/send` endpoint.

```
MeshCore radio ──BLE──> radioman meshcore sidecar ──HTTP──> meshcore-drawbridge ──appservice──> Matrix/Beeper
                                                          │
                                                          └── SQLite: cursor, bindings, links, dedupe
```

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE).

## Current scope

- Public/group channels and direct contacts, **text only**
- One Matrix room per MeshCore `channel:<n>` or `contact:<pubkey-prefix>`
- Owner-only writes from Matrix; bridge ghosts send MeshCore traffic into rooms
- Durable cursor, idempotent Matrix transaction IDs, dedupe across restarts
- Unencrypted bridge rooms (no E2EE); the operator can read them
- No attachments, reactions or edits yet

## Requirements

- A running `radioman-meshcore` sidecar with `/v1/health`, `/v1/messages` and
  `/v1/send`
- A Beeper/Matrix appservice registration for the bridge
  (`bbctl register --json sh-meshcore`)
- A database key: 32 random bytes, base64
- The sidecar bearer token

## Setup

### 1. Register the bridge with Beeper

```sh
umask 077
mkdir -p secrets data
bbctl register --json sh-meshcore > secrets/registration.json
```

The response includes `registration`, `homeserver_url`, `homeserver_domain` and
`your_user_id`. Keep the whole file; do not regenerate a registration that
already has rooms.

For the outbound-websocket path, `bbctl proxy` forwards Beeper's websocket to a
local HTTP appservice. It needs a local URL in the registration:

```sh
jq '.registration.url = "http://127.0.0.1:8080"' \
  secrets/registration.json > secrets/registration.local.json
mv secrets/registration.local.json secrets/registration.json
```

Copy `homeserver_url` exactly into `homeserver` (do not derive it from the owner).
The Beeper hungryserv URL looks like:

```
https://matrix.beeper.com/_hungryserv/YOUR_USER
```

### 2. Configure

```sh
cp config.example.toml config.toml
$EDITOR config.toml
```

Paths are relative to `config.toml`. Secrets are read from `*_FILE` (preferred)
or the environment; the file must be owner-only.

```sh
cargo run --release -- generate-key > secrets/database-key
chmod 600 secrets/database-key secrets/registration.json
# Put the sidecar token in a file, not the repo:
sudo cat /etc/radioman/meshcore.token > secrets/meshcore-token
chmod 600 secrets/meshcore-token
```

### 3. Check and run

```sh
export MESHCORE_DRAWBRIDGE_KEY_FILE=$PWD/secrets/database-key
export MESHCORE_TOKEN_FILE=$PWD/secrets/meshcore-token

cargo run --release -- check
cargo run --release -- run
```

In another terminal, connect the Beeper websocket to the local appservice:

```sh
bbctl proxy -r secrets/registration.json
```

The bridge pre-creates the configured `default_channel` room and invites the
owner. Whisper the bridge bot or open the room in Beeper and send a message; it
goes out on the MeshCore channel. MeshCore channel/contact messages appear in
the matching room.

## Bridge commands

Owner-only messages starting with `/mesh` or `!mesh` are handled by the bridge
and are never forwarded to MeshCore:

```text
/mesh status
/mesh advert [flood|zerohop]
/mesh name <name>
/mesh txpower <0-30>
/mesh pathmode <0|1|2>
/mesh autoadd on|off
/mesh channels
/mesh contacts
/mesh channel create <name>
/mesh channel key <index>
/mesh channel share <index>
```

`/mesh channel create` picks the first empty slot (1–7), generates a 16-byte
key, configures the radio, and creates the Matrix room. The response contains
the official share URI:

```
meshcore://channel/add?name=<name>&secret=<hex>
```

`/mesh channel key` and `/mesh channel share` return the key/URI for an
existing channel. New rooms are created with an owner invite. Set
`owner_token_env` (a user access token in an owner-only file) to have the
bridge accept the invite automatically; leave it unset for invite-only. **The key is a shared secret** and the bridge room is not
end-to-end encrypted; share it only where that is acceptable.

`advert` sends a flood advert by default; `zerohop` sends a local-only advert.
`autoadd off` sets the radio's `manual_add_contacts` flag, so it stops adding
every advert it hears. These commands require the sidecar's `/v1/advert` and
`/v1/settings` endpoints.

## Commands

```sh
meshcore-drawbridge check          # validate config, registration, sidecar, keys
meshcore-drawbridge run            # appservice HTTP + poller
meshcore-drawbridge rooms          # list bound MeshCore conversations
meshcore-drawbridge send --channel 0 --text "hello"
meshcore-drawbridge send --contact dc6c06a8050b --text "hello"
meshcore-drawbridge generate-key
```

## Configuration

See [`config.example.toml`](config.example.toml). Key fields:

- `channels`: which channel indexes to bridge; empty means all
- `contacts`: pubkey prefixes to bridge; empty means every inbound DM
- `auto_create_rooms`: create a room on first inbound message
- `meshcore.default_channel`: room pre-created at startup
- `owner`: the only Matrix user allowed to write into bridged rooms

The sidecar has its own `allowed_channels` / `allowed_contacts` allowlist; a
Matrix message can only reach a conversation the sidecar also permits.

## Security and limits

- Bridge rooms are **not encrypted**. Use them as bridge rooms, not E2EE rooms.
- The sidecar token is a full radio-control credential; keep it owner-only.
- Only the configured owner can send from Matrix. Other users are ignored.
- The bridge does not retry forever silently: a failed sidecar send rolls back
  its event claim and returns an error, so the homeserver retries.
- `bbctl proxy` is a separate process. If it exits, Beeper events stop even
  though the bridge and poller stay up.

## Build and verify

```sh
cargo fmt --all --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
