# Catalogue schemas — in-tree worked example

The canonical home for brand-neutral framework-tier catalogue
schemas (the `org.evoframework.*` namespace) is the spin-out
repository **[`foonerd/evo-catalogue-schemas`](https://github.com/foonerd/evo-catalogue-schemas)**.
This directory's role is **a single worked-example reference
for the on-disk schema shape** so a distribution authoring its
own per-distribution catalogue-schemas tree (or contributing to
the spin-out repo) has a checked-in TOML to read alongside the
contributor docs.

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

## Sibling repository

The brand-neutral framework-tier shelves (every shelf under
`org.evoframework.*`) live in
[`foonerd/evo-catalogue-schemas`](https://github.com/foonerd/evo-catalogue-schemas).
Plugin authors and distribution maintainers pin a specific tag
of the sibling repo in their CI / build manifests; distribution
packages bundle the schemas at
`/usr/share/evo-catalogue-schemas/` so plugin authors can
validate locally against the installed copy.

## Validation tooling

`evo-plugin-tool catalogue lint <path>` validates a catalogue
document's rack / shelf / subject grammar.

`evo-plugin-tool catalogue validate-shelf-schema` validates
every per-shelf schema TOML file under a schemas tree.
Resolution cascade:

1. `--schemas-path=<dir>` flag.
2. `$EVO_SCHEMAS_DIR` environment variable.
3. `/usr/share/evo-catalogue-schemas/` (distribution-installed).

The framework runtime never consumes the per-shelf schemas;
they are author-time / CI-time artefacts only. The steward
validates against the catalogue document at admission, not
against the per-shelf shape contracts.
