# Frontend

Status: engineering-layer positioning on frontend technology and deployment.
Audience: distribution authors choosing frontend stacks, integrators building on top of an evo distribution, anyone asking "what should the UI be?".
Related: `BOUNDARY.md` (the framework/distribution split; frontend is distribution-side), `CLIENT_API.md` (the protocol every frontend ultimately speaks), `PLUGIN_AUTHORING.md` (bridge plugins are plugins).

The framework has no opinion about frontend. This document is about making that explicit, cataloguing the deployment and technology shapes that distributions can reach for, and describing the **bridge plugin** pattern that turns the local Unix-socket protocol into any remote interface a frontend might want. The goal is technological versatility: the same steward should be able to back a Raspberry Pi kiosk, a web UI served locally, a mobile app over the network, a Home Assistant integration, or an automotive HMI - all at the same time, on the same device, if a distribution chooses.

## 1. Purpose

A consumer of the steward is anything that opens its client socket, speaks the protocol in `CLIENT_API.md`, and uses the responses. A **frontend** is the specific kind of consumer that presents the device to a human. The framework treats frontends as consumers like any other - the same client socket, the same four operations, no privileged interface.

This document is organised around three orthogonal questions every distribution answers:

1. **Where** does the frontend execute? (On the device, off the device, in a browser, in a terminal, on a phone.)
2. **What technology** does it use? (Web framework, native toolkit, terminal UI library, integration platform.)
3. **How** does it reach the steward? (Directly over the Unix socket, or through a bridge plugin exposing a remote interface.)

The answers are independent. A distribution may pick any combination of answers, and may even ship multiple combinations simultaneously - a local kiosk UI, a remote web UI via an HTTP bridge, and a Home Assistant integration via an MQTT bridge, all at once.

## 2. First Principle: Frontend is a Consumer

Three consequences of this flow from `BOUNDARY.md` and are worth stating plainly here:

1. **The steward has no frontend-specific code.** No special-cased requests, no preferential path, no UI-shaped data types. The client socket is the whole interface.
2. **Changing frontend is free.** A distribution can replace its entire frontend without touching any plugin or the steward. The only contract is the client socket.
3. **Multiple frontends compose without coordination.** Frontends do not know about each other. They all address the steward, see consistent state, and react to the same happenings.

The framework's non-opinionated posture is deliberate: appliances span from headless to voice-controlled to automotive to Home Assistant rooms, and a one-framework-fits-all choice would exclude most of those deployments.

## 3. Three Orthogonal Choices

The combinations a distribution will land on vary widely. Quick examples, all valid:

| Deployment | Technology | Integration |
|------------|------------|-------------|
| On-device kiosk browser | React | Direct Unix socket via WebSocket bridge |
| Local HTTP server | Angular | Direct Unix socket via HTTP bridge |
| Mobile app | Flutter | HTTP bridge over LAN |
| Desktop companion | Tauri | WebSocket bridge over LAN |
| Terminal | Textual (Python) | Direct Unix socket |
| Home Assistant add-on | HA custom component (Python) | MQTT bridge |
| Automotive HMI | Qt/QML (C++) | Direct Unix socket on the head unit |
| Voice assistant | Rhasspy / Willow / Alexa skill | HTTP bridge |
| None (headless) | - | - |

None of these is "the right answer". Each is right for its distribution's product.

## 4. Deployment Shapes

Where the frontend runs. The options below are not exhaustive; they are the shapes we have seen demand for.

### 4.1 On-device kiosk browser

A browser in full-screen mode, displayed on a connected screen (HDMI, DSI, touchscreen). Renders a web UI served from a local HTTP server also on the device. The kiosk plugin on the steward's side manages the browser process (see the kiosk rack's role in `CONCEPT.md` section 6).

Typical stack: Chromium in kiosk mode + a local HTTP server (part of the distribution) + a web UI in any framework.

### 4.2 On-device web, remote browser

The same local HTTP server, but the user opens it from another device on the network (phone, tablet, laptop). Network access is via the distribution's choice: some expose the HTTP port on the LAN directly; others put it behind a bridge plugin with authentication and TLS.

### 4.3 Native on-device

A native GUI application running directly on the device (no browser). Common in automotive, industrial, and embedded scenarios where a full browser is not wanted.

Typical stack: Qt/QML, GTK, Flutter, or an OEM's proprietary HMI framework.

### 4.4 Desktop companion app

A native or web-technology-based desktop app running on a separate computer (laptop, desktop), reaching the device over the network. Used for configuration tools, power-user interfaces, multi-device management.

Typical stack: Electron, Tauri, Qt, native Swift/Cocoa, WPF/.NET.

### 4.5 Mobile app

A native or cross-platform mobile app on iOS or Android, reaching the device over the network.

Typical stack: Flutter, React Native, Swift, Kotlin, native Jetpack Compose / SwiftUI.

