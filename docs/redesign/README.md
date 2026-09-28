# ZoneMinder redesign — document index

Start with **`ARCHITECTURE.md`** (diagnosis, design, plan). Then:

| Document | What it is |
|---|---|
| `ARCHITECTURE.md` | Root causes measured on the production box, the new design (passthrough segments + substream detection + browser-native playback), data model, phases, decisions, risks |
| `QUICK-WINS-PRODUCTION.md` | Ranked config/patch changes for the *current* ZoneMinder install; nothing applied yet, all need approval |
| `research/00-live-server-profile.md` | Read-only profile of 10.10.100.100 (hardware, GPU, monitors, DB sizes, filters, zones) taken 2026-09-27 |
| `research/01-online-research.md` | ZoneMinder GitHub issues/forums on the same pain points; how Frigate, moonfire-nvr, UniFi Protect, Blue Iris, Nx, mediamtx, go2rtc solve them; browser HEVC support in 2026; Quadro P2200 limits; 20 design implications |
| `research/02-alternative-architectures.md` | Deep dive into moonfire-nvr, Frigate (incl. nginx-vod config), go2rtc, UniFi Protect, mediamtx, browser playback, Rust ecosystem; recommended reference architecture and comparison table |
| `reviews/01-capture-pipeline-review.md` | Code review of ZoneMinder's C++ core: why each zmc costs a core and NVDEC is saturated, dead `AnalysisSource`, unscaled zones, decode modes, DB writes, zms playback |
| `reviews/02-web-events-review.md` | Code review + live timings of the PHP events/timeline/montage paths; exact SQL and EXPLAIN; 12 ranked root causes; patch-vs-replace verdict |
| `reviews/03-storage-db-review.md` | Frames/Stats/Events schema and triggers, zmfilter/zmaudit/AutoMove behaviour, deletion cost, proposed segment schema, migration approach |
| `reviews/04-zmng-code-review.md` | Independent code review of the new Rust core (`../zmng`) |
| `reviews/05-architecture-design-review.md` | Independent design review of `ARCHITECTURE.md` |

Code: `../zmng/` (README inside). ZoneMinder fork (reference, phase-0 patches): `../zoneminder/`.
