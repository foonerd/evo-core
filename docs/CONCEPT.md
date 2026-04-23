# EVO - Concept

Status: concept for evo, a brand-neutral steward fabric.
Audience: maintainers and plugin authors.
Vocabulary: load-bearing; used as defined.

## 1. Essence

Evo is a fabric for building appliance-class devices. It is a STEWARD that administers a CATALOGUE of concerns, admits PLUGINS that stock slots in that catalogue, and emits composed PROJECTIONS to any consumer that looks.

Evo itself is domain-neutral. A specific device (audio player, home automation controller, signage kiosk, scientific instrument) is defined by a CATALOGUE declaration and a curated plugin set - shipped as a DISTRIBUTION of evo.

Essence, for any evo-based device:

A device that performs the work declared in its catalogue, by composing contributions from plugins around subjects, while presenting coherent information to any consumer that looks.

Everything in this document is derivable from this sentence plus the fabric vocabulary. Anything in a built system that does not serve a rack declared in the catalogue is not essence; it is a plugin contribution that has found the wrong home, or it does not belong.

## 2. Fabric

The product is a STEWARD that administers a CATALOGUE. The catalogue is organised into RACKS. Each rack holds SHELVES. Each shelf has one or more SLOTS of declared SHAPE. PLUGINS stock slots. The steward is the sole authority; plugins never communicate directly.

Every contribution is keyed to one or more SUBJECTS. The steward keeps a canonical SUBJECT REGISTRY that reconciles the many external addressings plugins use into one canonical subject per real thing. Subjects are connected in a governed RELATION GRAMMAR; the steward keeps the resulting graph.

Consumers never address plugins. They address the steward, either by rack (structural query) or by subject (federated query). The steward composes contributions from every rack that has opinions about the subject, walks related subjects within a declared scope, and emits a PROJECTION. All outbound behaviour of the system is either a projection on demand or a streamed HAPPENING on the fabric's notification surface. There is no side channel.

```
                  +---------------+
                  |   CONSUMERS   |
                  +-------+-------+
                          |
                          |  projections / happenings
                          v
   +----------------------+-----------------------+
   |                                              |
   |                  THE STEWARD                 |
   |         (sole authority; no side channel)    |
   |                                              |
   |   catalogue       subjects       relations   |
   |   admission       projections    ledger      |
   |   happenings bus                             |
   |                                              |
   +-----^---------^---------^---------^----------+
         |         |         |         |
      plugin    plugin    plugin    plugin
```

Plugins stock slots on the steward and never address each other. Consumers address the steward, never plugins. The steward composes contributions around subjects and emits projections or happenings. This is the hub discipline the rest of the document assumes.

Two classes of originator exist inside the fabric besides external requests. APPOINTMENTS originate actions from time. WATCHES originate actions from observed conditions. Both produce instructions the steward dispatches as if from outside. The CUSTODY LEDGER tracks work entrusted to WARDENS - plugins that take custody of long-running operations. A separate FAST PATH serves real-time mutation without recomposition.

| Fabric concept | One-line role |
|----------------|---------------|
| Essence | The statement the steward enforces at startup and on admission. |
| Steward | Sole authority. Admits, places, composes, dispatches, projects, notifies. |
| Catalogue | Declared data. Racks, shelves, slots, shapes, relation grammar. |
| Rack | A concern. Holds shelves. Belongs to a family and a kind. |
| Shelf | A slot or slot-set of declared shape within a rack. |
| Slot | A typed opening that admits one or more plugin contributions. |
| Plugin | Any capability that stocks a slot. |
| Subject | A thing the catalogue has opinions about. Canonical identity held by the steward. |
| Relation | Typed directed connection between subjects. |
| Projection | Composed view emitted by the steward, keyed to a rack or a subject. |
| Happening | A transition event the steward emits on its notification stream. |
| Appointment | Time-originated instruction. |
| Watch | Condition-originated instruction. |
| Custody ledger | Registry of active warden assignments and their state. |
| Fast path | Real-time mutation channel alongside the structural slow path. |
| Distribution | A curated catalogue plus plugin set, shipped as a branded device. |

## 3. Distributions and Actors

Evo is brand-neutral. A device ships as a DISTRIBUTION of evo: a catalogue declaration plus a plugin set plus branding. Multiple distributions can exist for different domains.

The first distribution targets the audio-player domain. Its catalogue declares racks such as audio, audio sources, audio processing, artwork, metadata, branding, kiosk, networking, storage, library, appointments, watches, identity, lifecycle, observability. Its plugin set is curated for that domain. That distribution is shipped by the Volumio brand as a specific product.

Evo itself ships no racks. The catalogue is data the distribution provides.

Five actor positions exist in the evo ecosystem:

| Position | Role |
|----------|------|
| Evo project | Maintains the steward, fabric, SDK, tooling. |
| Distribution | Curates catalogue + plugin set + branding for a domain. |
| Vendor | Commercial or organisational entity that signs plugins under a claimed namespace with formal commitments. |
| Individual author | Person or small team signing plugins under their own name. |
| Operator | The person running the device. Sole authority on local trust and revocation. |

Full actor taxonomy, vendor contract, namespace governance, and trust-root relationships are in `docs/engineering/VENDOR_CONTRACT.md`.

## 4. Rack Catalogue

A distribution's catalogue declares racks. Racks belong to families and kinds.

Rack families: DOMAIN (what the distribution does), COORDINATION (when and why it acts), INFRASTRUCTURE (how the fabric operates over time).

