# Vendor Contract

Status: engineering-layer contract for vendors on evo.
Audience: vendor organisations (Sony, Volumio, FiiO, streaming services, component manufacturers), evo project maintainers, distributions, operators.
Vocabulary: per `docs/CONCEPT.md`. Plugin runtime contract in `PLUGIN_CONTRACT.md`. Plugin artefact contract in `PLUGIN_PACKAGING.md`.

This document defines what a VENDOR is on evo, what commitments a vendor makes, and what privileges a vendor receives in return. It sits alongside the plugin contracts but addresses a different question: `PLUGIN_CONTRACT.md` governs how a plugin behaves at runtime; `PLUGIN_PACKAGING.md` governs how a plugin ships as an artefact; this document governs how an organisation relates to the fabric.

## 1. Actor Taxonomy

Evo recognises five actor positions. Each has a defined relationship to the fabric. The word "vendor" in this document refers only to position 3.

| Position | Role | Trust root relationship | Typical examples |
|----------|------|-------------------------|------------------|
| 1. Evo project | Maintains the steward, fabric, SDK, tooling. | Owns `/opt/evo/trust/evo.pem`. Root of the trust hierarchy. | The evo-core project itself. |
| 2. Distribution | Curates a catalogue + plugin set + branding; ships as a product. | Ships `/opt/evo/trust/distribution/<distribution>.pem`. | Volumio (audio-player distribution). |
| 3. Vendor | Commercial or organisational entity that signs plugins under a namespace they control, carrying formal commitments. | Ships `/opt/evo/trust/vendor/<vendor>.pem` (if distribution-bundled) or is installed by operator into `/etc/evo/trust.d/<vendor>.pem`. | Sony, FiiO, Spotify, a DAC manufacturer, a streaming service. |
| 4. Individual author | Single person or small team signing plugins under their own namespace. | Operator installs key into `/etc/evo/trust.d/`. No vendor agreement, no formal commitments beyond licence. | Independent plugin authors, community contributors. |
| 5. Operator | The person running the device. Installs keys, grants trust, decides what runs. | Sole authority on `/etc/evo/trust.d/` and `/etc/evo/revocations.toml`. | End user, system administrator, integrator deploying devices. |

A DISTRIBUTION is always also a VENDOR at the distribution level. Volumio is a distribution (it ships a curated catalogue and plugin set) and a vendor (it signs its own plugins under `org.volumio.*`). The distinction matters when Volumio ships a plugin authored by FiiO: Volumio is the distribution, FiiO is the vendor, and the plugin ships under one or the other's namespace per the joint-trust arrangement (Section 7).

**Deployment stages and signing (dev, test, prod, open vs closed products)** are a **distribution and packaging** concern, not a new steward string in `evo.toml`. The reference model, tables, and Mermaid figures live in `BOUNDARY.md` section 6.2. `CONFIG.md` and `PLUGIN_PACKAGING` point there.

This document's subject is position 3. Positions 1, 2, 4, 5 appear where they interact with vendors.

## 2. What Constitutes a Vendor

A vendor is an entity that:

1. Registers a VENDOR PROFILE with the evo project or with a distribution that ships their plugins.
2. Claims one or more reverse-DNS namespaces they commit to govern.
3. Holds one or more ed25519 signing keys whose public halves are enrolled in at least one trust root.
4. Makes the formal commitments listed in Section 5 of this document.
5. Receives the formal privileges listed in Section 6 of this document.

An organisation that ships a plugin without registering is not a vendor - they are an individual author for trust purposes, regardless of their commercial nature. Vendor status is a formal status, not an inferred one.

An individual author becomes a vendor when they make the formal commitments. Nothing prevents a small team or a single developer from being a vendor if they are willing to carry the obligations. Conversely, a large organisation that declines the obligations is treated as an individual author.

## 3. Vendor Profile

A vendor profile is a document submitted to the evo project or a distribution. It contains:

