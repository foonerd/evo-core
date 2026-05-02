# evo-core: reference systemd material

This directory ships reference material distributions consume when
packaging the evo steward. Files here are **not** installed by evo-core;
evo-core has no package that drops a unit into `/lib/systemd/system/`.
Per [BOUNDARY.md](../../docs/engineering/BOUNDARY.md) sections 6 and 9,
packaging is distribution-owned. The framework specifies the unit
*shape*; the distribution ships the unit.

## Files

- `evo.service.example` — template systemd unit. Copy into the
  distribution's packaging tree (renaming to `evo.service`), fill in
  the distribution-owned choices marked in the file (especially
  `User=` / `Group=`), and ship the result on the target image.

## What the framework specifies

The template carries:

- The `ExecStart` path the framework expects (`/opt/evo/bin/evo`).
- The systemd directory primitives the framework relies on:
  `RuntimeDirectory=evo`, `StateDirectory=evo`,
  `ConfigurationDirectory=evo`. The steward verifies the modes and
  ownership of these per
  [PERSISTENCE.md](../../docs/engineering/PERSISTENCE.md) section 6.2.
- A tested baseline of hardening directives (`ProtectSystem=strict`,
  `ProtectHome=`, `PrivateTmp=`, `NoNewPrivileges=`, the kernel and
  control-group protections, `RestrictSUIDSGID=`, `RestrictNamespaces=`,
  `LockPersonality=`).

A distribution should preserve these unless it has a specific reason
to deviate. Deviations are the distribution's responsibility to
document in its own release notes.

## What the distribution chooses

- **Service identity** — `User=` / `Group=` (named account),
  `DynamicUser=`, or root. The framework observes only the effective
  UID; the user name and the account-creation mechanic are
  distribution choices. See
  [PERSISTENCE.md section 6.2](../../docs/engineering/PERSISTENCE.md)
  and
  [PLUGIN_PACKAGING.md section 5](../../docs/engineering/PLUGIN_PACKAGING.md).
- **Account-creation mechanics** — Debian postinst, OTA layer,
  image-build recipe, Yocto recipe, manual operator action. The
  framework has no opinion.
- **Additional hardening** — `CapabilityBoundingSet=`,
  `SystemCallFilter=`, cgroup memory / cpu / io limits, seccomp
  profiles, SELinux domains, AppArmor profiles. The template's
  hardening is a baseline, not a ceiling, per
  `PLUGIN_PACKAGING.md` section 5.
- **Init system** — whether to ship a systemd unit at all. A
  distribution targeting OpenRC, runit, launchd, or any other init
  ships an equivalent service definition for that system. The
  template here is one shape, not the only shape.
- **Activation policy** — whether to enable the unit at boot, mask
  it, wrap it in a distribution-specific multi-unit graph, or
  socket-activate.

## When the framework's requirements change

If a future evo-core release adds a directory the steward needs, an
environment variable it consumes, or a hardening directive that
becomes incompatible with a new feature, this template updates and
the release notes call out the change. Distributions that have forked
the template are expected to merge such changes; the framework does
not silently break consumer installations.