Rack kinds: PRODUCER (originates instructions), TRANSFORMER (moves or changes something), PRESENTER (renders projections to a surface), REGISTRAR (holds knowledge).

A rack may have more than one kind when the concern straddles.

The rack list is open. New racks declare new concerns. The steward reads the catalogue as data; no rack is compiled into evo.

## 5. Plugin Model

Plugins contribute to slots. They never address each other. They address only the steward, through a contract whose shape is declared by the slot they stock.

Two orthogonal axes classify every plugin:

| Axis | Values | Meaning |
|------|--------|---------|
| Instance shape | Singleton, Factory | One contribution forever, or many contributions over time driven by world events. |
| Interaction shape | Respondent, Warden | Answer discrete requests, or take custody of sustained work. |

Full plugin contract is defined in `docs/engineering/PLUGIN_CONTRACT.md`. Packaging, identity, signing, installation are in `docs/engineering/PLUGIN_PACKAGING.md`.

"Plugin" does not imply optional, third-party, or sandboxed. It is the universal term for any satisfier of a slot contract. Every plugin ships as an independently versioned artefact with a declared manifest. The only entity in the running system that is not a plugin is the steward.

## 6. Implementation Commitments

| Commitment | Statement |
|------------|-----------|
| Language | Rust for the steward and for in-process first-party plugins. |
| Base OS | Debian Trixie minimal (lite). Evo is a layer atop stock Debian, not a rootfs assembled from scratch. |
| Steward process | Single long-running process. Owns the catalogue, subject registry, relation graph, custody ledger, projection layer, happenings stream. |
| Plugin transport | Two transports of one contract: in-process (Rust trait, compiled into the steward or loaded as cdylib) and out-of-process (Unix-socket protocol, any language). |
| Plugin delivery | Each plugin is an independently versioned artefact with a declared manifest. |
| Trust classes | Declared in manifests, enforced by the steward, authorised by the signing key used. |
| Versioning | Shelf shapes are versioned. Plugin manifests declare the shape version they satisfy. The steward refuses plugins whose declared version is not in the slot's supported range. |
| Catalogue as data | Rack and shelf declarations are TOML the steward reads, not code. Adding a new rack is a catalogue edit plus the plugins to stock it. The steward is unchanged. |
| Domain neutrality | The steward has no knowledge of audio, networking, any service, any protocol. All domain knowledge lives in catalogues and plugins. |

## 7. Filesystem Layout on Target

Evo owns three roots on a Debian Trixie device:

| Root | Purpose | Writable |
|------|---------|----------|
| `/opt/evo/` | Read-only content shipped by packages. Binaries, vendor plugins, distribution plugins, catalogue declarations, static data. | No (package-owned). |
| `/etc/evo/` | Operator-editable policy. Steward configuration, trust keys, catalogue overlays, per-plugin config overrides. | Yes (root-owned, operator-edited). |
| `/var/lib/evo/` | Runtime mutable state. Subject registry, relation graph, per-plugin state and credentials, caches. | Yes (service-owned). |

Full layout in `docs/engineering/PLUGIN_PACKAGING.md`.

## 8. What the Fabric Does Not Do

| Concern | Whose problem |
|---------|---------------|
| Domain-specific logic (audio, networking, metadata, anything service-specific) | The distribution's plugin set. |
| Authentication with specific external services | The plugin that integrates that service. |
| Protocol parsing, codec handling, format decoding | The plugin that speaks that protocol or format. |
| UI rendering and styling | Consumers of projections. The steward emits projections in declared structural shape; how they are drawn is not its concern. |
| Cross-plugin coordination | Does not exist. Plugins cannot coordinate. All composition is through the steward on subject keys. |

## 9. Consequences

- Adding a new service, protocol, or data source is stocking existing shelves with new plugin contributions. The steward is unchanged. The catalogue is unchanged. The fabric is unchanged.

- Replacing a plugin is replacing one artefact on one slot. Every other plugin, every other rack, every consumer is unaffected because none of them addresses that plugin directly.

- Graceful degradation is structural. A consumer asking about a subject receives whatever the fabric can compose; missing contributions mean absent fields, not broken projections.

- Plugin authors never coordinate. The coordination cost of the plugin ecosystem is O(1) per plugin: each author learns the plugin contract once.

- Distributions can replace any plugin, including the most central (playback engine, network manager), by providing an alternative honouring the same contract. The steward does not privilege first-party over third-party.

- The rack list is open; the plugin population is open; the fabric is closed.

## 10. Deliberately Open

Concept-level decisions deferred to the engineering layer. Named here so downstream documents know what they must answer.

| Open question | Decision doc |
|---------------|--------------|
| Plugin contract (trait + wire protocol) | `docs/engineering/PLUGIN_CONTRACT.md` |
| Plugin manifest, signing, lifecycle, filesystem paths | `docs/engineering/PLUGIN_PACKAGING.md` |
| Vendor contract, actor taxonomy, namespace governance | `docs/engineering/VENDOR_CONTRACT.md` |
| Logging conventions (levels, format, fields, plugin integration) | `docs/engineering/LOGGING.md` |
| Subject identity resolution | `docs/engineering/SUBJECTS.md` |
| Relation grammar | `docs/engineering/RELATIONS.md` |
| Projection subscription protocol | `docs/engineering/PROJECTIONS.md` |
| Fast-path mechanism | `docs/engineering/FAST_PATH.md` |
| Steward startup and essence enforcement | `docs/engineering/STEWARD.md` |
| Trust class taxonomy detail | Covered in `PLUGIN_PACKAGING.md`. |