| Field | Purpose |
|-------|---------|
| Legal entity name | The name under which the vendor operates and signs agreements. |
| Contact: technical | Primary technical liaison for security disclosures, plugin issues, protocol questions. |
| Contact: security | Dedicated security disclosure contact. Separate from technical for response-time reasons. |
| Contact: business | For commercial coordination, certification discussions, deprecation notices. |
| Claimed namespaces | Reverse-DNS prefixes the vendor commits to govern. Multiple prefixes permitted (e.g. `com.sony.audio.*` and `com.sony.network.*`). |
| Signing key(s) | Public ed25519 keys, with fingerprints, each with declared purpose and rotation policy. |
| Support period declaration | Default minimum support period for plugins the vendor ships under this profile. May be overridden per-plugin in the manifest. |
| Security response SLA | Commitment on time-to-acknowledgement and time-to-patch for reported vulnerabilities. |
| Target trust classes | Maximum trust class the vendor expects to claim. Determines review depth at enrollment. |
| Jurisdiction | Legal jurisdiction under which the vendor operates. Relevant for content licensing, export controls, DRM. |

The profile is reviewed by the enrolling authority (evo project or distribution). Approval results in key enrollment; rejection results in a documented reason.

## 4. Namespace Governance

A namespace is a reverse-DNS prefix under which a vendor signs plugins.

### Namespace claim

A vendor claims a namespace by registering it in their profile. Claims are resolved by the enrolling authority:

1. Namespaces matching the vendor's verifiable domain ownership (DNS, legal name) are granted without dispute (Sony claiming `com.sony.*`).
2. Namespaces that do not match verifiable ownership require additional review (a vendor claiming a namespace they do not obviously own).
3. Conflicting claims for the same namespace are resolved in favour of the earlier verifiable ownership.

### Sub-namespace delegation

A vendor may delegate sub-namespaces to other vendors. Example: Sony reserves `com.sony.*` and delegates `com.sony.audio.<partner>.*` to a partner vendor. Delegation is expressed in the vendor profile as sub-namespace grants; the steward sees only the final prefix and the key authorised for it.

### Namespace retirement

A vendor may voluntarily retire a namespace. Retirement revokes the vendor's authority to sign new plugins under that prefix; existing plugins continue to function until individually revoked or updated by a successor vendor.

## 5. Vendor Commitments

A vendor commits to the following. Failure to honour these commitments is grounds for enrollment revocation.

### Runtime commitments

- Plugins signed by the vendor honour the plugin contract (`PLUGIN_CONTRACT.md`) in full.
- Plugins do not attempt plugin-to-plugin communication, steward introspection, or OS privilege escalation beyond their declared trust class.
- Plugins report state truthfully. A warden that reports "playing" when silent, a respondent that claims success on failure, or a factory that announces instances that do not exist constitutes a contract violation.
- Plugins release resources acquired at `load` when `unload` is delivered.

### Lifecycle commitments

- The vendor supports each shipped plugin for at least the support period declared in the plugin's manifest or the vendor profile default, whichever is longer.
- The vendor issues timely updates for security vulnerabilities within the declared security-response SLA.
- The vendor provides at least one release cycle of deprecation notice before removing plugins operators rely on.
- The vendor documents breaking changes between plugin versions.

### Security commitments

- The vendor discloses known vulnerabilities in shipped plugins to the enrolling authority within the declared SLA.
- The vendor rotates signing keys on a defined schedule and on any suspected compromise.
- The vendor maintains a security disclosure contact that responds within the declared SLA.
- The vendor does not ship plugins that collect operator data without declaring the collection in the plugin manifest and README.

### Operator-facing commitments

- The vendor declares all outbound network destinations used by the plugin in the manifest.
- The vendor declares all authentication requirements, including user-facing flows, in the plugin manifest.
- The vendor declares all hardware dependencies.
- The vendor publishes plugin documentation sufficient for an operator to understand what the plugin does, what data it handles, and how to remove it cleanly.

### Fabric commitments

- The vendor respects shelf shape versioning. Plugins declare the shape versions they satisfy; the vendor does not ship plugins targeting versions the steward does not speak.
- The vendor does not attempt to influence steward behaviour through channels other than the declared plugin contract.
- The vendor cooperates with revocation requests from the enrolling authority, including emergency revocation.

## 6. Vendor Privileges

In return for the commitments in Section 5, a vendor receives:

### Namespace authority

- Exclusive right to sign plugins under the claimed namespace(s).
- Sub-namespace delegation authority.
- Namespace appears in the enrolled trust root's authorisation metadata.

