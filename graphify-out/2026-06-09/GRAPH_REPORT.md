# Graph Report - toki  (2026-06-09)

## Corpus Check
- 74 files · ~148,048 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1548 nodes · 3079 edges · 73 communities (61 shown, 12 thin omitted)
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 26 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `8c8022f4`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Admin gRPC API|Admin gRPC API]]
- [[_COMMUNITY_Client egui App|Client egui App]]
- [[_COMMUNITY_Server Audio Relay|Server Audio Relay]]
- [[_COMMUNITY_Admin DB Layer|Admin DB Layer]]
- [[_COMMUNITY_Community 4|Community 4]]
- [[_COMMUNITY_Client Config Types|Client Config Types]]
- [[_COMMUNITY_Admin Protobuf (TS)|Admin Protobuf (TS)]]
- [[_COMMUNITY_Admin Auth & Cookies|Admin Auth & Cookies]]
- [[_COMMUNITY_Admin Config Loading|Admin Config Loading]]
- [[_COMMUNITY_Signaling Events|Signaling Events]]
- [[_COMMUNITY_Shared Server State|Shared Server State]]
- [[_COMMUNITY_Client HotkeysPTT|Client Hotkeys/PTT]]
- [[_COMMUNITY_admin-ui Dependencies|admin-ui Dependencies]]
- [[_COMMUNITY_TLS Redirect Server|TLS Redirect Server]]
- [[_COMMUNITY_Toki Protobuf (TS)|Toki Protobuf (TS)]]
- [[_COMMUNITY_Signaling Boot|Signaling Boot]]
- [[_COMMUNITY_Admin UI Components|Admin UI Components]]
- [[_COMMUNITY_Watch Stream Broadcast|Watch Stream Broadcast]]
- [[_COMMUNITY_Metrics Charts (TS)|Metrics Charts (TS)]]
- [[_COMMUNITY_TypeScript Config|TypeScript Config]]
- [[_COMMUNITY_Client Update Check|Client Update Check]]
- [[_COMMUNITY_IP ThrottleBackoff|IP Throttle/Backoff]]
- [[_COMMUNITY_Server Metrics|Server Metrics]]
- [[_COMMUNITY_Rooms & Dialogs (TS)|Rooms & Dialogs (TS)]]
- [[_COMMUNITY_Server TLS Setup|Server TLS Setup]]
- [[_COMMUNITY_Admin SPA Shell|Admin SPA Shell]]
- [[_COMMUNITY_Input Validation|Input Validation]]
- [[_COMMUNITY_Server Infra & Docker|Server Infra & Docker]]
- [[_COMMUNITY_Admin HTTP Router|Admin HTTP Router]]
- [[_COMMUNITY_Connection State|Connection State]]
- [[_COMMUNITY_Idle Client Reaper|Idle Client Reaper]]
- [[_COMMUNITY_Server Config Types|Server Config Types]]
- [[_COMMUNITY_Proto Codegen & Branding|Proto Codegen & Branding]]
- [[_COMMUNITY_Client Build & Packaging|Client Build & Packaging]]
- [[_COMMUNITY_Frequency Theme Mapping|Frequency Theme Mapping]]
- [[_COMMUNITY_Admin Audit UI (TS)|Admin Audit UI (TS)]]
- [[_COMMUNITY_Audit Writer Sink|Audit Writer Sink]]
- [[_COMMUNITY_Theme Provider (TS)|Theme Provider (TS)]]
- [[_COMMUNITY_Encrypted UDP Audio|Encrypted UDP Audio]]
- [[_COMMUNITY_gRPC Signaling Protocol|gRPC Signaling Protocol]]
- [[_COMMUNITY_Admin RPC Client (TS)|Admin RPC Client (TS)]]
- [[_COMMUNITY_Client GUI & Presets|Client GUI & Presets]]
- [[_COMMUNITY_Proto Build Script|Proto Build Script]]
- [[_COMMUNITY_Admin Auth & Proxy|Admin Auth & Proxy]]
- [[_COMMUNITY_Server Route Builder|Server Route Builder]]
- [[_COMMUNITY_Server Entrypoint|Server Entrypoint]]
- [[_COMMUNITY_Toki Brand Icon|Toki Brand Icon]]
- [[_COMMUNITY_Server Module Map|Server Module Map]]
- [[_COMMUNITY_Admin Main Entry|Admin Main Entry]]
- [[_COMMUNITY_Eval Definitions|Eval Definitions]]
- [[_COMMUNITY_Server Entrypoint Script|Server Entrypoint Script]]
- [[_COMMUNITY_Vite Dev Proxy|Vite Dev Proxy]]
- [[_COMMUNITY_Community 52|Community 52]]
- [[_COMMUNITY_Platform Icon Build|Platform Icon Build]]
- [[_COMMUNITY_admin-ui Entrypoint Script|admin-ui Entrypoint Script]]
- [[_COMMUNITY_Coverage Script|Coverage Script]]
- [[_COMMUNITY_Commit Convention Skill|Commit Convention Skill]]
- [[_COMMUNITY_Community 60|Community 60]]
- [[_COMMUNITY_Community 61|Community 61]]
- [[_COMMUNITY_Community 62|Community 62]]
- [[_COMMUNITY_Community 63|Community 63]]
- [[_COMMUNITY_Community 64|Community 64]]
- [[_COMMUNITY_Community 65|Community 65]]
- [[_COMMUNITY_Community 66|Community 66]]
- [[_COMMUNITY_Community 67|Community 67]]
- [[_COMMUNITY_Community 68|Community 68]]
- [[_COMMUNITY_Community 69|Community 69]]
- [[_COMMUNITY_Community 70|Community 70]]
- [[_COMMUNITY_Community 127|Community 127]]
- [[_COMMUNITY_Community 174|Community 174]]

