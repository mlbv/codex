# Model catalog history

Backend model catalogs continue to update capability metadata, tool descriptions, permission guidance, collaboration messages, and the surrounding model instructions. The bundled Personality section and default verbosity are applied after provider model selection. Explicit instruction files and explicit local catalogs retain their precedence.

The normal disk-backed model manager records upstream discrepancies in:

```text
$CODEX_HOME/model-catalog-changes/model-catalog-<SHA256>.md
```

Each report compares the upstream catalog's decoded `ModelInfo` fields with the catalog bundled in the running binary, before fallback merging and local personality customization. It lists added, removed, and changed models, then shows field-level unified diffs. Multiline instructions appear as readable text diffs. The header records the first observation time in UTC, client release, and hashes of the upstream and bundled catalogs.

The SHA-256 revision identifier hashes canonical catalog content: models are sorted, object keys are sorted, and arrays within each model keep their order. ETags, fetch times, and account identity are outside that content. Reordering models or object keys therefore reuses the same report, as does returning from revision B to a previously recorded revision A.

Network refreshes, cache loads, catalog originators, and restarted processes share this revision history. Reports are written completely before publication, and concurrent writers publish without replacing an existing revision. The filename is the durable deduplication record.

Network refreshes and eligible cached catalogs both enter the recorder. A report is created when the upstream catalog differs from the bundled catalog; a matching catalog creates no report. Its contents describe the upstream values, and the local personality customization applies afterward.

A failure to write a report is logged and model discovery continues with the received catalog.

Embedded callers using custom caches or disabled caching can choose a history location through `OpenAiModelsManager::with_upstream_changes_dir`. The normal disk-backed constructor selects the location above automatically.
