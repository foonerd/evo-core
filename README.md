# evo-core

Evo is a fabric for building appliance-class devices. A brand-neutral steward administers a declared catalogue of concerns, admits plugins that stock slots in that catalogue, and emits composed projections to any consumer.

This repository holds the steward, the plugin SDK, the plugin tooling, and the engineering-layer contracts that govern plugin authoring.

## Documents

| Document | Purpose |
|----------|---------|
| [docs/CONCEPT.md](docs/CONCEPT.md) | The fabric contract. Essence, steward, racks, shelves, plugins, subjects, relations, projections, happenings. Read first. |
| [docs/engineering/BOUNDARY.md](docs/engineering/BOUNDARY.md) | Framework/distribution boundary. Where evo-core ends and `evo-device-<vendor>` begins, the four contracts that cross the boundary, what evo-core must not contain, distribution integrator checklist. |
| [docs/engineering/PLUGIN_CONTRACT.md](docs/engineering/PLUGIN_CONTRACT.md) | The universal plugin contract. Rust trait and Unix-socket wire protocol, two transports of one contract. |
| [docs/engineering/PLUGIN_PACKAGING.md](docs/engineering/PLUGIN_PACKAGING.md) | Plugin manifest, identity, signing, filesystem layout on target, installation lifecycle, SDK and tooling. |
| [docs/engineering/VENDOR_CONTRACT.md](docs/engineering/VENDOR_CONTRACT.md) | Vendor contract. Actor taxonomy, namespace governance, vendor commitments and privileges, distribution relationships, revocation pathways. |
| [docs/engineering/LOGGING.md](docs/engineering/LOGGING.md) | Logging contract. Library, levels, default level, format, structured fields, logs-vs-happenings, plugin log integration. |
| [docs/engineering/SUBJECTS.md](docs/engineering/SUBJECTS.md) | Subject registry. Canonical identity, external addressings, announcements, equivalence and distinctness claims, reconciliation, merges and splits, provenance, operator overrides, persistence, happenings. |
| [docs/engineering/RELATIONS.md](docs/engineering/RELATIONS.md) | Relation graph. Typed and directed subject-to-subject connections, catalogue-declared grammar, multi-claimant assertions, scoped walks, cardinality handling, merge and split cascade, operator overrides, persistence, happenings. |
| [docs/engineering/PROJECTIONS.md](docs/engineering/PROJECTIONS.md) | Projection layer. Structural and federated queries, pull and push, catalogue-declared shapes, plugin contribution composition rules, subscription scopes, aggregation hints, caching, schema evolution, degraded states. |
| [docs/engineering/CUSTODY.md](docs/engineering/CUSTODY.md) | Custody ledger. Active warden-held work keyed by `(plugin, handle_id)`, record model, UPSERT semantics and the take/report race, `LedgerCustodyStateReporter` as the single integration point, ordering guarantees, access surfaces, invariants. |
| [docs/engineering/HAPPENINGS.md](docs/engineering/HAPPENINGS.md) | Happenings bus. Live notification surface for fabric transitions, current custody variants, `#[non_exhaustive]` variant contract, broadcast semantics, ordering relative to ledger writes, payload policy, integration points. |
| [docs/engineering/FAST_PATH.md](docs/engineering/FAST_PATH.md) | Fast-path mutation channel. Catalogue-declared commands against active wardens, rack/subject addressing, budgets, ordering, idempotency, relationship to projections and happenings, audit. |
| [docs/engineering/STEWARD.md](docs/engineering/STEWARD.md) | The steward process. Module structure, admission contracts, client-facing and plugin-facing protocols, shared state, concurrency model, configuration, deferred capabilities, invariants. |

## Distributions

Evo is domain-neutral. A device ships as a distribution of evo in its own `evo-device-<vendor>` repository: a catalogue declaration plus a plugin set plus branding plus frontend plus packaging. The framework imposes no upper bound on how many distributions exist or which vendors they target. The first distribution is `evo-device-volumio`, which uses Volumio's existing device functions as its reference feature set. See [docs/engineering/BOUNDARY.md](docs/engineering/BOUNDARY.md) for the boundary contract.

## Contributing

Developer workflow, prerequisites, local running, test commands, and repository conventions are documented in [DEVELOPING.md](DEVELOPING.md). Start there if you just cloned the repository.

## Status

Engineering layer plus a running v0 steward. Admits singleton respondents and wardens (in-process or over a Unix socket), maintains subject and relation registries, composes projections on demand, tracks active custodies in a ledger, and emits happenings on a bus. See `crates/evo` for the runnable binary, `crates/evo-plugin-sdk` for the plugin SDK, and `crates/evo-example-echo` / `crates/evo-example-warden` for reference plugins.

## License

Apache 2.0. See [LICENSE](LICENSE).
