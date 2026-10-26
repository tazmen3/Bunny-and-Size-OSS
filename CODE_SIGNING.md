# Code signing policy / Politique de signature du code

**Status — 26 October 2026: application to SignPath Foundation being prepared; not approved and not active.**

**Statut — 26 octobre 2026 : candidature auprès de SignPath Foundation en préparation ; aucune approbation et aucune signature active.**

## Current Windows releases / Versions Windows actuelles

The Windows binaries currently distributed with [Bunny and Size v0.3.0-rc.1](https://github.com/tazmen3/Bunny-and-Size-OSS/releases/tag/v0.3.0-rc.1) are **not digitally signed**. This applies to `bunny-and-size.exe`, `bunny-and-size-cli.exe` and the historical alias `sortsize.exe`. Windows may display `Unknown publisher` or a Microsoft Defender SmartScreen warning.

Les exécutables Windows de la version **v0.3.0-rc.1 ne sont pas signés**. L'éditeur peut être indiqué comme inconnu et SmartScreen peut afficher un avertissement. **Aucune archive existante, signature ou empreinte de cette release n'a été modifiée** par cette politique documentaire.

A code signing application is being prepared, but SignPath Foundation has **not yet been confirmed as the signing provider**, and no certificate or production signature has been issued for this project. The project's brand `TazmenWorld` is not a verified Authenticode publisher merely because it appears in PE metadata.

## Planned provider and attribution / Fournisseur envisagé et attribution

If the project is **accepted** into the SignPath Foundation program and actually starts signing releases through that service, the following attribution will apply:

> Free code signing provided by SignPath.io, certificate by SignPath Foundation

**This text describes a proposed future arrangement, not a currently provided service or an endorsement of existing releases.** Under this arrangement the certificate's publisher would be **SignPath Foundation**, not `TazmenWorld`.

**Cette mention est conditionnelle à une acceptation et à une signature effective ; elle ne signifie pas que les versions actuelles sont signées ni approuvées.**

## Roles and release approvals / Rôles et approbations

Proposed signing organization for this independently maintained project, subject to the provider's admission and role-approval requirements:

- **Project maintainer / source-code committer:** [@tazmen3](https://github.com/tazmen3).
- **Change reviewer / relecteur :** [@tazmen3](https://github.com/tazmen3) for externally proposed changes; the current project does not claim a separate independent review team.
- **Release and signing approver / approbateur de signature :** [@tazmen3](https://github.com/tazmen3), **proposed only**. There is currently no production signing account, approved signing request or active signing workflow.

Repository and signing access must use multi-factor authentication and the provider's required authorizations. Every eventual signing request will need an explicit authorized approval. Any additional reviewer or separation-of-duties requirement will be resolved with the provider before activation.

## Planned Windows release controls / Contrôles prévus

For future Windows releases, subject to provider approval:

- Build the binaries from a reviewed **public source snapshot** with a verifiable GitHub-hosted build process. Do **not** disclose private development history, unpublished files or credentials.
- Check Authenticode identity, certificate, required code-signing capabilities and timestamp for **all** distributed Windows executables, including the GUI, CLI and historical alias.
- Sign **before** calculating final release-package SHA-256 checksums, and verify the packages again after publication.
- Block the official Windows release path when required signatures are missing or invalid. An internal unsigned build is not a publicly signed release.
- Document any limitations honestly. A valid signature **does not guarantee** that SmartScreen warnings will disappear immediately or that a managed enterprise policy will allow execution.

No hosted signing pipeline is currently active. These are planned controls and technical preparations, not a claim of completed signed distribution.

## Privacy / Confidentialité

> This program will not transfer any information to other networked systems unless specifically requested by the user or the person installing or operating it.

The currently published application does not contain telemetry. It performs read-only storage analysis; optional remote owner resolution requires separate user consent. No user information is sent to a project-operated analytics service. Third-party or operating-system services invoked at the user's request are subject to their respective policies.

L'application publiée ne comporte pas de télémétrie. L'analyse du stockage est en lecture seule et la résolution distante des propriétaires nécessite un consentement séparé.

## Official downloads and verification / Téléchargements et vérification

Download only from the [official public GitHub releases](https://github.com/tazmen3/Bunny-and-Size-OSS/releases). Consult the [v0.3.0-rc.1 SHA-256 checksums](docs/releases/v0.3.0-rc.1/SHA256SUMS) and the [release limitations](docs/releases/v0.3.0-rc.1/PUBLIC_LIMITATIONS.md).

**Do not disable Microsoft Defender or SmartScreen globally to run this software.** Current Windows binaries are unsigned; this document is not a substitute for a valid digital signature.

**Ne désactivez pas globalement Microsoft Defender ou SmartScreen.** Cette politique ne remplace pas une signature numérique valide.

For the provider's requirements, see [SignPath Foundation conditions](https://signpath.org/terms).