## God Nodes (most connected - your core abstractions)
1. `TokiApp` - 62 edges
2. `AdminDb` - 33 edges
3. `Result` - 29 edges
4. `Painter` - 28 edges
5. `cn()` - 24 edges
6. `AdminApi` - 24 edges
7. `Session` - 23 edges
8. `Rect` - 21 edges
9. `Input` - 21 edges
10. `Result` - 21 edges

## Surprising Connections (you probably didn't know these)
- `Phosphor accent color (oklch(0.86 0.18 145) #7FFF90)` --semantically_similar_to--> `admin-ui index.html SPA entrypoint (main.tsx, phosphor theme)`  [INFERRED] [semantically similar]
  crates/client/assets/icon/icon-spec.md → admin-ui/index.html
- `Toki marketing presentation (EN)` --references--> `Toki`  [INFERRED]
  docs/toki-presentation.html → README.md
- `Toki marketing presentation (FR)` --references--> `Toki`  [INFERRED]
  docs/toki-presentation-fr.html → README.md
- `Stat()` --calls--> `cn()`  [INFERRED]
  admin-ui/src/App.tsx → admin-ui/src/lib/utils.ts
- `run()` --calls--> `is_audio()`  [INFERRED]
  crates/server/src/audio.rs → crates/proto/src/lib.rs

## Import Cycles
- 1-file cycle: `crates/client/src/app.rs -> crates/client/src/app.rs`
- 1-file cycle: `crates/server/src/tls.rs -> crates/server/src/tls.rs`
- 1-file cycle: `crates/client/src/audio.rs -> crates/client/src/audio.rs`
- 1-file cycle: `crates/client/src/config.rs -> crates/client/src/config.rs`
- 1-file cycle: `crates/client/src/hotkey.rs -> crates/client/src/hotkey.rs`
- 1-file cycle: `crates/server/src/admin/grpc.rs -> crates/server/src/admin/grpc.rs`
- 1-file cycle: `crates/client/src/runtime.rs -> crates/client/src/runtime.rs`
- 1-file cycle: `crates/server/src/audio.rs -> crates/server/src/audio.rs`
- 1-file cycle: `crates/server/src/admin/db.rs -> crates/server/src/admin/db.rs`
- 1-file cycle: `crates/server/src/signaling.rs -> crates/server/src/signaling.rs`
- 1-file cycle: `crates/server/src/admin/auth.rs -> crates/server/src/admin/auth.rs`
- 1-file cycle: `crates/server/tests/admin.rs -> crates/server/tests/admin.rs`
- 1-file cycle: `crates/server/src/admin/handlers.rs -> crates/server/src/admin/handlers.rs`
- 1-file cycle: `crates/server/src/admin/mod.rs -> crates/server/src/admin/mod.rs`
- 1-file cycle: `crates/server/src/admin/watch.rs -> crates/server/src/admin/watch.rs`
- 1-file cycle: `crates/server/src/config.rs -> crates/server/src/config.rs`
- 1-file cycle: `crates/server/src/metrics.rs -> crates/server/src/metrics.rs`
- 1-file cycle: `crates/server/src/reaper.rs -> crates/server/src/reaper.rs`
- 1-file cycle: `crates/server/src/state.rs -> crates/server/src/state.rs`
- 1-file cycle: `crates/server/src/throttle.rs -> crates/server/src/throttle.rs`

