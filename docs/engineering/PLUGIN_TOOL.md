# evo-plugin-tool (implementation contract)

Status: **normative** for `crates/evo-plugin-tool` (GAPS [20] **implemented** in v1 for `lint` / `sign` / `verify` / `pack` / `install`; this document is the long-term build contract, and a future crate `README` may still defer here).
Audience: evo-core implementers, packagers, CI and UI integrators. Packaging narrative remains in `PLUGIN_PACKAGING.md` sections 7 and 9.

The steward binary is `evo` (`/opt/evo/bin/evo`). The SDK CLI is **`evo-plugin-tool`** in the same directory, built from the workspace crate of the same name. It **must** call **`evo_trust`’s** signing and verification **APIs** (not reimplemented message formats).

---

## 1. v1 subcommand set

**Choice B (full author workflow in v1):** implement **`lint`**, **`sign`**, **`verify`**, **`pack`**, and **`install`** in the first released version. **`uninstall`** and **`purge`** stay **Phase 5** per `GAPS.md` until the operator shelf and lifecycle are aligned (see `PLUGIN_PACKAGING.md` §7).

---

## 2. Bundle directory name vs `plugin.name`

- **`manifest.plugin.name` is the source of truth** (reverse-DNS per `PLUGIN_PACKAGING.md` §4).
- After reading `manifest.toml`, the **install** path (and the effective bundle on disk) **must** use the name **`plugin.name`**. If the archive or stage directory’s **top-level** folder name **differs** from `plugin.name`, the tool **renames** (or rewrites the path to) **`plugin.name`** as part of promotion, then places under the target search root, e.g. `.../plugins/<plugin.name>/`. Do **not** leave a final tree whose directory name disagrees with the manifest.

---

## 3. `install` sources (versatile)

**v1 supports** all of:

- A **local path** to an unpacked bundle directory, or a path under **`plugin-stage/`** (see `PLUGIN_PACKAGING` §7).
- A **local file** in one of the **pack** formats (this document section 7 and `PLUGIN_PACKAGING` §9).
- A **http(s) URL** to a bundle: full client (TLS, **redirects** within reason, **timeouts**, **size limit** to be chosen in code and documented in `--help`; **no** silent unbounded download).

Failing operations **must not** leave a **partial** or half-valid tree inside any configured **`search_roots`**.

---

## 4. Ownership and mode (`--chown`)

- **`chown` / owner metadata** on the final plugin directory: **optional**. Expose a flag such as `--chown user:group` (exact spelling in clap) when the operator runs the tool with sufficient privilege; if omitted, the tool does **not** change ownership (unpack/promote runs with the process’s effective user).

---

## 5. `sign` — key material (match `evo_trust`)

- **Same algorithm and message** as the steward: **`signing_message`**, ed25519, 64-byte **`manifest.sig`**, as implemented in `evo_trust` (and exercised by `crates/evo-trust/tests/verify.rs`).
- **Private key:** PEM on disk, passed with **`--key`**, in the form **`evo_trust` and the test harness already use** (see `load_trust_root` / signing helpers). **Encrypted PEM** is out of v1 **unless** already supported by a shared helper in `evo_trust` at implementation time; otherwise document "clear PEM only for v1".

---

## 6. `verify` — trust root behaviour (follow the steward)

- **Default** trust behaviour **matches the shipped steward**: same default directories as `StewardConfig` / `PLUGIN_PACKAGING` §5 — **`/opt/evo/trust`**, **`/etc/evo/trust.d`**, revocations path, **`degrade_trust`**, **union** of keys, **`*.meta.toml`** per key.
- **Override flags** in the CLI (paths for opt + etc trust dirs, revocations, strict/degrade) so **CI and laptops** can pass temp dirs **without** a full FHS layout.

---

## 7. `pack` — archive formats

- **Only** the formats in `PLUGIN_PACKAGING` §9: **`.tar.gz` / `.tgz`**, **`.tar.xz` / `.txz`**, **`.zip`**; default format **`.tar.gz`** when the implementation must choose.
- **Sniffing** and extension rules: as in that section (magic bytes, extension, `file(1)`-class behaviour where practical).

