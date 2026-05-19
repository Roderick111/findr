# Changelog

## [Initial Release] - 2026-05-19

- Unified search combining filename fuzzy matching and full-text content search
- Tiered ranking: filename prefix > contains > content match > fuzzy-only
- PDF text extraction for content search
- Two-layer auto-indexing (quick diff + weekly full rebuild)
- Inline type filters (e.g., "resume pdf")
- Raycast extension with Search Files and Rebuild Index commands
- Content match position scoring (matches near start of document rank higher)
- Recency-based tie-breaking within tiers