## Hyperedges (group relationships)
- **Toki three deliverables (server + client + admin-ui)** — readme_toki_server, readme_toki_client, readme_admin_ui [EXTRACTED 1.00]
- **Subsystems sharing the Registry state** — user_guide_signaling_rs, user_guide_audio_rs, user_guide_reaper_rs, user_guide_state_rs [EXTRACTED 1.00]
- **UDP audio security flow (wire format + AEAD + replay protection)** — readme_udp_wire_format, readme_chacha20_poly1305, readme_replay_protection, user_guide_audio_rs [EXTRACTED 0.95]

## Communities (73 total, 12 thin omitted)

### Community 0 - "Admin gRPC API"
Cohesion: 0.06
Nodes (79): AdminApi, audit_log_filters_by_category(), authed(), AuthInterceptor, clear_all_channel_names_wipes_everything(), config_to_wire(), downsample(), downsample_caps_and_averages() (+71 more)

### Community 1 - "Client egui App"
Cohesion: 0.07
Nodes (61): Align2, App, Color32, Complex, Config, CornerRadius, Arc, AudioControl (+53 more)

### Community 2 - "Server Audio Relay"
Cohesion: 0.05
Nodes (65): AtomicUsize, AudioCmd, Arc, AtomicU32, AudioControl, AudioDevices, AudioGains, AudioLevels (+57 more)

### Community 3 - "Admin DB Layer"
Cohesion: 0.07
Nodes (50): AdminDb, audit_insert_load_filter_page_prune(), AuditRow, Backend, channel_names_crud_roundtrips(), connect_with_retry(), connect_with_retry_recovers_from_transient_failure(), delete_other_sessions_for_user_keeps_current() (+42 more)

### Community 4 - "Community 4"
Cohesion: 0.20
Nodes (14): BeepParams, Option, PlaybackBuf, SharedState, UnboundedReceiver, UnboundedSender, Vec, Decoder (+6 more)

### Community 5 - "Client Config Types"
Cohesion: 0.06
Nodes (42): AudioConfig, BeepConfig, ConnectionConfig, Default, Input, Option, Path, PathBuf (+34 more)

### Community 6 - "Admin Protobuf (TS)"
Cohesion: 0.03
Nodes (61): AuditEntrySchema, AuditFilterSchema, AuditLogRequest, AuditLogRequestSchema, AuditLogResponse, AuditLogResponseSchema, ChangePasswordRequest, ChangePasswordRequestSchema (+53 more)

### Community 7 - "Admin Auth & Cookies"
Cohesion: 0.06
Nodes (48): AdminUser, extract_session_cookie(), generate_admin_password(), generate_session_token(), generated_admin_password_has_expected_shape(), generated_session_tokens_are_unique_and_hex(), hash_password(), hash_then_verify_round_trips() (+40 more)

### Community 8 - "Admin Config Loading"
Cohesion: 0.08
Nodes (36): AdminConfig, Default, Option, Path, PathBuf, Result, Self, Config (+28 more)

### Community 9 - "Signaling Events"
Cohesion: 0.07
Nodes (36): ChangeFrequencyRequest, ChangeFrequencyResponse, AuditSink, Event, IpThrottle, Option, PttEvent, Request (+28 more)

### Community 10 - "Shared Server State"
Cohesion: 0.14
Nodes (18): Event, HashMap, Instant, IpAddr, Option, Sender, SharedChannelNames, SharedRegistry (+10 more)

