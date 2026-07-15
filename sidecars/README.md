# Bundled sidecars

Cockpit Tools vendors and adapts the following MIT-licensed projects:

- `grok-register`: browser registration and account credential capture. The
  Cockpit adapter emits machine-readable progress and account events.
- `grok2api`: Grok Build/Web/Console account pool and OpenAI-compatible API.
  Cockpit manages its local lifecycle, secrets, imports, and client key.

The original license and README are retained in each sidecar directory. Build
artifacts under each `bin/` directory are generated and intentionally ignored.
