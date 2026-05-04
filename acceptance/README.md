# acceptance/

Target descriptors and scenario plans for the `evo-acceptance` harness.

This directory ships publicly as a worked example. The framework's own pi5-prototype target descriptor and the v0.1.12 scenario plan are present so adopters building a distribution can read a complete, runnable example before authoring their own.

Three audiences:

- **Framework operators** validating a release against the framework's own reference hardware run the descriptor + plan as-is (after supplying a `<name>.local.toml` overlay with their machine's connection details — see §Local overrides).
- **Distribution authors** ship their own target descriptors (`targets/<their-device>.toml`) and scenario plans (`plans/<their-release>.toml`) for their devices, alongside their own synthetic plugins. The framework's plan is a structural template; its synthetic plugins target framework-only acceptance shelves and are not appropriate for distribution-tier validation.
- **Plugin authors** browsing the directory get a worked example of the descriptor + plan TOML shapes the harness consumes.

The harness binary itself lives in `crates/evo-acceptance/` (the reusable runner / parser / inspector / report writer); this directory carries the data files the runner consumes.

## Layout

- `targets/<name>.toml` — target descriptors (arch, connection model, declared capabilities). One per acceptance target. Committed.
- `targets/<name>.local.toml` — sibling overlay supplying machine-specific connection details (ssh host, user, key path). Gitignored, never committed; see §Local overrides below.
- `plans/<release>.toml` — per-release scenario plans (T1-T5 tiers).
- `CAPABILITIES.md` — running roster of canonical capability names referenced by descriptors and scenarios.

## Local overrides

Target descriptors are split into two files so the committed file can describe a class of hardware (capabilities, arch, connection model) without leaking machine-specific details (lab IP, login user, ssh key path) of any one engineer's prototype.

The committed `targets/<name>.toml` carries the literal sentinel `REPLACE_ME` in every machine-specific field. The harness loader parses the committed base, then if a sibling `targets/<name>.local.toml` exists it is merged on top field-by-field. After merge the loader refuses any descriptor that still carries the sentinel and points the operator at the expected overlay path.

`acceptance/targets/*.local.toml` is gitignored (see the workspace `.gitignore`). Never commit a `.local.toml`.

Worked example for `pi5-prototype`:

```toml
# acceptance/targets/pi5-prototype.local.toml
[ssh]
host     = "10.0.0.42"
user     = "lab-user"
key_path = "~/.ssh/id_ed25519"
```

Only the fields that differ from the base need to appear in the overlay; missing fields fall through to whatever the committed file declares (notably `port = 22`).

## Usage from the repo root

```bash
cargo run -p evo-acceptance -- inspect \
    --target=acceptance/targets/pi5-prototype.toml \
    --output=/tmp/pi5-inspection.md \
    --output-json=/tmp/pi5-inspection.json

cargo run -p evo-acceptance -- run \
    --target=acceptance/targets/pi5-prototype.toml \
    --plan=acceptance/plans/v0.1.12.toml \
    --release=v0.1.12 \
    --rc=1 \
    --output=/tmp/v0.1.12-rc.1-pi5-readiness.md
```

The harness reads target + plan as data; the runner sequences scenarios in tier order; the report module emits a Markdown readiness report (`run` subcommand) or a Markdown inspection report (`inspect` subcommand) plus an optional JSON sidecar for machine consumption.