### Community 11 - "Client Hotkeys/PTT"
Cohesion: 0.18
Nodes (7): Option, Result, Self, Input, InstalledHotkey, mouse_button_labels_round_trip(), parse_u16()

### Community 12 - "admin-ui Dependencies"
Cohesion: 0.05
Nodes (38): dependencies, @bufbuild/protobuf, class-variance-authority, clsx, @connectrpc/connect, @connectrpc/connect-web, lucide-react, @radix-ui/react-dialog (+30 more)

### Community 13 - "TLS Redirect Server"
Cohesion: 0.08
Nodes (33): AppState, redact_db_url(), redirect_handler(), run(), serve_redirect(), strip_host_port(), AuditEvent, AdminConfig (+25 more)

### Community 14 - "Toki Protobuf (TS)"
Cohesion: 0.06
Nodes (32): ChangeFrequencyRequest, ChangeFrequencyRequestSchema, ChangeFrequencyResponse, ChangeFrequencyResponseSchema, ChannelNameChanged, ChannelNameChangedSchema, DisplayNameChanged, DisplayNameChangedSchema (+24 more)

### Community 15 - "Signaling Boot"
Cohesion: 0.14
Nodes (28): Channel, Option, ServerConfig, SignalingClient, String, Vec, boot(), boot_with_config() (+20 more)

### Community 16 - "Admin UI Components"
Cohesion: 0.14
Nodes (14): useTheme(), ServerConfig, cn(), Badge(), badgeVariants, Card(), CardContent(), CardDescription() (+6 more)

### Community 17 - "Watch Stream Broadcast"
Cohesion: 0.15
Nodes (22): broadcast_stream(), mk_client(), next_generation(), run_broadcaster(), snapshot_carries_channel_names_for_all_named_freqs(), snapshot_groups_clients_by_frequency(), snapshot_marks_priority_only_on_matching_freq(), snapshot_now() (+14 more)

### Community 18 - "Metrics Charts (TS)"
Cohesion: 0.12
Nodes (16): ChartSeries, ChartWithAxes(), MetricSample, MetricsWindow, ServerHealth, ALL_FREQUENCIES, formatBytes(), formatClock() (+8 more)

### Community 19 - "TypeScript Config"
Cohesion: 0.09
Nodes (21): compilerOptions, allowImportingTsExtensions, baseUrl, isolatedModules, jsx, lib, module, moduleDetection (+13 more)

### Community 20 - "Client Update Check"
Cohesion: 0.13
Nodes (16): Context, Instant, Option, Result, String, UpdateShared, check_latest(), current_version() (+8 more)

### Community 21 - "IP Throttle/Backoff"
Cohesion: 0.23
Nodes (16): HashMap, Instant, IpAddr, Mutex, Option, Result, Self, allows_under_register_cap() (+8 more)

### Community 22 - "Server Metrics"
Cohesion: 0.15
Nodes (18): AdminDb, AtomicU64, Path, PathBuf, SharedByteCounters, SharedHealth, SharedLiveRate, SharedRegistry (+10 more)

### Community 23 - "Rooms & Dialogs (TS)"
Cohesion: 0.15
Nodes (14): Member, Room, channelNumber(), DialogContent, DialogHeader(), DialogTitle, DropdownMenuContent, DropdownMenuItem (+6 more)

### Community 24 - "Server TLS Setup"
Cohesion: 0.23
Nodes (17): Option, Path, PathBuf, Result, Self, TlsFiles, Vec, auto_gen_uses_data_dir_prefix() (+9 more)

### Community 25 - "Admin SPA Shell"
Cohesion: 0.16
Nodes (15): ServerInfo, useWatch(), login(), logout(), formatUptime(), NAV, Section, Shell() (+7 more)

### Community 26 - "Input Validation"
Cohesion: 0.17
Nodes (5): Result, Status, String, display_name(), frequency()

### Community 27 - "Server Infra & Docker"
Cohesion: 0.67
Nodes (3): docker-compose dev stack (server + admin-ui + postgres), postgres:15 service, Vite dev server (admin-ui dev, basic-ssl :5173)

