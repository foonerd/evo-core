# Operator credentials — guide for integrating external API consumers

This document explains how to integrate an external API consumer (a
home-automation system, an MCU sensor trigger, a logging pipeline, a
shell script) with an evo device that has been switched out of `Open`
tier into `Secure` or `Secure-industrial`.

The operator is the device owner. Every step below assumes the
operator is sitting in front of (or SSH'd into) the device.

## Reading the rest of this document

The first time the device boots it admits the operator UI on the LAN
without any credential — the operator opens a browser at
`http://<device>/`, picks a DAC, scans tracks, plays music. **That is
the `Open` tier (the default). Nothing in this document applies to
it.**

When the operator decides to allow external automation systems (Home
Assistant, ESPHome sensors, scripts on another machine, dashboards)
to drive the device, they switch the device to `Secure` or
`Secure-industrial`. The external systems present a credential; the
operator's own browser continues to work without one as long as it's
on the same LAN.

This document is how the operator creates those credentials and how
each external platform consumes them.

## Two surfaces, two URLs

The device exposes the operator UI and the external API on
distinct origins:

| Surface | URL | TLS? | Auth? |
| ------- | --- | ---- | ----- |
| **Operator UI** (browser) | `http://<device>/` (port 80, plain HTTP) | No — handled internally on loopback | No credential under any tier (admitted via LAN-trust) |
| **External API** (this doc's audience) | `https://<device>:8443/api/v1/...` | Yes — direct to the framework's HTTPS listener | Bearer credential required under `Secure` / `Secure-industrial` |

The operator never needs to type the HTTPS URL, never sees a
certificate warning, never clicks "Advanced → Proceed". External
API consumers do present a cert: they handle it the standard
way (pin the cert, or use `--insecure` / `verify=False` for the
operator's domain trust posture). The recipes below show
both patterns.

## Quick reference

| What you want | Command |
| ------------- | ------- |
| Create a named credential (per-credential inventory) | `evo-plugin-tool admin auth create-bearer-token --name <name> --reason <reason> --scope <kind:scope> [--scope ...]` |
| Mint a one-off narrowed token (SSH/CLI dev path) | `evo-plugin-tool admin auth mint-bearer-token --reason <reason> --ttl-seconds <n> [--scope <capability-name>] [--scope ...]` |
| Create a time-limited named credential | Add `--expires-in-seconds <n>` (or `0` for never) to `create-bearer-token` |
| List every credential | `evo-plugin-tool admin auth list-bearer-tokens` |
| Revoke a credential | `evo-plugin-tool admin auth revoke-bearer-token --token-id <id> --reason <reason>` |
| Reset to open (lockout recovery) | `evo-plugin-tool admin auth reset-credentials-to-open --reason <reason>` |

All four require operator SSH access on the device (or a session
already admitted under the operator's UID via the unix socket); the
HTTPS-side surfaces are gated on `write:auth` / `read:auth` /
`step_up:system_admin`.

## Scopes

Each credential carries a list of `<kind>:<scope>` permissions.
Recognised kinds:

| Kind | Meaning |
| ---- | ------- |
| `read` | The credential may read state under that scope. |
| `write` | The credential may mutate state under that scope. |
| `step_up` | The credential may perform high-privilege admin operations under that scope. |

Common scopes the framework exposes (not exhaustive — see the
runtime's `describe_capabilities` op for the live list):

| Scope | What it covers |
| ----- | -------------- |
| `audio` | Audio playback / transport state. |
| `multiroom` | Multi-room group composition + audio plane control. |
| `plans` | Scheduled jobs (playlists, alarms, automations). |
| `plugins` | Plugin inventory + introspection. |
| `subjects` | Subject substrate (state subscribed by UIs). |
| `system` | Device identity + system-level introspection. |
| `auth` | Credential management itself. |
| `discovery` | Network discovery / peer enumeration. |
| `ui` | Schema-first UI surfaces. |
| `observatory` | Live observatory ring (debugging surface). |
| `witness` | Witness chain (audit surface). |

Pick the narrowest set that satisfies the consumer. A door-sensor
that fires a playlist needs `read:audio + write:multiroom`, not
the full operator set. A home-automation hub that drives every
audio surface plus the playlist scheduler needs `read:audio +
read:multiroom + write:multiroom + write:plans`. The logging
pipeline that just tails the observatory needs `read:observatory`
and nothing else.

If `--scope` is omitted entirely, the credential carries the
comprehensive operator set (every read + every write + every
step_up). This is the right shape for the operator's own personal
shell scripts, never the right shape for a tier-1 IoT device.

## Expiry policy

Each credential has an operator-set expiry policy. Two options:

| `--expires-in-seconds` | Meaning |
| ---------------------- | ------- |
| omitted (or `0`) | `Never`. The credential does not expire on its own. Operator revokes explicitly when the consumer is retired. **This is the default and the right shape for IoT-platform consumers** (ESP8266, ESPHome sensors, etc.) that cannot maintain a refresh loop. |
| any positive integer | The credential expires after that many seconds. Suitable for short-lived dashboard sessions, time-bounded delegations, etc. |

The framework's wire-layer ceiling caps any single credential at
100 years (a sentinel chosen so `Never` policy maps to a finite-on-
the-wire expiry while remaining effectively indefinite to the
operator).

## End-to-end recipes per platform

The recipes below assume the operator has already created a
credential and captured its `--token--` output. Replace
`<TOKEN>` with the actual bearer string and `<HOST>` with the
device's mDNS name or IP.

### Arduino / ESPHome / Tasmota / shell script

Plain HTTP/HTTPS with an `Authorization` header. This is what tier-1
IoT consumers reach for; nothing on the wire beyond a static header.

```bash
# Trigger a playlist on a door-close event. ESPHome's `http_request`
# integration; same shape works in Arduino + Tasmota.
http_request:
  url: "https://<HOST>:8443/api/v1/play_playlist"
  method: POST
  verify_ssl: false   # domain-CA cert; pin separately if you can
  headers:
    Authorization: "Bearer <TOKEN>"
    Content-Type: "application/json"
  json:
    playlist_id: "morning"
```

```bash
# Same call from a shell — verified against three rig architectures.
curl -k \
  -X POST \
  -H "Authorization: Bearer <TOKEN>" \
  -H "Content-Type: application/json" \
  -d '{"playlist_id":"morning"}' \
  https://<HOST>:8443/api/v1/play_playlist
```

### Home Assistant

The `rest_command` integration is the natural fit. Define one entry
per action you want HA to drive; HA stores the bearer once in
`secrets.yaml`.

```yaml
# configuration.yaml
rest_command:
  evo_play_playlist:
    url: "https://<HOST>:8443/api/v1/play_playlist"
    method: POST
    verify_ssl: false
    headers:
      Authorization: "Bearer !secret evo_bearer"
      Content-Type: "application/json"
    payload: '{"playlist_id":"{{ playlist_id }}"}'
  evo_set_volume:
    url: "https://<HOST>:8443/api/v1/set_volume"
    method: POST
    verify_ssl: false
    headers:
      Authorization: "Bearer !secret evo_bearer"
      Content-Type: "application/json"
    payload: '{"percent":{{ percent }}}'
```

```yaml
# secrets.yaml
evo_bearer: "<TOKEN>"
```

Trigger from an automation:

```yaml
automation:
  - trigger:
      platform: state
      entity_id: binary_sensor.front_door
      to: 'on'
    action:
      - service: rest_command.evo_play_playlist
        data:
          playlist_id: morning
```

### Node-RED

The `http request` node carries the bearer in an `Authorization`
header. Configure a Function node to set the headers on every
outbound request:

```javascript
msg.headers = {
    "Authorization": "Bearer <TOKEN>",
    "Content-Type": "application/json"
};
msg.payload = { playlist_id: "morning" };
return msg;
```

Pipe into `http request` configured for `POST https://<HOST>:8443/api/v1/play_playlist`.

### Python

```python
import requests

BEARER = "<TOKEN>"
HOST = "<HOST>"
HEADERS = {"Authorization": f"Bearer {BEARER}"}

resp = requests.post(
    f"https://{HOST}:8443/api/v1/play_playlist",
    json={"playlist_id": "morning"},
    headers=HEADERS,
    verify=False,  # domain-CA — pin certificate in production
    timeout=5,
)
resp.raise_for_status()
print(resp.json())
```

### WebSocket clients (browser, JS, Python)

Two paths to authenticate a WS upgrade. Use whichever your client
library supports.

**Authorization header path** — works from any client that can set
HTTP headers on the upgrade request:

```python
import asyncio, websockets, ssl
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

async def main():
    async with websockets.connect(
        "wss://<HOST>:8443/api/v1/ws",
        additional_headers={"Authorization": "Bearer <TOKEN>"},
        ssl=ctx,
    ) as ws:
        await ws.send('{"op":"describe_capabilities","request_id":"r1"}')
        print(await ws.recv())

asyncio.run(main())
```

**Subprotocol path** — works from browser JS where `fetch`/`WebSocket`
APIs do not let you set arbitrary headers on the upgrade. The
framework recognises `Sec-WebSocket-Protocol: evo.bearer.<TOKEN>`.
The `evo.` namespace distinguishes this subprotocol from any
other `bearer.*` protocol another WebSocket application on the
same host might use; the framework echoes the exact subprotocol
back in the 101 response per RFC 6455 §4.2.2 so compliant clients
(Node `ws`, every browser stack) accept the handshake.

```javascript
const ws = new WebSocket(
    "wss://<HOST>:8443/api/v1/ws",
    `evo.bearer.<TOKEN>`,
);
ws.onopen = () => ws.send(JSON.stringify({
    op: "describe_capabilities",
    request_id: "r1",
}));
ws.onmessage = ev => console.log(ev.data);
```

## Recovery: I got locked out, what now?

The operator UI's session uses LAN-trust admission and does NOT
require a credential. If the browser refuses to connect, the
sequence is:

1. Confirm the operator UI is reachable on port 80:
   `curl -sI http://<HOST>/`
   Expect a `200 OK` regardless of tier. If this fails, the
   UI runtime is not bound on port 80 — check
   `systemctl status evo-ui` on the device.

2. Confirm the framework substrate is reachable via the UI
   runtime's reverse-proxy:
   `curl -s -o /dev/null -w "%{http_code}\n" http://<HOST>/api/v1/describe_capabilities`
   Expect a `200` regardless of tier — `describe_capabilities` is
   anonymous.

3. Confirm the browser is connecting from a LAN address. The
   framework classifies RFC1918 (10/8, 172.16/12, 192.168/16),
   loopback, link-local, and IPv6 unique-local as LAN. Anything
   else is WAN and requires a credential.

4. If the device is in `Secure` or `Secure-industrial` tier and the
   browser is on a WAN origin, either:
   - Move the browser onto the LAN, or
   - Mint a credential for that browser session via SSH (see the
     `create_bearer_token` flow above), or
   - Reset the device to `Open` tier via the lockout-recovery
     gesture (next step).

5. **Lockout-recovery gesture** (requires SSH access):

   ```bash
   # SSH into the device, then:
   evo-plugin-tool admin auth reset-credentials-to-open \
       --reason "operator-lockout-recovery"
   sudo systemctl restart evo
   ```

   This purges every credential record and every revocation entry,
   and the next boot admits `Open` tier. The operator's browser
   reaches the device on next visit; external API consumers will
   need fresh credentials, which the operator can mint once the
   reset is complete.

## How the operator chooses a tier

The three tiers and their trade-offs:

| Tier | When to pick it |
| ---- | --------------- |
| `Open` (default) | The operator owns the LAN and trusts every device on it. Simplest path — operator opens browser, works. No credentials needed. |
| `Secure` | External automation systems exist (Home Assistant, ESPHome sensors, scripts on another machine). Each consumer gets its own credential with custom scopes; expiry is operator-set per credential, defaulting to `Never`. The operator's own browser continues to work without a credential on the LAN. |
| `Secure-industrial` | Same as `Secure` plus the operator wants per-credential expiry warnings + audit visibility ahead of expiry, typical of enterprise integrations. The list output shows credentials expiring within seven days; the operator rotates before lockout. |

The toggle is `EVO_AUTH_TIER` in the systemd unit drop-in (CLI
control) or, once landed, the UI tier-toggle screen. The
operator's own browser admits on LAN-trust in every tier so the
operator never locks themselves out.
