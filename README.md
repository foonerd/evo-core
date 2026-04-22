# evo-core

Evo is a fabric for building appliance-class devices. A brand-neutral steward administers a declared catalogue of concerns, admits plugins that stock slots in that catalogue, and emits composed projections to any consumer.

This repository holds the steward, the plugin SDK, the plugin tooling, and the engineering-layer contracts that govern plugin authoring.

## Documents

| Document | Purpose |
|----------|---------|
| [docs/CONCEPT.md](docs/CONCEPT.md) | The fabric contract. Essence, steward, racks, shelves, plugins, subjects, relations, projections, happenings. Read first. |
| [docs/engineering/PLUGIN_CONTRACT.md](docs/engineering/PLUGIN_CONTRACT.md) | The universal plugin contract. Rust trait and Unix-socket wire protocol, two transports of one contract. |
| [docs/engineering/PLUGIN_PACKAGING.md](docs/engineering/PLUGIN_PACKAGING.md) | Plugin manifest, identity, signing, filesystem layout on target, installation lifecycle, SDK and tooling. |
| [docs/engineering/VENDOR_CONTRACT.md](docs/engineering/VENDOR_CONTRACT.md) | Vendor contract. Actor taxonomy, namespace governance, vendor commitments and privileges, distribution relationships, revocation pathways. |
| [docs/engineering/LOGGING.md](docs/engineering/LOGGING.md) | Logging contract. Library, levels, default level, format, structured fields, logs-vs-happenings, plugin log integration. |
| [docs/engineering/SUBJECTS.md](docs/engineering/SUBJECTS.md) | Subject registry. Canonical identity, external addressings, announcements, equivalence and distinctness claims, reconciliation, merges and splits, provenance, operator overrides, persistence, happenings. |
| [docs/engineering/RELATIONS.md](docs/engineering/RELATIONS.md) | Relation graph. Typed and directed subject-to-subject connections, catalogue-declared grammar, multi-claimant assertions, scoped walks, cardinality handling, merge and split cascade, operator overrides, persistence, happenings. |
| [docs/engineering/PROJECTIONS.md](docs/engineering/PROJECTIONS.md) | Projection layer. Structural and federated queries, pull and push, catalogue-declared shapes, plugin contribution composition rules, subscription scopes, aggregation hints, caching, schema evolution, degraded states. |

Further engineering-layer documents (fast-path, steward startup) are deliberately open. See `docs/CONCEPT.md` section 10.

## Distributions

Evo is domain-neutral. A device ships as a distribution of evo: a catalogue declaration plus a plugin set plus branding. The first distribution is audio-player-shaped, shipped as Volumio.

## Status

Engineering layer, plus a working v0 steward skeleton. See `crates/evo` for the runnable binary and `crates/evo-plugin-sdk` for the plugin SDK.

## License

Apache 2.0. See [LICENSE](LICENSE).