---

## 8. Exit codes (CI and UI)

Stable across releases for the same class of error (numeric values fixed in the first public release; document in `--help` and this table).

| Code | Meaning |
|------|--------|
| `0` | Success. |
| `1` | Usage / flags / bad CLI input (incl. manifest parse where not trust-related). |
| `2` | **Trust / signature / key authorisation** (`evo_trust` / verification failure, wrong manifest.sig length, etc.). |
| `3` | **I/O and filesystem** (unreadable path, full disk, failed rename, atomic promote failure). |
| `4` | **Network** (URL fetch: DNS, TLS, timeout, size exceeded, non-2xx) when `install` fetches. |

(If a subcommand has no network path, `4` is unused in that run.)

---

## 9. `pack` default file name and version in the name

- **`--out` always wins** when the operator passes it.
- If **`--out` is omitted** and the implementation invents a default filename: **accept both** conventions: prefer **`<plugin.name>-<version>.<ext>`** when the manifest exposes a suitable **version** field for the archive name; else **`<plugin.name>.<ext>`** (version segment omitted). The implementation documents which manifest field supplies `<version>` (e.g. plugin or package version per schema).

---

## 10. Binary and packaging location

- Built as **`evo-plugin-tool`**; installed next to the steward, **`/opt/evo/bin/evo-plugin-tool`**, as in the filesystem tree in `PLUGIN_PACKAGING` §3.

---

## 11. Diagrams

The following figures mirror this document; they are not additional normative rules.

### 11.1. v1 author workflow (subcommands)

```mermaid
flowchart LR
  subgraph dev["Author machine"]
    D[Plugin bundle directory<br/>manifest.toml + artefact]
    D --> L[lint]
    L --> S[sign --key]
    S --> P[pack]
  end
  P -->|archive| ship[Ship .tar.gz / .tar.xz / .zip]
  ship --> I[install]
  subgraph check["Before promote"]
    I --> V[verify]
  end
  V -->|OK| T["search_root / plugin.name"]
```

`install` runs the same trust checks as `verify` (and implied lint via manifest/artefact load). Operators may run `verify` alone in CI on an unpacked tree.

### 11.2. install resolution (source → promote)

```mermaid
flowchart TD
  SRC{Source?}
  SRC -->|local directory| LD[Use bundle path]
  SRC -->|local archive| EX1[Extract to temp]
  SRC -->|https or http| DL[Download to temp, size cap]
  DL --> EX1
  EX1 --> B[Single top-level bundle dir]
  LD --> M[Read manifest plugin name]
  B --> M
  M --> N{Top dir name matches manifest?}
  N -->|no| R[Promote using manifest name for path]
  N -->|yes| R
  R --> V[verify trust and signature]
  V -->|fail| X[No writes under search roots]
  V -->|ok| C[Copy to search root under plugin name]
  C --> O[Optional chown on Unix]
```

### 11.3. Exit code routing (rough map)

Labels avoid `::` in raw node text. The code still checks the real Rust types (`TrustError`, `ureq::Error`, `io::Error`).

```mermaid
flowchart TD
  E[anyhow Error] --> T{is TrustError}
  T -->|yes| c2[exit 2]
  T -->|no| U{is ureq Error}
  U -->|yes| c4[exit 4]
  U -->|no| I{is io Error}
  I -->|yes| c3[exit 3]
  I -->|no| c1[exit 1]
```

---

## 12. Related

- `GAPS.md` [20] — v1 `lint` / `sign` / `verify` / `pack` / `install` are implemented; `uninstall` / `purge` remain Phase 5.
- `evo_trust` — `signing_message`, `verify_out_of_process_bundle`, `load_trust_root`, `RevocationSet`, key meta.
- `StewardConfig` / `docs/engineering/CONFIG.md` / `SCHEMAS.md` §3.3 — default paths for `verify` parity.
- `PLUGIN_PACKAGING` §7 (Strategy A/B), §9 (subcommands, archive rules).
