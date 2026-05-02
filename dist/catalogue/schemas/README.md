# Catalogue schemas — in-tree skeleton

This directory is the in-tree skeleton of the catalogue-schemas
publication PLUGIN_PACKAGING.md §10 describes. It exists so a
distribution authoring its own catalogue has a worked example
of the on-disk shape inside the framework source tree before
the standalone sibling repository (`evo-catalogue-schemas`)
materialises.

What lives here today:

- `<rack>/<shelf>.v<N>.toml` — one TOML file per `(rack,
  shelf, shape version)` tuple. The TOML carries the shelf's
  request types, payload shapes, and acceptance criteria for a
  plugin claiming to satisfy that shape. The framework
  validates a plugin's manifest `target.shelf` and
  `target.shape` against the catalogue at admission; this
  directory is what a plugin author reads to know what code
  contract that admission gate translates to.
- `<rack>/<shelf>.v<N>.changes.md` — optional change-history
  pointer for a shelf shape. Authored when a shelf bumps from
  shape N to N+1 (with N kept under `Shelf.shape_supports`
  during the migration window per `CATALOGUE.md` §4.2).

Today this skeleton ships only `example/echo.v1.toml` for the
example respondent shelf the in-tree reference distribution
uses. Distributions declaring their own racks and shelves
(audio, sensors, signage, …) ship their own schemas under
their own per-distribution catalogue-schemas directory or
publish a sibling repo of their own.

## Future sibling repository

The framework intends to spin out a `foonerd/evo-catalogue-schemas`
repository hosting the brand-neutral framework-tier shelves
(those under the `org.evoframework.*` namespace) as a separate
versioned surface. That spin-out is a follow-on org-level
action; the in-tree skeleton here remains as the canonical
reference for the on-disk shape regardless of where individual
schemas are published.

## Validation tooling

`evo-plugin-tool catalogue lint <path>` (already shipped under
[evo-core's `evo-plugin-tool`](../../../../crates/evo-plugin-tool/))
validates the catalogue document grammar (rack and shelf
declarations, subject-type vocabulary, predicate constraints,
inverse consistency, the `shape_supports` discipline). A
companion `validate-shelf-schema` subcommand reading the
per-shelf schema files in this directory is on the same
follow-on cycle as the sibling repo spin-out; the plugin
author's local-build feedback loop today is `cargo build` plus
the `evo-plugin-tool catalogue lint` pass against the
distribution's own catalogue.