### Trust class access

- Authority to sign plugins up to the trust class approved in the vendor profile.
- `standard` and `privileged` classes available to vendors by default; `platform` class reserved for the evo project and by special arrangement to distributions.
- Upgrade path from `standard` to `privileged` based on demonstrated track record (per distribution or evo project policy).

### Distribution relationships

- Eligibility to have plugins included in distributions (Volumio, other distributions).
- Access to distribution-specific shelf schemas and early notice of shelf shape changes.
- Co-signing arrangements (Section 7) where the distribution adds its trust to the vendor's.

### Support

- Direct technical contact with evo project maintainers or distribution packagers for protocol-level questions.
- Early access to protocol change proposals that affect their plugins.
- Access to a vendor-facing issue tracker for plugin-fabric interaction problems.

### Fabric identity

- Vendor name appears in admission happenings, enabling operators to see which vendor's plugin admitted or refused.
- Vendor documentation linked from operator-facing plugin listings.

## 7. Distribution and Vendor Joint Trust

A distribution may ship plugins authored by vendors. Three arrangements exist:

### Arrangement A: Vendor-signed, distribution-bundled

The plugin ships signed by the vendor's key, under the vendor's namespace, bundled in the distribution's artefact.

| Aspect | Detail |
|--------|--------|
| Plugin namespace | Vendor's (`com.vendor.*`). |
| Signing key | Vendor's. |
| Trust root | Vendor's key in `/opt/evo/trust/vendor/` or `/etc/evo/trust.d/`. |
| Distribution commitment | Distribution has validated the vendor and committed to shipping the vendor's key with their distribution artefact. |
| Revocation path | Primary: vendor. Secondary: distribution may force-revoke by shipping an update that removes the key. Tertiary: operator may revoke locally. |
| Example | Volumio ships FiiO's DAC driver plugin, signed by FiiO, under `com.fiio.dacs.*`. |

### Arrangement B: Distribution-signed, vendor-authored

The plugin is authored by the vendor but signed by the distribution under the distribution's namespace. The vendor's role is visible in the manifest (author field) but not in the signing chain.

| Aspect | Detail |
|--------|--------|
| Plugin namespace | Distribution's (`org.volumio.*`). |
| Signing key | Distribution's. |
| Trust root | Distribution's key. |
| Distribution commitment | Distribution has validated the vendor's code and accepts primary responsibility. |
| Revocation path | Distribution only. |
| Example | Volumio adopts a community-contributed plugin after review and ships it under `org.volumio.community.*`. |

### Arrangement C: Co-signed

The plugin carries two signatures: vendor's and distribution's. Both trust roots must verify. The steward refuses admission if either fails.

| Aspect | Detail |
|--------|--------|
| Plugin namespace | By agreement; typically vendor's. |
| Signing keys | Both vendor's and distribution's. |
| Trust root | Both keys present. |
| Distribution commitment | Joint endorsement. |
| Revocation path | Either party may unilaterally revoke. Either revocation disables the plugin. |
| Example | A high-trust integration where both Sony and Volumio vouch for the plugin's integrity. |

The plugin manifest declares which arrangement applies. The steward's admission logic handles all three uniformly: try every key in the trust root, admit if all required signatures verify.

## 8. Certification (Future)

Certification is a possible future tier of vendor status. At v1 of this contract, no certification program exists; the contract is deliberately flat.

When certification is introduced, it will layer on top of vendor status, not replace it. A certified vendor has undergone additional review (security audit, code review, interoperability testing) and receives additional privileges (platform-class eligibility, prominent placement in operator-facing listings, etc.). Certification details will be defined in a separate document when the program is ready.

## 9. Revocation Pathways

Revocation is how a trust relationship ends. Four pathways exist.

### Voluntary vendor revocation

The vendor revokes one of their own plugins. Reasons: security vulnerability, deprecation, withdrawal from a service.

1. Vendor publishes a revocation entry listing the install digest(s) to revoke.
2. Distributions distribute the revocation to their users via regular update channels.
3. Operators may also add revocations directly to `/etc/evo/revocations.toml`.
4. Steward refuses the revoked plugin at next load.

### Enrollment revocation