### Community 28 - "Admin HTTP Router"
Cohesion: 0.24
Nodes (13): Admin, Body, Option, Response, Router, String, boot(), extract_session_cookie() (+5 more)

### Community 29 - "Connection State"
Cohesion: 0.17
Nodes (10): ConnState, HashMap, Option, S, SharedState, shared(), String, VecDeque (+2 more)

### Community 30 - "Idle Client Reaper"
Cohesion: 0.24
Nodes (11): AuditSink, Duration, Event, Sender, SharedRegistry, SharedServerConfig, String, Vec (+3 more)

### Community 31 - "Server Config Types"
Cohesion: 0.20
Nodes (7): Default, Self, SharedServerConfig, String, default_values_match_legacy_constants(), ServerConfig, shared_default()

### Community 32 - "Proto Codegen & Branding"
Cohesion: 0.20
Nodes (10): protoc-gen-es codegen (protobuf-es v2 → src/gen), buf proto module (crates/proto/proto), Toki app icon (Concept C Speaker Grille), Toki.icns / Toki.ico platform artifacts, client main.rs window icon (include_bytes! 256px → egui IconData), toki-icon.svg master source, Phosphor accent color (oklch(0.86 0.18 145) #7FFF90), admin-ui index.html SPA entrypoint (main.tsx, phosphor theme) (+2 more)

### Community 33 - "Client Build & Packaging"
Cohesion: 0.09
Nodes (15): Code, Input, GamepadCode, Keycode, debounce_byte(), debounce_is_per_byte_so_a_held_button_survives_axis_churn(), debounce_reports(), device_to_code() (+7 more)

### Community 34 - "Frequency Theme Mapping"
Cohesion: 0.29
Nodes (7): Option, String, channel_of_label(), channel_of_label_round_trips_with_frequency_of(), frequency_label(), frequency_of(), frequency_of_clamps_overflow()

### Community 35 - "Admin Audit UI (TS)"
Cohesion: 0.24
Nodes (7): AuditEntry, AuditFilter, auditIcon(), auditTone(), META, Audit(), TABS

### Community 36 - "Audit Writer Sink"
Cohesion: 0.33
Nodes (8): AdminDb, AuditSink, String, UnboundedReceiver, AuditEvent, channel(), record(), run_writer()

### Community 37 - "Theme Provider (TS)"
Cohesion: 0.29
Nodes (5): Ctx, Theme, ThemeCtx, ThemeProvider(), App()

### Community 38 - "Encrypted UDP Audio"
Cohesion: 0.25
Nodes (18): Arc, AtomicBool, AtomicU32, Cmd, Mutex, UnboundedReceiver, UnboundedSender, DeviceState (+10 more)

### Community 39 - "gRPC Signaling Protocol"
Cohesion: 0.33
Nodes (6): String, format(), format_code(), format_mouse(), mouse_button_indices_round_trip(), MouseButton

### Community 40 - "Admin RPC Client (TS)"
Cohesion: 0.38
Nodes (5): Admin, Snapshot, WatchState, admin, transport

### Community 41 - "Client GUI & Presets"
Cohesion: 0.30
Nodes (14): HashMap, Instant, BackendState, brief_tap_does_not_commit(), freq_up_down_accumulate_net_delta(), hold_commits_after_threshold(), longest_held_input_wins_the_bind(), memory_recall_fires_once_per_press_edge() (+6 more)

### Community 42 - "Proto Build Script"
Cohesion: 0.40
Nodes (4): Box, Error, Result, main()

### Community 43 - "Admin Auth & Proxy"
Cohesion: 0.04
Nodes (45): 1. Open-mode LAN deployment, 2. Password-gated public server, 3. Production server behind a reverse proxy, 4. Docker Compose (server + admin UI + Postgres), 5. Customizing the PTT binding from the CLI, Admin database backends, Admin UI, Admin UI environment variables (+37 more)

### Community 44 - "Server Route Builder"
Cohesion: 0.50
Nodes (3): build(), AppState, Router

### Community 46 - "Toki Brand Icon"
Cohesion: 0.83
Nodes (4): Toki Brand Mark, Toki Application Icon (SVG), Toki Icon Raster 512px (PNG), Toki Admin UI Icon (SVG)

### Community 47 - "Server Module Map"
Cohesion: 0.17
Nodes (11): Edge cases, Examples, Git Conventional Commit, Step 1 — Read the diff, Step 2 — Identify the type, Step 3 — Pick a scope, Step 4 — Detect breaking changes, Step 5 — Look for issue references (+3 more)

### Community 52 - "Community 52"
Cohesion: 0.12
Nodes (12): Duration, Instant, Option, Result, SharedByteCounters, SharedRegistry, SocketAddr, PeerTarget (+4 more)

### Community 60 - "Community 60"
Cohesion: 0.12
Nodes (16): Admin database backends, Admin panel, Admin UI environment variables, Architecture, Client features, Docker, Documentation, Layout (+8 more)

### Community 61 - "Community 61"
Cohesion: 0.18
Nodes (10): App Icon — Toki, Don't change, Files in this folder, How to integrate, Linux, macOS (`.icns`), Re-rendering from the master, Rust framework specifics (+2 more)

### Community 62 - "Community 62"
Cohesion: 0.29
Nodes (6): Code signing, macOS packaging, One-time setup, Producing the app, Updating the icon, What ends up inside the bundle

### Community 63 - "Community 63"
Cohesion: 0.33
Nodes (5): Files, How the icon reaches the user, Regenerating individual PNGs, Regenerating the platform artifacts, Toki app icon — build artifacts

### Community 66 - "Community 66"
Cohesion: 0.15
Nodes (16): Arc, AtomicBool, AtomicU64, Channel, Instant, PttEvent, Sender, SignalingClient (+8 more)

### Community 67 - "Community 67"
Cohesion: 0.18
Nodes (12): Result, SocketAddr, BeepKind, pcm_from_bytes(), pcm_from_bytes_ignores_trailing_partial_sample(), resolve_audio_endpoint(), resolve_audio_endpoint_passes_through_routable_addr(), resolve_audio_endpoint_resolves_advertised_dns_name() (+4 more)

### Community 68 - "Community 68"
Cohesion: 0.14
Nodes (14): ClientConfig, Clone, Box, Response, Future, Output, Send, Service (+6 more)

### Community 69 - "Community 69"
Cohesion: 0.23
Nodes (8): Error, Self, Encoder, AudioEncoder, opus_emits_one_packet_per_10ms_frame_and_decodes(), opus_flush_emits_partial_tail_then_clears(), pcm_encoder_emits_one_packet_per_frame(), pcm_flush_is_a_noop()

### Community 70 - "Community 70"
Cohesion: 0.22
Nodes (4): Option, compatible(), is_audio(), major_minor()

### Community 127 - "Community 127"
Cohesion: 0.20
Nodes (11): Vec, HashSet, HidApi, HidDevice, changed_mask(), emit_changed_bits(), emit_changed_bits_maps_set_bits_to_hid_inputs(), hid_is_bindable_generic() (+3 more)

## Knowledge Gaps
- **494 isolated node(s):** `skill_name`, `evals`, `name`, `private`, `version` (+489 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **12 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `custom_tls_connector()` connect `Community 68` to `Signaling Events`, `Community 67`, `Community 4`, `Community 69`?**
  _High betweenness centrality (0.107) - this node is a cross-community bridge._
- **Why does `Pin` connect `Signaling Events` to `Admin gRPC API`, `Community 68`?**
  _High betweenness centrality (0.106) - this node is a cross-community bridge._
- **Why does `Code` connect `Client Build & Packaging` to `Admin gRPC API`, `Signaling Boot`, `Client Config Types`, `gRPC Signaling Protocol`?**
  _High betweenness centrality (0.092) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `cn()` (e.g. with `Shell()` and `Stat()`) actually correct?**
  _`cn()` has 2 INFERRED edges - model-reasoned connections that need verification._
- **What connects `skill_name`, `evals`, `name` to the rest of the system?**
  _494 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Admin gRPC API` be split into smaller, more focused modules?**
  _Cohesion score 0.057821782178217825 - nodes in this community are weakly interconnected._
- **Should `Client egui App` be split into smaller, more focused modules?**
  _Cohesion score 0.06930693069306931 - nodes in this community are weakly interconnected._