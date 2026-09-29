# Development documentation

These pages are for people working on Rushls itself. User documentation is in the [parent directory](../README.md).

| Page | Covers |
| --- | --- |
| [Architecture](architecture.md) | Crate layout and responsibilities |
| [CI](ci.md) | What each CI job checks, and the tools it needs |
| [Releasing](releasing.md) | Release builds, tagging, and container publication |
| [Certificate renewal](certificate-renewal.md) | The Apple HLS test certificate workflow |
| [Load testing](load-testing.md) | The soak harness and its limits |
| [Configuration design](configuration-design.md) | Why the configuration is shaped the way it is |
| [GAP validation](gap-validation/README.md) | Player evidence for gap recovery, regression checks, and dated investigations |
| [HLS specification](resources/README.md) | The archived HLS draft that source comments cite |

## Records

Point-in-time reviews, kept for their reasoning. They describe the code as it was on their date.

- [Configuration ledger](configuration-ledger.md): the implementation work the configuration redesign implied.
- [Metrics review, 2026-09-14](reviews/metrics-2026-09-14.md)
- [Credential review, 2026-09-23](reviews/credentials-2026-09-23.md)
- [Native ingest parity](reviews/native-ingest-parity.md): what the FFmpeg-based ingest did, and what replaced it.