The enrolling authority (evo project or distribution) revokes a vendor's enrollment. Reasons: repeated commitment violations, security incident response failure, namespace dispute resolved against the vendor.

1. Authority removes the vendor's key from the trust root it controls.
2. Plugins signed by that key fail verification at next load.
3. Authority publishes the revocation reason.
4. Vendor's existing plugins are not retroactively invalidated unless the install digests are explicitly revoked.

### Operator revocation

The operator revokes a specific plugin or vendor key locally.

1. Operator adds an entry to `/etc/evo/revocations.toml` (by digest) or removes the vendor's key from `/etc/evo/trust.d/` (by key).
2. Steward refuses the affected plugins at next load.

### Emergency revocation

For active security incidents, the evo project or a distribution may issue an emergency revocation that propagates through urgent update channels distinct from regular updates. The revocation is effective immediately upon receipt; the steward refuses the plugin without waiting for operator action.

Emergency revocations require a documented post-incident report within 30 days.

## 10. What the Fabric Does Not Handle

The fabric is agnostic to commercial matters. The following are explicitly outside the fabric's concern and must be handled inside the vendor's plugin if the vendor requires them:

- Billing, subscription management, payment processing.
- Licence enforcement, time-limited trials, seat limits.
- DRM, content protection, entitlement servers.
- User account management for the vendor's own service.
- Analytics, telemetry, usage reporting to the vendor's infrastructure.

If a vendor's plugin requires any of these, the plugin declares it in the manifest (for operator-facing transparency), implements it internally, and handles all associated state (credentials, entitlements, user accounts) using the per-plugin `credentials/` and `state/` directories the fabric provides.

The fabric never participates in license decisions. A plugin's `load` verb may return "unauthorised" as a recoverable error, and the plugin may request user interaction to collect credentials; beyond that, commerce is the plugin's problem.

## 11. Vendor Responsibilities to Operators

A vendor's plugin runs on an operator's device. The vendor owes the operator:

- Clear disclosure, in the manifest and in documentation, of what data the plugin collects and where it is sent.
- Clear disclosure of outbound network destinations.
- A working uninstall path that leaves the operator's device in a clean state.
- A response contact for security concerns.
- Published documentation sufficient to operate the plugin independently of the vendor's website (read: the documentation is in the plugin artefact, not only on a web server the vendor may take down).

## 12. Vendor Responsibilities to the Fabric

A vendor's plugins run inside the fabric. The vendor owes the fabric:

- Adherence to the plugin contract.
- No attempts to coordinate with other plugins, introspect the steward, or escalate privilege.
- Truthful manifests - declared trust class, declared resource use, declared capabilities match reality.
- Cooperation with revocation requests from the enrolling authority.
- Timely security disclosures within the declared SLA.

## 13. Relationship to Existing Contracts

This document does not redefine any runtime or packaging behaviour. It defines the ORGANISATIONAL RELATIONSHIP within which runtime and packaging occur.

| Contract | Governs |
|----------|---------|
| `PLUGIN_CONTRACT.md` | How a plugin behaves at runtime. |
| `PLUGIN_PACKAGING.md` | How a plugin ships as an artefact. |
| `VENDOR_CONTRACT.md` (this document) | How an organisation enrolls, signs, and operates as a vendor. |

A plugin shipped by a vendor satisfies all three. A plugin from an individual author satisfies the first two and, for trust purposes, is treated as vendor-less (individual author position).

## 14. Versioning of This Contract

This document describes vendor contract version 1. Subsequent versions may add categories (e.g. certification tiers), refine SLA expectations, or adjust namespace governance. Major revisions require vendor profiles to be re-reviewed; minor revisions are additive.

## 15. What This Document Does Not Define

- Specific SLA timings (time-to-acknowledge, time-to-patch). Those are negotiated at enrollment and recorded in the vendor profile.
- The vendor profile submission process (where to send it, review timeline). That is an operational matter for the evo project and distributions.
- Distribution-specific vendor programs. Distributions may layer additional requirements and privileges on top of this contract.
- Legal text of any vendor agreement. This document is an engineering contract; legal agreements are separate documents that reference it.
- Pricing, commercial terms, or revenue share. None of these exist at the fabric level.