### 4.6 Terminal / CLI

A text-mode UI, either full-screen (TUI) or command-oriented (CLI). Appropriate for headless devices, SSH-administered appliances, and as a diagnostic companion alongside a GUI.

Typical stack: Textual (Python), Ratatui (Rust), Bubble Tea (Go), Ink (Node.js), or plain shell scripts for one-shot commands.

### 4.7 Voice and gesture

Microphone and speaker (voice) or camera and sensor (gesture) interfaces that translate human input into requests. Usually a separate process that listens for wake words, performs speech-to-text, maps intents to `op = "request"` calls, and uses text-to-speech for feedback.

Typical stack: Rhasspy, Willow, or a commercial assistant (Alexa, Google Assistant) via a skill or action.

### 4.8 Home automation integration

The device appears inside an existing home automation system as an entity, a service, a thing, or a device. Users interact via the home automation system's existing UI rather than any UI the distribution ships itself.

| Target | Integration shape |
|--------|------------------|
| Home Assistant | A custom component (Python) or a MQTT device following the HA MQTT discovery protocol. |
| openHAB | A binding (Java/Kotlin) or a MQTT thing. |
| Node-RED | Custom nodes (JavaScript) or MQTT/HTTP flows. |
| Apple Home / HomeKit | Via the HomeKit Accessory Protocol, usually through a bridge. |
| Google Home / Matter | Via Matter's device types and Google Home integrations. |
| Amazon Alexa | Via Smart Home Skills. |

These are not frontends in the narrow sense - they are bridges to other systems whose frontends render the device. A distribution that cares about broad home-automation compatibility typically ships an MQTT bridge plugin and lets each target system discover the device that way.

### 4.9 Proprietary or embedded HMI

Automotive head units, industrial control panels, medical displays, smart appliances with their own OEM UI frameworks. The OEM already has a UI technology commitment; the steward is the backend service they integrate with.

The technology is whatever the OEM ships: Qt in Automotive Grade Linux; proprietary C++ frameworks in some automakers; AUTOSAR-compliant HMIs in industrial; native SDK-based UIs in appliances. The integration shape is whatever that ecosystem supports - direct Unix socket if the HMI is on the same device, a bridge plugin exposing HTTP or a proprietary protocol if not.

### 4.10 Headless (no frontend)

No UI at all. The device is controlled entirely by external systems (other appliances, automation, voice), accessed via bridge plugins, or observed through monitoring agents.

Valid and common. A distribution is not required to ship a frontend.

### 4.11 Composed

Multiple of the above simultaneously. A single device might run:

- A kiosk browser on its built-in display.
- An HTTP bridge exposing a REST interface for a mobile app.
- An MQTT bridge for Home Assistant discovery.
- A terminal CLI for diagnostics over SSH.

Each is an independent consumer. The steward does not coordinate between them; they all see consistent state because the steward is the single source of truth.

## 5. Technology Shapes

What the frontend is built in. The framework imposes nothing; this section names the common choices so a distribution knows the landscape.

### 5.1 Web frameworks

For any UI rendered in a browser, whether kiosk, local HTTP, or remote. All of these are first-class; no preference:

| Framework | Notes |
|-----------|-------|
| React | Ubiquitous; large ecosystem; good for teams already invested. |
| Angular | Strong for larger applications with explicit structure. |
| Vue | Approachable; popular in Europe and Asia. |
| Svelte | Compile-time framework; small runtime; fast. |
| Solid | React-like API with fine-grained reactivity. |
| Lit | Web Components standard; small footprint; framework-agnostic interop. |
| HTMX | Server-driven; minimal JavaScript; back-to-basics. |
| Vanilla JS/TS | No framework; appropriate for tiny UIs or embedded contexts. |

All of these consume the steward via either a WebSocket bridge (for subscription support) or an HTTP bridge (for request/response only). The protocol is the same; the framework is the distribution's choice.

### 5.2 Cross-platform native

For native applications targeting desktop, mobile, or multiple platforms:

| Toolkit | Platforms | Notes |
|---------|-----------|-------|
| Flutter | iOS, Android, desktop, web | Dart; strong mobile story; growing desktop. |
| React Native | iOS, Android, some desktop | JavaScript; large ecosystem. |
| Tauri | Desktop (Linux, macOS, Windows) | Rust backend + web frontend; small binaries. |
| Electron | Desktop | Node + Chromium; large binaries; well-understood. |
| Qt | Desktop, mobile, embedded | C++ or QML; widely used in automotive and industrial. |
| GTK | Linux desktop, mobile | C / Rust / Python bindings. |

### 5.3 Traditional native

For platform-specific apps where cross-platform is not a goal:

