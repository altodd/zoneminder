# ZoneMinder redesign — document index

Start with **`ARCHITECTURE.md`** (diagnosis, design, plan). Then:

| Document | What it is |
|---|---|
| `ARCHITECTURE.md` | Root causes measured on the production box, the new design (passthrough segments + substream detection + browser-native playback), data model, phases, decisions, risks |
| `CUTOVER.md` | Phase 2 runbook: install next to ZoneMinder, storage tiering set-up, in-place import of the old events, TLS/Tailscale/Home Assistant, parallel-run checklist, the cut over itself, roll back, restore drill |
| `QUICK-WINS-PRODUCTION.md` | Ranked config/patch changes for the *current* ZoneMinder install; nothing applied yet, all need approval |
| `research/00-live-server-profile.md` | Read-only profile of 10.10.100.100 (hardware, GPU, monitors, DB sizes, filters, zones) taken 2026-09-27 |
| `research/01-online-research.md` | ZoneMinder GitHub issues/forums on the same pain points; how Frigate, moonfire-nvr, UniFi Protect, Blue Iris, Nx, mediamtx, go2rtc solve them; browser HEVC support in 2026; Quadro P2200 limits; 20 design implications |
| `research/02-alternative-architectures.md` | Deep dive into moonfire-nvr, Frigate (incl. nginx-vod config), go2rtc, UniFi Protect, mediamtx, browser playback, Rust ecosystem; recommended reference architecture and comparison table |
| `reviews/01-capture-pipeline-review.md` | Code review of ZoneMinder's C++ core: why each zmc costs a core and NVDEC is saturated, dead `AnalysisSource`, unscaled zones, decode modes, DB writes, zms playback |
| `reviews/02-web-events-review.md` | Code review + live timings of the PHP events/timeline/montage paths; exact SQL and EXPLAIN; 12 ranked root causes; patch-vs-replace verdict |
| `reviews/03-storage-db-review.md` | Frames/Stats/Events schema and triggers, zmfilter/zmaudit/AutoMove behaviour, deletion cost, proposed segment schema, migration approach |
| `reviews/04-zmng-code-review.md` | Independent code review of the new Rust core (`../zmng`) |
| `reviews/05-architecture-design-review.md` | Independent design review of `ARCHITECTURE.md` |
| `reviews/06-phase2-code-review.md` | Code review of the phase-2 work (notifications, health, backup/restore); findings fixed in-session |
| `reviews/07-coverage-audit.md` | Test-coverage audit after phase 2 (61 % lines) with the prioritized test list |
| `reviews/08-phase3-code-review.md` | Code review of the phase-3 work (previews, objects, transcode, PTZ, federation, audio, PWA); H1/M1–M7 and the lows fixed in-session |
| `reviews/09-phase3-architecture-review.md` | Architecture review against ARCHITECTURE.md at 23 cameras / 17 TB; the must-fix table's items are addressed in §10.5, the rest listed there as deferred |
| `reviews/10-coverage-audit-2.md` | Second test-coverage audit (72 % lines) after phase 3 |
| `TEST-DEPLOY-2026-09-28.md` | The side-by-side test install on securityserver (isolation, before/after, findings) |
| `reviews/11-field-review-2026-09-30.md` | Two days into the side-by-side test: UI/server/detection/zmNinjaNg findings, fixes deployed, production ZoneMinder findings, next steps and Review filter ideas |

Code: `../zmng/` (README inside). ZoneMinder fork (reference, phase-0 patches): `../zoneminder/`.