- **iOS**: Swift + SwiftUI or UIKit.
- **Android**: Kotlin + Jetpack Compose or traditional Views.
- **macOS**: Swift + SwiftUI or AppKit.
- **Windows**: WinUI, WPF, WinForms, or native Win32.
- **Linux**: Qt, GTK, or low-level X11/Wayland toolkits.

### 5.4 Terminal UI

For CLI or TUI consumers:

| Library | Language | Shape |
|---------|----------|-------|
| Textual | Python | Rich TUI framework; reactive; modern. |
| Ratatui | Rust | Low-level TUI; high performance. |
| Bubble Tea | Go | Elm-inspired; functional. |
| Ink | Node.js | React-for-terminals. |
| tview / termui | Go | Widgets-based; fast start. |
| ncurses | C / any binding | Traditional; portable. |

For one-shot commands, plain shell scripts plus the `socat`-based framing in `CLIENT_API.md` section 6.6 are enough.

### 5.5 Integration platforms

Where the UI is the integration platform's UI, not ours:

| Platform | Integration language |
|----------|----------------------|
| Home Assistant | Python (custom components) or YAML + MQTT (device discovery). |
| openHAB | Java / Kotlin (bindings) or MQTT / HTTP things. |
| Node-RED | JavaScript (custom nodes) or HTTP / MQTT flows. |
| ioBroker | JavaScript (adapters). |
| Homebridge | JavaScript (plugins) bridging to HomeKit. |
| Domoticz | Python / LUA / HTTP. |

Each has its own contract for how external devices appear. The bridge plugin on the evo side speaks the platform's expected protocol (usually MQTT, sometimes HTTP); the platform-side integration is minimal.

### 5.6 Proprietary frameworks

Whatever the target ecosystem uses. Distributions shipping into automotive, industrial, or white-label appliance markets will meet frameworks not on this list. The pattern is the same: bridge plugin exposes whatever the target framework consumes; the target framework renders the UI.

## 6. Integration Shapes

How the frontend reaches the steward. Three shapes, with the choice determined by where the frontend runs.

### 6.1 Direct local (on-device)

Frontend runs on the same device. It opens the Unix socket directly and speaks the protocol.

Appropriate for: on-device kiosk, on-device native, on-device terminal, on-device voice. The frontend has filesystem access to the socket and is part of the distribution's trust boundary.

Latency is microseconds. No authentication needed - the distribution controls who has filesystem access.

### 6.2 Bridge plugin (the pattern)

A **bridge plugin** is a respondent that runs on the steward's side, terminates the local Unix socket, and exposes its own remote interface. It is a plugin like any other; `PLUGIN_AUTHORING.md` covers how to write one.

Shape:

```
[Remote consumer]  <--remote protocol-->  [Bridge plugin]  <--client socket-->  [Steward]
```

The bridge plugin:

- Listens on a network interface (TCP port, Unix socket at a different path, whatever).
- Speaks whatever remote protocol the distribution chose (HTTP, WebSocket, MQTT, gRPC, plain TCP framed, etc.).
- Authenticates remote clients (TLS certificates, tokens, basic auth, OAuth, whatever is appropriate).
- Translates between its remote protocol and the steward's client socket.

Because the bridge is a plugin, it is sandboxed to its declared trust class, it is a distribution-specific piece of code that does not live in evo-core, and swapping bridges (HTTP -> WebSocket, WebSocket -> gRPC) is a plugin change, not a steward change.

### 6.3 Common bridge flavours

Distributions tend to ship one or more of:

| Bridge type | Protocol | Typical consumers |
|-------------|----------|-------------------|
| HTTP bridge | REST over HTTP/HTTPS | Web frontends, mobile apps, third-party scripts |
| WebSocket bridge | WebSocket over HTTP/HTTPS | Web frontends needing live updates, mobile apps |
| MQTT bridge | MQTT over TCP/TLS | Home automation systems, IoT platforms |
| gRPC bridge | gRPC over HTTP/2 | Microservice environments, polyglot ecosystems |
| ZeroMQ / NATS bridge | Message bus | Event-driven architectures |
| Proprietary protocol bridge | Whatever | Integration with OEM-specific backplanes |

A common shape for a consumer-friendly web UI is a combined HTTP + WebSocket bridge where:

- `GET /api/v1/custodies` maps to `op = "list_active_custodies"`.
- `POST /api/v1/request/<shelf>/<type>` maps to `op = "request"`.
- `GET /api/v1/ws` upgrades to WebSocket and streams translated happenings.

The mapping is the bridge plugin's business. Different distributions will make different mapping choices; there is no one blessed REST-ification of the client protocol. Vendor-specific bridges ship in the `evo-device-<vendor>` repository.

### 6.4 Security and trust for remote interfaces

The core protocol assumes local trust (`CLIENT_API.md` section 9). Bridges restore the security properties that the network environment needs:

- TLS for transport encryption.
- Authentication (tokens, certificates, OAuth, mutual TLS).
- Rate limiting against abuse.
- Input validation at the bridge boundary.
- Audit logging of remote requests.

None of this is in the steward. All of it is in the bridge plugin. A distribution serious about remote access ships a production-grade bridge; a distribution serving only local consumers can ship nothing at all.

## 7. Compositional Patterns

Real distributions ship more than one frontend. Some common combinations:

### 7.1 Primary UI + diagnostic CLI

The main frontend is a kiosk or web UI for end users. A terminal CLI sits alongside for operators, accessed over SSH. Both hit the client socket directly; they do not coordinate.

### 7.2 Primary UI + voice

The main frontend renders the visible device state. A voice assistant (Rhasspy, Willow, Alexa) sits alongside and issues `op = "request"` calls on spoken commands. Voice feedback comes from TTS driven by the voice process, not by the primary UI.

### 7.3 Primary UI + home automation

The device has its own UI for direct users, and simultaneously appears inside Home Assistant or openHAB for integration into broader home automation. The MQTT bridge plugin exposes the device to the home automation system; the primary UI works independently.

### 7.4 Primary UI + mobile app

On-device UI for local interaction; mobile app for remote control. The mobile app reaches the device via an HTTP or WebSocket bridge over the LAN. Both see consistent state through the steward.

### 7.5 Headless + everything else

Some distributions ship no on-device frontend at all. The device is operated entirely through bridges: HTTP for a web interface on another device, MQTT for home automation, voice via a separate assistant. The device itself has no screen.

### 7.6 Primary UI + proprietary upstream

The device is a component inside a larger product (an automotive infotainment system, an industrial control cabinet, a smart-home hub). The upstream product has its own HMI framework and operates the device through a proprietary bridge. The "primary UI" from the end user's perspective is the upstream product's UI, not ours.

## 8. What a Frontend Must Not Do

The framework imposes one constraint on frontends: they must not address plugins directly. Every interaction with a plugin goes through the steward via the client socket. The bridge pattern preserves this: the bridge talks to the steward through its own client socket, not to plugins via shortcut paths.

In practice, this rules out:

- Opening sockets that specific plugins happen to expose.
- Running plugin binaries directly from the frontend.
- Reaching into plugin filesystem state.
- Calling OS commands that bypass the steward (e.g., `systemctl restart <plugin>.service`).

If a frontend needs something a plugin has but the steward does not expose, the fix is to widen the plugin's request interface (a new `request_type`), not to bypass the steward. This is the same discipline that keeps the plugin ecosystem coordination-free.

## 9. Security and Trust

Security considerations for frontends are deployment-shape-specific:

| Deployment | Primary threat model | Mitigation |
|------------|----------------------|------------|
| On-device direct | Local user has full access (often fine for appliances with known operators) | Filesystem permissions on the socket |
| On-device local HTTP (kiosk only) | None beyond local user | Bind to localhost only |
| On-device local HTTP (LAN exposed) | LAN-reachable attackers | Authentication at the HTTP layer; TLS; bridge plugin enforces |
| Remote browser over WAN | Internet-reachable attackers | TLS, strong authentication, rate limiting, probably a reverse proxy; bridge plugin enforces all of it |
| Home automation MQTT | Whatever MQTT broker's security posture is | Broker-level TLS + credentials; MQTT bridge enforces |
| Proprietary / embedded HMI | Depends on the upstream product's threat model | Defer to upstream; bridge speaks whatever they demand |

The steward itself has no authentication layer. Every security boundary is at a bridge plugin. This is a deliberate split: the steward is the trusted core; the bridge is the distribution-specific policy enforcement point. A distribution that gets its bridges right can safely expose the device anywhere; a distribution that skips the bridge layer must keep the steward strictly local.

## 10. Non-Prescription

This document names technologies, patterns, and combinations. It does not prescribe any of them. A distribution is sovereign over:

- Which frontend(s) to ship, if any.
- Which frameworks and libraries to use.
- Which deployment shape(s) to target.
- Which bridges to implement and how.
- What security posture to enforce.
- How to version and evolve all of the above.

The framework provides: a stable client protocol, a plugin SDK, a documented boundary, and reference examples. What you build on top of those is yours.

## 11. Further Reading

- `BOUNDARY.md` section 6 - frontend is a distribution component.
- `CLIENT_API.md` - the protocol every frontend speaks, with language examples.
- `PLUGIN_AUTHORING.md` - how to write a bridge plugin (a bridge is just a respondent).
- `PLUGIN_CONTRACT.md` - the plugin wire protocol a bridge must terminate.
- `CONCEPT.md` section 6 - the kiosk rack's role for on-device surfaces.
- `VENDOR_CONTRACT.md` - trust classes for bridge plugins that cross security boundaries.
- `DEVELOPING.md` section 6 - talking to a running steward for frontend development.
