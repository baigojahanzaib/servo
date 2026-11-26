# Feather idle-RAM levers — what needs upstream Servo changes

Feather's idle budget is <150MB RSS. Feather-side work shipped in
`feather/shell` (lazy session restore, jemalloc background purging,
`js_mem_max` cap) leaves ~210-280MB on a GPU-less host, of which roughly
65MB is llvmpipe software-GL overhead that disappears on real hardware.
The remaining levers below need engine-side changes; sketches live here
rather than as hacks in feather-shell.

## 1. Bounded image cache (net/image_cache.rs) — SHIPPED on feather/image-cache-bound

`ImageCache` retained every decoded image for the lifetime of the
pipeline — no size bound, no eviction.

Implemented: `completed_loads` is now capped at 128 MiB of decoded
raster bytes per `ImageCache` (per pipeline). `CompletedLoad` records
its decoded size; `completed_loads_bytes` + `completed_loads_lru` drive
`enforce_completed_load_budget` after each completed load. Eviction
reuses `remove_loaded_image` (drops the entry + `DeleteImage` to
WebRender); hits promote entries in the LRU. Entries whose decoded
`Arc<RasterImage>` is still referenced outside the cache (DOM nodes
hold one via `ImageRequest.image`) are skipped — evicting them frees
nothing and forces a re-fetch, so the cap is soft when a page displays
more than the budget.

Measured (feather-shell, llvmpipe host, 40x 1600x1200 PNG ≈ 307 MiB
decoded):

| Scenario | Before | After |
|---|---|---|
| Slideshow page (images unreferenced after display) | 839 MiB RSS | **657 MiB** (−182) |
| All-visible grid | 855 MiB | ~831 MiB — soft cap, no churn |

The naive version without the `Arc::strong_count` check caused an
evict→refetch→re-decode cycle (80 fetches for 40 images) for no RSS
gain — the pinned-entry check is required.

Caveat: on a page that *keeps* all its images referenced, the cache is
not the retainer — the DOM is. Shrinking that case needs script-side
discard of decoded surfaces (like imglib's discard-on-memory-pressure),
a much bigger change.

## 1b. Hidden-document image surface discard — SHIPPED on feather-engine

Follow-up to §1: releases decoded surfaces for the DOM-referenced case
when a WebView is hidden (background tab in feather-shell).

Chain: `WebView::hide()/show()` now also sends
`EmbedderToConstellationMessage::SetWebViewHidden` →
`SetThrottled` → `Document::update_visibility_state(Hidden|Visible)`.
Bonus: `document.visibilityState` and timer throttling now follow tab
visibility, which upstream had not wired.

On `Hidden`, `update_visibility_state` releases every retainer of the
decoded `Arc<RasterImage>`:

1. `image_cache.discard_completed_loads()` — clears completed loads and
   sends `DeleteImage` for each `ImageKey` (pending loads/listeners and
   the key cache are kept).
2. `window.layout_mut().drop_cached_layout_trees()` — new `Layout`
   method dropping box/fragment/stacking trees + `resolved_images_cache`.
3. `animating_images.node_to_state_map.clear()` — per-node animation
   state pins `Arc<RasterImage>` too.
4. Per node: `node.clear_layout_data()` — **required**: each node's
   `InnerDOMLayoutData.self_box` keeps its `LayoutBox` alive after the
   tree root is dropped, and `ReplacedContents`/`ImageInfo` inside pins
   the image. This was the last retainer found via `Arc::strong_count`
   instrumentation.
5. Per `HTMLImageElement`: `discard_decoded_image()` takes
   `current_request.image` (state/url/metadata kept).

On `Visible`, `restore_decoded_image()` re-fetches each `CompletelyAvailable`
image (`fetch_image` → re-decode → repaint); layout data and trees are
rebuilt by the normal reflow.

Measured (feather-shell, llvmpipe host, same 40-image loop page,
40x1600x1200 PNG ≈ 307 MiB decoded):

| Build | Page loaded | Tab hidden (~60-90s settled) |
|---|---|---|
| stock servoshell (`main` @170fe92389a, no feather patches) | 944 MB, stays resident | n/a — no tab-hide path |
| feather-engine (image-cache bound only) | 1039 MB | 1027 MB — no release |
| feather-engine + discard lever | 835 MB | **712 MB** (−315 MB vs no-lever) |

Restore verified on screen: re-showing the tab re-fetches all 40 images
and re-renders the grid; repeated hide/show cycles release again each
time. Note `Image::as_raster_image()` returns a clone — counting
`strong_count` through it overstates by one.

Remaining gap: RSS release lags behind the logical free (jemalloc/GC
retains segments briefly); a deeper lever for *visible* tabs would need
per-image offscreen discard driven by IntersectionObserver/paint bounds —
the cache-bound already covers DOM-unreferenced images, and DOM-referenced
on-screen ones must stay decoded by definition.

## 2. Lazy subsystem init

Subsystems that could start on first use instead of at `Servo::new`:

- WebGPU/wgpu (`dom_webgpu_enabled` is already off by default — keep it)
- WebRTC, ServiceWorker (off by default — keep them off in Feather)
- Font fallback / system font scans
- The gamepad polling thread (`dom_gamepad_enabled` defaults true —
  Feather could set it false if it doesn't ship gamepad support)

## 3. Suspended/discarded WebViews

Feather now lazy-restores tabs (about:blank until activated). A stronger
primitive — discarding a loaded tab's browsing context and re-creating it
on activation — would let long-running sessions shed background tabs
entirely, matching Chrome's tab discarding. Needs a constellation-level
"discard" message that tears down the pipeline + script thread while
preserving the session-history entry. Not currently exposed on `WebView`.

## 4. Thread-pool sizing

`thread_pool_workers_max`, `thread_pool_async_runtime_workers_max`,
`thread_pool_webrender_workers_max`, `layout_threads` were each measured
at ~0 idle-RSS effect — pools are created lazily and idle threads cost
little RSS. Left at defaults.

## 5. What prefs could NOT fix (measured, single DDG tab, ~60s idle)

Baseline ~322MB RSS / ~275MB PSS (llvmpipe host):

| Lever | RSS | Verdict |
|---|---|---|
| `layout_threads` 3→1 | 323MB | no effect |
| `js_mem_max` 128 | 321MB | no idle effect; kept at 192 as a runaway cap |
| `js_mem_gc_malloc_threshold_base_mb` 38→16 | 321MB | no effect |
| thread pools halved | 322MB | no effect |
| jemalloc background_thread + decay | 320MB | −2MB + ongoing decay |
| `LP_NUM_THREADS=2` | 320MB | no effect (llvmpipe cost isn't threads) |
| `narenas:1` (MALLOC_CONF test) | 318MB | marginal |
| lazy session restore (5 tabs) | 214–281MB | only loads the active tab |

The floor on this host is Servo runtime (~180MB) + llvmpipe (~65MB).
On real GPU hardware expect ~215-255MB with one loaded tab.

## 6. Reflector-root revival — nav-leak measurements

`release_reflector_roots()` (reflector-root revival, HEAD 9ec4a62b) makes
dead pipelines' objects *collectable*. Whether RSS actually falls depends
on collection pressure. 30-nav WebDriver loop (10 sites ×3), single
servoshell PID RSS in MB, macOS arm64 16GB host:

| Config | Start→End | Shape |
|---|---|---|
| stock servo/main | 411→3355 | monotone |
| stock `--zealous-gc` | 414→2460 | still climbing |
| feather-engine (roots released) | 410→2953 | mild sawtooth, real mid-run drops |
| engine `--zealous-gc` | 411→5130–6599 (2 runs) | monotone — worst config tested |
| engine + `JS_GC` on pipeline teardown (tested, not pushed) | 411→6126 | monotone, ~1.9× stock |
| engine + `JS_MaybeGC` on teardown (tested, not pushed) | →3916 @ nav18 | wedged at nav19 |
| engine + `MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0` | 411→5261 | monotone |
| stock + feather env (js_mem 192/50/300 + jemalloc 5s decay + bg thread) | 415→3482 | slower monotone |
| engine + feather env (same) | 411→5213, 5598 (2 runs) | monotone, ~1.5× stock |
| engine + feather env, no `js_mem_max` cap | 411→5718 | monotone — cap ruled out |
| engine + `JS_GC` every 8th teardown (tested, not pushed) | 411→4364 | sawtooth-ish but ~25% over stock; wedged at nav8 in 1 of 2 runs |

### Bisect: the 1.5× is IntersectionObserver, not the reflector fix

`test/bisect-*` = stock-baseline + stacked cherry-picks, same prefs/env
as the rows above, same-window runs:

| Build | End MB | Verdict |
|---|---|---|
| stock | 2681 | baseline |
| reflector-roots only | 3209 | ≈ stock, with reclaim drops |
| + image-cache bound | 2697 | ≈ stock |
| + hidden-document discard | 2767 | ≈ stock same-window |
| + IntersectionObserver (b4a6153e2) | 4086 | **~1.5× — culprit** |
| + h265 (full engine code) | 3799, 4583 | same as IO layer |
| IO build, `dom_intersection_observer_enabled=false` | 2584 | ≈ stock |

The pref-off A/B on an identical binary pins it: live IntersectionObserver
objects retain ~1.3GB across navs on this site list (observers hold
Element targets/roots; most crawled sites register them). The spec-gap
fixes are not implicated — enabling the feature is. Fix direction: unroot
observer targets at pipeline teardown, not revert.

**Fix shipped (816b8e39e):** `Document::clear_intersection_observers` now
runs in `Window::clear_js_runtime` — drains each observer's target list,
element `registered_intersection_observers` entries, and queued entries.
Post-fix same-window runs (feather-env, IO on): **3263, 3272** vs pre-fix
4086 and IO-off-on-fixed-binary **3392** — IO-on now tracks the IO-off
curve; the residual over the earlier 2681 stock reading is ambient drift,
not IO. IO verified functional (callback fires on scroll via WebDriver).

Verdict (macOS): every lever that forces reclaim — eager GC on teardown,
zeal GC, allocator decay — makes things *worse*. The ~1.5×-over-stock
full-branch number decomposes in the bisect below: the reflector-root
revival itself is clean (≈ stock, with natural-GC reclaim drops); the
retention comes from IntersectionObserver being enabled. The Windows
forced-GC sawtooth (~4GB peak) did not reproduce here. Purge/decay can't
help because retained objects are still *reachable* — they only become
dirty pages after a GC, and eager GC is the part that hurts.

`js_mem_max=192` is NOT the cause: patched+feather-env without the cap
ends at 5718 (vs 5213/5598 with it). Throttled GC (every 8th teardown)
lands at 4364 — better than eager but still ~25% over stock AND it
wedged at the first GC'd exit in one of two runs. GC-on-teardown at any
cadence, zeal, and the cap are all ruled out. The bisect below shows the
~1.5× over stock is NOT the revival — it's live IntersectionObservers.

## 1c. E2E scoreboard + hidden-tab CPU caveat (Nov 24)

Feather on feather-engine vs stock servoshell (upstream main), llvmpipe host:

| Metric | Stock | Feather | Δ |
|---|---|---|---|
| Cold start → paint | ~297ms | ~296ms | parity |
| Idle RSS, 1 DDG tab | 663MB | 566MB | −97MB |
| 4-tab (2 grid + slideshow + DDG) | 2503MB resident | 433MB lazy-restored / 1287MB hidden | −49% |
| Binary | 206MB / 153MB stripped | 89.5MB production | −57% |

CAVEAT resolved (commit 75697a2bc78): a hidden tab whose JS keeps
mutating images (slideshow cycling `<img src>`) churned at ~390-460% CPU —
each timer tick refetched + decoded a surface the hide path discards, AND
every hidden-doc update-the-rendering produced a reflow → display list →
WR raster of work that is never displayed. Fixed with three gates on the
real visibilityState:
- `fetch_image` early-returns while hidden; a `src` mutated while hidden
  queues in `pending_request` and `restore_decoded_image` (on Visible)
  now prefers the pending request's URL, so the latest src wins.
- `Document::needs_rendering_update` returns false while hidden —
  reasons accumulate and flush in one update on show.
- `update_the_rendering` step-3 non-renderable filter skips hidden docs
  (the WHATWG "visibility state is hidden" TODO).

RESULTS (llvmpipe, looping 40-image slideshow every 350ms):

| Tab state | CPU | net fetches |
|---|---|---|
| hidden, before | ~390-460% | 1 fetch+decode/tick |
| hidden, fixed | **0.0%** | **0** |
| shown, cycling | ~110-120% (legit per-tick raster) | 0 (warm cache) |
| stock servoshell hidden-equivalent | ~53% | — |
| re-show | RSS returns to shown level; newest image renders |

ALSO REQUIRED — embedder-side: the fix needs an on-demand repaint loop.
Feather's shell had TWO independent CPU-burn bugs found while isolating
this: egui-winit answers `repaint: true` for `WindowEvent::RedrawRequested`
itself, and `if response.repaint { request_redraw() }` re-requested a
redraw after every redraw → self-sustaining ~290fps repaint loop that
rasterized the active webview on ANY page (even static), plus an
unconditional `request_redraw()` per servo wake in `user_event`. Shell
fix: skip re-requesting redraw when the event IS RedrawRequested, and
drop the unconditional redraw (new-frame notifications already request
redraw via `notify_new_frame_ready`). Result: feather idle on static
page = 0.0% CPU vs ~390% before. Stock servoshell doesn't have this bug
(it already only repaints on `notify_new_frame_ready`).

## 1d. External-scheme navigations (wa.me fix) — SHIPPED on feather-engine

Top-100 crawler defect queue flagged wa.me as the only real engine defect:
wa.me → api.whatsapp.com renders, then page JS does
`location.href = 'whatsapp://chat/'` → document navigation to an
unregistered scheme → UnsupportedScheme net-error page.

Chrome parity: other engines treat a document navigation into a scheme
they can't handle as a *cancelled* navigation handed to the OS /
interstitial — the previous page stays, no error page.

FIX (commit 1ea96fb):
- `net/fetch/methods.rs` `scheme_fetch` fallback: when the scheme has no
  protocol handler AND `request.destination == Destination::Document`,
  return `NetworkError::LoadCancelled` instead of `UnsupportedScheme`.
  servoparser renders no error page for LoadCancelled ("the next load
  will show a page"), so the previous document stays.
- `net/http_loader.rs` redirect step 6: an HTTP(S) redirect target with
  a non-navigable scheme gets the same treatment, scoped to
  Destination::Document.
- Subresource fetches to unknown schemes still hard-error per spec.

VERIFIED in feather-shell: wa.me renders the real WhatsApp landing
page and its JS deep-link logs "Load cancelled" with the page intact;
a JS `location.href='whatsapp://chat/'` test page stays rendered; a
restored whatsapp:// session tab loads about:blank, no error page.

## WPT pass-rate baseline (macOS arm64, 2026-11-24)

Subset: `tests/wpt/tests/html/dom` + `tests/wpt/tests/css/cssom` = 695 test
files, 76,928 subtests. Same flags both sides:
`./mach test-wpt <dirs> -r --binary <bin> --log-raw <log> --processes 4`
(~90 s per run). Stock = upstream main 170fe9238; engine = feather-engine.

|                     | Stock    | feather-engine |
|---------------------|----------|----------------|
| test-level PASS+OK  | 580/695  | 580/695        |
| test-level FAIL     | 104      | 104            |
| test-level TIMEOUT  | 10       | 10             |
| test-level ERROR    | 1        | 1              |
| subtests PASS       | 68,507/76,928 (89.1%) | 68,508/76,928 (89.1%) |
| subtests FAIL       | 8,397    | 8,396          |

Zero test-level diffs. The single subtest delta
(`getComputedStyle-inset-inline-containing-block-vertical.html`, a sub-ULP
float rounding case) re-ran both ways on stock → flaky, not real.

**Verdict: our branch is compat-neutral on dom/cssom** — memory/IO/cache
work neither helps nor hurts pass rate, as expected. This subset is the
weekly scoreboard; widen it (e.g. add `tests/wpt/tests/dom`) for deeper
coverage when needed.

## 1e. Suspended-webview freeze — JS-heap GC on hide — SHIPPED on feather-engine

INVESTIGATION (what a hidden document still does, feather-engine):
Full throttling was already wired upstream — `webview.hide()` →
`SetWebViewHidden` → `pipeline.send_throttle_messages()` → script
`SetThrottled` clamps timers to `pref!(js_timers_minimum_duration)`=1000
+ `PaintMessage::SetThrottled`; `window.throttled()` gates animations and
rAF; `update_the_rendering` is skipped entirely; image fetch/decode is
gated on visibility (§1c); decoded image surfaces + cached layout trees
are discarded on hide (§1b/§1c). What still runs: layout queries still
trigger full `window.reflow()` (required for correct values, Chrome same);
FontContext is `Arc` per-script-thread shared across docs (not
doc-scopable — ruled out). The uncovered gap: **the JS heap**.
SpiderMonkey GC is allocation-driven, and a throttled hidden page
allocates too rarely to ever trigger collection → dead objects sit in
the heap for as long as the tab stays hidden.

FIX (commit e434b31): `document.rs` `update_visibility_state` runs
`window().gc(cx)` on the visible→hidden transition — a full GC while the
pause is invisible to the user. Semantically safe (only unreachable
objects are collected) and instant.

MEASURED (page holding ~240MB of dead JS objects, feather, llvmpipe VM):
- stock servoshell:      ~470MB, resident (no hide path exists)
- feather-engine before: 506→509MB hidden, 0MB released after 90s
- feather-engine + fix:  453→274MB on hide, **−178MB released immediately**
Re-showing the tab works normally; a second hide cycle releases again.
Pairs with §1b/§1c: hide now frees images AND dead JS heap.

Remaining non-levers (documented so nobody re-tries): per-document layout
cache suspension (caches already dropped on hide), layout/font cache
freezes (FontContext is shared per script-thread), reflow gating
(correctness), timer suspension (already clamped).

## WPT scoreboard extension: dom + intersection-observer (macOS arm64, 2026-11-24)

Subset: `tests/wpt/tests/dom` + `tests/wpt/tests/intersection-observer` =
1,028 files, ~67,500 subtests. Both binaries run with
`--binary-arg=--pref=dom_intersection_observer_enabled` so the IO suite is
a real A/B (stock default is pref-off). ~5 min per run.

|                      | Stock+IO    | feather-engine+IO |
|----------------------|-------------|-------------------|
| test-level PASS+OK   | 1,000/1,028 | 1,000/1,028       |
| TIMEOUT/ERROR/CRASH  | 12/15/1     | 12/15/1           |
| subtests PASS        | 65,345 (96.72%) | 65,360 (96.74%) |

Zero test-level diffs; the 1 CRASH (multiple-append-mutated-in-unload)
hits both builds identically → environmental.

15 subtest deltas — ALL engine wins, all from b4a6153e2's IO spec-gap
fixes: 13× rootMargin/scrollMargin absolute-length units (cm/mm/q/in/pt/pc),
1× scroll-margin intersection, 1× detached-document target observation.
**Zero regressions in the IO suite from the teardown fix.**

CAUGHT BY THIS RUN: the IO teardown fix had a RefCell double-borrow —
`clear_intersection_observers` drained the Vec while holding the
borrow_mut, and `clear_registrations` → `disconnect_from_owner` →
`remove_intersection_observer` re-borrows it → panic on pipeline exit.
Fixed in 5d1ef8ba9 (collect-then-clear).

## IO pref "anomaly" resolved (2026-11-24) — measurement artifact, not a bug

Earlier observations that `typeof IntersectionObserver` returned 'undefined'
without `--pref` on a binary supposedly built from a branch with
`dom_intersection_observer_enabled: true` in `const_default()` turned out to
be a **stale servoshell binary** in `/tmp/engine` (its `Preferences` static
was compiled before b4a6153e2's prefs.rs flip was in the checkout).

Verified on a fresh `target/release/servoshell` (head 1b093c1ba): with NO
--pref flag, WebDriver eval returns `function` for IntersectionObserver —
IO is correctly on by default in the engine. Instrumenting `prefs::set`
confirmed `io=true` is written at startup; no code path flips it off.

Consequences for prior measurements:
- All WPT/crawler comparisons are UNAFFECTED — both sides ran with
  `--binary-arg=--pref=dom_intersection_observer_enabled`, a valid A/B.
- The "IO-off" scoreboards (nav loops, crawler) ran with the pref forced on
  via prefs-file/--pref, so they measured the intended state anyway.
- Lesson: always rebuild + re-hash the preserved `/tmp/engine` binary after
  pulling feather-engine; a servoshell on port 7000 from a prior run can
  also shadow fresh probes (pgrep before probing).

## 1f. Downloads pipeline — SHIPPED on feather-engine

Upstream Servo had ZERO download support: no `WebView` delegate hook, no
constellation message, nothing in servoshell. A navigation to a
`Content-Disposition: attachment` response rendered the body as an inline
text wall; an undisplayable MIME type fell into the generic "unknown
content" path with the URL lost.

PIPELINE (all on feather-engine):
- `script/dom/servoparser/mod.rs::load_document` — after the MediaType
  verdict (HTML spec step 2), fires `ScriptToConstellationMessage::
  DownloadRequested { webview_id, pipeline_id, url, mime,
  content_disposition }` when `is_attachment || media_type.is_none()`.
  For attachments `media_type` is forced to `None` so the body never
  renders: the parser takes the existing inline "Downloading content of
  type (…)." page instead of streaming the payload.
- `NavigationParams` gained `content_disposition: Option<String>`,
  populated in `process_response` from the response's
  `Content-Disposition` header (needed to resolve the download filename
  embedder-side).
- `constellation/constellation.rs` forwards the message as
  `ConstellationToEmbedderMsg::DownloadRequested(webview_id, url, mime,
  content_disposition)` (registered in `tracing.rs`); `servo.rs`
  dispatches it to `WebViewDelegate::notify_download_requested(webview,
  url, mime, content_disposition)` — the new delegate hook embedders
  implement (default no-op).

## Per-nav retention: GC triggers don't reclaim it (2026-11-24)

Question: after the reflector-root + IO-teardown fixes release JS roots at
pipeline exit, can a low-cost trigger reclaim the memory promptly instead of
waiting for natural GC?

Experiments (30-nav WebDriver loop, feather-env prefs, end-RSS):

| variant | end RSS |
|---|---|
| patched engine, natural GC | ~2953 MB |
| + js_mem_gc_incremental_enabled | 4366 MB |
| + incremental slices driven from the event loop (WorkBudget, 8ms/turn cap) | 4631 MB |
| + deferred blocking JS_GC per pipeline exit (queued past the `NoGC` scope in clear_js_runtime) | 4177 MB |

Every explicit trigger is *worse* than natural GC. Reasons found in the source:
- `JS_MaybeGC`/`maybeGC` only collects if zones already past eager triggers —
  not a real periodic trigger (and a blocking GC when it does fire).
- Incremental slices without a driver stall mid-collection; driven slices keep
  running under continued nav allocation with write-barrier overhead, so
  reclamation lags further behind growth.
- A blocking `JS_GC` per nav is pure cost: the retained memory is mostly NOT
  JS-heap garbage — it's Rust-side allocations (layout, image cache, DOM
  arenas in jemalloc), which GC can't touch. GC only frees the JS wrappers.

Conclusion: there is no GC-trigger fix for per-nav retention. The remaining
sawtooth is Rust-allocator memory; the levers that work are jemalloc decay
prefs (already in featherprefs) and Rust-side release-at-teardown work
(e.g. `image_cache().clear()` in handle_exit_pipeline_msg). `JS_GC` under
`Window::clear_js_runtime`'s `cx.no_gc()` scope is suppressed — earlier
"eager GC" experiments measured nothing; the correct deferred point is the
next event-loop turn.

## 1g. Downloads round 2 — body handoff + a[download]/blob — SHIPPED

The double-fetch and the markup-initiated gaps from §1f are closed.

- `script/dom/download.rs` (new): `DownloadSpool` streams a response body
  to `temp_dir()/servo-download-<pid>-<nanos>.part` as chunks arrive
  (bounded memory; the file is deleted on drop unless `finish()` hands
  the path out). `send_download_finished` and
  `AnchorDownloadFetchContext` (a `FetchResponseListener`) live here.
- Nav downloads: `load_document` creates a spool when it detects a
  download, `process_response_chunk` appends BEFORE the synthesized-page
  early return, and `process_response_eof` finalizes at the top AND after
  the deferred `load_document` call (the <1445-byte sniffing-buffer case).
  `NavigationParams` gained `content_length` (parsed from headers) for
  progress totals. Result: the embedder gets the body the navigation
  already fetched — no second fetch, signed URLs and POST results work.
- `a[download]`: `follow_hyperlink` (links/relations.rs) checks the
  `download` attribute before navigation and starts a
  `create_a_potential_cors_request_with_claim` fetch with
  `AnchorDownloadFetchContext` — `Destination::None`,
  `ensure_blob_referenced_by_url_is_kept_alive` claims the blob, so
  `blob:` and `data:` URLs download through the same path. The
  `download` attribute value is sent as `filename_hint`.
- New message flow: `DownloadRequested` gained `filename_hint` +
  `content_length`; added `DownloadProgress {received, total}` (≥1MiB
  granularity) and `DownloadFinished {body_path, error}` — body_path
  ownership transfers to the embedder (same-process path). All three
  forward through constellation to `WebViewDelegate::
  notify_download_progress` / `notify_download_finished` (default no-ops).
- Verified end-to-end in feather-shell: attachment + octet-stream navs,
  a[download] with markup filename winning, blob: and data: downloads,
  50MB throttled progress, cancel-then-finish cleans the temp file,
  collision naming.

STILL MISSING: no engine-side cancel (embedder marks cancelled and
deletes the delivered temp file — the fetch itself runs to EOF; a real
cancel needs a fetch-abort message into net), no multipart/range resume,
no save-As prompt.

## Per-nav retention resolved: jemalloc purge at pipeline teardown (693146a10)

Attribution (vmmap + MallocStackLogging, macOS): the ~2-3GB retained RSS is
NOT framework buffers despite the misleading "CoreMedia Capture Data" vmmap
label — stack traces show those regions are `_rjem_je_extent_alloc_mmap`
(i.e. jemalloc arena extents). jemalloc mallctl after ~18 navs:
allocated(live) ~1.0-1.8GB vs mapped ~1.6-2.0GB → several hundred MB-1GB of
mapped-but-unallocated dirty pages. The rest of RSS is SpiderMonkey's own
mmap'd GC heap + non-heap regions.

Levers measured on the 30-nav loop (feather-env prefs):
- `MALLOC_CONF dirty_decay_ms:1000,muzzy_decay_ms:1000`: end 3592 — decay
  returns pages but slower than navs produce them.
- `+ background_thread:true`: end 4551 (oscillating troughs ~2.1-2.7G) —
  no better than natural.
- `arena.4096.purge` (MALLCTL_ARENAS_ALL) in `handle_exit_pipeline_msg`:
  ends 2725 / 3500 (end is reddit-heavy sensitive), mid-loop sits
  ~1.4-2.6GB vs ~2.4-3.3GB unpurged — ~600MB-1GB sustained win.

Shipped: `servo_allocator::purge_arenas()` (mallctl arena.purge on all
arenas; no-op on non-jemalloc builds) called once per pipeline exit in
handle_exit_pipeline_msg. Cross-platform via jemalloc; Linux siblings get
it for free.

Audit trail for future passes (already-clean teardown, don't re-check):
`pipeline_to_node_ids` removed at exit; `image_cache().clear()` sends
WR `DeleteImage` for every cached key; `document.unload` →
`clean_up_media_elements` → `reset_media_player`; `animation_manager`
cleared; `pipeline_exited` → `transaction.remove_pipeline` (WR scene).
Remaining RSS after purge = live jemalloc allocs (layout/DOM still alive
in bfcache/history) + SpiderMonkey GC heap — next lever would be auditing
which Rust objects survive teardown (layout snapshots, font caches), not
allocator tuning.

## Object-level audit: bfcache depth is the dominant per-nav lever (no code needed)

Follow-up to the purge work: WHY does live jemalloc alloc stay ~1-1.8GB?
Answer — session history keeps nav'd-away documents fully alive
(`NeedsToReload::No`) until evicted past `session_history_max_length`
(default **20**) or until the document unloads unsalvageable. Up to ~20
dead documents × DOM+layout+JS global each = the sawtooth mass. Eviction
path: `session_history.rs` eviction → `NeedsToReload::Yes` → close_pipeline.
Unsalvageable docs die sooner via `DiscardDocument` → replace_reloader(Yes).

Measured (30-nav loop, feather-env prefs, purge already shipped):

| history depth | end RSS | mid-loop |
|---|---|---|
| 20 (default) | 2725 / 3500 | 1.4-2.6 GB |
| 5 | 1884 | 1.6-2.3 GB |
| 1 | 2386 | 1.3-2.4 GB |

Cutting depth 20→5 roughly halves end RSS and ~doubles how much a dead
document's memory returns at teardown. 1 gives no further win — floor is
live current-doc + SM heap + fonts. **Lever is the pref
`session_history_max_length` — set 5 in feather prefs.json; no engine
patch.** Chrome's bfcache is ~5-10 entries + memory-bounded, so 5 matches
mainstream UX (still gives working back-forward).

Stock servoshell honours the same pref (it's upstream's), so this is a
feather-config win, not a patch — both sides of any A/B must use the same
depth or the comparison is meaningless.

### Linux cross-verify of the arena-purge lever (Nov 24, llvmpipe host)

30-nav loop (40-img page + ~60MB JS garbage per nav, same page as the
macOS runs): stock upstream servoshell climbs 1.0GB → **7.79GB RSS and
never comes back**; feather-engine head sits **flat ~1.1-1.2GB for the
whole loop** (servoshell AND feather-shell alike). The lever is safe and
working on Linux jemalloc — no regression, identical flat curve.

Honest attribution: reverting ONLY the purge (same tree minus
7d02b70553b) stays flat too — ~1.1-1.25GB at 3s/nav AND at 800ms/nav.
Linux jemalloc's default dirty-page decay already reclaims teardown
pages between navs, and the image-cache bound dominates the stock-vs-ours
delta (stock's unbounded decoded cache = ~200MB/nav pile-up). So the
~1GB mapped-but-unallocated problem Mac attributed on macOS is
platform-specific: purge is essentially free insurance on Linux and the
real win stays on macOS. Reclaim confirmed working; incremental Linux
delta unmeasurable in this workload.

### Idle-floor decomposition (Nov 24, llvmpipe host, feather-engine 81fc006)

Goal: explain the ~420MB settled idle RSS vs the <150MB budget — find
what's releasable. Measured via /proc/PID/smaps on a settled
single-DDG-tab feather process, compared against about:blank.

**Feather @ about:blank = 311MB RSS.** Floor is structural:

- **77MB** feather binary pages (file-backed; shared/reclaimable under
  memory pressure — not live heap)
- **55MB** libLLVM-15 + **8MB** swrast_dri — llvmpipe-only (~65MB total
  tax; vanishes on real GPUs)
- **38MB** glibc [heap] — mostly mesa/LLVM allocations (same tax)
- **~116MB anonymous, 509 VMAs** — live engine+shell runtime: 65 threads
  (8×llvmpipe workers, 8×JS Helper, 4×jemalloc bg, 4×WRWorker +
  WRSceneBuilder + WRRenderBackend, StyleThread×2, GlobalPool×4,
  Script, Constellation, FetchThread, ResourceManager, storage trio,
  tokio pool…), surfman/GL contexts + framebuffers, SM runtime,
  jemalloc live heap, egui atlas, IPC. jemalloc decay already purges
  freed pages every 5s, so this residue is *live* allocations, not
  decay candidates.

**A loaded DDG tab adds ~110MB** on top (→420MB): DOM+layout trees,
SM heap (js-gc-heap 6→17MB), decoded images, font/shaping caches,
WebRender picture/tile state, net caches.

**Releasable vs structural, ranked:**

1. Decoded image surfaces on *visible-but-idle* tabs — only sizable
   pool (~40-80MB on image-heavy pages). The hide-discard lever already
   releases them for hidden tabs; extending to idle-visible means the
   next paint/scroll must re-fetch+re-decode — real resume jank, so
   deferred (NOT clean). If attempted: gate on compositor keeping the
   painted frame in its FBO, re-decode on demand.
2. Font/shaping caches, glyph runs — few MB each, low value.
3. Thread stack trimming (65 threads) / WR worker count — small each,
   upstream-invasive.
4. `js_mem_max=192` already caps SM growth; `session_history_max_length=5`
   caps dead-doc retention; image cache bounded at 128MiB — all shipped.

**Verdict:** no clean big lever at idle-on-load. The <150MB budget is
unreachable on this architecture today: ~180-250MB is engine runtime
(binary pages + SM + compositor + thread pools + one live document).
Remaining path is upstream — smaller runtime footprint, fewer threads,
shared surfaces — not a shell or prefs fix. Linux note: the 30-nav
flat-933MB result shows retention is solved; the *floor* is the problem,
and it's ~2.5× budget even after llvmpipe subtraction (~355MB).

### 1g. Idle-surface-discard prototype (llvmpipe VM, feather-engine) — DROPPED

Coordinator asked: prototype discarding decoded surfaces on a
*visible-but-idle* tab, measure resume jank vs RSS saved, ~100ms budget.

**Implementation** (prototyped, then reverted — stash `idle-surface-discard
prototype` on feather-engine): new
`EmbedderToConstellationMessage::DiscardWebViewIdleResources` →
`ScriptThreadMessage::DiscardIdleResources` →
`Document::discard_idle_resources(cx)` — the Hidden-branch discard
(image_cache completed loads + `drop_cached_layout_trees` + animating
state + per-node `clear_layout_data` + per-img `discard_decoded_image` +
`window.gc`) with NO visibility change; `idle_resources_discarded` flag →
lazy `restore_idle_resources` called from `Window::reflow` (every repaint
stimulus funnels there). Shell trigger: Ctrl+Shift+D + resize/wheel
instrumentation on `notify_new_frame_ready`.

**Numbers** (3 tabs, active = 40×1600×1200-PNG page, llvmpipe):
- settled RSS ~865-887MB; discard frees ~303MB instantly (850→547 in the
  first run — the pool is real)
- **but it never holds**: the discard itself marks the document dirty
  (`clear_layout_data`/tree drop) → `needs_rendering_update` → the very
  next reflow (within ~1-2s, zero user input) lazily restores → re-fetch
  from HTTP cache (0 network GETs) + re-decode → RSS spikes to ~1100MB
  then decays back to a ~905MB plateau ≈ baseline. Net saving ≈ 0.
- resume→first-frame: 2-5ms (WR retains the last painted tiles — the
  page never visibly flickers while restore races the reflow)

**Why it fails structurally:** a visible document can't keep its painted
frame while its surfaces are dropped — dropping the pins
(DOM `current_request.image`, image-cache entry, ReplacedContents in
layout_data) requires invalidating layout data, which forces a reflow,
which re-requests the surfaces. Suppressing the dirty flag instead risks
the compositor receiving a blank display list — page blanks while the
user reads it. Either way the ~300MB pool cannot be held on a *visible*
tab. The hidden-tab path (§1a/§1b) already covers the real case; a
visible idle tab's honest floor stands at the §1f decomposition.

**Residual cost if pursued anyway:** a ~300MB churn cycle + decode CPU
burst every idle→wake, for ~0 steady-state gain. Dropped per the
coordinator's janky→drop criterion; the honest number is "save doesn't
hold", not "jank too high".

## 1h. Permanent-webview wedge fix (bb5c9bc9b94, 2026-11-24)

**Bug**: after any navigation that never fires LoadFinished (wa.me/api.whatsapp.com, and the other consistent-timeout sites in the tranco-500 crawl), the webview was permanently dead — every subsequent navigation timed out in that process, for webdriver AND real users. NOT the arena purge/download spool/hidden-doc GC (Windows lane's prime suspect was a red herring).

**Root cause — three cooperating layers**:
1. `webdriver_server::handle_command` pre-command `wait_for_navigation_to_complete` blocked EVERY command (incl. the next Get) for the full 30s page-load timeout because the stuck webview never left `navigating_webviews`. → `Get` now exempt: clears stale navigating state, since a new navigation inherently abandons the old one.
2. `constellation::load_url` silently `return None`-dropped new top-level navigations while a pending session-history change existed. → now supersedes: closes the stuck pipeline, latest navigation wins (mirrors the iframe-path supersede).
3. Fetch-layer-cancelled navigations (`NetworkError::LoadCancelled`, e.g. redirect into whatsapp:) vanished silently in `script_thread::handle_fetch_metadata` — no abort, pending change pinned forever. → now sends `AbortLoadUrl` mirroring the 204/205 path.
Plus: unnavigable schemes dropped at `load_url` entry before any pipeline exists.

**Verify**: example.com ok → wa.me timeout → example.com 12ms ok (was: permanent timeout loop). Underlying defect remains: api.whatsapp.com never fires LoadFinished — separate engine bug, affects the ~10 consistent-timeout tranco sites.

## 1i. Tranco top-500 macOS crawl (2026-11-25) — baseline + second defect class

500-site WebDriver crawl on feather-engine head → `crawler/baselines/tranco500-macos.jsonl`
(feather repo). **330 ok / 500; 157 infra-side apexes (dns 123, tcp 22, cert 12);
13 engine-verdict fails → ~96% real-site compat.**

**Wedge-fix verified on macOS**: webex.com nav → clean `{"error":"timeout"}` at 30s,
then same session navigates example.com ~1s. `bb5c9bc9b94` works on arm64.

**Wedge signature on macOS (for future crawls):** one bad nav kills the whole
WebDriver HTTP port (`/status` and `/session` both hang). Burned rows record
`driver_error` with `load_ms=None`, or `nav_error` w/ "Connection refused" +
load_ms~0 — nav_error is NOT in the crawler's TRANSIENT list, so strips miss
it; requires a per-site fresh-engine retest pass. Watchdog addition: restart on
≥6 consecutive transient rows (streak), cap retries at 3 attempts/site.

**NEW DEFECT — post-nav driver hang (still live post-wedge-fix, deterministic):**
7 sites break the WebDriver session even on a fresh `158f0e329` engine:
- `webex.com`, `meraki.com`, `cisco.com` — nav starts, then every later request
  hangs; no row produced in 240s. Pages never settle (webex loops tracker
  requests forever).
- `alidns.com`, `imdb.com`, `smartadserver.com`, `stanford.edu` — nav returns,
  then a post-nav request dies → `driver_error` every attempt.
Distinct from the timeout-supersede wedge: the driver itself stops answering
mid-flow. Likely a post-load probe (execute/screenshot) hanging on a
never-settling page, or the unfinished-navigation class via a different path.
Minimal repro: servoshell --webdriver + nav one of these + probe
`/session/{id}/title` after.

**Real engine fails worth investigating:** `load_error` on hosts that pass
dns+tcp+tls probes: nflxvideo.net, elasticbeanstalk.com, mybluehost.me,
gcdn.co, huawei.com. `slack.com` consistent >30s timeout (wa.me/aol class).

## 1i. Load-event stall family — drain race fixed; residual is upstream net-class (2026-11-25)

**Drain race fix** (`23f1a29f30e`): bootloader pages (api.whatsapp.com) insert the next async
script batch synchronously from each executing script. `wait_until_asap_scripts_have_executed`
checked the sets after execution — already re-populated — so the step-7 drain never completed.
Fix: `has_finished_all_asap_scripts` only gates on parser-inserted scripts; dynamic insertions
still execute but don't re-arm the spin. Spec-letter is ambiguous (prepare-a-script sets
'delaying the load event' unconditionally for external scripts) but browsers observably don't
let post-parser-end insertions re-gate indefinitely; commit message has the honest reading.

**Navigate-and-cancel for external schemes** (`deab4229838`): the bb5c9bc9b94 silent early-drop
in `load_url` is removed. Document navigations into unnavigable schemes now proceed, cancel at
the fetch layer (`LoadCancelled` → `AbortLoadUrl`), and leave the current document untouched —
matching what browsers do. **Integration point for a future shell "open in app?" prompt hook:
the removed early-drop site in `constellation.rs::load_url` is where an embedder callback for
external schemes would slot in** (surface the scheme+url to the shell, let it prompt/discard).
Don't build it yet — noted per coordinator.

**The residual stall is NOT a feather regression.** Probed stock: wa.me ends on `whatsapp://chat/`
with title "Error loading page" — stock HARD-ERRORS; the fast "loads" I measured in the wedge
era were error pages. On feather-engine the landing survives, its boot chain terminates
naturally (~21 fetches in the first second, then total network silence), yet load never fires:
a handful of script **body streams** never finish reaching the document — net logs `got 200
response` for exactly the scripts stuck in the document's `blocking_loads` (data:-URI images
transiently stick too but do resolve). **aol.com reproduces the same stall on STOCK servo** —
so this family is an upstream-class fetch-completion/net bug (chunked body stream dies
mid-flight, no error ever surfaces), not a feather patch. Fix belongs upstream in the net →
script fetch handoff; the stall signature is: `blocking_loads` entries whose URLs already show
`got 200 response` in the net log.

## 1j. WebDriver post-nav hang — root cause + layer-1 fix (2026-11-25)

Whole-actor hang after nav on webex/meraki/cisco-class sites: a long
page JS task (minutes-long timer callback, heavy NumberToAtom churn)
starves the single script thread; driver's `wait_for_ipc_response` had
no timeout so the single dispatcher thread parked forever — even /status
died. Fix `7d150c23b`: bound every `wait_for_ipc_response*` at session
script timeout + `IPC_RESPONSE_MARGIN` (30s), expiry → classified
`timeout` error. Verified on all 7 sites: clean fail-fast + /status
responsive; 5 sites flipped to ok in the macOS baseline.
Layer 2 (runaway JS = "slow script" class) is a product decision, deferred.

## 1j. Progressive media playback diagnosis (Nov 25, stage-0 MSE)

Enabling `media-gstreamer` in the feather shell surfaced a truncation
scare: test-videos.co.uk Big Buck Bunny ended at 1.3s of a 10s clip in
every configuration. The investigation ladder, in order of what was
eliminated:

1. **servo's byte delivery is correct.** Instrumented htmlmediaelement:
   all bytes of every fetch (initial + seek refetches) reach
   `push_data` with correct bookkeeping. The loss was never in script.
2. **qtdemux demuxes all 300 samples correctly** — "no next entry, EOS"
   is *correct* end-of-stream for a fully-buffered file, not a defect.
3. **The burn is real but file-specific**: the whole video consumed in
   1.3s wall-clock because nothing throttles to presentation time —
   reproducible in pure `gst-launch-1.0` (no servo!) with fakesink
   sync=true, glimagesink, even the clocksync element, on gst 1.20.
   ffmpeg-generated mp4s (with or without audio) and VP9+Opus webm all
   sync perfectly at exact realtime duration in the same harness.
   Verdict: gst-1.20-vs-that-file quirk, NOT a servo bug. Don't use
   test-videos.co.uk BBB for media verification; generate assets with
   ffmpeg instead.
4. **Real servo bug found and fixed (`74d9743`)**: eager appsrc EOS
   permanently poisons the source for post-seek refetches (pushes after
   EOS are dropped as FlowError::Eos→Ok). EOS is now deferred to
   need-data-drain and disarmed by seeks.
5. **Remaining known hole: replay-after-end.** appsrc EOS is final and
   push-mode byte seeks don't flush upstream, so reseeking after EOS
   never revives the stream. Fix direction: inject a flush into the
   wrapped appsrc or recreate it when a seek arrives post-EOS.

Verified working end-to-end on Linux: MP4 h264(+aac), mp4 video-only,
webm vp9+opus — realtime playback, exact forward/backward seeks, stable
pause, resume. WebM/VP9+Opus (the MSE target family) works — the
WebM-first plan in docs/mse-plan.md holds.

## 1k. MSE stage 1 — DOM surface (Nov 25, feather-engine `a1f0fcde987`)

MediaSource/SourceBuffer/SourceBufferList exist per spec; isTypeSupported
answers honestly from the gst registry via `ServoMedia::can_play_type`.
Verified live: vp9/opus webm true, fake codecs false, spec-correct
InvalidStateError behavior in closed state.

Binding-authoring lessons that cost compile iterations (read before writing
the next interface):

- Trait signatures are the ground truth: attribute accessors are
  `Get`/`Set`-prefixed only for `[Throws]` attributes or when codegen decides
  so — read the generated trait in
  `target/debug/build/servo-script-bindings-*/out/Bindings/<Iface>Binding.rs`
  before writing the impl, not after.
- `double` args arrive as `Finite<f64>`; `unrestricted double` attrs stay `f64`.
- Static methods take `&D::Window` (concrete `&Window`) as first param.
- Constructors implicitly receive cx — never list 'Constructor' in
  Bindings.conf `cx` arrays (hard build error).
- Methods that need cx for object creation: list the method name in
  Bindings.conf `'cx': [...]` — do NOT use `script_runtime::temp_cx()`.
- Union args arrive as `codegen::UnionTypes::FooOrBar` enums holding DomRoot
  values; changing a signature to a union breaks all callers — construction
  is `Union::Variant(DomRoot::from_ref(&**x))`.
- Event handlers: `event_handler!(name, GetOnname, SetOnname);` inside the
  methods impl, no manual plumbing.
- `#[no_trace]` on struct fields that aren't JSTraceable (Uuid etc).
- webidl files are auto-scanned (no registration list); DOM impls need
  `pub(crate) mod` in the parent mod.rs only.
- cargo check from the servo repo root fails on missing pkg-config deps;
  always check/build from `~/feather/shell` (the proven env).

Probe harness: write results into `document.title`, read with
`xdotool search --name PROBE | xdotool getwindowname` — no webdriver needed
for pure-JS surface verification.

## 1l. MSE stage 3 architecture + decode validation notes (Nov 25, in progress)

Player instantiation path: `HTMLMediaElement::create_player` (line ~2345) →
`ServoMedia::get().create_player(...)` → gst backend `GStreamerPlayer::setup`
builds `playbin3` with uri `servosrc://` handled by ServoSrc (one appsrc in a
GstBin, script pushes bytes through it). For MSE the plan: new `servomse://`
element — one appsrc PER SOURCEBUFFER (YouTube uses separate audio+video
SourceBuffers; each SourceBuffer bytestream is a standalone valid fragmented
stream), feeding demux inside playbin3. Bytes flow: appendBuffer (script) →
new servo-media player API push_buffer(track,bytes) → appsrc.

Isolated decode validation (gst-launch, /tmp/msetest/segs): ffmpeg-made
fragments. qtdemux eats frag.mp4 (frag_keyframe+empty_moov) fine with
openh264dec; matroskademux eats webm segments with vp9dec. KEY RISK RETIRED:
concatenated webm segments (cat seg0 seg1 seg2 — simulating sequential MSE
appends) decode through matroskademux with NO errors. avdec_h264/avdec_vp9
do NOT exist on stock Linux (gst-libav absent) — decoders are openh264dec +
vp9dec. gst-libav absence also confirms WebM-first: MP4 decode depends on
openh264 (Cisco, BSD) being present.

Key integration facts: eos semantics — endOfStream() is the ONLY eos for
MSE; duration/seekable come from the MediaSource object not the stream;
seeks within buffered ranges must hit appsrc-cached data or the segment
index (the plan's per-buffer segment index in script space). SourceBuffer
append order is arbitrary — script-space reordering by appendWindow/
timestampOffset before pushing to appsrc.

Probe harness pattern that works without webdriver: results into
document.title, read via `xdotool search --name PROBE | xdotool
getwindowname` after sleep 10. Probe pages in /tmp/msetest/probe*.html.

## 1m. MSE stage 3 KEY INSIGHT — push API already exists (Nov 25)

`servo_media::player::Player` trait ALREADY exposes `push_data(Vec<u8>)` and
`end_of_stream()` — the byte-injection API MSE needs. Progressive playback
feeds bytes through the same path from the fetch machinery
(htmlmediaelement.rs:3988 DataBuffer::Payload → player.push_data).

Therefore stage 3/4 minimal path needs NO new gst element for the
single-SourceBuffer case: attach creates a normal servosrc-backed player,
appendBuffer routes bytes to player.push_data, endOfStream routes to
player.end_of_stream. Concatenated webm segments demux fine (validated
1l). The multi-pad servomse element (separate audio+video appsrc pads) is
deferred to stage 5 where two SourceBuffers must coexist.

Glue gaps being built: (1) attach_media_source must create the player —
currently creates none; (2) SourceBuffer needs media-source backreference,
MediaSource needs element backreference; (3) appendBuffer routing +
updating flag + update events; (4) EOS only from endOfStream — must NOT
let servosrc auto-EOS; (5) StreamType choice for MSE player.

## SAB/Atomics shim experiment verdicts (Nov 25, macOS lane — stage 0 of the SAB unlock)

Decisive measurements with a pure-JS fake-SAB/Atomics shim (ArrayBuffer subclass + functional Atomics ops stubs, injected via feather's boosts.json userscript system — zero engine changes):

- **claude.ai / Cloudflare turnstile: UNLOCKED** — the challenge completes and the full sign-in app renders with the shim. Cloudflare's use of SAB is SHALLOW (probe-level). The real implementation (stages 1-2 per docs/sab-atomics-scoping.md) is de-risked by measurement.
- **accounts.google.com: UNCHANGED** — still "browser may not be secure" with the shim. Binding constraint for Google is the non-JS signals (TLS/JA3 prime suspect per docs/google-signin-decision.md); accepted limitation, do not chase.

Symptom: `typeof SharedArrayBuffer === 'undefined'` in servo (anti-bot payloads probe it) → check: mozjs-sys makefile.cargo `--disable-shared-memory` flag (CONFIRMED CAUSE — the engine implementation is compiled out entirely), `JS::RealmCreationOptions::setSharedMemoryAndAtomicsEnabled` in mozjs/js/public/RealmOptions.h (the embedder opt-in that exists but is dead while the build flag is off), servo's WebIDL surface in components/script_bindings/webidls (no SharedArrayBuffer.webidl exists — bindings work needed even after the flag flip), structured clone in script (needs a share-not-copy branch for real implementation).

Symptom: postMessage/structuredClone of a real SharedArrayBuffer throws DataCloneError (cross-agent sharing dead even though the built-ins exist) → check: CloneDataPolicy literals in components/script/dom/bindings/structuredclone.rs write/read paths (CONFIRMED CAUSE — both hardcode allowSharedMemoryObjects_: false + allowIntraClusterClonableSharedObjects_: false, and use StructuredCloneScope::DifferentProcess; per mozjs/js/public/StructuredClone.h shared-memory cloning requires the policy opt-in AND is only possible intra-process — servo's agents are in-process threads so SameProcess scope is the correct configuration for window↔worker messaging), the realm option sharedMemoryAndAtomics_ in script_bindings/interface.rs (status: FIXED 398467e6a — enabled, built-ins verified live), mozjs build flags (status: ruled out — desktop builds compile shared memory in by default; --disable-shared-memory is WASI-only). FIXED 5f26d9f7a: write/read now default SameProcess scope + policy allowed, sabCloned veto removed; IndexedDB persistence paths use explicit DifferentProcess variants. Verified TRULY-SHARED memory + cross-agent Atomics on macOS. CAUTION for future work: flipping the policy flags while keeping DifferentProcess scope is memory-corruption-fatal (silent death, no panic) — scope and policy must always be coherent. Residual: claude.ai turnstile STILL stalls even with correct shared memory — SAB was necessary but not sufficient; remaining suspects per the Cloudflare doc: OffscreenCanvas (confirmed absent), wasm threads, Atomics.wait/notify edge behavior.

## 1n. MSE post-stage-3 state (Nov 25, feather-engine 41d0c9c5f64)

DONE: stages 1-3 core. appendBuffer->push_data->decode->playback verified.
Next in order: (1) buffered-range indexing — SourceBuffer needs segment
timestamps; design per plan: parse segment boundaries script-side OR query
player.buffered() (player trait already exposes buffered()! check what it
returns for push streams — player.rs ~line 356 reads servosrc buffered
ranges); (2) duration from stream end; (3) updating abort on remove/seek.
Probe harness: /tmp/mediatest/probe5.html served on :8741, feathers run
with `pkill -x feather; rm -f session.json` first, read title via xdotool
after sleep 14. Test segs: /tmp/mediatest/seg_v0.webm seg_v1.webm.

## CSP opaque-origin panic — fixed upstreamably (Nov 25, macOS lane)

`csp.rs` frame-ancestors ancestor walk `.expect()`ed every ancestor origin to parse
as a URL; opaque origins (sandboxed/srcdoc frames — universal ad-tech pattern on
amazon/bbc/cnn/espn) serialize as `null` → panic → with feather's `panic=abort`
production profile the whole browser died at 5-40 real-world tabs. Root-caused
during the bakeoff2 memory-scaling pass (deaths had no crash reports/jetsam traces
because abort+strip leaves nothing; NOT memory pressure — reproduced at 5 tabs/1GB).

Fix `1bf410a0e`: skip opaque entries in both the same-origin fast path and the
cross-origin constellation path, continuing the walk upward (spec step 6.2 is itself
incomplete — it assumes the parse cannot fail). Verified 20/20 consecutive 6-tab
amazon/bbc/cnn/espn/reddit/ebay sessions survive on macOS (previously
near-deterministic death by ~tab 6 in the same scenario).

Upstreamable-gold notes: the spec gap is real (w3c/webappsec-csp step 6.2 of
frame-ancestors navigation check); a servo upstream PR should reference it.
WPT CSP-subset regression run not yet done — no servoshell build fits the macOS
box's disk (needs ~10GB, had ~2GB); behaviorally no test can regress since the
only changed path previously crashed. Lane with disk should run
`tests/wpt/tests/content-security-policy` stock-vs-patched when convenient.

## Clipboard multi-format write fix (9edd19e2e, feather-engine)
`write_content_to_the_clipboard` iterated all drag-store items issuing one
`SetClipboardText` per text item — last-write-wins regardless of MIME type, so
Sheets-style copies (text/plain + application-internal payloads written after)
put internal GUID tokens on the clipboard instead of cell text. Fix: write only
the `text/plain` item (first match), falling back to the first text item when
no explicit plain type exists. Verified live on macOS: multi-format copy page
with plain-first/internal-second ordering now yields the plain text; plain
static, multi-script (CJK+accented), and editable-input copies unaffected.

## Symptom index (clipboard investigations)

Symptom: copy of static page text writes nothing to the system clipboard → check: `run_default_keydown_handler` in document_event_handler (owns key→editing-action routing; status: ruled out — copy chain verified end-to-end live, exact drag-selected text lands in the pasteboard), `DocumentSelectionDragHandler`/`document.selection` population (prime suspect when it reproduces — the original observation most likely was a drag that never formed a DOM selection; not reproducible since), arboard fallback (status: ruled out on macOS — pasteboard writes verified).

Symptom: copy puts an internal GUID/token on the clipboard instead of visible text (Sheets-class apps) → check: `write_content_to_the_clipboard` in editing (owns copy-event data-store→clipboard writes; CONFIRMED cause — it flattened every text item with one SetClipboardText each, last-write-wins regardless of MIME type; FIXED in 9edd19e2e: only text/plain written), `DataTransferItemList::set_data` ordering (status: ruled out — insertion order is faithful to the page's calls).

Symptom: copy works in-page but never reaches the system clipboard → check: `clipboard` module in servo/clipboard_delegate (owns arboard access; CONFIRMED trap — silent in-memory fallback on any arboard error; loud warn-once added in a922f7fa4), `DefaultClipboardDelegate` wiring (status: ruled out — delegate chain is sound).

Symptom: script thread wedges during paste (page unresponsive, no crash) → check: paste-path `GetClipboardText` blocking reads in editing paste handling and `EmbedderClipboardProvider::get_text` in text_input (CONFIRMED unbounded IPC — bounded to 5s in a922f7fa4), script-event-loop starvation by runaway JS (status: ruled out for this symptom — separate slow-script class, see PR #58 docs).

## 1o. MSE stage 5 — multi-track servomse (IN PROGRESS checkpoint, 2026-11-25)

Landed/working tree: `servomse` GstBin element (components/media/backends/gstreamer/mse_source.rs) — one appsrc per SourceBuffer track, sometimes src pads, uridecodebin autoplugs each pad into its own decodebin (verified by topology walk — sometimes-pad sources DO autoplug correctly in playbin). StreamType::MediaSource plumbed: script → create_player → uri servomse://. Track routing verified correct end-to-end (video bytes track 0, audio track 1).

**CRITICAL PIPELINE-CREATION BUG FIXED (the reason everything since stage 3 silently used servosrc)**: `HTMLMediaElement::resource_fetch_algorithm` calls `create_media_player` FIRST THING, before the blob-ms attach short-circuit. The player was always created with media_source=None → StreamType::Seekable. Fix: hoist the blob-url→attach check ABOVE create_media_player. Mechanism lesson: any element-level state that affects player construction must be set before the fetch algorithm runs, or the fetch algorithm is the wrong place to detect it.

**OPEN BUG, partially solved (debugging in harness /tmp/mseharness)**: data now flows — typefind detects video/webm + audio/webm at probability 100 per track, matroskademux+vp9dec+opusdec negotiate 1920x1080@30 and decode actively, demux src pads link multiqueue. REMAINING: playbin never leaves Paused — no frames reach the fakesinks despite decode chain being fully wired. Next suspects: timestamp/preroll semantics, multiqueue backpressure, or insufficient data reaching demux tail. Harness iterations: GST_DEBUG=6 works in harness (lower levels silent); grep task-thread ids for flow evidence; pad probes fire correctly when attached before pushes.

SOLVED sub-cause chain (each verified in harness):
(1) dynamic appsrc NULL state → fix: `sync_state_with_parent` after bin add.
(2) **THE big one — `is-live=true` on appsrc makes basesrc wait for PLAYING before emitting ANY buffer (`gst_base_src_wait_playing_unlocked: live source waiting for running`), but playbin can't reach PLAYING because preroll needs those buffers — circular deadlock.** Fix: do NOT set is-live on MSE appsrcs. ServoSrc doesn't either. Lesson: "live" in gst means timestamped-at-arrival sources; MSE segments carry their own timestamps, they must flow in PAUSED like file data.
(3) ruled out: sometimes-pad autoplug (works perfectly — uridecodebin creates one decodebin per pad), ghostpad wrapping (microtest2 green), dynamic-add-then-link pattern itself (microtest green), activation ordering, state cycling.

### Symptom index additions (per standing rule)
- Symptom: two-track MSE plays audio but video chain dies "Mismatch reparsing track type / Expected 1, found 2" → check: `HTMLMediaElement::resource_fetch_algorithm` ordering (owns player creation — WAS the confirmed cause: player created pre-attach as servosrc, both tracks' bytes concatenated into one stream), `servomse` track routing (ruled out — instrumentation showed correct per-track sizes), matroskademux chaining tolerance (ruled out — probe9 single-track same bytes clean).
- Symptom: MSE playback works but zero servomse activity → check: whether `create_media_player` ran before `attach_media_source` (confirmed cause — see above); symptom masks itself completely because the servosrc fallback plays single-track content fine.
- Symptom: pipeline stalls pre-preroll with no error, no EOS, position frozen at 0 → check: dynamically-added element states (`state=Null` in topology walk means missing `sync_state_with_parent` — confirmed first cause), then appsrc `current-level-bytes` property (if bytes accumulate = streaming task never started — OPEN, see above).
- Symptom: gst element added to running bin accepts data but never processes → check: `sync_state_with_parent` call (owns state propagation for runtime-added elements — gst_bin_add does NOT reliably propagate; silent because pushes return Ok into the internal queue regardless).

Symptom: gst streaming thread or script thread wedges permanently after a seek (playback frozen, all later media ops stall) → check: `SeekChannel::_await` in servo-media gstreamer player.rs (owns the seek roundtrip — was an unbounded recv; FIXED with 5s bounded try_recv_timeout, seek reported as failed on timeout), `SeekLock::unlock` ack wait in servo-media-player lib.rs (same class, same fix). Unbounded-IPC wedge family — same pattern as the clipboard GetClipboardText and WebDriver wait_for_ipc_response fixes; any NEW cross-thread recv introduced in media code must be bounded at review time.

## macOS video playback verification + symptom index entries
Progressive MP4 (h264/aac 720p, ffmpeg-generated) plays to completion in
feather on macOS with correct frame rendering — gstreamer pipeline initializes
fine against the GStreamer.framework build (needs GST_PLUGIN_PATH +
DYLD_FALLBACK_LIBRARY_PATH pointing at the framework at runtime, plus the
rpath symlink dir for the dylibs). EOS 540MB RSS after playback.

Symptom: video plays first time but replay-after-end never advances (playing state, time frozen at 00:00) → FIXED root cause chain (macOS lane, instrumented end-to-end): (1) servo-media `GStreamerMediaPlayer::play` (owns forwarding play into the gst pipeline — CONFIRMED PRIMARY CAUSE: its `if !paused return` guard no-op'd because nothing ever sets paused=true at EOS, so replay never forwarded play() and the pipeline never resumed consuming); (2) `connect_end_of_stream` handler in player.rs (owns EOS bookkeeping — CONFIRMED SECOND CAUSE: never mirrored EOS into the paused flag); both fixed by always forwarding play + setting paused at EOS. HTMLMediaElement ended/replay spec logic (status: ruled out — internal_play_steps correctly fires ended=true and the spec seek-to-0 once the player state is consistent), servosrc pending-EOS deferral in source.rs (status: ruled out — appsrc self-unseals on successful seek per gst-1.22 do_seek is_eos=FALSE, and bytes post-seek are accepted with zero rejections), seek/re-fetch chain (status: ruled out — completes fully post-seek: seek propagates, need-data refires, re-fetch EOFs), qtdemux consumption (status: ruled out as independent defect — it resumes once the pipeline is actually set playing). RESIDUAL UNFIXED: the seek_channel `._await()` blocking wait in the appsrc seek_data callback can wedge the gst streaming thread in principle (unbounded-IPC class, same family as clipboard); position query reports 0 briefly post-seek which can confuse `ended_playback` precision logic.

Symptom: video element renders black/no frames on macOS → check: GST_PLUGIN_PATH/DYLD fallback env at runtime (CONFIRMED requirement — without the framework plugin path no playback pipeline exists), rotation metadata in render path (status: Windows-lane defect, unverified on macOS), gst pipeline init (status: ruled out — frames render correctly with env set).

- Symptom: gst pipeline stuck pre-preroll forever, zero errors, events flow but no buffers → check: `is-live` property on any appsrc/basesrc in the chain (CONFIRMED cause — live sources hold all output until PLAYING, which can never arrive without their data; evidence string `live source waiting for running` in gst_base_src_wait_playing_unlocked). Also check `current-level-bytes` on each appsrc: nonzero-and-static = upstream blockage, draining-then-stall = downstream backpressure.
- Symptom: typefind errors "Stream doesn't contain enough data" on EOS → check: whether EOS raced ahead of data because a pad flush event destroyed queued buffers in transit (evidence: burst of `_gst_buffer_free` finalizes at flush time) — likely artifact of manual state cycling, NOT a real MSE hazard (ruled out for normal operation).

## Symptom index additions — media/memory investigations (Windows lane)

### Symptom: memory grows without bound scrolling an infinite media feed (synthetic mp4 autoplay test: +386MB over 25 scroll bursts)

→ check: `HTMLMediaElement` player lifecycle in `components/script/dom/html/embedded_content/htmlmediaelement.rs` (owns per-element gstreamer pipeline; confirmed cause — players were never suspended/torn down when scrolled offscreen, ~15MB/player accumulating with count; single player is flat over time so NOT a frame-queue leak)
→ check: net `ImageCache` in `components/net/image_cache.rs` (owns decoded image bytes; status: ruled out — already LRU-capped per pipeline and evicts with WR `DeleteImage`; dropping the cap 128→16MB left reddit scroll growth unchanged at ~+1100MB)
→ check: WR-side image resources / DOM node retention (status: ruled out at synthetic scale — infinite-images +3MB and infinite-DOM +6MB over the same scroll protocol)

FIXED on feather-engine: viewport-aware eviction — video elements entirely beyond −2vh/+3vh bands are paused AND fully torn down (`reset_media_player`), resumed with position restore inside the 1.5vh/0.5vh hysteresis band; scan in `Document::scan_media_viewport_eviction` throttled to 500ms from the scroll dispatch path. Verified: synthetic +386→+154MB (residual = band-resident players, by design). Hysteresis mandatory — pausing alone does NOT release gst memory (measured), teardown is required.

### Symptom: memory grows ~+1000MB deep-scrolling reddit (chrome flat on the same scroll)

→ check: `HTMLMediaElement` / media players (status: ruled out — instrumented scan shows ZERO active media elements during an entire reddit deep scroll; reddit's DASH/HLS videos never become players in servo. The video-eviction fix cannot affect reddit.)
→ check: net `ImageCache` (status: ruled out — see above; 16MB cap experiment)
→ check: heavy DOM/layout accumulation on real posts (status: ruled out — realistic-weight synthetic ~600 elements/post incl. nested comments+svg grew only +16MB over the same scroll protocol)
→ check: per-pipeline costs of ad/tracking iframes (prime suspect, mechanism evidence: infinite iframe-image synthetic grew +111MB with only ~15 frames ≈ 7MB per iframe pipeline that is never released; the net image cache is per-PIPELINE so each iframe also gets a private 128MB decoded-image budget with zero cross-document pressure. Matches both magnitude and the wild run-to-run variance since ad injection varies per load. Unconfirmed on live reddit — no direct iframe-count measurement yet.)
→ check: JS heap growth from scroll-triggered execution (unexplored)

DECOMPOSITION (2026-11-25, controlled variants): empty iframes cost ~0.5MB each (nearly flat +8MB/15 frames) vs image-filled iframes ~7MB each (+111MB/15 frames) — per-iframe cost is ~90% PIXEL DATA (decoded images + canvas backing stores), NOT pipeline structure. Images of identical weight in the MAIN document are flat (+3MB) because the single pipeline's 128MB net-cache cap actually binds; iframe pipelines never reach their private caps so eviction never triggers. Canvas backing stores are outside the net cache entirely — untested share on real sites.
→ note: run-to-run variance on live reddit is extreme (+118 to +1060 with identical binary) — single measurements are meaningless here; control content or repeat 3x minimum.

### Symptom: browser process gone mid-measurement during reddit deep scroll

→ check: teardown/resume churn panic in media pipeline (suspect, unconfirmed — one observation, stderr capture now mandatory on all runs)

## 1p. MSE remove() + YouTube-choreography validation (2026-11-25)

`SourceBuffer::Remove` implemented: validates updating/range, spec lifecycle (updatestart/update/updateend from queued task, updating synchronous-true then false), range subtraction rebuilt into the script-side TimeRangesContainer (piecewise: overlap clips at [rs,re) boundaries; no fractional merging).

probe10 verified end-to-end in feather-shell: strictly updateend-gated alternating video/audio appends (YouTube's exact feed pattern), endOfStream, playback continues while remove(0,4) evicts — post-remove buffered ranges exactly [4.3-5.3][8.5-9.5], clock advancing. This is the permanent stage-6 regression harness: it replicates YouTube's append choreography without the bot wall.

### Symptom index additions (per standing rule)
- Symptom: `remove()` throws NotSupportedError or leaves buffered unchanged → check: `SourceBuffer::Remove` in components/script/dom/media/sourcebuffer.rs (owns the range subtraction — was unimplemented stub, now implemented and verified), TimeRangesContainer piecewise arithmetic (prime suspect for boundary bugs — half-open [rs,re) semantics, no fp epsilon merging).
- Symptom: YouTube page loads but no videoplayback segment requests → check: bot-wall layer FIRST before any engine work — `rr*.googlevideo.com` NXDOMAIN or IPv6-only resolution means CDN null-routing (CONFIRMED environmental on v4-only VMs — engine is innocent; evidence: 9 videoplayback requests issued = player JS accepted our MSE), "Sign in to confirm you're not a bot" overlay (anti-bot, not an engine defect), embed Error 153 (embedder-identity rejection).
- Symptom: MSE playback appears to work but eviction logic never fires → check: buffered end reaching the eviction window (if buffered is indexed correctly per §82af8c9113e the problem is downstream), updateend-gated append loop (ruled out — verified correct lifecycle in 41d0c9c5f64).

## 1q. MSE seek re-feeding — timebox investigation results (2026-11-25)

Harness evidence (seektest in /tmp/mseharness, pure-gst replication):
(1) gst pipeline seek events FAIL OUTRIGHT in push mode — `seek()` returns BoolError "Failed to seek": the TIME seek converts to a byte seek upstream, appsrc cannot serve it, whole seek is rejected (pipeline keeps playing linearly — nothing happens at all).
(2) Manual flush_start/flush_stop(reset_time) pushed downstream on servomse's ghost src pads succeeds mechanically — matroskademux resets and re-parses a fresh stream cleanly post-flush (full EBML header re-read, update_tracks, SimpleBlocks parse 'ok'). Demux recovery is NOT the problem.
(3) BUT playback never resumes: position frozen at exactly the last pre-flush frame, zero buffers reach sinks even when post-flush data is timestamp-aligned with the current position. The post-flush restart needs the serialized event sequence (stream-start/caps/segment) re-emitted in-stream before data.
(4) Naively pushing stream-start/segment events from the application thread into the live chain HANGS/crashes the pipeline immediately — they are serialized events that must originate inside the streaming flow, not from outside threads.

Conclusion: correct implementation = the flush+event re-sequence must be driven from within the data path itself (what WebKit's WebKitMediaSrc does with its own streaming thread — PausedTask state machine, seek-pending flags processed in-stream). NOT an hours-scale fix; honestly 2-4 days. Full writeup in this file; harness seektest preserves the repro.

### Symptom index additions (per standing rule)
- Symptom: seek on MSE/push-mode playback does nothing at all (position never moves, no error surfaced) → check: `PlayerInner::seek` in components/media/backends/gstreamer/player.rs (owns seek dispatch — gst seek events FAIL OUTRIGHT upstream of appsrc in push mode, confirmed "Failed to seek" BoolError; the failure is silent in the player), script-side seek event wiring (ruled out — events fire and currentTime sets correctly).
- Symptom: after manual pad flush on MSE source, demux appears dead → check: matroskademux push-mode recovery (ruled OUT — re-parses fresh stream cleanly post-flush, verified in gst debug log), post-flush event re-sequencing (CONFIRMED suspect — stream-start/segment must be re-emitted inside the streaming flow; pushing them from application threads hangs the pipeline).
- Symptom: pipeline position frozen at last frame after MSE flush despite data flowing → check: serialized event sequence after flush_stop (prime suspect — buffers without preceding segment events are dropped/misscheduled downstream), vp9dec keyframe requirement (unchecked — vseg files start with keyframes so unlikely), sink re-preroll state (unchecked).

## 1r. MSE seek swallow (v1) — verified nonfatal (2026-11-25)

Two-layer seek swallow for StreamType::MediaSource: player-level early-return in `PlayerInner::seek` AND script-level early-return at the top of `HTMLMediaElement::seek` when media_source is attached. BOTH are required — player-level alone still wedges: the element sets current_playback_position to the seek target and entering the seeking state pins position reporting forever (seek completion never arrives). probe11 verified: playback clock continues advancing through a seek attempt (1.82→3.82→11.99), no freeze, no error, paused=false.

### Symptom index additions (per standing rule)
- Symptom: playback position frozen at exactly the seek target after any seek attempt, clock never advances again, no error → check: `HTMLMediaElement::seek` seeking-state completion path (CONFIRMED cause — element enters seeking state, position pinned to target, `seek_end` never fires because the gst-level seek can't complete in push mode; player-level swallow alone is INSUFFICIENT), player position reporting (ruled out — pipeline keeps playing fine underneath).

### Symptom: images on infinite-scroll pages accumulate decoded bytes without bound (big-image synthetic +214MB over 25 scrolls)

→ check: DOM pin `HTMLImageElement.current_request.image` (status: INSUFFICIENT — dropping it verified working, but memory unchanged)
→ check: net cache `completed_loads` entry (status: INSUFFICIENT — evicting it alongside the pin verified working, memory unchanged; also the reason ALL cache-budget approaches — per-pipeline 128MB, 16MB, or the shipped global 384MB — can never bound live-document images: eviction skips entries pinned by strong_count>1)
→ check: layout `ReplacedContents` in the fragment tree (CONFIRMED final pin — the decoded Arc<RasterImage> is shared single-copy across DOM/cache/layout; ALL THREE pins must drop to free anything. The layout pin only drops via layout-data invalidation → reflow → box reconstruction → image re-request → instant re-pin. Churn is structural: infinite scroll triggers constant reflows via content insertion.)
→ VERDICT: no cheap safe win. The real fix requires either content-visibility-style layout skipping of offscreen subtrees (servo lacks it — large feature) or byte-swappable RasterImage shells with paint-time re-decode (deep engine surgery). Weeks-scale either way. The pin-release implementation was built, verified mechanically correct (113 evictions logged in one scroll test), measured useless, and REVERTED — do not re-attempt the same two-pin variant.
→ note: images below the initial viewport appear NOT to decode until scrolled near (has_image=false below viewport in scan logs) — servo already has partial lazy behavior; retention is for once-visible content only.

## 1s. Signed-in YouTube verification + cookie persistence + real-world MSE recon (2026-11-25)

STAGE 6 VERIFIED: signed-in YouTube plays in feather. Cookie path: user signed into Chrome on the VM, cookies decrypted from Chrome's profile sqlite (Linux v10 = AES-CBC, PBKDF2("peanuts","saltysalt",1,16), IV=16 spaces), converted to servo's cookie_jar.json format (`{version:1, cookies_map{regkey:[{cookie as Set-Cookie-string, host_only, persistent, creation_time, last_access, expiry_time}]}, max_per_host:150}`; SystemTime serializes as `{secs_since_epoch, nanos_since_epoch}`; cookie field is a string parsed by cookie::Cookie::parse — one bad cookie fails the WHOLE jar silently; map keys are eTLD+1 reg domain of the cookie domain attribute, domains written WITHOUT leading dot), dropped into feather's data dir. Verified: personalized feed, pre-roll ad then main video continuous playback with captions, live cookie rotation (SIDCC rotation proves the jar is bidirectionally live).

CRITICAL SHELL FIX: feather's ServoBuilder never set opts.config_dir → resource_thread ran with config_dir=None → cookie_jar.json NEVER read — feather had zero cookie persistence before this fix. Shell now points config_dir at feather's data dir. NOTE: jar writes happen only on clean CoreResourceMsg::Exit — pkill'd sessions never persist jar mutations, but startup reads work regardless.

Recon verdicts (2026-11-25): dailymotion PLAYS. twitch FAILS (black player, page+live-data fine). vimeo FAILS ("Technical Difficulties" catch-all pre-playback). shaka demo FAILS (app UI never populates, pre-player). bitmovin blocked by Cloudflare (environmental).

### Symptom index additions (per standing rule)
- Symptom: cookies never sent to any site / every session starts logged-out despite jar file existing → check: shell ServoBuilder opts.config_dir (CONFIRMED cause when None — resource_thread skips jar load entirely; the jar file location is silently irrelevant), cookie_jar.json deserialize (silent total failure if ANY single cookie string fails Cookie::parse — verify with a trivially-simple test cookie entry keyed to 127.0.0.1 against a local logging server before suspecting individual cookies), cookies_map key format (must be eTLD+1 of the cookie's domain attribute, no leading dot — a leading-dot domain breaks domain_match for all subdomains).
- Symptom: twitch page fully works but player black forever → check: HLS/fMP4+H264 path (prime suspect — twitch is avc-only, supported_codecs=av1,h265,h264 in their usher call; transmuxed hls.js fMP4 appends never exercised in our WebM-first validation), avc decode presence in the gst registry (check avdec_h264/openh264 — avdec absent on stock Linux), `Could not set the playback rate: NonSeekableStream` warnings (MSE/live stream rate-set unhandled), "Trying to exec media action on an unknown client context" (media client lifecycle mismatch, unchecked). DOM surface/network/CDN ruled out — m3u8 master playlists fetch 200 fine.
- Symptom: vimeo video page shows generic "Technical Difficulties" → uncharacterized, occurs BEFORE any media init evidence; suspect player feature-detection JS failing, not yet investigated.
- Symptom: shaka-player demo header/footer render but content area never populates → uncharacterized framework-level JS failure, pre-player; not evidence about MSE itself.

## 1t. Silent hard crash on heavy real sites — amazon, reproducible (2026-11-25)

Observed first during bakeoff persite measurement (process disappeared mid-run), reproduced deliberately 3/3: amazon.com alone, pure wheel scrolling (10 bursts x2 rounds), dies in round 1-2 every time. Also reproduces with bbc+amazon+reddit multi-tab + Ctrl+Tab. NOT tab-switch-dependent, NOT memory (dies at ~600MB on 34GB box).

Evidence negatives: zero ERROR/panic lines in stderr (log ends mid-normal-network-activity), NO Windows Error Reporting event even with LocalDumps configured, NO procdump exception capture. CRITICAL: with procdump actively attached the process SURVIVES all 5 rounds — classic heisenbug/timing-sensitive behavior, race-flavored. Gstreamer-in-path irrelevant (crashes with gst removed). Media eviction teardown churn ruled out as cause (crash predates AND reproduces without gst).

### Symptom index additions (per standing rule)
- Symptom: feather process silently disappears while scrolling a heavy real site (amazon verified; no panic output, no WER event, no exception capture) → check: multiprocess/threading races in script/layout/compositor (PRIME SUSPECT — timing-sensitive heisenbug confirmed by procdump-attachment survival; access violation in native code or silent abort suspected but unproven), OOM (ruled out — dies at <1GB RSS), gstreamer media lifecycle including offscreen-eviction teardown/resume churn (ruled out — reproduces with gstreamer entirely absent from PATH), tab-switch Ctrl+Tab injection (ruled out — reproduces with pure scrolling in a single tab), panic=abort with output loss (ruled out-ish — stderr file capture shows nothing, but a panic inside a thread whose abort precedes flush cannot be fully excluded; the heuristic evidence argues native), Windows Error Reporting pipeline (ruled out as EXPLANATION — LocalDumps configured correctly yet nothing captured, consistent with deliberate abort/ExitProcess paths).
- Open follow-up: stock servoshell control test (does upstream servo crash identically? — discriminates feather-shell vs engine), then instrumented/debug-engine build for the exception identity. Until characterized, treat ALL heavy-site long-scroll measurement processes as crash-prone: capture stderr always, verify process aliveness before trusting a measurement tail.

## §1t — MSE seek implementation (Linux lane, 2026-11-25)

**Design (harness-validated before engine implementation):** push-mode MSE
pipelines cannot execute gst seeks. Seek = (1) push flush_start/flush_stop
on the servomse ghost src pads — safe from the application thread, resets
demuxers/decoders; (2) recreate every track's appsrc inside servomse,
re-targeting the SAME ghost pad (pad identity stable for decodebin3) —
a fresh appsrc naturally re-emits stream-start/caps/segment in-stream on
the first post-seek push, avoiding the manual serialized-event injection
that wedges the pipeline (§1q finding 4); (3) the page re-appends media
naturally on its 'seeking' handler. Validated in /tmp/mseharness
(seektest2): position advancing continuously post-seek, first try.

**Engine pieces:** servomse seek_reset/reset_track (mse_source.rs);
player.rs MSE seek branch routes to seek_reset (small, inside the
MediaSource match arm only — no overlap with progressive flushing-seek);
HTMLMediaElement spec seek restored, completion queued directly because
data arrival is app-driven; SourceBuffer init caching + buffered clearing
+ init re-feed on first post-seek append; MediaSource.notify_seek fan-out.

**Spec deviations (deliberate):** buffered ranges are CLEARED on seek —
the flush genuinely drops pipelined media and reporting it buffered would
stop players re-feeding; seek completion fires without waiting for data
decode (impossible when data is app-driven).

**Symptom index additions:**
- Symptom: playback does not resume after MSE seek → check: servomse
  seek_reset (owns flush+recreate), SourceBuffer init_cache/needs_init_refeed
  (media-only post-seek appends are UNDECODABLE without re-fed init —
  prime suspect if data flows but no frames), HTMLMediaElement seek wedge
  (ruled out — completion now queued directly, verified seeking flag clears),
  manual in-stream event injection (ruled out — wedges pipeline, §1q).
- Symptom: seeking/seeked events fire twice per seek → cosmetic, observed
  in probe6; suspect: position-driven completion path + explicit queued
  completion both firing. Not user-visible harmful.
- Symptom: post-seek position wrong/stale → check: matroskademux timestamp
  passthrough after flush (verified correct — position lands on target),
  init re-feed prepending (corrupts timestamps if double-init appended;
  the already_has_init check guards this).

**Verification (probe6, live engine):** seek to 5.0 → position 5.35 then
7.88 advancing, seeking+seeked fired, seeking flag false, rs 4, playing.

## §1u — fMP4/H.264 mechanism diagnosis (2026-11-25)

**Verdict: the fMP4/H.264 engine path WORKS** — probe7 live-verified:
isTypeSupported true for avc1/mp4a, fMP4 init+media appends decode and play
(t advancing, rs 4, zero errors). Pure-gst harness agrees.

**Symptom index additions:**
- Symptom: twitch-class sites black player with working page chrome → check:
  SourceBuffer::SetTimestampOffset (accept-and-ignore — hls.js transmuxing
  rewrites ALL timestamps through it; PRIME SUSPECT), live-edge semantics
  (seekable ranges/duration Infinity unimplemented), avdec_h264 absence
  (ruled out — openh264dec covers decode, verified), qtdemux push-mode fMP4
  (ruled out — verified working in harness + engine), isTypeSupported mp4
  (ruled out — returns true correctly).
- Symptom: vimeo "Technical Difficulties" / shaka demo shell broken → NOT
  media machinery; player JS fails before any append. General web-compat.
- Symptom: fMP4 MSE append produces no playback → NOT a real symptom class;
  verified working end-to-end. If seen, suspect timestampOffset confusion
  or missing init re-feed post-seek instead.

## 1u. Silent heap-corruption crash — deep characterization (2026-11-25, Windows lane)

THE BUG: feather.exe dies silently on heavy real sites with NO input needed — reddit 60s idle, amazon 75s idle, bbc/amazon scroll round 1-2. Exit 0xC0000374 STATUS_HEAP_CORRUPTION. With full pageheap (IFEO GlobalFlag=0x02000000, PageHeapFlags=3): deterministic 0xc0000005 in verifier.dll heap validation, then process exit 0xc0000421. Timing-sensitive: survives under procdump attachment (heisenbug).

VERIFIED NEGATIVES (do not re-walk):
- NOT input-driven: crashes with zero input injected, pure idle.
- NOT media: reddit has zero media players, dies at 60s idle.
- NOT gstreamer/gst-lifecycle: crash unchanged with gst absent from PATH (note: gst dlls sit beside the exe, so PATH removal does NOT disable gst — the dlls load regardless; media ruled out by the reddit result instead).
- NOT winhit subclass: knockout build crashes identically.
- NOT jemalloc purge commit: servo-allocator build.rs gates jemalloc_enabled to non-Windows — purge_arenas is an INERT STUB on Windows. All jemalloc theories dead for Windows.
- NOT OOM (600MB on 34GB box), NOT tab-switch injection, NOT scroll-specific.
- NOT engine-shared-by-default: servoshell built from same feather-engine tree SURVIVES amazon scroll x5 AND amazon idle 375s+ (controls verified valid: cursor over window, scroll events confirmed in log, gst dlls present in both runtime dirs). feather dies in the same minutes. THE BUG IS FEATHER-SHELL-SPECIFIC.

CAPTURE TECHNIQUE (reuse it): heap-corruption fastfail bypasses SetUnhandledExceptionFilter/WER/procdump. Working pipeline: AddVectoredExceptionHandler(1) + first-fatal-only capture of CONTEXT registers + raw stack qwords; dll bases boot-constant so post-hoc module mapping works. MiniDumpWriteDump cannot run inside the corrupted process (allocates). Module used: shell/src/crashdump.rs (NOT committed — diagnostic only). Symbolization: dbghelp SymFromAddr via python ctypes + feather.pdb beside the exe copy.

SYMPTOM INDEX additions (per standing rule):
- Symptom: feather process silently vanishes on heavy real sites (idle or scrolling, no panic output, no WER, exit 0xC0000374/0xC0000421) → check: feather-shell's engine-driving loop and GL/texture workload (PRIME SUSPECT area — the only substantive unsafe surface differing from servoshell: per-frame webview.paint() pattern, egui_glow texture uploads, favicon decode cache calling into engine every frame), feature-selection deltas vs servoshell (feather lacks multiprocess/devtools/webgl/webgpu/subtlecrypto/brotli/max_log_level that servoshell enables; feather adds no-wgl — untested as a group), style-traversal parallel churn (crash correlates with 98% style:: recalc storms but shared with servoshell — context not cause), mozjs-internal jemalloc frames appear in fault chain (coarse public-only resolution — UNRELIABLE, do not over-read).
- Resolution limit hit: release+line-tables PDB resolves only sparse publics; mozjs C++ has no debuginfo in release either. Exact corrupting function requires full debug=2 rebuild (hours) or upstream-quality repro.

REPRO HARNESS (exact): session.json single tab amazon/reddit; launch feather-crash.exe copy of feather.exe; wait 60-90s; dies with zero input. Scroll injection accelerates but is unnecessary. servoshell.exe same-site control survives 5x longer.

## Symptom: Atomics.wait throws TypeError ("waiting is not allowed") in workers despite shared memory working
→ check: FutexThread canWait flag in mozjs AtomicsObject (owns the gate — `cx->fx.canWait()` checked in atomics_wait_impl), JS_SetFutexCanWait embedder API in jsapi.cpp (owns opt-in; CONFIRMED CAUSE — servo never called it, so every agent defaulted to no-blocking), dedicatedworkerglobalscope/sharedworkerglobalscope runtime setup (fixed 659eec46d: flag set at worker context creation; spec: only the window main-thread agent may not block).

## Symptom: worker posts pre-wait message then never wakes despite main-thread Atomics.notify
→ check: JS_SetFutexCanWait (status: ruled out for the wake path — with the flag set, probe verified worker blocks ~600ms, notify returns exactly 1 waiter, woken worker observes updated value; sabwait2.html localhost probe), lost-wakeup race in probe design (status: ruled in for the first flaky probe run — notify fired before worker parked; not an engine defect), turnstile live behavior (status: real-SAB claude.ai STILL stalls with correct wait/notify — challenge restarts, Ray ID cycles; residuals: OffscreenCanvas absent (confirmed missing), wasm threads, canvas/webgl fingerprint hardening).

## Symptom: OffscreenCanvas undefined on window and worker despite servo shipping an implementation
→ check: dom_offscreen_canvas_enabled pref in components/config/prefs.rs (CONFIRMED CAUSE — defaults false; the 614-line implementation in components/script/dom/canvas/offscreencanvas.rs is complete enough for 2d use and WPT-exercised upstream; flipping the pref enables constructor/2d/fillRect/transferToImageBitmap on window+worker, verified), getContext('webgl') on OffscreenCanvas (status: broken/throws TypeError even after the pref flip — separate unowned gap), HTMLCanvasElement.transferControlToOffscreen + createImageBitmap (status: ruled out — both present and functional).

## Symptom: claude.ai turnstile still loops/restarts with real SAB + working Atomics.wait/notify + OffscreenCanvas-2d all verified
→ check: WebGL-on-OffscreenCanvas TypeError (prime suspect — unguarded access would kill the payload chain), WebGL fingerprint surface generally, wasm threads (unprobed), behavioral/TLS signals (out of JS-engine scope — do not chase per Google decision), payload opacity (sufficiency unprovable from outside; shim stays the only working path).

## Symptom/probe verdicts: wasm capabilities (turnstile residual sweep)
→ check: WebAssembly basic compile/exec (status: ruled out — present and correct), WebAssembly.Memory({shared:true}) construction (status: ruled out — constructs OK), wasm atomic instructions (status: INCONCLUSIVE — hand-assembled probe module failed with a byte-encoding error in the probe itself, not an engine verdict; needs a properly assembled threads-test module or wat2wasm tooling).

## Symptom: canvas.getContext('webgl') absent/null everywhere in feather
→ check: feather shell Cargo.toml servo feature list (CONFIRMED CAUSE — webgl is in servo's default_web_features but feather's minimal list omitted it, so even on-window WebGL was absent; fixed by adding the feature, feather PR #112), offscreencanvas.rs get_or_init_webgl_context downcast::<Window> (status: confirmed structural gap — offscreen-in-worker returns null even with the feature on; workers have no Window global), WebGL RENDERER/VENDOR strings (status: loud fingerprinting tell — literally report "Mozilla/Servo"; hardening decision unowned).

## Symptom: claude.ai turnstile still loops with FULL real stack (SAB + Atomics.wait/notify + OffscreenCanvas-2d + WebGL 1/2 all verified present)
→ check: worker-offscreen-WebGL null return (prime suspect if payload runs GL work in a worker unguarded), wasm threads atomics (inconclusive — probe encoding was broken), fingerprint surface beyond JS (TLS/HTTP2 — out of scope per Google decision), behavioral signals. NOTE: four necessary APIs fixed and upstreamable regardless; sufficiency unprovable from outside.

## §1v — fmp4 buffered indexing + timestampOffset empirical correction

- Symptom: mp4 MSE appends never populate buffered → scan_cluster_timecodes
  is webm-only (CONFIRMED CAUSE, fixed — tfdt fallback added); player append
  loops that gate on buffered stall even though decode/playback work.
- Symptom: fmp4 live-edge playback broken by missing timestampOffset →
  RULED OUT as a playback blocker: huge tfdt timestamps play fine, position
  just reports container time. timestampOffset remains a timeline-correctness
  feature, not a playback gate. seekable ranges + duration=Infinity for live
  are now the prime live-semantics suspects.

## Symptom index addendum — heap-corruption knockout bisect (Windows, 2026-11-25)

Symptom: silent heap-corruption death (0xC0000374 / pageheap AV) on heavy sites, idle, no input, ~60s on reddit — feather shell only, servoshell control survives everything.
→ ruled out, verified NOT the cause:
- favicon decode path (knockout died same) — `feather shell update_favicon_cache`
- paint ordering / make_current-before-paint (knockout died same) — `shell gui.run paint`
- ALL chrome collectively — minimal-shell build (only webview.paint + composite blit) still dies at 60s — bug is in CORE INTEGRATION LOOP, not chrome. `App::run` / `ServoBuilder` / webview lifecycle layer.
- winhit / hit-test injection, media/gstreamer, jemalloc-purge (inert on Windows), webgl/webgpu feature absence, input handling, OOM, tab-switching
- js-mem prefs (js_mem_max=192 etc.) — servoshell with FULL feather pref set survives 255s+ — prefs exonerated
- user_content_manager attachment — died at 315s with ucm detached (5x delay but still died; treat as noise until replicated — death-time variance unmeasured)
- winit ControlFlow busy-loop vs Wait — Wait build dies at 60s identical — event pacing NOT the trigger
→ prime remaining suspects (core integration loop): ServoBuilder opts/config differences, multi-webview infrastructure, hidpi scale-factor handling, embedder_traits usage differences, the winit ApplicationHandler shell-servo glue itself.
→ bisect status: STALLED on measurement noise — death-time variance uncharacterized, each knockout test costs 6min. Next step recommended: full debug=2 build for a real capture stack, or replicate baseline 3x to calibrate variance before further knockouts.
→ harness facts: repro = work/feather-crash.exe + session.json reddit single-tab idle, dies ~60-75s; example.com survives 255s+ (content-driven); capture module crashdump.rs uncommitted in feather shell tree; full pageheap ACTIVE on feather-crash.exe in registry.

## Symptom: claude.ai turnstile — nondeterministic pass/loop with full real stack + WebGL string spoof
→ check: WebGL RENDERER/VENDOR masked strings (status: spoofed locally to ANGLE-Metal/Google-Inc. values, verified live — NOT PUSHED, experiment only; verdict: insufficient alone, cold-state still loops), cf_clearance cookie hypothesis (status: plausible — the one observed pass occurred with accumulated storage present; a clearance from background-running challenge machinery can't be ruled out, so attribution to the spoof is ambiguous), turnstile internal machinery (status: runs DEEP — service worker + caches + IndexedDB + localstorage all registered under challenges.cloudflare.com origin; the payload executes fully rather than dying on API checks), verdict class (status: nondeterministic — passed once after minutes of runtime, loops other times; server-side risk scoring on aggregate signals means remaining tells are statistical/behavioral + TLS, a different work class than API presence).

## Symptom index addendum 2 — structured-clone crash stack (2026-11-25, pageheap capture symbolized)

Symptom: heap-corruption/AV death, content-driven, feather shell.
→ CONFIRMED CRASH SITE (from symbolized capture, thread dies here): `JS_ReadStructuredClone` deserialization of a MessagePort postMessage payload — stack: `structuredclone::read_with_scope` ← `GlobalScope::route_task_to_port` ← `post_messageport_msg` task; inside: `WriteBytesToJSStructuredCloneData` (read-side reconstruction copy), `JSStructuredCloneReader::read`, `BufferList<SystemAllocPolicy>::IterImpl::Advance/AdvanceAcrossSegments`, `AllocateBytes/AllocateSegment`, `js_arena_malloc`.
→ module attribution: EVERYTHING in feather.exe (mozjs statically linked — no external dll involvement except ntdll/verifier/ucrtbase). The earlier "mystery 0x7ff7b module" was a red herring from pre-module-list guessing.
→ under FULL pageheap out-of-bounds WRITES fault at write time (guard pages per allocation); a READ fault inside deserialization therefore suggests UAF-read or malformed-bytes read rather than classic heap corruption propagating silently.
→ mechanism facts from code review: write side copies bytes OUT of the JS buffer into a Rust Vec (structuredclone.rs write — CopyJSStructuredCloneData + set_len); read side copies bytes back IN (WriteBytesToJSStructuredCloneData) then calls JS_ReadStructuredClone. Byte-copy logic itself looks correct on inspection. SameProcess scope sets allowSharedMemoryObjects=true — serialized form may embed RAW BUFFER REFERENCES rather than copies for shared-memory-backed objects.
→ suspect areas for the UAF/malformation: (a) transferred object backing stores (transferred_image_bitmaps / offscreen_canvases / ports) lifetime across pipeline teardown; (b) feather's webview lifecycle differences — tab sleep/teardown/peek — tearing down pipelines while port messages are in flight (servoshell never does this); (c) victim-only reading — clone buffers are merely the densest memory traffic.
→ CAPTURE TOOLING THAT WORKS: crashdump.rs now captures module list (CreateToolhelp32Snapshot) + registers + 512 stack qwords; attr.py buckets addresses by module; symbolize.py resolves via dbghelp against shell/target/release/feather.exe (pdb adjacency — do NOT point it at the renamed copy). mozjs C++ frames RESOLVE with line-tables-only pdb — full debug=2 build NOT needed for attribution.
→ OPEN CONTROL GAP CLOSED PENDING: servoshell was only control-tested on amazon/webgl — never reddit (the fastest killer). servoshell+reddit idle test in flight — if it dies, this is an UPSTREAM bug misfiled as shell-specific.

## Symptom index addendum 3 — variance collapse + paint-gating fix (2026-11-25)

Symptom: heap death on heavy sites.
→ MEASUREMENT WARNING — death-time variance is 30-420s for identical configs. The minimal+wait+no-ucm 'survival' at 420s did NOT replicate (died 60s on rerun). ALL single-run knockout survivals are noise; dying-results remain valid negatives but survival-results are meaningless without replication. Black-box bisect closed as statistically unworkable at 6min/run.
→ CODE-VERIFIED shell difference (not bisect-dependent): feather composited UNCONDITIONALLY on every winit RedrawRequested; servoshell gates paint on needs_repaint (window.rs repaint logic — only composites when notify_new_frame_ready or UI change marked dirty). Painter::render() composites unconditionally when called and can self-cascade via refresh-driver FrameStart.
→ FIX IN TESTING (feather shell, uncommitted): webview_needs_paint Cell flag — set in notify_new_frame_ready, gates webview.paint() in redraw. Acceptance test: 5x reddit-idle 420s runs (baseline never survived once). Also restores ucm + favicons + keeps ControlFlow::Wait (servoshell parity, battery win).
→ mechanism theory being tested: rAF-active pages (reddit ad/tracker loops) sustain a composite feedback cycle whose rate is shell-determined; rate scales memory churn; churn scales hit probability of the structured-clone race/UAF. Explains: content-dependence, input-independence, loose time-bomb clustering, shell-specificity.
→ if fix passes: race still exists in engine but becomes unreachable in practice — document as mitigated-not-fixed; upstream-quality repro still valuable. If fix fails: gating stays as pure battery win; hunt resumes on webview-lifecycle/delegate behaviors.

## Symptom: feather burns ~84% of one core playing 720p video (Safari: ~4%)
→ check: vtdec hardware decode (status: ruled out — gst-launch decode-only baseline measures ~1% CPU; hardware decode confirmed active), gst-side videoconvert YUV→BGRA software conversion (status: real but minor — decode+convert pipeline totals ~9.6% CPU in isolation, so ~9 points of the 84), per-pixel frame copies/texture upload (status: ruled out as DOMINANT — 320×180-native video at same display size still burns 75.7%; cost does not scale with frame pixels), display-size rasterization of video rect (status: ruled out — 320px-wide display of same video still 84%), COMPOSITOR/REPAINT PATH (CONFIRMED DOMINANT CAUSE — a plain 30fps rAF background-color animation with NO video/decode/upload/gst involvement burns 77.1%; feather pays ~2.5ms+ main-thread CPU per repaint regardless of what changed. Video is just a 30fps invalidator; ALL animated content pays this — CSS animations, rAF, transitions). window-size scaling partial (500×400 window ~50% vs large window ~84% — consistent with viewport-proportional rasterization plus per-frame constant). open suspects within the repaint path: full display-list rebuild per frame vs tile caching failure (webrender), egui shell chrome repainting in lockstep with content, deprecated macOS GL translation layer cost (glrFlushDrawFramebufferToIOSurface/glTexSubImage2D visible in profile — GL-on-macOS is CPU-translated), main-thread serialization of rasterize+composite.

## Symptom index addendum 4 — the measurement contamination discovery (2026-11-25)

Symptom: 60s idle-death on reddit-class pages, feather only.
→ PRIME SUSPECT IDENTIFIED: the repro binary carried TEMP memreport instrumentation (dirty tree only, NEVER in main — verified git show main:shell/src/app.rs has zero refs) — automatic create_memory_report at elapsed 50s+110s in the window-event handler. Deaths at ~60s = 50s mark + measurement duration. Survival outliers (315/420s) = probabilistic measurement crash; pass both marks and nothing remains to kill the process.
→ CRASH SIGNATURES (7 captures, all contaminated builds): signature A = structured-clone postMessage deserialization; signature B = INSIDE memory measurement itself (get_reports/compute_size/malloc_size_of_including_raw_self<MessagePort>/Document::collect_reports/perform_memory_report). Both involve MessagePort memory — consistent with one event: concurrent memory measurement of cross-pipeline shared structures (MessagePort impls are shared across globals/pipelines) racing live clone traffic.
→ ENGINE LATENT RACE (real, documented, upstream-relevant): memory measurement of shared-across-pipelines structures is not synchronized — servo never measures in production so upstream never hits it; any embedder triggering create_memory_report on a multi-pipeline workload may crash. THIS IS A REAL FIND regardless of the contamination outcome.
→ PROCESS LESSON (mandatory reading): black-box bisection against an instrumented binary is void — the repro baseline MUST be a known-clean build before any knockout work. Hours of knockout results (favicon/ucm/controlflow/minimal-shell/paint-gating) are ALL contaminated and must be re-evaluated; treat them as void, not as exonerations.
→ paint-gating fix: kept regardless of verdict — correct servoshell semantics + battery win — but it is NOT validated as a crash fix.
→ verdict test in flight: no-memreport build 5x reddit-idle acceptance.

## Symptom index addendum 5 — repaint-cost profiler attribution (2026-11-25)

Symptom: feather pays ~7-8ms+ CPU per composite frame; animated content capped below display rate.
→ check: webrender DisplayList build (status: RULED OUT — profiler avg 0.01ms/frame), webrender scene building / tile cache (status: RULED OUT — profiler avg 0.02ms/frame; tile caching healthy), webrender frame/render phase (status: CONFIRMED DOMINANT INTERNAL TERM — profiler "Frame CPU total" avg 7.76ms/frame at large viewport, excludes feather-side blit+egui+present so true cost higher), fixed driver overhead (status: RULED OUT — cost is pixel-proportional: same anim runs 61fps full-rate in a 500x400 window vs 31fps large), full-viewport texture upload per frame (status: RULED OUT by sample attribution — glTexSubImage2D ~0 samples in redraw; blit_framebuffer 42/5336 ~1%, IOSurface flush 71/5336 ~1%), Apple GL translation layer draw processing (status: CONFIRMED DOMINANT CAUSE — glDrawElements/gleDrawArraysOrElements_ExecCore/gldRenderFillPolygonPtr = 4139/5336 ~78% of redraw samples: CPU-side vertex processing + software polygon fill in the deprecated GL-to-Metal shim; pixel-proportional scaling explained by triangle fill area). THE fix lever is getting off legacy GL on macOS (ANGLE-Metal); embedder-copy elimination is worth only ~1-2%.
→ partial-present viability: webrender supports buffer_age partial present but servo Painter hardcodes buffer_age 0 AND surfman 0.14 exposes NO damage-aware present API at all (present_surface/swap_buffers have no damage parameters; CGL backend has no damage plumbing) — damage-aware fix requires new surfman API + platform work, not a config flip. Fix levers ranked: (a) eliminate embedder-side full-window copies (blit+egui texture upload), (b) ANGLE/Metal to remove GL translation tax, (c) damage plumbing through surfman — largest integration surface.

## Symptom index addendum 6 — ANGLE-on-macOS integration cost reality (2026-11-25)
→ surfman 0.14 angle backend: windows-cfg'd at module level, D3D11-flavored implementation — ungating for macOS requires backend adaptation, not a config flip.
→ mozangle 0.7.1 build data contains ZERO Metal-backend sources (macos branch exists for the shader translator only) — ANGLE-Metal on macOS requires extending mozangle's vendored chromium build (week-scale, not days). The repaint-cost fix investment should be sized accordingly.

## Symptom index addendum 7 — sw-rasterize (SW-WR) fallback scoping (2026-11-25)
→ webrender 0.70 SHIPS the swgl-based software rasterizer behind feature `sw_compositor` (`swgl` dep) — Firefox's SW-WR. The renderer duck-detects swgl contexts via `swgl_*` entry points in the GL implementation — no separate render mode, you hand it a software GL context.
→ INTEGRATION SURFACE for servo: (a) gleam 0.15.1 has NO swgl bridge — a Gl-trait-to-swgl forwarding shim must be written or extracted from Firefox's webrender_bindings glue (gleam's Gl trait is large; swgl implements the subset webrender uses); (b) surfman bypass — swgl contexts are pure-memory, no windowing/ANGLE/surfman needed for rasterization; (c) present path — final pixels land in system memory and must reach the screen; feather's present chain is itself GL (egui/winit window), but present cost measured only ~1-3% so a single full-window upload+draw through legacy GL is acceptable — circularity is NOT fatal.
→ KEY UNKNOWN: whether multithreaded SIMD swgl rasterization is actually cheaper than Apple's GL-shim draw processing for servo workloads. Firefox public data: SW-WR competitive with GPU-WR on many pages, sometimes faster on weak GPUs — but apple-silicon CPU is fast, so plausibly a large net win vs 78%-CPU-draw-submission status quo. UNKNOWN until measured.
→ CHEAPEST VALIDATION EXPERIMENT (design): stand-alone harness linking swgl + gleam shim rendering representative display lists (solid-color fullscreen rects, text runs, rounded corners) comparing CPU-cost-per-frame vs the measured 7.76ms+ webrender-through-Apple-GL baseline. Est. 1-2 days just to validate, before any servo integration.
→ ESTIMATE if validated: gleam-swgl bridge + painter rewiring + feather present path = days-to-week, comparable uncertainty to ANGLE but all-Rust (no chromium build surgery). Does NOT benefit webgl/canvas GL content — only compositing/rasterization.

## §1w — timestampOffset + live duration (2026-11-25)

- timestampOffset applied via in-place tfdt rewriting at append time
  (sourcebuffer appendBuffer → rewrite_tfdt_offsets). WebM offsets
  unimplemented — fmp4-only need so far. Negative shifts saturate at 0.
- ms.duration=Infinity propagation fixed (was falling through to derived
  duration — is_finite() rejected Infinity; live streams need it).
- Symptom: segments land at wrong timeline positions / overlapping appends
  after transmux → check rewrite_tfdt_offsets (owns shifting), the moof-
  presence guard in appendBuffer (must fire on concatenated init+media too),
  timescale parsing (mdhd — wrong timescale = wrong shift magnitude).
- Symptom: live stream ends prematurely / wrong duration → check
  duration_value Infinity propagation (mediasource.rs), seekable mirroring
  buffered (already correct for MSE).
- TWITCH VERIFIED PLAYING after the tfdt indexing fix (§1v): live video,
  chat, full UI. hls.js append loop was starving on empty buffered.
## Symptom index — FINAL VERDICT: heap/clone crash culprit confirmed (2026-11-25)

Symptom: silent death on reddit/amazon-class pages, idle, ~60s, feather shell.
→ CONFIRMED CAUSE: fork commit 5f26d9f7abf 'structured-clone shared memory — SameProcess scope for in-process messaging'. Evidence: pre-SAB engine (4-file revert — structuredclone.rs + idbcursor/idbobjectstore/idbrequest.rs) + feather shell survives 3/3x420s idle reddit vs 4/5 deaths at 60s with the commit (P(chance)≈0.008).
→ MECHANISM: SameProcess scope lets SM embed raw memory references in the serialized clone stream. Servo's port plumbing copies serialized bytes OUT to a Rust Vec (write: CopyJSStructuredCloneData+set_len in structuredclone.rs), ships the Vec across threads/pipelines/port-transfer queues with arbitrary delay, then reconstitutes a fresh clone buffer on the receiver thread (read_with_scope: WriteBytesToJSStructuredCloneData then JS_ReadStructuredClone). The byte-copy transposition is outside SM's ownership/lifetime machinery — embedded references dangle → UAF-read. Pageheap-full makes it a deterministic AV in BufferList iteration/allocation during deserialization.
→ WHY DEATHS CLUSTER ~60s: ad-tech iframes spin up post-load; heavy postMessage/port-transfer traffic begins ~50-60s in.
→ WHY servoshell survives: UNRESOLVED — same engine, same commit — shell interaction affects exposure (pump timing/lifecycle). The engine bug is real regardless; do NOT re-investigate shell differences unless crash reappears post-revert.
→ FIX DIRECTIONS: (a) revert commit — stability now, loses SAB cloning; (b) correct fix — clone data for in-process traffic must stay inside SM ownership (carry the clone buffer, not byte copies) — nontrivial design, belongs to SAB lane.
→ CAUTION for future lanes: yesterday's persite disappearance predates this commit — IF a second rarer bug exists it will show post-revert; do not assume.
→ symptom index negative results (all verified): chrome/favicon/ucm/controlflow/paint-gating/prefs/media/winhit/input/memreport-instrumentation/OOM/webgl-features — NOT causes. measurement-race in memreport (capture signatures B) is a REAL but separate latent issue: concurrent memory measurement of cross-pipeline MessagePort structures can crash — only triggered when create_memory_report is invoked, which production never does.

## content-visibility implementation — Stage 0/1 state + design notes (2026-11-25)

STATUS:
- stylo parsing SOLVED locally: patched copy at C:/Users/Administrator/repos/stylo-local (copy of pinned rev 0ab78ce + patches). Changes: (1) longhands.toml — removed engine="gecko" from [content-visibility], [contain-intrinsic-width/height/block-size/inline-size]; added servo_restyle_damage="rebuild_box" + animation_type="discrete" to content-visibility; (2) values/specified/box.rs — added ComputeSquaredDistance + ToAnimatedZero derives to ContentVisibility enum; (3) stylo_static_prefs/preferences.toml — layout.css.contain.enabled=true. stylo COMPILES + servo workspace builds against it via LOCAL-ONLY [patch] section in servo Cargo.toml (uncommitted — replace with fork repin when baigojahanzaib/stylo exists — BLOCKED ON REPO CREATION, escalated to coordinator).
- `contain` needs no stylo patch (servo_pref already exists, just defaulted false).
- upstream stylo latest verified: content-visibility STILL gecko-only — fork is the only path, ever.

IMPLEMENTATION DESIGN for Stage-1 (relevance + paint-skip), for whoever continues:
- Spec hook: document.rs update-the-rendering step 16.2.3-4 (marked TODO) — BUT NOTE the early-return guard: the whole block currently returns early when resize_observers is empty — the cv-determination loop must run regardless of resize observers, so the guard needs restructuring (move cv determination before/independent of the resize-observer early return).
- Relevance state: store per-element (Element has interior-mutability slots pattern — follow how other per-element rendering state is stored); document-side pass iterates elements with content-visibility:auto used-value, computes viewport intersection (use the same intersection machinery IntersectionObserver uses — components/script/dom/intersectionobserver.rs — it already solves root-margin intersection against the scrolling viewport) with a UA margin (start with 50% viewport each side, spec-free choice).
- Paint-skip integration point: components/layout/display_list/ — fragment-level: skipped subtree = element's box still produces its own fragment/background/borders but descendants produce NO display items. Cleanest hook: wherever fragments recurse into children during display-list building (paint_traversal / stacking context tree construction) — check a flag threaded from the element's relevance state through box/fragment construction. Because Stage-1 still lays out skipped subtrees, fragment geometry is correct; only display items are elided.
- Invalidation: relevance transitions must dirty layout paint-only (not rebuild boxes) — use restyle damage repaint-level invalidation; the determination pass runs inside update-the-rendering which already precedes paint.
- KEY UNEXPLORED RISK: servo's display-list construction may not have a clean per-subtree elision point if items are emitted during a single pass tied to fragment tree walk — evaluate paint_traversal.rs first.
- contain-intrinsic-size + size containment needed only for Stage-2 (eviction); Stage-1 correct without them because layout still runs.
- threading: determination runs on script thread; layout reads element state during reflow on the same thread — no cross-thread concern (layout is same-thread in servo).

## content-visibility Stage 1 — landed on cv-wip branch (2026-11-25)

Verified mechanisms (function-indexed, for future lanes):

- **cargo `[patch]` only applies at the workspace ROOT of the build being run.** Shell builds
  use feather/shell/Cargo.toml as root — the patch section in servo/Cargo.toml is INERT there.
  Both need the section while the local-path infrastructure exists.
- **Partial patching of the stylo workspace splits resolution into crates.io duplicates**
  (servo_arc 0.6.0 crates.io vs git both in tree, selectors losing its git source) which breaks
  feature unification (servo_arc/serde lost) — symptom: `Arc<Url>: serde::Deserialize not satisfied`
  in servo-url. ALL NINE stylo crates must be patched together: style, stylo_atoms, stylo_dom,
  stylo_malloc_size_of, stylo_static_prefs, stylo_traits, selectors, servo_arc, to_shmem.
- **`cargo check -p servo-script` standalone does not work on Windows** — builds mozjs with jit
  disabled which needs python3 in the configure shell; only full shell builds use the correct
  feature set. Validate engine changes via the shell build, never standalone cargo check.
- **Paint elision for a subtree requires four sites in `paint_traversal.rs`** because fragments
  are reached through multiple paths: stacking-context entry, `traverse_stacking_container`,
  the Box arm of `traverse_block_level_descendants_decorations`, and `traverse_box_in_a_line_box`.
  Eliding fewer leaves reachable paths that still paint.
- **A change in the skip set must force `need_new_display_list`** in `LayoutImpl::handle_reflow`
  before the skip check — scrolling alone dirties nothing, so without this the display list never
  rebuilds when relevance changes.
- **Hit-test traversal deliberately does NOT skip** (`PaintTraversal::traverse` without the set) —
  spec: skipped content remains hit-testable/findable since layout still runs.
- Relevance determination runs in `run_the_resize_observer_steps` — the early-return for empty
  resize observers had to be restructured so determination runs regardless.

Stage-1 known gaps (deliberate, per scoping doc): no focus/selection relevance term, no
contain-intrinsic-size consumption (skipped elements lay out at natural size), no memory win
yet (layout runs — Stage 2 is eviction), dynamic cv toggling and print path untested.

## Symptom index addendum 8 — SW-WR validation verdict + integration facts (2026-11-25)
→ VERDICT: webrender-over-swgl composites a representative 1440x900 animated frame at ~1.05ms aggregate CPU vs 7.76ms+ through Apple GL translation — ~7x win, multithreaded (sustained 690fps harness vs 31fps feather anim). SW-WR is the validated direction for macOS repaint cost; ANGLE shrinks to webgl-only scope. Harness: /tmp/swwr-val (standalone cargo project, webrender 0.70 + feature sw_compositor).
→ INTEGRATION FACTS (verified): swgl::Context implements gleam's Gl trait DIRECTLY — no shim crate needed, contrary to earlier scoping. Detection automatic via RENDERER string 'Software WebRender'. GOTCHA: WebRenderOptions::clear_caches_with_quads defaults TRUE and hits an unimplemented swgl path (DepthFunc(GL_ALWAYS) asserts in gl.cc) — must be set false for SW-WR. swgl needs explicit framebuffer backing: Context::create + make_current + init_default_framebuffer(x,y,w,h,stride,buf); readback via get_color_buffer(fbo, flush).
→ STATUS of alternatives: ANGLE-on-Metal confirmed NOT viable via mozangle (vendored ANGLE has only d3d+gl backends, no metal/vulkan; non-windows egl builds panic) — standalone upstream ANGLE build would be needed, week+ scale, chromium-build risk. mozangle gl-backend on macOS pointless (same deprecated GL).

## SW-WR painter integration plan (2026-11-25, pre-implementation)

ARCHITECTURE: implement a new `SoftwareRenderingContext` in components/shared/paint/rendering_context.rs implementing the existing `RenderingContext` trait — NOT painter surgery. The painter already treats the rendering context as an abstraction (`gleam_gl_api()`, `make_current`, `read_to_image`, `size/resize/present`), so an swgl-backed implementation slots in without touching webrender wiring at all.
- backing: swgl::Context::create + one memory framebuffer sized to the viewport, reallocated on resize; make_current = ctx.make_current; gleam_gl_api = Rc::new(ctx) (swgl implements Gl directly — verified in validation harness); present = no-op (nothing to swap — the pixels are already in memory); read_to_image = memcpy out of the framebuffer (replaces the gl.read_pixels+flip path, likely FASTER than today).
- WebRenderOptions REQUIRED delta: clear_caches_with_quads: false (swgl panics otherwise — verified), upload_method: Immediate (auto-selected by renderer-string detection, verify).
- embedder present path: feather uses OffscreenRenderingContext + render_to_parent blit into egui today. With swgl the final frame is memory — replace the blit with a direct memory→egui-texture upload in the shell (small shell patch, measured present cost ~1-3% headroom). Alternatively keep the existing callback shape by making the software context expose the framebuffer through the same callback interface — decide at implementation, prefer whichever touches fewer shell layers.

FEATURE GATING: servo pref `paint_software_rendering` (or env var for v1) — selection happens where feather constructs the rendering context (shell/app.rs WindowRenderingContext::new + offscreen_context). macOS-first; other platforms untouched.

KNOWN INTEROP GAPS (v1 accepted, documented):
- WebGL canvases: compositing expects surfman texture-sharing (create_texture). SoftwareRenderingContext returns None there — webgl content renders black/missing under SW-WR v1. ACCEPTED for the milestone because (a) most compat-critical pages don't need webgl to render, (b) ANGLE-webgl is queued separately, (c) the eventual bridge is webgl-readback-to-memory — follow-up work. HONEST LIMITATION: this is a rendering regression for webgl pages vs today — the feature must ship behind a pref/default-off until the bridge exists, OR accept webgl broken-by-default on macOS in exchange for the universal CPU win — user decision at PR time.
- Video: gst frames are CPU-side texture uploads — should just work (upload = memcpy in swgl). Verify with test720.mp4.
- glow_gl_api(): trait requires a glow::Context — check what consumes it; may need a panic-or-stub decision since glow expects real GL.

VERIFICATION: (1) repaint-cost suite re-run — anim.html CPU must drop from ~77% toward single digits; (2) pixel-diff vs GL-rendered reference pages (screenshot comparison on a few stable test pages); (3) video playback still works; (4) crash-gate: reddit idle repro (same standard as always).

ESTIMATE: core context + painter glue 1-2d, shell present adaptation half day, verification half day. THEN SAB-clone implementation.

## 1x. Timeout-"regression" bisect verdict: NO regression — crawl artifacts of the pre-wedge-fix binary (2026-11-25)

**Question**: the tranco500 crawl showed consistent timeouts (nvidia, baidu, adobe.io, ubuntu.com, theguardian.com, googleadservices, adtrafficquality) that stock servo supposedly handled — suspected regression from arena purge (7d02b70553b), download spool (92080fd7), or hidden-doc GC (826f3a5).

**Method**: fresh servoshell per site, retest of all 9 crawl-timeouts + 3 load_errors + slack.com, stock (`/tmp/servo-main` release) vs feather-engine HEAD post-wedge-fix.

**Verdict — bisect moot, culprit already fixed**: feather-head == stock or faster on ALL 13 sites (ubuntu.com 2135ms vs stock 18232ms). The crawl timeouts were 100% artifacts of running the crawl on a binary built BEFORE the wedge fix (bb5c9bc9b94): rows recorded in a chunk after an unfinished navigation had wedged the webview. `--stop-on-timeout` restarts mitigated but the wedge can also pin without producing a timeout row first. ALL THREE bisect suspects (arena purge, download spool, hidden-doc GC) are **ruled out without a single bisect build** — post-fix feather matches stock everywhere.

Residual genuine fails, identical in stock → upstream class, not feather bugs: elasticbeanstalk.com, gcdn.co, mybluehost.me, nflxvideo.net (all `load_error` in both). aol.com fails in both — stock hangs to 30s timeout, feather fails fast with nav_error at ~9s (feather behavior arguably better, still a fail). slack.com loads fine in both post-fix (was "consistent timeout" pre-fix — also wedge artifact).

**Symptom index — Symptom: consistent page-load timeouts on sites stock handles → check**: unfinished-navigation wedge in `constellation::load_url`/`webdriver_server` (confirmed cause historically — fixed bb5c9bc9b94; verify the crawling binary POSTDATES the fix before bisecting anything), arena purge at pipeline teardown (status: ruled out — post-fix parity with stock on all repro sites), download-spool servoparser changes (status: ruled out — same evidence), hidden-doc GC (status: ruled out — same evidence; also crawl documents never go hidden). Lesson for the index: **crawl data is only as good as the binary date** — wedge-family bugs poison every row after the triggering navigation in the same engine process.

## 1y. Tranco-500 final clean dataset + roblox Linux crash confirmation (2026-11-25)

**Cross-lane confirmation**: the SameProcess-clone revert (bundled in verdict commit 8024ffda808) fixes the Linux crash too — pre-revert servoshell died hard on roblox.com 100% (whole process gone, webdriver port connection-refused); post-revert roblox navigates cleanly in 2.7s. Contradicts the Windows-lane "servoshell survives" data point — the UAF is engine-level and can kill servoshell, not just feather shell; Windows servoshell survival was timing luck. NOTE: HEAD no longer clones SharedArrayBuffers — turnstile-class SAB work is reverted-blocked until the ownership-preserving replacement lands.

**Final tranco500-linux dataset** (`crawler/runs/tranco500-linux-final.jsonl`, every row produced or re-verified on post-revert binary with per-error fresh-engine isolation): 500/500 coverage — 337 ok, 123 dns_no_address, 22 tcp_unreachable, 12 cert_error, 4 load_error, 2 timeout. Real-site compat (excluding infra-dead hosts): **337/343 = 98.3%**. The entire defect tail is 6 sites and ALL reproduce identically in stock upstream servo (aol.com + wa.me consistent-timeout, elasticbeanstalk/gcdn/mybluehost/nflxvideo load_error — upstream net-class residuals per §1i). **Zero feather-specific defects in the tranco 500.**

**Crawler methodology lessons (symptom-index additions):**
- Symptom: crawl row says nav_error/timeout on a major site that obviously works (google.com, wikipedia.org) → check: whether an EARLIER site in the same engine process crashed/wedged it (post-crash rows are fallout garbage); rows are only trustworthy pre-first-error in their chunk OR produced with fresh-engine isolation. Correct attribution ordering: the FIRST error row in a contiguous error run is the suspect, everything after is fallout.
- Symptom: servoshell process vanishes mid-crawl (webdriver port goes connection-refused, rows flip to instant-error at 0ms) → check: structuredclone.rs SameProcess scope (confirmed cause historically — reverted; the UAF mechanism is documented in §1u/line 1276), any future re-implementation of shared-memory cloning that copies serialized bytes out of SM ownership.
- crawl.py now exits for restart on nav_error/driver_error too, not just timeout (`--stop-on-timeout` semantics widened); xrun.sh implements the 3-strikes consistent-failure attribution loop.
- Timeout-bisect verdict from §1x stands: arena purge (7d02b70553b), download spool (92080fd7), hidden-doc GC (826f3a5) all ruled out — post-fix parity with stock on every repro site.

## SW-WR integration outcome + symptom index (2026-11-25, macOS lane)

### Verdict: negative result for repaint cost — measurement-driven
swgl integration is COMPLETE and pixel-correct behind FEATHER_SWWR=1 (default off), but A/B
measurement on the same binary shows NO significant win: anim 52-62% both backends, video
GL 74% / SW 70% (~marginal), static idle 0.2% both. The harness validated rasterization in
isolation but rasterization is NOT the binding cost of a repaint in situ.

### Symptom index entries
- Symptom: black page area with swgl active, webrender initialized fine, high CPU → check:
  shell present path end-to-end (owns getting pixels to screen — confirmed cause: readback
  existed in engine but nothing in the shell ever displayed it; a backend swap is not done
  until pixels reach the window), swgl context threading (status: ruled out — make_current
  in readback correct but was never the issue), webrender composite target (ruled out).
- Symptom: red/blue channels swapped under swgl, green/yellow fine → check: byte order of
  swgl memory (confirmed cause: BGRA memory layout, GL native; RgbaImage contract is RGBA —
  swizzle in readback), row flip (ruled out — orientation was correct).
- Symptom: sustained ~55% CPU on static-looking page during tests → check: modal system
  dialogs stealing osascript keystrokes (confirmed cause once: devtools-auth dialog up meant
  navigation never happened — the ANIM page was still running; measurement contamination),
  paint-gating (ruled out — gated idle is 0.2% under both backends).
- Symptom: repaint CPU cost unchanged after composite backend swap → check: egui/shell
  present path (prime suspect — the shell's own egui render passes through Apple GL every
  repaint regardless of webrender backend), upstream pipeline (prime suspect for rAF
  content: JS/style/layout/display-list/scene-building per frame), webrender rasterization
  backend (ruled out for total cost — A/B shows it is a minority share), texture upload /
  readback copies (ruled out — NOREAD experiment: identical CPU with zero readback).
- Environmental: `sample`/`spindump` on macOS need the Developer-Tools-Access auth dialog
  approved in the GUI (password devin) or they hang silently forever — and the modal dialog
  intercepts osascript keystrokes while up, contaminating any concurrent test navigation.

### Experiment infrastructure (reusable)
FEATHER_SWWR_NOREAD=1 (paint but skip readback+upload) and FEATHER_SWWR_NOUPLOAD=1 (readback
but no texture update) env switches in shell/src/app.rs — binary-search cost attribution of
the present path in a single build.

## Repaint frame attribution — the complete decomposition (2026-11-25, FEATHER_TIMING)

Measured with env-gated wall-time instrumentation in the shell redraw (FEATHER_TIMING=1),
anim.html 30fps, ms/frame on main thread:
- GL mode:   paint=6.9  guipaint=10.6  present=11.4  frame-total=29
- SW-WR mode: paint=0.3 readback=0.3  guipaint=16.4  present=10.2  frame-total=28
CPU-math check: ~28ms x ~19fps-effective ~= 560ms/s ~= measured 56-62% CPU — wall-times are
real CPU, not vsync blocking. Anim achieves only ~19 of 30 target fps — main thread saturated.

### Symptom index entries
- Symptom: repaint CPU high regardless of webrender backend → check: shell present path
  (CONFIRMED binding cost: egui tessellation+GL draw of chrome/content = 10-16ms plus
  surfman present() = 10-11ms of every ~28ms frame — 75-95% of repaint cost lives here,
  NOT in the engine), webrender rasterization (ruled out as binding: swgl takes it to
  0.3ms with zero total improvement because cost relocates to texture upload),
  upstream pipeline JS/layout/DL (ruled out for simple anim: 97.6% of samples on main
  thread; pipeline threads negligible).
- Symptom: present() costing 10-11ms per frame → check: surfman/NSOpenGL CGLFlushDrawable
  path (prime suspect — constant across all modes; the macOS GL window-system tax),
  vsync blocking (ruled out as explanation: CPU math closes without it).
- Symptom: texture upload cost appearing in guipaint under swgl → mechanism: egui texture
  upload of the full-frame content image through Apple GL each repaint (~6ms at retina);
  this is where raster cost RELOCATES if rasterization moves to CPU — any CPU-raster design
  must count upload as part of its cost.
- Experiment machinery now in tree: FEATHER_TIMING=1 (per-phase ms/frame log),
  FEATHER_NOPAINT / FEATHER_SWWR_NOREAD / FEATHER_SWWR_NOUPLOAD (isolation switches —
  caveat: NOPAINT removes the pipeline throttle so frames run unthrottled; do not interpret
  its absolute CPU as upstream cost).

## 1z. Timeout-wedge "regression" verdict: NOT live on Linux HEAD — full survival chain verified (2026-11-25)
**Claim tested**: "any single timed-out page load permanently kills the browser session" — ordered bisected among arena purge / download spool / hidden-doc GC.
**Verdict: no repro on Linux, nothing to bisect.** Full chain on HEAD (post-SAB-revert):
- Webdriver: aol.com times out with proper error at exactly 30s → example.com nav succeeds → current-url + title both confirm real load. Session fully healthy post-timeout.
- Real feather shell: aol.com loads + streams forever (never-finishing load class) → omnibox nav to example.com succeeds, window retitles. No wedge.
- The three bisect suspects are exonerated for the wedge itself, not just crawl artifacts — they all PREDATE the original passing wedge-fix verification, and survival still works with them in-tree today.

**Traps that produce false wedge positives (symptom-index entries):**
- Symptom: webdriver navigation NEVER returns (no response even at 95s+) → check: whether the client set an explicit pageLoad timeout — servo DOES implement the spec's 300s default (TimeoutsConfiguration::default uses DEFAULT_PAGE_LOAD_TIMEOUT=300_000, present since the upstream merge; CORRECTED in §1za — earlier revision of this entry wrongly claimed unbounded). Without `/timeouts` the nav command still hangs for up to FIVE MINUTES before erroring, and every later command queues behind it — to anyone watching at 60-120s it looks exactly like the permanent wedge. Harnesses must always POST /timeouts explicitly. The TRUE historical permanent wedge was unbounded IPC reply waits parking the dispatcher thread (even /status died) — fixed in 7d150c23be2 (bounded at script timeout + 30s margin); any binary predating that commit shows the real wedge.
- Symptom: typing into feather's omnibox does nothing in Xvfb/xdotool testing → check: Xvfb has no WM, so winit never gets focus — must `xdotool windowfocus <id>` explicitly; ctrl+a/click selection feedback can misleadingly work while keystrokes go nowhere.
- Symptom: post-timeout crawl rows garbage → see §1y (crash-fallout ordering; first error row is the suspect, the rest are fallout).

**Platform caveat**: this verdict is Linux-only. If the wedge reproduces on Windows/macOS, it is platform-specific — the Linux exoneration does not transfer, but the repro sequence above (30s timeout + post-timeout nav + current-url verification) is the exact test to run there.

## Metal present path (stage 1 landed, feather PR #120)

Symptom: entire wgpu/Metal window contents vertically flipped on macOS → check: surfman WindowRenderingContext layer attachment (CONFIRMED cause — NSOpenGL attachment installs a bottom-up-coordinate layer on the window; any CAMetalLayer composited inside inherits the flip; fix: create NO GL window machinery in metal mode), egui.wgsl vertex transform (ruled out — math correct by inspection), machine/wgpu-level presentation (ruled out — bare-wgpu fullscreen triangle rendered correct orientation).

Symptom: wgpu frame wall-time high (~14ms) but CPU low → check: surface.get_current_texture blocking (CONFIRMED — 13.7ms of the 14.3ms guipaint wall is vsync FIFO backpressure, CPU-free; the frame lands exactly on one 60Hz vsync period meaning the pipeline is vsync-paced not CPU-bound; real GPU present work ~1ms/frame: texupload=0.28 tess=0.03 updbufs=0.11 renderpass=0.1 submit+present=0.46). Do NOT read wall-time frame totals as cost in a vsync-throttled pipeline — use CPU%.

Measured outcome (anim.html benchmark): GL path shell present 21-27ms/frame CPU → Metal ~1ms/frame; whole-process CPU 77-97% → ~24%. Passes the <=4ms success gate of docs/metal-present-design.md. egui-wgpu 0.34.3 + wgpu 29 is a viable present stack for egui 0.34.3 shells on macOS arm64.

## SAB structured-clone correct implementation (landed 2f1ca404c)

Symptom: postMessage of SharedArrayBuffer throws DataCloneError → check: structuredclone::write CloneDataPolicy + scope (FIXED — SameProcess scope with permissive policy for in-process messaging paths; realm gate stage-1 was necessary but not sufficient).

Symptom: ~60s idle-death crash on postMessage-heavy pages after naive SameProcess flip → check: byte-copy transposition of SameProcess clone streams (CONFIRMED cause of 5f26d9f7a crash — CopyJSStructuredCloneData copies only the byte segment; embedded backing-store references dangle when the SM buffer drops; correct fix is carrying the SM-owned buffer end-to-end, implemented via process-global registry keyed by u64 since StructuredSerializedData is serde-transported even in-process).

Invariants future lanes must not break:
- SameProcess streams never leave SM ownership as bytes. The registry key is single-consumer: any path that deserializes the SAME StructuredSerializedData more than once (performance entries per getEntries, history state per navigation) MUST use DifferentProcess — multi-read of a carried buffer panics on second read by design.
- BroadcastChannel fanout uses DifferentProcess: SAB-over-broadcast throws DataCloneError (interim deviation; proper fix is per-recipient serialization).
- clone_for_broadcast copying sm_buffer_key would double the ownership — the warn guard there must stay.

## §1za. Correction: webdriver default page-load timeout exists (300s, spec-compliant) — and the final wedge-bisect verdict (2026-11-25)

The §1z trap entry misread the evidence: servo webdriver has ALWAYS (since the upstream merge b4a6153e26d) defaulted `page_load` to Some(300_000) per the spec — verified in components/webdriver_server/timeout.rs both at the merge commit and at HEAD. The observed 95s hang was spec-compliant behavior (would have errored at 300s), not a missing default. There is nothing to implement.

Final verdict on the timeout-regression bisect order (3x repeated): the wedge does not exist on Linux feather-engine HEAD. Matrix test on a51dbc57445, one servoshell process: THREE timeout events (aol x2, wa.me), each erroring at exactly 30.0s with explicit client timeouts, each followed by verified successful navigation (url+title checked): example.com 0.1s, ubuntu.com 2.4s, example.org 0.1s, example.com 0.0s, example.com 0.2s. Harness /tmp/wedge-matrix.py, log /tmp/wedge-matrix-out.txt. Arena purge, download spool, hidden-doc GC all in-tree throughout — none is a culprit; no bisect is possible against a non-reproducing symptom.

Symptom index additions:
- Symptom: "one timed-out page load permanently kills the session" reported by any harness → check IN ORDER: (1) was the harness binary built before 7d150c23be2 (bounded IPC waits, the REAL wedge fix — pre-fix binaries genuinely wedge, dispatcher thread parks forever)? (2) does the harness POST explicit /timeouts — if not, the nav command sits for the full 300s spec default looking dead? (3) is the "wedge" actually process death — roblox-class crash (pre-SameProcess-revert binary)? (4) ONLY THEN suspect engine regressions. On Linux HEAD all four checked: negative, negative, negative, negative.

## Metal stage-2 evaluation (verdict doc feather PR #122)

Symptom: getContext('webgl'/'webgl2') returns null everywhere under FEATHER_METAL or FEATHER_SWWR → check: WebGLThread::create_webgl_context painter_surfman_details_map (CONFIRMED cause — webgl piggybacks on the painter's surfman connection; swgl mode has no surfman connection, nothing to piggyback on; NOT caused by Metal itself — identical failure under FEATHER_SWWR alone), wgpu/Metal surface interference (ruled out — swgl-only repro).

Measurement rule: input latency in a vsync-paced pipeline is NOT measurable as present-path wall time — both GL and Metal block CPU-free on vsync. The honest metric is event-loop saturation: achieved rAF rate. GL achieves 23.1/30fps (frame wall ~29ms ≈ 2 vsyncs, misses deadlines); Metal achieves full 30.0/30 (frame wall exactly 1 vsync). get_current_texture blocking does NOT add input latency vs GL — it halves worst-case latency.

Bridge options indexed for the future implementer: (1) independent surfman connection + readback-composite for webgl textures, 3-5d; (2) swgl-backed webgl contexts — swgl IS a full GL implementation, potentially cheaper, CPU-bound. Evaluate (2) first.

## Unresolved observations

Observation: 2026-11-25, one GL-mode (default, non-metal) feather launch died silently mid-run on fpsprobe.html — no panic in log (18 lines, ends at favicon 404), process simply gone. Not reproducible on immediate retry with identical input. Suspect class: segfault in GL path (rust panics would print). If this recurs anywhere it becomes a priority — watch for it.

## WebGL bridge for software compositing (landed 015512402)

Symptom: getContext('webgl') null under swgl/metal → FIXED: independent hardware-GL surfman connection for the WebGL thread + readback composite (glReadPixels BGRA row-flipped at swap time → shared map → ExternalImageSource::RawData). Verified: runtime shader compile+link+draw correct under FEATHER_METAL, animated compositing correct orientation.

Mechanism notes for future lanes:
- swgl CANNOT back webgl: shader_source panics on any shader not build-time-transpiled — no runtime GLSL compilation exists in swgl, and webgl is nothing but runtime shaders. Dead end, do not re-walk.
- webgl frames in software mode register as ExternalImageType::Buffer; the outer handler in shared/paint/lib.rs must pass RawData through with UNFLIPPED texel rect (GL textures use flipped uv, memory buffers are already flipped at readback).
- JS readPixels returning 0,0,0,0 after present is SPEC-CORRECT when preserveDrawingBuffer=false (default) — not a bug.
- WebGL thread surfaces are IOSurface-backed on macOS — a zero-copy optimization over glReadPixels exists but is unwired; current readback cost is one copy per canvas-frame.

## §1zb. Real-YouTube seek verification — harness-verified seek FAILS on real content (2026-11-25/26)

End-to-end verification of the §1t seek implementation against signed-in real YouTube (watch?v=Uc726aogLjw, servoshell webdriver probes /tmp/seekverify2.py, /tmp/seekdiag2.py, /tmp/appendtrace.py): the seek implementation passes the controlled harness but FAILS on real YouTube. Defects, most damning first:

1. Position frozen at X.001 values (11.001, 30.001, 40.001 observed) after seeks — playback feeding continues (network appends flow, verified) but position never advances. rs stuck at 4. Sometimes position frozen even PRE-seek (7.873373 across reads in one run) — non-deterministic across runs.
2. Post-seek position misaligned when it does advance: seek +60 from t≈8 → position reports 43.4 advancing to 50 — matches neither target (~68) nor any obvious landmark.
3. Buffered index does not match the presentation timeline on real YouTube: buf end 11.0 while playing at t=15 in one run; buf 43 while t=8 in another. Post-seek buf jumps to 71.001.
4. Seek-to-0 hard-fails: media error + emptied events — pipeline teardown, player dead.
5. getVideoPlaybackQuality unimplemented (returns undefined) — no frame-counter motion detection available.

INSTRUMENTED FACTS (appendtrace): YouTube NEVER uses timestampOffset for regular appends (always 0) — segments carry timeline position in their internal tfdt. Post-seek YouTube re-appends init segments (small 220-266 byte appends) + media segments, including re-appending byte-identical lengths already fed pre-seek. So the "page re-appends on its own seeking handler" assumption holds — data IS flowing post-seek; the failure is in position/timeline machinery, not starvation.

Symptom index additions:
- Symptom: position frozen at exactly X.001 while playback feeding continues (rs 4, no error) → check: post-flush-recreate position derivation in servomse/mse_source.rs (owns the gst segment base after appsrc recreation — prime suspect: recreated appsrc emits segment starting at/near 0 and position mapping clamps or mismatches), video track decode stall after seek (first-frame-only decode if post-seek media doesn't align), per-track position selection logic in servo-media player (which track provides position and whether it re-bases after recreate). NOT the network layer (appends verified flowing), NOT timestampOffset handling (YouTube uses 0 always).
- Symptom: buffered ranges disagreeing with playback position on real YouTube (index ahead OR behind position) → check: tfdt-scanning indexer vs multi-init quality-switch streams (YouTube switches init segments across bitrates — each quality has own init; indexer may mis-parse across the switch), appendWindow clipping unimplemented?, the X.001 offset pattern suggesting +1ms frame-duration artifacts. WebM indexer verified accurate on uniform content — fMP4 multi-init real-world content is the untested class.
- Symptom: media error + emptied events immediately after seek-to-zero → check: seek_reset path with target 0 / pipeline state when seek arrives while previous recreate still in flight (three rapid seeks in probe), error propagation from qtdemux on re-fed init segments.

## Metal default-on milestone (feather PR #123, 2026-11-25)

Metal is the default present path on macOS; FEATHER_METAL=0 is the escape hatch. All stage-2 gates passed with the webgl bridge in place: webgl probes+real site, GL regression clean, input latency improved (29.6 vs 23.1 rAF fps), video correct. The repaint-cost program that started with the video-CPU diagnosis is closed: present cost 21-27ms → ~1ms/frame.

## Real-world validation findings (2026-11-26)

Symptom: instant abort loading youtube.com in swgl/metal mode, "unknown shader ps_quad_gradient DITHERING" → FIXED (enable_dithering:!is_software_webrender): swgl lacks runtime shader compile; webrender build strips DITHERING variant. Rule: any webrender feature that mints new shader variants must be checked against swgl's build-time shader set before enabling in software mode.

Symptom: youtube watch page aborts with GLib pthread_mutex_lock invalid-argument (vp9/gst media threads) → PRE-EXISTING, reproduces identically in GL mode — NOT metal-related. Prime suspect: servo-media gst threading on macOS with youtube's streaming format (vp9+adaptive). Local progressive mp4 plays fine. Owner: LINUX LANE is root-causing this (assigned 2026-11-26) — repro facts for it: URL https://www.youtube.com/watch?v=jNQXAC9IVRw (first youtube video, vp9); death signature in stderr: 'GLib (gthread-posix.c): Unexpected error from C library during pthread_mutex_lock: Invalid argument. Aborting.' preceded by gstpad.c 'Got data flow before segment' warnings from capsfilter0/vp9dec0; process dies within ~20s of page load, no rust panic. Reproduces 100% on macOS in BOTH GL and metal modes on the same binary. Local progressive 720p mp4 through gst/vtdec plays fine for minutes — the failure is specific to youtube's adaptive/vp9 chain (mse-ish streaming), not decode generally.

## Real-world validation of Metal default-on (2026-11-26, feather PR #127)

Real-world CPU paired measurements (windowed top sampling, same binary): metal vs GL — bbc scroll 9.5/29.8%, reddit scroll 1.4/6.2%, maps scroll 0.9/6.0%. Rendering pixel-correct on real sites; webgl organic validation via google maps passes through the readback bridge.

Symptom: macOS "Developer Tools Access" password dialog appears during automation-driven input injection (osascript System Events keystrokes) and steals focus, silently degrading scroll-injection benches → password is the machine account password (devin); dismiss once and the grant persists. Also: cmd+a does NOT select-all in the secure password field; click field first, then type. Computer-tool coordinates are 1024x768 scaled — divide screenshot-pixel coords by (1120/1024, 840/768) before clicking.

Symptom: bench harness `wait` with no arguments in a script that launched feather with `&` hangs forever (waits on feather itself) → check: harness backgrounding structure. Never bare `wait` in bench scripts.

Symptom: webgl-heavy content dramatically worse in swgl/metal mode than GL mode (83% vs 62% CPU at 800x600 canvas) while small canvases are fine → FIXED (single-shot readback): the readback bridge issued one read_pixels PER SCANLINE for the vertical flip; driver-call overhead scales with rows, not bytes. Rule: never loop GL driver calls per scanline when a single full-frame read plus in-memory transform works — driver round-trip cost dominates memory bandwidth on unified memory. Also indexed: IOSurface zero-copy evaluated and deprioritized — the memcpy is nearly free on unified memory and stride-padding makes it inexpressible in webrender's tight-packed RawData path anyway.

## §1zc. Streaming EBML indexer — real-YouTube buffered fixed, forward seek works, backward stall isolated (2026-11-26)

Root-caused the §1zb swiss-cheese buffered: YouTube splits single WebM clusters across multiple appendBuffer calls at arbitrary byte boundaries (64KB chunks verified — captured appends begin mid-cluster with continuation bytes). The flat per-append scanner missed split clusters and false-positive-detected the 1F43B675 byte pattern inside binary payload. Content is WebM (VP9/opus) with ABSOLUTE cluster timecodes in ms (0, 10.0, 10.7, 20.0 — parsed from raw bytes); YouTube never uses timestampOffset.

FIX: WebmIndexer in ebml.rs — streaming EBML parser with cross-append state. Walks element structure properly: skips payloads by exact declared sizes (payload bytes never misread as headers), descends into Segment, extracts Timecode by parsing cluster-head child elements, carries partial elements in a bounded pending buffer across appends. Plus cross-append span estimation in sourcebuffer.rs (single-cluster-per-append batches use the gap from the previous append's last timecode instead of a fixed 1s guess).

VERIFIED on real YouTube: buffered ranges now contiguous — single growing range [0, 64.06] vs prior swiss cheese; eviction trim visible and correct. Forward seek WORKS end-to-end: seeking/seeked fire at target, position advances post-seek (53.4→56.4 at correct rate). Parser handles pathological fragmentation (32768-byte partial cluster carried across feeds, consumed accounting verified exact in debug logs).

REMAINING DEFECTS (indexed, next work item):
- Backward seeks stall: position frozen at exactly target+0.001 (50.001 observed), seeking/seeked fire correctly but playback never resumes. YouTube then enters a re-append loop (same segment byte-lengths recycling endlessly — the player re-feeding holes while stalled). Post-seek pipeline resume is the suspect, not indexing: forward seeks with identical machinery DO resume.
- Position reporting flaky pre-seek: frozen at ~7.8-8.7s across reads in most runs while playback actually continues (verified by later jump to 16.02). Freeze zone coincides with YouTube's low→high quality switch (new init segment appended ~5-10s in). Suspect: quality-switch init re-feed disturbing the position/clock chain, NOT the indexer (indexer output verified correct in the same runs).
- Seek-to-0: position read 80.001 — nonsensical; error+emptied events in earlier runs. Uninvestigated.

### Symptom index additions
- Symptom: backward seek stalls at exactly target+1ms while forward seeks work → check: post-flush-recreate pipeline resume in servomse seek_reset for backward-in-time data (prime suspect — the recreated appsrc re-segments fine for forward but backward re-fed media may arrive with timestamps LOWER than the running pipeline clock/segment base and get dropped as late), player re-append loop as a SYMPTOM not a cause (verified: player re-feeds correctly-formed segments while stalled), indexing layer (ruled out — contiguous correct ranges verified pre/post seek).
- Symptom: position frozen at ~8s pre-seek while playback continues → check: YouTube quality-switch init re-feed interaction with position reporting (prime suspect — freeze coincides with the second init segment arrival), per-track clock selection after caps renegotiation.

## §1zd. Seek algorithm code-reading findings — structural bugs beyond indexing (2026-11-26)

Code review of HTMLMediaElement::seek after live-probe chaos (non-deterministic positions, seeks landing at wrong targets, EMPTIED cascades):

1. SEEKABLE-ABORT BUG: step 8 aborts the seek entirely (no seeking event, no player seek) when seekable is empty — and seekable for MSE mirrors buffered, which handle_seek CLEARS on every seek. So after one seek, until the page re-appends enough, all further seeks silently no-op. Compounded by YouTube evicting aggressively behind the playhead — backward seeks into evicted regions clamp to the nearest surviving range end instead of the user target (spec-correct clamping applied to an artificially-empty index). The spec assumes buffered persists across seeks; our deviation (clearing because the flush genuinely drops pipelined data) structurally breaks the spec algorithm that consumes it. OPEN DESIGN TENSION: clearing is needed so the page re-feeds post-seek, but clearing breaks seekability/draining semantics. Candidate resolution: keep a shadow 'ever-buffered' seekable union independent of the flush-drop index.
2. Position-reporting chain after flush+recreate remains unmapped: positions post-seek sometimes advance from pre-seek values (pipeline never reset?) and sometimes freeze at target+1ms. Whether seek_reset even executes in observed runs is UNVERIFIED — gst debug categories showed no flush lines, but those categories don't log event flow at level 2-4. Next instrumentation: eprintln in seek_reset + player seek routing (cheap, decisive) before any further theory.
3. YouTube's own recovery behaviors dominate black-box observables: the player tears down and recreates the video element (EMPTIED events) when positions misbehave, making per-run behavior chaotic. Controlled-harness verification (local WebM MSE page, no player JS) is the only clean signal — black-box YouTube probing is diminishing-returns until the position chain is mapped.

### Symptom index additions
- Symptom: second and later seeks on MSE do nothing at all (no seeking event even) → check: seekable-empty abort in HTMLMediaElement::seek step 8 interacting with the buffered-clear-on-seek deviation (CONFIRMED structural cause by code reading — seekable mirrors buffered, cleared on prior seek), YouTube eviction amplifying it.
- Symptom: seek lands at wrong position (clamped far from target) → check: step-8 nearest-seekable-range clamping against a flush-cleared/evicted index (CONFIRMED mechanism), NOT a pipeline seek execution issue.

## §1ze. Position-offset correction + the post-seek resume-stall as the last blocker (2026-11-26)

Mechanism chain now fully mapped with instrumentation evidence: (1) seeks DO execute — eprintln verified player routing + seek_reset running on every probe seek; (2) positions post-seek misreport because matroskademux re-bases at the first post-seek buffer — playbin reports time-since-seek, correct pre-seek only by coincidence (clusters start at 0); (3) user seeks are clamped before reaching the pipeline (probe set 30, pipeline received 22.181 — nearest seekable end against the flush-cleared index).

FIX LANDED: mse_position_offset in player.rs — the MediaSource-branch seek stores the (clamped) target as offset; position-update events report pipeline position + offset. Strictly inside MSE code paths; progressive/seekable untouched. Mechanistically necessary regardless of the remaining stall — without it even a perfectly-resuming pipeline would report wrong positions.

REMAINING BLOCKER (the last one for real-YouTube seek): post-seek playback RESUME is flaky-to-dead. Instrumented facts: seek completes (seeking flag clears), data flows (YouTube re-appends verified), but position pinned at exactly the target and never advances in most runs — the pipeline accepts re-fed bytes without producing advancing playback. Rare runs do advance — race flavor. Seek-to-0 additionally hard-errors the pipeline (error+emptied events — probably the same stall cascading into YouTube's recovery teardown). This is a gst pipeline-state problem inside the flush+recreate design: re-preroll, multiqueue/decodebin3 state after flush, or segment/clock re-derivation on the recreated appsrc. NEXT INSTRUMENTATION PLANNED: GST_DEBUG pad probes on the recreated appsrc output + demux output post-seek — count buffers actually flowing vs dropped, compare working vs stalled runs.

Disk hygiene (user standing rule): pruned 31GB — feather shell debug/incremental (27G) + stock servo target tree (4.4G, binary preserved at /tmp/stock-servoshell). Free space 7.2G→38G.

## Heavy webgl2 dogfooding (2026-11-26, feather docs/webgl2-heavy-dogfood.md)

Symptom: three.js skinned-mesh demo renders model SOLID BLACK while rest of scene correct (metal mode, persists past load) → suspects ranked: compressed texture formats (KTX2/basis) undecoded, vertex-texture/float-texture skinning path, sRGB-in-readback (weakened by aquarium textured fish being correct). Owner: unclaimed defect DQ-webgl-black-skinning.

Symptom: heavy continuous webgl saturates a core in BOTH modes at ~7-8fps on large canvases (aquarium 1024²) → INVARIANT cost, not a bug: GPU sync + swgl CPU raster of dirty region per the zero-copy eval. Metal ~25% cheaper than GL but both bound. Do not re-investigate as a regression.

Symptom: shadertoy.com unreachable — Cloudflare security-verification wall → known turnstile limitation, NOT a webgl defect. Indexed so nobody misattributes.

POSITIVE: webgl2 fully verified — real WebGL 2.0 contexts, core instancing/VAO/texImage3D, GLSL ES 3.00 compile+link+draw correct through the readback bridge in metal mode. Aquarium (instanced textured geometry, multi-pass) pixel-perfect.

## sec1zf — post-seek resume-stall ROOT-CAUSED + FIXED (027251a9ed2)

**Symptom: post-seek playback stall (position pinned at target, never advances) → check: servomse seek_reset lossy-handover race (CONFIRMED CAUSE — fixed), matroskademux state (status: ruled out — when handover is lossless the demux re-parses init+clusters fine), appsrc enough-data signaling (status: ruled out — pushes returned Ok throughout), multiqueue/decodebin3 (status: partially involved downstream but not the root — flow resumes once input is lossless).**

Evidence chain (pad probes + push counting on signed-in YouTube):
- post-seek pushes accepted: 268, buffers exiting ghost pads: 3 — data swallowed at the source, never reaching demux
- stall vs resume is a RACE: identical builds sometimes resume (19+ buffers out) depending on whether re-appends arrive during the flush/recreate window
- pushes in the window land in the old appsrc being destroyed, or hit a removed track key → bytes silently lost → demux starves permanently (first cluster partial/unparseable)
- fix: seek_reset holds the tracks mutex across the entire flush+recreate so pushes block and land in the new appsrcs losslessly. Verified: consecutive runs resume with advancing position.

**Symptom: post-seek position reads higher than seek target (~78-88 when target 30) → check: htmlmediaelement seek clamping vs seekable ranges (prime suspect — clamp target varies run to run with buffered extent; sec1zd structural tension unresolved), mse_position_offset double-counting (status: ruled out arithmetically — neither rebase nor absolute pts theory reproduces observed values without a varying clamp input), demux timebase re-basing (mechanism confirmed but correctly compensated by the offset fix).**

**Symptom: rs collapses to 0 / currentTime resets to 0 some seconds after a stalled seek → check: YouTube player.js error recovery resetting MSE state (prime suspect — application-level response to starvation, not engine). Not an engine crash.**

Debug-instrumentation recipe (reusable): eprintln in mse_source push_buffer + pad probe on ghost pads post-flush — buffer OUT count vs push count is the decisive lossless-handover metric.

## sec1zg — multi-seek end-to-end state: two fixes landed, resume-flakiness precisely layered

**Symptom: seek target clamped upward/backward seeks impossible → check: MSE seekable mirroring flush-cleared buffered union (CONFIRMED CAUSE — FIXED in 4c14e896601: seekable=[0,duration] for finite MSE, matching Chrome/Firefox; clamping eliminated, targets accepted exactly).**

**Symptom: post-seek position wildly wrong (~90 for target 60) → check: mse_position_offset double-count (CONFIRMED CAUSE — FIXED in 4c14e896601: matroskademux outputs ABSOLUTE cluster timecodes, pipeline positions are natively correct presentation times; offset removed; verified correct advancing positions when playing). The earlier rebase theory (sec1ze) is REVISED: demux does not rebase; pre-seek correctness was coincidental (streams start at 0).**

**Symptom: post-seek playback frozen at exact target forever (rs=4, correct data indexed) → multi-layered, partially unresolved:
- layer 1: byte loss in flush+recreate handover (CONFIRMED — FIXED in 027251a9ed2, atomic reset under tracks lock)
- layer 2: YouTube feed flakiness — sometimes never re-feeds post-seek (aborts=0, append count frozen); cause unknown, suspect engine-side seek-event completion or timing; NOT a constant — other runs re-feed correctly (73 appends, correct target ranges indexed)
- layer 3: when fed correctly, pipeline consumption still stalls in most runs — bytes verified entering appsrcs (pushes ok) but playback doesn't advance; pad-probe discrimination of source-exit vs downstream starvation inconclusive across flaky runs. Suspects: decodebin3 stream-id handling of recreated appsrc stream-start (new random stream-id on existing topology), segment/running-time vs pipeline clock mismatch, post-flush decoder keyframe re-priming.
- negative results: appendBuffer errors — none ever observed; feeding logic itself — ruled out as constant cause (feeds correctly in some runs); seekable clamping — ruled out post-fix.**

**Symptom: position collapses to ~0 + rs=0 some seconds after a stalled forward-out-of-range seek → check: YouTube player.js error-recovery teardown/restart (prime suspect — player recreates MSE pipeline, restarts feeding from 0; NOT an engine crash).**

**Probe recipes (reusable): (1) JS wrap SourceBuffer.prototype.appendBuffer/abort counting calls+bytes+errors on window — distinguishes page-side feeding vs engine-side consumption; (2) eprintln push counting + pad probes on servomse ghost pads — decisive lossless-handover metric is buffers-out vs pushes-in; (3) probes must be installed inside seek_reset since pads don't exist earlier. Startup flakiness note: youtube watch pages sometimes serve ads (dur<60) — gate probes on dur>60 with retry+nudging play().**

Honest mission assessment: single-seek sometimes works end-to-end now (correct position, advancing playback); multi-seek does not. The remaining stall is multi-layered race-flakiness centered on the flush+recreate design's interaction with decodebin3 — resolving it may require replacing the design with timestamp-rewriting append pipelines (WebKit-style), est. 1-2d expert gst work, outcome uncertain.

## sec1zh — Reddit heap attribution: the category table IS measured (Linux, working instrument) (2026-11-26)

**Instrument (now working on Linux, pushed)**: parallel+timeout memory-report collector in components/profile/mem.rs (fires all reporters before waiting, 20s per-reporter bound — a wedged reporter no longer takes down the report) + engine-side timer trigger: `SERVO_MEMDUMP_AFTER=<secs>` env — thread inside servo fires reports independent of shell event loops and LoadStatus (reddit never reaches Complete), repeats every 60s. Servoshell also gained unconditional --memory-output full-row dumps (upstream was hitrace-only).

**THE TABLE — idle reddit tab, signed-out, settled ~45s+, RSS 1162MB, stable across 3 dumps 2min apart:**

| MB | category |
|---|---|
| 342 | webrender/images — WR-side decoded+uploaded surfaces |
| 309 | net image-cache — CPU decoded copies, main pipeline ALONE |
| 272 | DOM text nodes (reddit shreddit DOM) |
| 78 | JS malloc-heap, all realms summed |
| 51 | layout trees (box-tree 48 + stylist + stacking-ctx) |
| 49 | JS gc-heap used, all realms |
| 34 | JS non-heap |
| 29 | net memory-cache (raw undecoded resources) |
| 28 | DOM other+element+attr nodes |
| ~2 | webrender display-list/other |

Cross-check rows: jemalloc-heap-allocated 777 / active 798 / mapped 885; smaps anonymous 829 of which js-gc-heap 78 (SM's own mmap, outside jemalloc). Resident 1162. Attributed total slightly exceeds resident due to overlap between measurement axes — directionally correct per category.

**THE HEADLINE FINDING: images own >55% of the tab — and the same decoded bytes are effectively held TWICE (342 WR-side + 309 net-cache CPU-side ≈ 651MB combined).** Second-order finding: **the net image-cache per-pipeline cap (MAX_COMPLETED_LOAD_BYTES=128MB) is measurably violated — 309MB in ONE pipeline's cache.** Either the cap doesn't count the same bytes the reporter does, eviction never triggers (pinned-in-use images can't evict?), or LRU is broken. THIS is the highest-leverage concrete memory bug now identified: fixing the cap enforcement alone should bound the CPU copy at 128MB; the deeper fix (single-copy ownership — cache shells with paint-time re-decode, already documented as weeks-scale) attacks both copies at once.

**Corrections of record**: (1) the earlier note "dropping the cap 128→16MB left scroll growth unchanged" is consistent ONLY if the cap is non-functional — reframe: those experiments measured nothing because the cap never binds. (2) Windows lane's 2.8GB vs our 1162MB — different content/scroll depth, same structure; table ratios are what transfer.

**Symptom index — symptom: single idle reddit tab RSS >1GB → check: net image_cache enforcement (prime suspect — cap measurably violated, 309/128MB), double-ownership of decoded surfaces net-cache vs webrender (confirmed structural, 2x copies), DOM text-node mass on shreddit (confirmed 272MB — content-visibility addresses layout not DOM), JS heaps (ruled out as dominant — only ~160MB all realms), layout trees (ruled out — 51MB).**

**Measurement caveats**: measurement-race crash (MessagePort concurrency) NOT hit in 3+ consecutive reddit dumps on Linux — treat as rare. vsize/total-row sums are hierarchical double-counting — never sum all rows; attribute per leaf categories. The url() rows key by document — reddit shows google-recaptcha webworker (29MB malloc + 20MB non-heap!) and accounts.google.com iframes as separate realms — third-party JS is ~90MB of the JS total.

## Black-skinning root cause + fix (Nov 26, mac lane) — SOLVED

Symptom: three.js skinned-mesh / ANY MeshStandardMaterial content renders solid black in both GL and Metal
modes, while basic/phong materials and the ground plane render correctly → check: `validate_internal_format`
in components/script/dom/webgl/validations/tex_image_2d.rs (owns texImage2D internalformat acceptance —
CONFIRMED CAUSE: rejected unsized GL_RG 0x8227 because TexFormat::Red/RG had empty compatible_data_types
via the catch-all), texture upload paths image-element/image-bitmap/blob-bitmap (status: ruled out — all
verified uploading+sampling correctly via probe), skinning/bone-texture pipeline (ruled out — procedural
SkinnedMesh repro correct), shadow mapping (ruled out — shadowMap+castShadow repro correct), MSAA/antialias
(ruled out), canvas size (ruled out), RG16F half-float sized upload+sampling (ruled out — raw probe exact).
MECHANISM: three.js r186 added unconditional `material.dfg = texture2D(dfgLUT, ...)` to standard-material
lighting; the LUT is a 16x16 RG+HalfFloat DataTexture uploaded with UNSIZED GL_RG internalformat — invalid
in strict WebGL2 but accepted by all browsers via driver passthrough. servo rejected it → texture never
allocated → shader sampled 0 → `multiScatteringCompensation = 1 + spec*(1/EssMs - 1)` with EssMs=0 →
division by zero → NaN → black. FIX: feather-engine d4a8a04e3 — Red/RG compatible types added;
EXT_color_buffer_float implemented (core-profile GL exposes float rendering without the ES extension
string — is_supported now `!is_gles || extension-present`); OES_texture_float/half_float_linear same.
Bisect artifacts (three r183-r187 + probes) in /tmp/vtest — key ones: pbrtest186.html (minimal repro),
rg16fprobe.html, glbtestlive.html. DIAGNOSTIC LESSON: bisecting the three.js VERSION (r185 ok/r186 black)
cracked it in 4 steps when mechanism-guessing was stalled; for library-dependent rendering defects, bisect
the library version FIRST. Also: the texImage InvalidEnum error appeared in WORKING versions too (r183) —
error-presence is not proof of causation; verify against a passing baseline before chasing.

## sec1zi — image-cache cap verdict: structurally toothless, memory is DOM/WR-owned (2026-11-26)

**Symptom: net image-cache far exceeds the 128MB per-pipeline cap (309-338MB on reddit) → check: encoded-vs-decoded byte miscounting (status: RULED OUT — cap counts image.bytes.len() = decoded RGBA), broken LRU/eviction (status: RULED OUT — eviction logic correct as designed), total pinning by external Arc holders (CONFIRMED CAUSE — instrumented: 103/103 images pinned, 100%: every cache entry is Arc-shared with a DOM/render holder; evicting pinned entries frees nothing by construction so the cap has nothing it can legally free).**

Consequences:
- The cache does not OWN the memory it measures — it mirrors bytes owned by DOM image nodes and paint-path references. Cap enforcement can only ever affect genuinely-unreferenced images (scroll-accumulation case, where it already works: post-DOM-removal unpinned images get evicted).
- §1037 pin-release dead end is now explained mechanistically a second time: dropping a pin does nothing because bytes are Arc-shared and re-pinned every paint — do not re-attempt a third time.
- The idle-memory image mass decomposes into: DOM-side decoded bytes (the Arc) + WR-side copy (webrender/images 342MB — independent ownership, uninvestigated lifetime) + the encoded originals (small). Lever ranking: (1) WR-side CPU-copy lifetime scoping — hours-scale, biggest tractable piece; (2) lazy/offscreen decode — weeks-scale structural.

**Instrumentation recipe (reusable)**: eprintln inside enforce_completed_load_budget gated on >half-cap — prints per-pipeline total/pinned bytes+counts. IMGCACHE-DBG in working tree only, never committed.

### sec1zi addendum — WR-side copy scoping verdict (hours-scale check, 2026-11-26)
webrender/images (342MB) is CPU-side bytes retained by upstream webrender 0.70's resource cache — WR keeps CPU copies of every registered image so textures can be re-uploaded after texture-cache eviction. The keys persist because servo never deletes them (same pinning reflection: image-cache eviction is what sends WR DeleteImage, and pinned images never evict). Attacking this = vendoring/forking webrender (crates.io upstream, NOT a servo fork — Cargo.toml:290) and redesigning resource-cache retention: weeks-scale, upstream-grade surgery, NOT hours-scale. On software-GL hosts (llvmpipe/SWGL) there is no GPU copy at all — the WR bytes ARE the rendering surface, so no savings are possible there at all; on real GPUs the CPU copies are still retained by design. CONCLUSION: no hours-scale cheap win exists in the image-memory subsystem in either direction; both copies are structural. The only real lever remains the documented weeks-scale redesigns (single-copy ownership, lazy/offscreen decode-at-visibility). The image-cache cap mission is closed as premise-false: the cache never owned the bytes.

## sec1zj — image-memory stage 0: same-bytes VERIFIED, real-feed baseline, scroll-crash blocker (2026-11-26)

**Same-bytes hypothesis confirmed at every link (code-verified, not measured)**: (1) servo uploads via GenericSharedMemory::from_arc_vec(bytes.clone()) — in single-process mode (use_ipc() false unless --multiprocess) this is InProcess zero-copy, the SAME Arc; (2) WR stores it as CachedImageData::Raw(Arc<Vec<u8>>) unchanged (resource_cache.rs:134); (3) both reporters measure op(ptr) on the SAME heap allocation — WR's own comment admits no double-counting protection. CONSEQUENCE: webrender/images and net image-cache rows in memreport are the same physical bytes counted twice; true decoded image mass on reddit ≈340MB not 650. The single-copy-at-a-time design in feather docs/image-memory-residency.md stands on this basis.

**Baseline on VERIFIED real feed** (title "Reddit - The heart of the internet", 28 shreddit-posts, 124 imgs, scrollH 13659 — the js_challenge URL parameters persist post-challenge but the feed is real): idle-top dump: resident 1769MB, wr_images 338, net image-cache 291, dom text 267, layout 59.

**NEW DEFECT — scroll crash**: window.scrollBy on real reddit feed crashes servoshell: GLib-GObject-CRITICAL g_object_ref_sink assertion then signal 11 in thread "Script#3", first scroll never returns, webdriver connection resets. BLOCKS every verification gate of image-memory stages 2-4 (can't verify scroll-eviction behavior if scrolling crashes). Symptom index — symptom: crash signal 11 in Script#3 on first scrollBy of reddit feed → check: scroll-event handling of shreddit custom elements (prime suspect — shreddit uses resize/intersection observers heavily), G_IS_OBJECT assertion in glib layer immediately preceding (ordering evidence — the glib error fires first, suggests GObject lifecycle corruption propagating into script thread).

**Measurement methodology correction (important for all lanes)**: webdriver /session creates a NEW blank webview — JS introspection runs against about:blank while the URL-argument page loads in the original webview. All webdriver-driven measurement MUST navigate the session itself (POST /session/{id}/url) — never pass the URL as a servoshell argv and introspect via webdriver. Conversely memreport is process-wide so either way works for memory measurement alone.

## sec1zk — reddit "scroll crash" verdict: NOT scroll — gstreamer teardown use-after-finalize (2026-11-26)

**Symptom: signal 11 in thread Script#3 shortly after scrolling reddit feed → check: scroll event handling (RULED OUT — plain 20000px page survives, bbc.com survives 8 scrolls, reddit itself survives 4/5 identical repro attempts with scrollY reaching 16000), servoshell/webdriver harness (RULED OUT), gstreamer object lifecycle in media teardown (CONFIRMED CAUSE — crash log shows gst_bus_set_flushing assertion on a dead bus, then g_object_ref '!object_already_finalized', then g_object_ref_sink 'G_IS_OBJECT failed', all within 1ms before the fatal signal).**

Mechanism chain: scrolling is innocent but causally adjacent — moving through the feed starts/stops/tears down video posts (qtdemux/mp4 evidence in the warnings immediately preceding the crash), and video element teardown hits the documented "there be dragons" disposal path in GStreamerPlayer::drop (player.rs ~1205): `unsafe { player.run_dispose() }` justified by the comment "nobody uses the player after this moment" — the crash is that claim being violated. The servo authors knew: the comment references upstream gstreamer MR13022 as the real fix. Content randomness (whether video posts are near the scroll path) explains the flakiness.

Ruled out: pure create/destroy lifecycle churn — a synthetic MSE teardown loop (460 rapid cycles of video+MediaSource+appendBuffer+abort+remove) survives cleanly. Garbage-fed pipelines never reach the playing state though — the fatal window likely requires a LIVE pipeline mid-teardown (bus messages in flight while dispose finalizes the bus).

Fix options in cost order: (a) intentional-leak mitigation — skip run_dispose when inner is non-unique (the dragons condition is already detected! currently only debug_asserted, compiled out in release) — bounded memory cost per torn-down video, eliminates the whole class; (b) serialize teardown — NULL state + bus drain before dispose, ordering complexity; (c) true root-cause of the violating refholder — 1-2 days gst/glib internals, uncertain. NOTE: the dragons detection already exists in-code — option (a) is small and surgical.

## Hardware video decode investigation (Nov 26, mac lane) — vtdec already active; vp9 gap measured marginal

Symptom: video playback CPU high on macOS → check: gstreamer vtdec/VideoToolbox hardware decode
(status: CONFIRMED ALREADY ACTIVE — gst_vtdec_handle_frame + VideoToolbox frames in thread samples of
normal playback; nothing to wire). videoconvert/software-conversion cost (status: ruled out as
significant — ~zero activity in samples). Repaint/compositing path (prime suspect, confirmed dominant —
consistent with the repaint-cost diagnosis: video is a 30fps invalidator).

MEASURED (bundled .app, env-clean, 30fps looped local files, windowed top sampling):
static page ~0%, h264-720p 39.1%, h264-1080p 52.0%, vp9-720p 46.6%, vp9-1080p 58.6%.
Software vp9 decode costs only ~+7pp over hw-decoded h264 at the same resolution — the compositing
path dominates both. Codec coverage: gst 1.22.3 vtdec handles h264/h265/mpeg2/jpeg/prores — NO vp9/AV1
(vp9 hw wiring landed upstream in gst 1.24+). YouTube serves vp9 → falls to libav software decode.
vtdec SRC caps include video/x-raw(memory:GLMemory) — hw frames CAN stay GPU-side in gst, but servo's
appsink handoff forces system memory (cheap-ish on unified memory, nonzero).

VERDICT: no software-decode defect exists for h264/HEVC — hardware decode was already working.
The vp9 gap is real but marginal (~7pp of ~47-59% playback CPU); upgrading bundled GStreamer to 1.24+
for vtdec-vp9 is the only available lever and is risky while the vp9/gst threading crash (youtube,
indexed for the Linux lane) is unresolved — a newer gst may also change that crash's behavior either way.
Deprioritized behind the compositing-cost program which dominates video CPU regardless of codec.

## sec1zl — teardown-crash FIX landed: intentional-leak mitigation, 10/10 gate (2026-11-26)

Fix (sec1zk option a) in GStreamerPlayer::drop: the dragons condition (non-unique inner / extra player refs) is now checked BEFORE dispose; when detected the player is intentionally leaked (mem::forget) instead of run_dispose — eliminating the use-after-finalize crash class at a bounded memory cost. Loud comment + upstream MR13022 reference in-code. [playerleak] eprintln counter logs each leak with running total.

Verification gate PASSED: 10/10 deep-scroll reddit sessions survived (8 scrolls each, scrollY 14000-16000 — video posts in path); pre-fix crash rate was ~1/5 sessions.

LEAK COST MEASURED: 7-13 leaked players per 8-scroll reddit session (mean ~11) — the dragons condition is PERVASIVE, not occasional: without the fix those players were all disposed unsafely and the crash was the unlucky intersection. Per-leak cost is pipeline object shells not frame data (pipeline stopped first) — est. tens-to-hundreds of KB each, so single-digit MB per heavy scroll session. Acceptable against the image masses, but it ACCUMULATES over very long sessions — indexed as a known bounded-leak cost; proper fix remains upstream MR13022 or the true refholder root-cause.

Calibration: healthy progressive-mp4 teardown does NOT trip the leak path (0 leaks) — the threshold distinguishes correctly.

Symptom index — symptom: SIGSEGV in Script#3 during/after scrolling video-heavy feeds, preceded by gst object-finalized assertions → check: GStreamerPlayer::drop disposal race (CONFIRMED cause class — fixed by intentional-leak mitigation), scroll input handling (ruled out), [playerleak] counter in stderr (diagnostic: nonzero count means the hazardous condition occurred and was survived).

## sec1zm — image-memory stage 1: byte-swap machinery resurrected onto current feather-engine (2026-11-26)

Applied the imgmem-wip machinery (284-line diff, 4 files) cleanly onto current feather-engine: encoded_bytes retention at decode, DecodedBytes interior-mutable residency (Resident/Evicted behind RwLock, shared across all pin holders through the same allocation), materialize_decoded() accessor (re-decodes synchronously from encoded source when evicted; re-checks after write-lock upgrade), evict_decoded() (static images only, frames.len()==1), global resident-byte counter, env-gated enforcement (FEATHER_IMGMEM=1, 384MB budget — default OFF so behavior-neutral). RasterImage keeps its original bytes field for initialization; residency shares the same allocation (no duplication).

Stage-1 verification gates PASSED: compile-clean; visual correctness on image-heavy sites (reddit feed + r/pics — photos/avatars/banners decode and paint correctly); memreport rows flow unchanged (image rows same order of magnitude as baseline modulo content variance). KNOWN COST: encoded_bytes retention is unconditional — every decoded image now also retains its encoded source (~10-20x smaller; est. +15-30MB on reddit-class pages). This is the design's intentional insurance payment, not a regression.

CRITICAL REMINDER for stage 2 (restated so it can't be lost): byte eviction alone frees ZERO for any painted image in single-process servo — WR's CachedImageData::Raw holds a clone of the same Arc allocation (verified sec1zj). The stage-2 eviction unit must be (decoded residency + WR key deletion) together, with re-decode + re-upload on re-visibility. The byte-swap layer is necessary-but-insufficient scaffolding.

## sec1zn — stage-2 mechanism map + refined design (2026-11-26)

Image-key lifecycle mapped (function-indexed): upload is ONE-SHOT at decode completion — set_webrender_image_key (net/image_cache.rs ~150) allocates from key_cache batch, calls paint_api.add_image, sets raster_image.id = Some(key) permanently. Display lists reference the key via ImageResolver::image_key_from_cached_image (layout/context.rs ~283) which just reads raster_image.id — no upload-state awareness anywhere. DeleteImage happens only on structural cache eviction (rare due to pinning). CONCLUSION: no re-upload path exists at all — deleting a WR key today permanently breaks rendering of that image. Stage 2 must build re-upload from scratch.

REFINED DESIGN — key decisions:
1. **Re-upload hook**: image_key_from_cached_image is the natural site — every display-list image reference flows through it. Needs: upload-state tracking on RasterImage (Uploaded(key)/Evicted) replacing the plain id field, and a re-upload path that materializes decoded bytes + calls paint_api.add_image + re-keys. COMPLICATION: add_image needs paint_api access and key allocation which currently live in the net-process image cache store — must be plumbed into the layout-side resolver or done via a synchronous cache request. Half-day plumbing minimum.
2. **The LRU-touch oracle FAILS as designed in imgmem-wip**: display lists are viewport-AGNOSTIC — servo builds fragments for the entire document regardless of scroll position, and lists do not rebuild on pure scroll (compositor-side scrolling). So display-list reference does not mean visible, and even visible images stop being touched on static pages. Eviction cannot use naive reference-touch ordering.
3. **Therefore viewport relevance must be computed geometrically at display-list-build time**: fragment rect vs viewport+margin — the builder knows both. This is the same verdict cv-wip needs (synergy: compute once, share). Content insertions on infinite scroll trigger rebuilds often enough for the verdict to stay fresh on exactly the pages that matter.
4. Eviction trigger remains budget-pressure (global), but candidate ordering uses the geometric verdict: never evict in-viewport images; evict furthest-offscreen-first; hysteresis margin to prevent scroll-flap churn.
5. Failure mode analysis: worst case remains re-decode+re-upload churn on rapid scrolling — bounded by hysteresis; correctness never at risk because re-materialization is always possible from encoded bytes.

IMPLEMENTATION ORDER for stage 2 (each independently verifiable): (a) upload-state tracking + re-upload plumbing — correctness-preserving even with eviction off (verify: forced eviction unit test, image reappears); (b) geometric viewport verdict at display-list-build — measure hit rates, no behavior change; (c) eviction-unit atomics (bytes+key together); (d) budget trigger + ordering + hysteresis, gates per design doc.

## sec1zo — stage 2a landed: eviction-unit + re-upload machinery; black-flash confirms step-order mandatory (2026-11-26)

Implemented (env-gated FEATHER_IMGMEM=1, default off): eviction unit now deletes WR key alongside decoded bytes (paint_api.update_images DeleteImage + key recorded in evicted_upload_keys — required because single-process WR holds a clone of the same Arc; byte eviction alone frees zero). Re-upload-on-cache-read in get_completed_image_if_available: evicted keys re-materialize (re-decode from encoded) and re-add via paint_api.add_image REUSING THE SAME KEY — no RasterImage mutability surgery needed, permanent id association stays valid. Budget now env-configurable (FEATHER_IMGMEM_BUDGET_MB) for testing.

Verification: compile-clean; forced eviction on r/pics (20MB budget) — evictions fire, 6 re-uploads observed in one scroll cycle, zero crashes, 236 imgs present. BUT the visual gate exposed the predicted gap exactly: visible post image renders BLACK after eviction cycle — because re-upload only triggers on cache READ, and painted images are never re-read (layout resolves keys once at display-list-build time; scrolling does not rebuild). Keys are baked into display lists; the re-uploaded data never reaches already-built lists' visible images unless a reflow happens.

CONSEQUENCE (design validation, not surprise): the step ordering in sec1zn is MANDATORY — eviction must not target visible images, which requires the geometric viewport verdict (step b) BEFORE eviction is safe to enable for real. The black-flash is not a bug in 2a; it is 2a operating exactly as designed without its prerequisite. Stage 2b (geometric verdict, shared with cv-wip) is the next implementation and is the true core of stage 2. Mechanical pieces confirmed working and retained: eviction-unit atomics, re-decode re-upload path, same-key reuse (validates that WR accepts re-adding a deleted key — was an open question).

## Energy bakeoff (2026-11-26, macOS VM) — symptom index entries

Symptom: high battery drain / CPU in video playback vs other browsers → check: swgl compositing of video frames (per-frame full repaint, readback+raster+upload — cannot amortize on changing content; prime suspect, confirmed dominant), gst vtdec decode path (status: ruled out — hardware decode already active, vtdec in samples), vp9 software decode (minor, ~7pp). Measured: video 720p feather 448 ms/s vs chrome 234 vs safari 86 — the single largest measured energy gap; closing requires GPU-side compositing (see scoping doc, weeks-scale).

Symptom: energy measurement on macOS VMs → check: powermetrics cpu_power sampler (status: ruled out — no pmgr IO registry entry on VirtualMac2,1, physically impossible), powermetrics tasks sampler (works: per-process CPU ms/s + package-idle wakeups are the rigorous proxy ingredients; relative deltas meaningful, watts are not).

Symptom: browser idle battery drain → check: paint-gating + Metal present path (status: verified fixed — feather idle measures 0.1 CPU ms/s and ZERO package-idle wakeups vs chrome 12.0/3.6 safari 5.0/15.7; an idle feather genuinely does nothing).

Harness: feather repo shell/tools/ebench/ (measure.sh parse.py runall.sh) — reruns the full 12-cell matrix in ~65min on any mac.

## sec1zp — byte-swap made REAL: sole-owner residency; measured -1GB on reddit deep-scroll; corruption gap confirmed (2026-11-26)

CRITICAL FIX over sec1zo: the stage-1 resurrection was accounting fiction — RasterImage kept its original `bytes` field alongside the new residency RwLock, so evict_decoded dropped the lock's Arc while `self.bytes` pinned the same allocation. Eviction counters fired, WR keys died, physical memory never moved (first measurement: cache rows INCREASED 861MB, RSS unchanged 3840 vs 3812 baseline). Fix: deleted the `bytes` field entirely — the RwLock is now the SOLE owner of decoded bytes. Consumer surface was tiny (favicon used first_frame only for width/height; descriptor/snapshot/to_shared already materialize). Accounting made Drop-balanced via ResidentBytes newtype (constructor increments, drop decrements — covers eviction swap, image drop, cache removal with zero manual call-site accounting). Manual MallocSizeOf reports encoded + resident-decoded non-triggeringly (measurement does not re-materialize — verified by design of the impl reading the lock fast-path only).

MEASURED WIN (reddit signed-in, 8x1500px deep scroll, SERVO_MEMDUMP_AFTER=78, budget 100MB):
- resident total: 3812MB (no eviction) -> 2775MB (eviction on) = -1037MB (-27%)
- webrender/images: 872 -> 60MB; reddit image-cache rows: 595+221 -> 249+64MB
- 357 evictions / 1281MB cumulative churn / 26 re-uploads; zero crashes; SURVIVED deep scroll
- residual cache mass = retained encoded bytes (the insurance cost — larger than the 15-30MB idle estimate on deep scrolls, scales with distinct images seen) + animated + under-budget resident

CORRUPTION GATE — resolved NEGATIVE, exactly as the design predicted: scrollback to top shows BLACK RECTANGLE for images evicted-while-visible (verified visually: kitchen-aid post black, freshly-loaded apple ad renders fine). Zero-corruption is UNACHIEVABLE with budget-only eviction because (a) display lists bake keys and never rebuild on pure scroll, (b) cache-read re-upload only fires on reflow, (c) WR cannot render a deleted key no matter the scroll position. Both directions of the problem need geometric machinery: eviction must not target visible images AND scroll-approach must trigger re-upload. There is no cheaper trigger — confirmed by this measurement. This is stage 2b, the core multi-day cost, now confirmed unavoidable rather than speculative.

Symptom index: symptom "image renders black after scrolling back to it (FEATHER_IMGMEM on)" -> check: display-list key baking (owns: keys permanent once built; pure scroll never rebuilds) prime suspect/confirmed; cache-read re-upload path (status: working but insufficient — only fires on reflow); WR key deletion (status: working as designed); NOT a re-decode failure — re-uploads verified mechanically sound.

## sec1zq — stage 2b implementation map: mechanism terrain fully explored (2026-11-26)

Explored paths and verdicts for the geometric machinery (prerequisite for corruption-free eviction):
- PAINTER-SIDE scroll-driven re-upload: DEAD. Paint retains only the ScrollTree + iframe rects + epochs per pipeline (pipeline_details.rs) — display lists are NOT retained paint-side (they ship to WR). No geometry to walk. Do not re-attempt.
- WR-internal demand signal (renderer notices missing key at frame render): architecturally ideal but WR is upstream crates.io webrender 0.70, unforked — vendoring WR is the documented weeks-scale cost. Not for alpha.
- KEY POSITIVE FACT: scroll positions ALREADY flow paint->layout (webview_renderer.rs send_scroll_positions_to_layout_for_pipeline) for sticky positioning/IO/scroll events. Layout owns fragment geometry (retained fragment tree) AND holds an image_cache handle (layout/context.rs). Layout is the correct home for both halves of the geometric machinery.
- THE DESIGN that falls out: (1) at display-list-build time, record per-image spatial index entries (cache key, fragment rect, scroll node) in a side table owned by layout — cheap because layout already walks everything; (2) on scroll-position updates (riding the existing paint->layout path, throttled to scroll-end or ~100ms), query the spatial index for evicted images intersecting viewport+margin and call the existing cache re-upload path; (3) at eviction time, consult the same index so visible images are never evicted in the first place. Both halves share ONE geometric index — this is the economy of the design. Open implementation questions: fragment-tree lifetime vs index invalidation on reflow; spatial index cost on shreddit-scale trees (thousands of images — need a flat sorted structure, not a tree walk); whether scroll updates arrive frequently enough (they ride input events, not WR frame scrolls — verify: does compositor-side scroll send position updates per frame or only on scroll-end? If only per input event batch, re-upload latency may be visible as brief black flash even in correct operation — acceptable if bounded).
- Hysteresis note: eviction margin must exceed re-upload margin by a hysteresis band or images oscillate evict/re-upload at viewport edges (design doc already says this; now it's concrete WHICH margins: the same spatial index query with two different paddings).

Disk hygiene note for this lane: target/release is now ~30GB after the full rebuild — incremental artifacts pruned once already; if space pressure returns, `cargo sweep` or delete incremental dirs is safe (git is the backup).

## NV12 video pipeline (2026-11-26) — symptom index entries

Symptom: high CPU/energy in video playback → check: videoconvert NV12→BGRA pass (status: FIXED — planar-NV12 pipeline 2e312865a, ~22-26% video CPU reduction; frames now reach webrender as R8+RG8 planes rasterized by brush_yuv shader), general compositing machinery swgl-raster+present (prime suspect for residual — video is a 30fps invalidator; not video-specific, compositing-bound), gst copy-out + texture-upload copies (minor, ≤10% ceiling, deprioritized).

Symptom: video colors wrong (green/purple shift, washed out) after pipeline changes → check: YuvColorSpace/ColorRange hardcoded Rec709+Limited in layout/display_list/mod.rs push_yuv_image (correct for HD h264; wrong for SD BT.601 content — if SD video looks wrong, this is why; proper fix parses gst colorimetry caps), chroma plane order in VideoFrameData::NV12 (verified correct for vtdec output), UV plane dimensions (RG8 half-width half-height — a full-width UV mistake shows as horizontal chroma doubling).

Measurement lesson: copy-pass elimination measured HALF its predicted win (22% vs ~50%) because per-pixel conversion compute merely moves from videoconvert into the rasterizer shader — only memory bandwidth and extra passes are truly saved. When costing pipeline optimizations, separate compute (often conserved) from bandwidth/passes (actually eliminated).

## sec1zr — stage 2b LANDED: geometric eviction verdict + scroll-driven re-upload; scrollback corruption FIXED (2026-11-26)

The geometric machinery is implemented and verified end-to-end:
- Shared registry pixels::IMAGE_GEOMETRY (WR key -> content-space rect + near_viewport bit), written by layout at display-list-build time (image fragment push site, initial verdict vs unscrolled viewport) and updated on EVERY scroll-offset change — both the paint->layout renderer path (set_scroll_offsets_from_renderer, input scrolling) and the script path (set_scroll_offset_from_script, programmatic scrolling — required because webdriver/JS scrolling rides it).
- Eviction-time verdict: enforce_resident_byte_budget skips images whose near_viewport bit is set — visible content is never evicted (kills the black-rectangles-on-visible-content class at the source).
- Scroll-driven re-upload: on scroll updates, keys transitioning far->near are collected and re-uploaded via the new ImageCache::reupload_evicted_images trait method (reverse-indexed batch, reuses the same key so baked display-list references become valid again on the next frame — no rebuild needed). Re-upload margin = 1 viewport ahead, so images return BEFORE entering the visible region.
VERIFICATION (r/pics, the exact testbed that black-rectangled pre-2b): 12x1000px deep scroll + incremental scrollback, budget 100MB — 23 re-upload batches / 116 re-uploads / 392 evictions, zero crashes, and the top-post photo renders CORRECTLY on scrollback (visually verified against the pre-2b black-rectangle baseline). Memory rows stay bounded: webrender/images 109MB, cache rows small. Mechanism confirmed: display lists do not need rebuilding — re-adding the same WR key fixes rendering on the next frame.
V1 approximation limits (documented, not blocking): single-bit verdict at 1x margin for both decisions (no oscillation possible since consistent, but no hysteresis band); nested scrollers/transforms not accounted (root-scrolled feeds exact); dominant-scroller approximation uses max scroll offset across nodes; re-decode is synchronous on the scroll path (jank risk on slow devices — stage 3 async re-materialization); registry entries are never pruned (small leak, few hundred KB worst case); multi-webview shares one registry (fine for alpha).
Symptom index: symptom "image black after scrollback (FEATHER_IMGMEM)" -> status FIXED by geometric verdict + scroll-driven re-upload; if recurs check: scroll offset sign convention (positive-down verified working on reddit), script-vs-renderer scroll path coverage (both hooked), registry entries fresh after reflow (rebuild overwrites).

## sec1zs — async re-upload offload attempt: crashes webrender, REVERTED; sync path is the shipped design (2026-11-26)

Attempted stage-3 cheap core: moving the re-upload batch (reverse-index build + re-decode + add_image) off the layout/scroll thread onto the cache thread pool. Result: webrender render_api RecvError panic then pthread_mutex_destroy/signal-11 during active scrolling — the WR render backend dies when image updates arrive from pool threads mid-scroll. Mechanism unproven but the timing correlation is exact (sync path never crashes across all prior verification runs; async crashed on first deep-scroll test). Candidate mechanisms: (a) paint_api/CrossProcessPaintApi add_image not safe from arbitrary pool threads in single-process mode; (b) concurrent in-flight batches racing WR texture-cache updates; (c) pool-thread re-decode lifetime interacting with WR frame rendering. NOT investigated further — cost/benefit poor because the sync path is verified-correct and the jank it risks is unmeasured.
DECISION: sync re-upload is the shipped design. Re-pin/re-decode scroll-path jank is a DOCUMENTED RESIDUAL — if user-visible jank is reported on slow devices, the correct stage-3 is a properly-designed async pipeline (bounded queue, single dedicated re-decode thread, generation-tagged uploads), not a thread-pool spawn.
Symptom index: symptom "webrender RecvError panic + signal 11 during scrolling" -> check: image updates arriving from non-layout threads (prime suspect when FEATHER_IMGMEM re-uploads are pool-offloaded — reverted); NOT the gstreamer teardown class (different signature: no gst assertions precede).

## Pre-alpha dogfood (2026-11-26, shell-new macOS) — symptom index entries

Symptom: in-page clicks missing small targets (links/input fields dead) while big buttons hit → check: forward_page_input rel() coordinate conversion in shell-new src/app.rs (prime suspect — clicks land down-right of physical position, verified: center-click of 300x100 anchor logged at 227,100 hitting BODY; scale confusion between egui logical pts / winit physical px / scale_factor), servo input pipeline itself (status: ruled out — keydown events reach document fine, clicks DO register, just at wrong coords), engine focus handling (status: ruled out as primary — fields never receive correct-position clicks to begin with).

Symptom: typing in web forms dead → check: same coordinate defect as above (confirmed cause — input fields never gain focus because clicks miss them; keyboard delivery itself verified WORKING: doc keydown events fire for every key).

Symptom: shell becomes zombie after sustained use — omnibox/new-tab silently dead but chrome responsive → check: shell action-processing dispatch logic (prime suspect — main thread verified parked in normal mach_msg idle, NOT an event-loop wedge; egui repaints fine; actions specifically never execute), winit/egui event delivery (status: ruled out — events still update UI state visibly).

Symptom: video element black despite playback → check: gst plugin loading (status: ruled out — vtdec hw decode verified active, 993 samples in gst_vtdec_handle_frame; startup plugin-error log is benign fallback path), frame handoff to compositor / media client-context registration (prime suspect — "unknown client context" warnings coincide), initial-paint invalidation gap (related: pages don't composite until first input).

Symptom: localhost dev servers unreachable "client error (Connect)" → check: IPv6/IPv4 resolution fallback (localhost→::1 vs IPv4-only server; 127.0.0.1 works fine) — no happy-eyeballs in servo net.

## Hydration-stall class root cause (2026-11-26) — symptom index entries

Symptom: SPA renders but never becomes interactive though JS partially runs (analytics fire, spinners animate) → check: MISSING BOOT-CRITICAL APIs first (confirmed cause class) — requestIdleCallback/cancelIdleCallback undefined (NO webidl/impl in servo at all, unguarded call throws sync mid-boot — prime suspect, YouTube boot scheduler uses rIC), window.scheduler missing, crypto.subtle missing, visualViewport missing, serviceWorker missing, navigation api missing, history.scrollRestoration missing; shadow DOM rendering (status: ruled out — verified painting correctly); customElements lifecycle (status: ruled out — define/upgrade/connectedCallback/built-in extends all verified working); single unguarded missing-API call unwinds the whole boot routine at the throw point — explains uniform failure across unrelated heavy apps.

Symptom: cannot diagnose engine JS failures in embedding → console output and uncaught exceptions invisible in shell-new (nothing surfaces); servo's devtools server exists behind devtools_server_enabled pref but shell doesn't wire it; diagnosis requires blind localhost-probe pages — highest-leverage tooling fix available.

Lesson: probe pages testing typeof/guarded-call behavior loaded in the target browser beat log spelunking when no console exists — write results visibly into the DOM, click page once to force the first paint (F4 invalidation gap).

## sec1zt — stage 4 landed (hysteresis + budget tuning) + VIDEO-PLANE CRASH root-caused (independent of imgmem) (2026-11-26)

Stage 4 policy: dual-margin hysteresis implemented — eviction protection at 2x viewport (wide), re-upload trigger at 1x viewport (tight); the band between them neither evicts nor re-uploads so fast scroll direction reversals cannot thrash evict/re-decode cycles. Budget default now 256MB (2x the 100MB verified zero-corruption, headroom for image-dense viewports); with geometric protection the budget is a backstop.

CRASH INVESTIGATION — major negative-correlation result: the "webrender dies mid-scroll" crash class is NOT caused by image eviction. Control experiment on r/pics deep-scroll: eviction-OFF died 3/3, eviction-ON died 2/3 — same signature both: `Unexpected format RG8` panic in WRRenderBackend texture_cache.rs:488 then scene-sender SendError / signal 11. ROOT CAUSE: video plane uploads — media_frame_renderer.rs uploads NV12 video as R8 luma + RG8 chroma plane images; WR's shared texture-cache arrays only support alpha8/16 + RGBA8/BGRA8 and PANIC on RG8 allocation. Flakiness explained: large planes get standalone allocation (safe), small/medium planes fit the shared arrays (panic) — so whether a feed scroll crashes depends on the sizes of videos encountered. Pre-existing upstream-grade defect in video rendering, exposed by video playback enablement — UNRELATED to all image-memory work. The sec1zs async-offload crash attribution was WRONG in retrospect — same class.
Fix options documented: (a) vendor WR + make shared-cache allocation fall back to standalone for unsupported formats instead of panicking (proper, weeks-scale vendoring cost); (b) servo-side chroma expansion — convert UV plane to BGRA before upload (quarter-res conversion, moderate per-frame CPU, self-contained in media_frame_renderer); (c) gstreamer caps negotiation forcing BGRA output (full-frame conversion cost, simplest). Recommend (b) if this becomes alpha-priority.
Symptom index: symptom "Unexpected format RG8 panic WRRenderBackend then signal 11 while scrolling video-containing feeds" -> check: media_frame_renderer NV12 plane upload (confirmed cause — RG8 chroma planes hitting shared texture cache); NOT image-memory eviction (ruled out by control experiment 3/3 deaths with eviction off); NOT scroll itself; size-dependent — small/medium videos crash, large don't.

## sec1zu — FINAL ATTRIBUTION measurement protocol note (2026-11-26)
The stage-4 closeout table compares: sec1zh/sec1zj baselines (idle-top: WR 342 + cache 309 same-bytes ≈340MB real image mass, RSS 1162; deep-scroll: WR 872MB, RSS 3812+) vs post-machinery measurements. All measurements on signed-in reddit, SERVO_MEMDUMP_AFTER timed dumps.

## sec1zv — IMAGE MEMORY MISSION CLOSEOUT: final attribution table (2026-11-26)

Protocol: signed-in reddit, SERVO_MEMDUMP timed dumps, deep scroll 8-24 x 1000-1500px. Same-bytes correction applies throughout (WR + cache rows count the same physical allocation — real mass is NOT their sum).

| measurement | before machinery | after machinery | delta |
|---|---|---|---|
| resident total, deep scroll 12000px | 3812MB (growing 4523 by 24k equiv) | 2775MB | -1037MB (-27%) |
| resident total, deep scroll 24000px | (baseline run showed continuous growth 3812->4184->4524 across dumps) | 2374MB FLAT | bounded flat |
| webrender/images row, deep scroll | 872MB and growing | 98-111MB FLAT across depths | -88%, non-growing |
| net image-cache rows, deep scroll | 595+221MB | 36-249MB (encoded retention + protected-resident) | bounded |
| scrollback correctness | n/a (machinery off) | zero corruption — visually verified r/pics exact-failure-testbed | PASS |
| crashes in verification | teardown class (fixed separately) | zero eviction-attributable (video RG8 crash proven independent by control 3/3) | clean |

What the numbers mean: image decoded-byte mass is now BOUNDED FLAT across infinite scroll — the single-copy-at-a-time invariant holds: visible+protected images cost decoded bytes, everything else costs only encoded bytes (10-20x smaller) until scrolled back. The residual encoded retention (~tens of MB scaling with distinct images seen) is the documented insurance cost. Remaining documented residuals: synchronous re-decode on scroll path (unmeasured jank risk, proper async is the rejected-cheap-path lesson of sec1zs), registry entry accumulation (KB-scale), DOM text-node mass 272MB (next category — untouched).

Mission structure as built: stage 1 byte-swap machinery (sole-owner residency, Drop-balanced accounting), stage 2 eviction unit + geometric verdict + scroll-driven re-upload, stage 3 async REJECTED with evidence (sync is the design), stage 4 dual-margin hysteresis + 256MB backstop budget + default-on in feather shell (feather#140 merged).

## requestIdleCallback implementation (2026-11-26, feather-engine 402c303cf)

Symptom: youtube blank white after rIC landed (was grey skeleton before) → UNRESOLVED whether progress (hydration advanced past skeleton into broken real-content render) or regression — bisect by reverting rIC locally and comparing. Mechanism note: IdleCallbackTick no-op frame callback keeps the rAF pump alive while any idle callback is pending; recursive idle re-registration (common site pattern) = continuous rAF-rate ticking = CPU cost — not spec-true idle detection, documented v1 approximation.

Lesson: generated webidl callbacks invoke via `callback.Call__(cx, args..., ExceptionHandling::Report)` NOT `.call()`; dictionary fields without defaults generate as Option<T>; methods need cx param only when codegen decides (compile errors tell); CrossProcessInstant needs #[no_trace] in dom structs; double returns need Finite<f64> wrap.

## F1 final root cause (2026-11-26) — correction of earlier coordinate hypothesis

Symptom: in-page links/inputs appear dead → check: POST-NAVIGATION PAINT INVALIDATION in shell-new compositing (CONFIRMED CAUSE — verified live: clicks register exact css coords (149,148 vs true 150,150), click events fire on correct targets (target=A), anchor activation proceeds, navigation fetches+loads the target (200 in net log), omnibox eventually updates — but the new page NEVER PAINTS until subsequent input interactions nudge the surface; links were never dead, navigation was invisible); forward_page_input coordinate math (status: RULED OUT on macOS scale1 AND windows scale2 — exact registration both lanes); anchor activation/navigation delegate path (status: RULED OUT — navigation completes end-to-end); blit geometry at scale1 (status: RULED OUT — visual box position matches layout position within 2px). MEASUREMENT LESSON: the original down-right-coordinate evidence was measurement error — the click position was calculated from assumed element positions rather than verified visual positions; never infer input-transform defects without a self-reporting target element (one that logs its own hit coordinates).

## YouTube results-render stall — root cause chain (2026-11-26, macOS lane)

Symptom: SPA hydration appears complete, search data arrives (200), results never paint → check: `ytd-two-column-search-results-renderer` hidden attribute removal (prime suspect — never removed in servo; force-removal renders everything), Polymer property-effects/observer timing for visibility triggers (open), custom element upgrade machinery (ruled out — constructors/attachShadow/connectedCallback/upgrade all probe-verified on the live page), nested flex layout (ruled out — probe passes), content-visibility (ruled out — unsupported in servo), Trusted Types (ruled out — correctly implemented; probe throws were eval artifacts), boot throws (ruled out — zero console errors with full exception+rejection visibility).

Key diagnostic pattern: when an entire subtree is 0x0@0,0, walk UP the chain before theorizing layout bugs — the cause was a single display:none host at the top. Zero-geometry descendants are the trivial consequence.

Tooling lesson: an evaluate-javascript escape hatch in the shell (/tmp/feather-eval.js hook) converts blind-probe hours into direct-measurement minutes. Console+exception+rejection visibility is necessary but not sufficient — silent logic failures need DOM inspection.

## YouTube final unhide chain + negative results (2026-11-26, macOS lane, round 2)

Symptom: results fully built in DOM but never visible → check: hydration element replacement (CONFIRMED — boot instance gets unhidden at T+1952ms, hydration then swaps in a NEW element instance stamped with hidden again; the final unhide step never executes on it; verified by identity tracking: current element has zero hidden-ops in log). Binding churn ruled out (MutationObserver — nothing re-sets hidden). scheduler.postTask ruled out (zero references in YouTube's entire JS payload — would have been wasted implementation). Trusted Types ruled out (fully correct). Unhide machinery generally ruled out (100+ successful removals across other elements).

Post-manual-unhide functionality measured: search results render fully (thumbnails, badges, metadata), SPA click-through to /watch works, watch page renders full chrome incl. live chat. YouTube visible gaps reduced to exactly two: final unhide step + video frame compositing (F3).

Diagnostic tooling pattern that worked: persistent eval hook (re-injected at navigation commit) + monkeypatched Element.prototype.setAttribute/removeAttribute/hidden-setter with element identity tagging (WeakMap ids) — this is the general instrumentation pattern for "which JS touches which DOM state when" questions in servo. Also: shell-new spin-throttle Duration subtraction underflow panic — TOCTOU between guard check and subtraction; any elapsed()-then-subtract pattern in the shell needs saturating_sub.

## sec1zm — RG8 video-plane crash FIXED: planar YCbCr upload (2026-11-26)

FIXED — commit f9180e2bf1d. NV12 video chroma is now deinterleaved into separate Cb/Cr planes and uploaded as planar YCbCr — all-R8 planes, fully shared-cache compatible, natively supported by WebRender's brush_yuv shader. Deinterleave cost is a quarter-resolution byte shuffle — negligible against decode cost.

Root-cause chain for the record: WebRender `is_allowed_in_shared_cache` (texture_cache.rs) checks SIZE constraints but NEVER checks whether the format is supported by the shared arrays — RG8 planes under TEXTURE_REGION_DIMENSIONS entered shared cache and select() panicked "Unexpected format RG8". Size-dependent flakiness explained: large planes went standalone (safe), small/medium hit shared arrays (panic). This is an upstream WR defect — the proper upstream fix is a format-support check in is_allowed_in_shared_cache, but the planar path is strictly better for servo anyway (no upstream dependency, no vendoring).

Verified: (1) SMPTE color bars render with CORRECT colors (grey/yellow/cyan/green/magenta/red/blue + bottom stripes) — video visibly plays, not black, not color-shifted; (2) 8/8 deep-scroll bursts on r/pics survive to scrollY 15649 with video content in path, zero RG8 panics — pre-fix rate ~2/3 crashes at this depth.

Symptom index: symptom "Unexpected format RG8 panic WRRenderBackend then signal 11 scrolling video feeds" -> status FIXED at f9180e2bf1d — planar upload means RG8 is never uploaded to WR at all. Symptom "video renders black despite active decode" (dogfood F3) -> FIXED same commit — planes now reach WR in a format its shader actually rasterizes.

Symptom: video colors wrong on SD content → check: Rec709+Limited hardcoded in display_list/mod.rs push_yuv_image (unchanged by this fix — correct for HD h264, wrong for BT.601 SD; proper fix parses gst colorimetry caps).

Threading note for future work: MediaFrame now carries image_key + uv_key + v_key; uv_key+v_key both present = planar, uv_key alone = NV12 (legacy path retained), neither = BGRA. Key lifecycle: generated per frame-format-change in media_frame_renderer render(), deleted in delete_frame_keys.

Residual cost: deinterleave allocates two new vectors per frame (half-res each) — memcpy-bound, unmeasured but small vs decode. The gst copy-out remains as documented in the NV12 pipeline notes.

## sec1zn — media teardown stress: seek-ack panic FIXED + youtube-test environmental block (2026-11-26)

REPRO HARNESS for youtube-class media crashes without youtube (youtube is bot-walled on v4-only VMs even with valid cookies — verified LOGIN_INFO sent yet still walled; youtube-nocookie embed also walled, zero videoplayback requests): /tmp/vp9stress.html — 4 concurrent looping vp9 decoders + create/destroy churn every 3s. This reproduces teardown-race crashes in minutes.

FIXED crash class (commit in player.rs): seek-ack channel Disconnected panic at player.rs seek_data callback — teardown drops the ack receiver while a gst-streaming-thread seek is in flight; ack_channel.send(()).unwrap() panicked. Reproduced 5 panics in 2min of churn pre-fix; ZERO panics in 3min of identical stress post-fix. Mechanism codec-agnostic but exposed by vp9-style decode churn.

Symptom index: symptom "panic player.rs seek_data ack_channel Disconnected during video teardown/seek churn" -> FIXED — send failure is benign (nobody waiting), now let-_. Rule for this file: ALL cross-thread channel sends in player.rs teardown-adjacent paths must be let-_ style — the receiver lifetime is never guaranteed during teardown.

OPEN — playerleak volume: the intentional-leak mitigation fires constantly under teardown churn (74 leaks in 3min extreme stress; real-world infinite-feed teardown rate is much lower but nonzero). Each leak pins a gst pipeline shell. PROPER FIX candidate: deferred-dispose retry — instead of permanent mem::forget, queue the player for a dispose retry when refs drain (poll on next player creation or a timer). Not attempted — design change beyond crash-fix scope.

UNRESOLVED — the "vp9 threads crash on YouTube" as a named defect: no documented signature exists in FEATHER_MEMORY or lane history (grepped). Direct vp9 playback (1080p webm loop) does NOT crash. YouTube itself untestable on this VM environmentally. If the crash resurfaces with a concrete signature, the vp9stress harness is the reproduction starting point.

## sec1zo — vp9/YouTube abort hunt: exhaustive negative-results matrix (2026-11-26)

MISSION CONTEXT: coordinator directive to root-cause "gst YouTube abort (vp9 threads crash)". CRITICAL META-FINDING: no such crash signature is documented anywhere — not in FEATHER_MEMORY, not in feather/docs/defect-queue.md, not in lane session history. All greps negative. The defect as named may be misremembered, misattributed, or fixed-incidentally by the planar-YCbCr/teardown work. This entry is the negative-results index so future lanes don't re-walk the search.

REPRO MATRIX — all on post-planar-fix feather, all CLEAN (zero panics/aborts):
1. Progressive vp9 loop 1080p webm, minutes — clean, renders correct.
2. Concurrent vp9 teardown churn (4 decoders, destroy/recreate 3s cycle) — clean post-seek-ack-fix (was: 5 seek-ack panics/2min pre-fix).
3. Random-seek stress 2.5 seeks/sec — clean.
4. MSE vp9 append choreography (updateend-gated 64KB chunks, YouTube pattern) dual-player churn, 5+min — clean, renders correct.
5. MSE adaptive resolution alternation — test artifact: arbitrary byte-splicing corrupts ebml stream (matroskademux "reading large block" errors are the SPLICE being invalid, NOT a product defect — proper repro needs cluster-aligned ffmpeg-generated segments, not built).

Symptom index: symptom "gst abort/crash during YouTube vp9 playback" -> check: seek-ack teardown race (FIXED — sec1zn); RG8 planar class (FIXED — sec1zm, was the actual cause of multiple misattributed crash reports); progressive vp9 decode (ruled out — minutes of loop clean); vp9 teardown churn (ruled out post-seek-ack-fix); MSE append churn (ruled out 5min clean); adaptive resolution reconfigure (UNTESTED properly — needs cluster-aligned segments). If the crash resurfaces: capture the exact signature BEFORE investigating — every hour spent on the wrong signature is waste.

ENVIRONMENTAL: real YouTube untestable on this VM — bot wall even with valid cookies sent (LOGIN_INFO present in requests, SIDCC server-side rotation invalidates within a day), youtube-nocookie embed walled too. YouTube testing requires either fresh cookie extraction from a live signed-in Chrome (portal-keyring encryption blocks the sqlite path — needs CDP-based extraction) or a non-null-routed network.

NEW CONFIRMED DEFECT (found via the stress harness): playerleak volume — 423 leaks in 5.5min of MSE churn stress. The intentional-leak mitigation converts crashes to leaks at high rate under teardown-heavy use. Deferred-dispose-retry is the proper-fix candidate.

## sec1zq — MSE memory leak class FIXED: sourcebuffer quota enforcement (2026-11-26)

ROOT CAUSE chain verified: appsrc push accepts bytes unconditionally regardless of max-bytes (the setting only drives need/enough-data signals) — servo's servomse pushed every appended byte in with no level check — memory grew unboundedly for any video whose pipeline doesn't consume fast enough, which is EVERY paused/preloaded/never-played video (reddit feed posts: 88MB measured idle, 2 SourceBuffers). Enforcement now at push time: level+size > 50MB/track cap → push rejected → QuotaExceededError from appendBuffer (spec-compliant, Chrome-matching behavior). Rejected bytes never enter the engine nor the indexer.

Symptom index: symptom "memory grows unboundedly on MSE video sites / SourceBuffer rows tens-hundreds of MB" -> FIXED at the quota commit — check servomse push_buffer level enforcement if it regresses. Symptom "video buffers stop growing / appends throw QuotaExceededError" -> now EXPECTED spec behavior when the page doesn't evict; pages that mishandle QuotaExceeded may stop buffering — not an engine defect.

Verification notes: regression pass = updateend-gated append choreography still completes, playback unaffected (playing videos never hit cap — queues drain). Accepted cost: one byte-copy clone per appendBuffer in the reordering (memcpy-speed, negligible vs decode). OPEN refinement: the 50MB/track cap is the pre-existing design constant — tuning it lower for memory-tight alpha is a one-line change with real tradeoffs (youtube forward-buffer depth). The mass attribution caveat: gst-internal appsrc queue bytes don't show in malloc_sizeof rows — memory verification for MSE must use RSS or behavior-level evidence, not the SourceBuffer row alone.

## sec1zr — XHR retention verdict: NOT an engine defect (2026-11-26)

Investigated the 79MB/663-object XMLHttpRequest mass from the reddit attribution. Servo stores response bytes in `response: DomRefCell<BytesMut>` inside the XHR object — spec-correct behavior: response/responseText must remain available while JS retains the object. The retention is PAGE-caused (reddit's JS holds hundreds of XHR objects alive in closures/pools), not engine waste — no fix available without violating spec. Negative index entry so nobody re-walks it: XHR response mass is a website-behavior datum, not a feather defect. Compare: the SourceBuffer mass looked similarly page-caused but turned out to be engine-fixable (sec1zq) because the spec HAS a quota mechanism — XHR has no equivalent pressure valve.

## sec1zs — DOM text-node mass characterized: CONTENT-dominated, representation dead-end (2026-11-26)

Controlled measurement (memreport rows on synthetic pages): (a) one 1MB text node → 2.0MB row — content costs ~2x byte size when JS-assigned (parser-created content likely ~1x); (b) 10,000 nodes × 10 chars → 3.21MB row — **~321 bytes per-node overhead** (node struct + reflector + String headers). Element nodes: ~473B each.

Applied to reddit's 224MB text mass: even a generous 30k-node estimate accounts for only ~10MB overhead — **the mass is ~95% CONTENT bytes**. Conclusions: (1) representation optimization is a dead end (~5% ceiling) — do not pursue; (2) the content itself must be shredded-JSON-state-dominated (shreddit serializes megabytes of state into script/element text) — page-inherent like the XHR mass; (3) the only theoretical levers are architectural and dubious: dropping script text post-execution violates spec (scripts can re-read textContent), string interning is marginal. VERDICT: text-node mass is not tractable engine-side. Negative index entry — do not re-walk unless the measurement methodology itself is questioned.

Symptom index: symptom "js/dom/text-nodes rows hundreds of MB" -> check: page content volume FIRST (shreddit-class JSON-state embedding), not engine representation. Per-node overhead ~321B, per-element ~473B are the calibrated constants for estimating overhead share on any page: overhead_MB ≈ nodes×321/1M — if that's small vs the row, the mass is content and engine-intractable.

## sec1zt2 — consolidation verification: all fixes converge on real reddit (2026-11-26)

Post-everything measurement (planar video + seek-ack fix + MSE quota, feather debug build, idle r/pics, 5 consecutive 60s memdumps): image-cache converges 711MB transient → **267MB flat at cap** across 4 steady-state dumps — eviction convergence confirmed in the wild; the 711 first-dump was the initial decode-burst transient before eviction catches up (bounded, self-correcting — but worth knowing: eviction lags burst loads). SourceBuffer **68MB flat** across all steady-state dumps — bounded consistent with quota enforcement (though under the 100MB 2-track cap, so enforcement not definitively stress-proven in the wild — the synthetic paused-append test remains the proof). RSS 1474MB idle — vs sec1zj-era 1162MB: the delta is media/gst preload cost + encoded retention growth, all now bounded.

Remaining open engineering item, deliberately not attempted: playerleak deferred-dispose-retry. Risk assessment: the dragons-condition refcount semantics are subtle (glib clone counting unverified), a wrong implementation resurrects the use-after-finalize crash class (alpha-critical) — risk-adjusted ROI poor vs the few-MB leak cost. Documented for a future careful session.

## YouTube boot instability — OOM + define_properties panic (2026-11-26, macOS lane, round 3)

Symptom: YouTube boot succeeds fully sometimes (video plays end-to-end), wedges mid-boot other times (stuck hidden, partial chrome), crashes outright occasionally ("Servo crashed! Send failed (Disconnected)") → check: SpiderMonkey memory pressure during boot (PRIME SUSPECT — live evidence: `uncaught exception: out of memory` in youtube main module + simultaneous script-thread panic), js_mem_max (ruled out as cause — unset/-1 = no configured cap), RSS system starvation (unverified — 1GB at crash time, needs profiling).

CONFIRMED ENGINE DEFECT regardless of memory: `define_guarded_properties` in script_bindings/interface.rs unwraps `define_properties` — when SM allocation fails (OOM), JS_DefineProperties errors, the unwrap PANICS, the script thread dies, the whole pipeline crashes. The panic converts a recoverable page-level OOM into a fatal pipeline crash. Signature: `panicked at interface.rs:434 called Result::unwrap() on an Err value: ()`.

Unifying theory for the YouTube hidden-attribute stall: boot corruption from memory pressure explains the run-to-run variance — full boot (video test), stuck-hidden boot (most common), crash boot. The final unhide step may be a victim of corruption rather than a missing primitive. Ruled out for the unhide this round: yt-navigate event chain (all events fire correctly on both watch AND search navs — start/player-updated/finish/page-data-updated), scheduler.postTask (unused by YouTube entirely), Trusted Types, CE machinery.

Meaningful functionality baseline established: watch→watch SPA navigation flawless, miniplayer works, autoplay chain works, video playback correct post-planar-fix (F3 confirmed fixed end-to-end on real YouTube MSE).

## sec1zt3 — THE NEXT MEMORY FRONTIER: gst decode-pipeline mass is RSS-dominant and reporter-invisible (2026-11-26)

Deep-scroll verification with ALL fixes (eviction ON, r/pics video-heavy, scrollY 22500, 15/15 scrolls zero panics): every reporter category bounded flat — image-cache 266→287MB at cap, webrender/images 235→218, SourceBuffer 66→27 (quota working in the wild), text-nodes ~flat. **Yet RSS = 7.2GB.** The memreport/jemalloc accounting is now blind to the dominant mass: gstreamer decode pipelines allocate through system malloc/glib — NOT jemalloc — so video memory is completely invisible in memreport rows. Frame math: one 1080p NV12 frame ≈ 3MB; every preloaded video's pipeline holds multiqueue+decoder-pool frames; video-heavy infinite feeds preload dozens-hundreds of pipelines. Compare closeout-era RSS 2374MB at similar depth BEFORE video enablement — the multi-GB delta is the video pipeline mass.

Symptom index: symptom "RSS huge but memreport categories all bounded/small on video-heavy pages" -> check: gst pipeline frame memory FIRST (system-malloc invisible to jemalloc rows — verification must use RSS not memreport), multiqueue/decoder bufferpool retention in paused/preloaded pipelines. NOT image cache/eviction regression — verify categories before suspecting them.

OPEN MISSION-CLASS questions: (1) how many concurrent gst pipelines does a feed scroll accumulate and when do they die (shreddit virtualization destroys post elements — does teardown actually happen?); (2) multiqueue/decoder pool sizes per pipeline — the multiqueue default buffers are time-based not byte-capped for video; (3) whether preloaded-but-never-played videos can be prevented from prerolling deep frame buffers at all (decode-on-play activation — the natural architectural fix, echoes the image-memory lesson: the only real win is not decoding what isn't visible).
## sec1zt3 — THE NEXT MEMORY FRONTIER: gst decode-pipeline mass is RSS-dominant and reporter-invisible (2026-11-26)

Deep-scroll verification with ALL fixes (eviction ON, r/pics video-heavy, scrollY 22500, 15/15 scrolls zero panics): every reporter category bounded flat — image-cache 266→287MB at cap, webrender/images 235→218, SourceBuffer 66→27 (quota working in the wild), text-nodes ~flat. **Yet RSS = 7.2GB.** The memreport/jemalloc accounting is now blind to the dominant mass: gstreamer decode pipelines allocate through system malloc/glib — NOT jemalloc — so video memory is completely invisible in memreport rows. Frame math: one 1080p NV12 frame ≈ 3MB; every preloaded video's pipeline holds multiqueue+decoder-pool frames; video-heavy infinite feeds preload dozens-hundreds of pipelines. Compare closeout-era RSS 2374MB at similar depth BEFORE video enablement — the multi-GB delta is the video pipeline mass.

Symptom index: symptom "RSS huge but memreport categories all bounded/small on video-heavy pages" -> check: gst pipeline frame memory FIRST (system-malloc invisible to jemalloc rows — verification must use RSS not memreport), multiqueue/decoder bufferpool retention in paused/preloaded pipelines. NOT image cache/eviction regression — verify categories before suspecting them.

OPEN MISSION-CLASS questions: (1) how many concurrent gst pipelines does a feed scroll accumulate and when do they die (shreddit virtualization destroys post elements — does teardown actually happen?); (2) multiqueue/decoder pool sizes per pipeline — the multiqueue default buffers are time-based not byte-capped for video; (3) whether preloaded-but-never-played videos can be prevented from prerolling deep frame buffers at all (decode-on-play activation — the natural architectural fix, echoes the image-memory lesson: the only real win is not decoding what isn't visible).

## sec1zt4 — RSS forensic numbers: the 5GB gap decomposition starting point (2026-11-26 continuation of sec1zt3)

Precise numbers from the deep-scroll measurement (scrollY 22500 r/pics, all fixes): RSS/Pss_Dirty 7.4GB anonymous-private (real, not shared). jemalloc-heap-allocated only 1.68GB, system-heap-allocated 0.55GB — so ~5GB of RSS is NOT live allocation in either heap. Prime suspect decomposition: (a) jemalloc mapped-but-not-allocated retention — freed decoded bytes whose dirty pages jemalloc keeps for reuse (CHECK jemalloc-heap-mapped vs allocated ratio in future dumps — if mapped≈RSS the mass is retention, and the arena-purge lever applies directly — investigate whether eviction frees should trigger purge); (b) gst/glib system-malloc frames — but system-heap-allocated says only 0.55GB, weakening the gst-frame theory unless glib uses its own allocator outside system-heap accounting; (c) only 7 decodebin instances existed — NOT hundreds of pipelines, and only 4 playerleaks — both pipeline-count and leak theories WEAKENED. Resumption protocol: compare jemalloc-heap-mapped against RSS at matched timestamps; if mapped≈RSS, the fix is purge-after-evict in the image eviction path (cheap, high value). This is the single most important open memory question — all category rows are bounded yet the process eats 7GB.

## sec1zt5 — RSS forensics FINAL CLUE: giant anonymous mmaps, neither heap (2026-11-26)

smaps analysis of the 7.4GB-RSS scrolled process: a single **3.3GB anonymous rw-p mmap** plus ~40 more 100-500MB anon mmaps — together ~5-6GB, explaining nearly the whole gap. These are NOT jemalloc (heap-mapped only 2GB) and NOT system-malloc. Allocation pattern (huge contiguous anonymous mmaps) matches engine-level arena/zone allocators: prime suspects (1) **mozjs GC heap retention** — spidermonkey allocates arena chunks and notoriously never returns them; gc-heap/used row was only 22MB meaning gigabytes of EMPTY GC arenas retained — if confirmed this is a GC-tuning/GC-param problem not a leak; (2) llvmpipe/software-GL texture memory if textures mmap outside jemalloc. RESUMPTION PROTOCOL: (a) test on a NON-JS-heavy scroll (wikipedia) — if the giant mmaps don't grow, it's mozjs/GC; (b) mozjs GC params: servo exposes js options — shrinking/nursery/zeal settings; the key question is whether GC heap shrinkage (compact/empty-chunk-release) ever runs during idle. NOTE: this finding reorders the memory roadmap AGAIN — if the 3-5GB is GC retention, it dwarfs every category fixed so far and is a single-mechanism fix.

## sec1zt6 — RSS forensics DECISIVE COMPARISON: low-JS/no-video page stays flat (2026-11-26)

Wikipedia (List of chemical elements — long page, moderate JS, NO video) scrolled 15×1500px same protocol as the reddit deep-scroll: final RSS **431MB** vs reddit's **7.4GB**. Same engine, same scroll depth. CONCLUSIONS: (a) the giant-mmap mass is NOT inherent to scrolling/rendering/JS-churn generally — it's specific to video-bearing feeds; (b) mozjs-GC-retention theory WEAKENED (wiki runs plenty of JS); (c) the prime suspect returns to VIDEO decode memory with refined mechanism: **glibc per-thread malloc arenas** — gst decode threads allocate through glibc thread arenas which mmap independently (64MB-chunk growth pattern matches the ~40 observed 100-500MB anon mmaps) and are INVISIBLE to both jemalloc accounting AND (plausibly) the system-heap-allocated row if that only measures the main arena. The 3.3GB single contiguous mmap remains unexplained by this theory — candidate: glibc arena coalescing appearance in smaps (adjacent arenas merge visually) or llvmpipe texture memory for video frames. NEXT RESUMPTION STEP: scroll r/pics and timestamp-correlate mmap growth against vp9 decode start events in the gst log; if confirmed, fixes are (1) glibc arena tuning (MALLOC_ARENA_MAX=2 env — trivial to test, potentially multi-GB win), (2) gst decode buffer pooling caps. MALLOC_ARENA_MAX=2 is a one-line test with possibly the largest single memory ROI in the project.

## sec1zt7 — MALLOC_ARENA_MAX=2 confirmed: −3.2GB on reddit scroll (2026-11-26)

Decisive test: identical r/pics 15×1500px deep-scroll with MALLOC_ARENA_MAX=2 env: RSS **4.2GB vs 7.4GB** baseline — glibc thread arenas were retaining ~3.2GB. Confirms the sec1zt6 thread-arena theory: gst decode threads allocate via glibc arenas which never return memory cross-thread. One-line env fix, biggest single memory ROI in the project. Remaining 4.2GB vs wiki's 431MB still unexplained — the 3.3GB contiguous mmap and other masses persist; investigation continues but the cheap lever is proven. Implementation note: glibc reads malloc env tunables at ptmalloc_init (first glibc-malloc call) — since servo's Rust side allocates through jemalloc, first glibc malloc happens in C libs (gst/glib) AFTER main starts, so setting the env early in main() may be early enough — MUST be empirically verified in the feather shell before shipping; fallback is re-exec-with-env pattern. Arena count tradeoff: 2 arenas may serialize allocations across threads slightly — measure startup/perf impact if visible, but memory win dwarfs it. Symptom-index: Symptom: RSS grows multi-GB on video-heavy feeds invisible to memreport rows → check glibc thread arenas (prime suspect, CONFIRMED −3.2GB with MALLOC_ARENA_MAX=2), jemalloc retention (ruled out — heap-mapped only 2GB), mozjs GC retention (weakened — wiki flat).

## sec1zt8 — residual forensics post-arena-cap: the giant mmap is NOT arenas (2026-11-26)

Post-cap measurement (MALLOC_ARENA_MAX=2, r/pics scrolled, RSS 4.5GB): a **single 2.6GB contiguous anonymous rw-p mmap** persists plus [heap] 173MB and several 100-250MB mmaps. Since arenas are now capped, the giant contiguous mmap CANNOT be glibc thread arenas — revised attribution: most likely **jemalloc's own extent regions** (jemalloc mmaps large contiguous chunks; heap-mapped was ~2GB in the uncapped measurement, consistent scale) — meaning the residual giant-mmap mass IS inside jemalloc accounting after all, and the real question becomes the sec1zt4 second gap: jemalloc-heap-allocated 1.68GB vs sum-of-category-rows — i.e. LIVE-but-unaccounted allocation inside the Rust heap, not retention. Suspects for unaccounted live jemalloc mass: (a) mozjs GC/JS objects measured only partially by gc-heap rows; (b) layout/script structures outside the reporter tree; (c) WR internal structures. RESUMPTION: compare jemalloc-heap-allocated vs heap-mapped and category-sum at matched timestamps; if allocated≈1.7GB ≈ giant mmap, the frontier is 'unaccounted live rust heap' — use heaptrack or jemalloc prof (MALLOC_CONF=prof:true) for definitive attribution. The arena-cap fix (feather#153 merged) already removed 3.2GB; the residual ~4.5GB decomposes as ~2GB jemalloc (partially unaccounted) + [heap]+glibc ~0.7GB + remainder TBD.

## js_mem_max 192MB was the YouTube boot-crash cause — raised to 1GB (2026-11-26, macOS lane)

The shell's `js_mem_max=192` runaway cap (kept from memory-pressure work, predates heavy-SPA dogfooding) hard-caps SpiderMonkey — YouTube boot exceeds it: uncaught 'out of memory' in the main module + define_properties unwrap panic at interface.rs:434 killing the pipeline. Explains boot variance: full/wedged/crashed depending on when the ceiling hit. Fixed in shell-new: 1GB default, FEATHER_JS_MEM_MAX env override. HONEST NEGATIVE: the hidden-attribute stall is NOT memory-caused — clean 1GB boot still ends stuck-hidden with 19 renderers built; that defect is deterministic logic, memory was only crash/wedge variance. Rule for future memory work: caps tuned against lightweight sites silently break heavy SPAs — verify against YouTube before adopting memory ceilings.

## sec1zt9 — MEMORY PROGRAM CLOSEOUT: combined eviction+arena-cap measurement (2026-11-26)

Final end-to-end number, r/pics deep scroll 15×1500px = scrollY ~22500, identical protocol across all runs, signed-out feed with video posts:

| config | RSS at depth 22500 |
|---|---|
| baseline (pre-program, growing unbounded) | 7.4GB |
| eviction only (FEATHER_IMGMEM=1) | ~7.2GB |
| arena cap only (MALLOC_ARENA_MAX=2) | 4.2GB |
| **combined (feather shell default config)** | **1.46GB FLAT** |

**−5.9GB, 80% reduction, zero panics, zero playerleaks.** Settled flat (1458→1462→1463MB across minutes). Non-obvious interaction: combined is far better than either lever alone — eviction frees jemalloc image mass fast enough that less churn crosses into glibc arenas, and capped arenas can't retain what little gst churn remains. IMPORTANT nuance for all lanes: eviction-only measurements were nearly useless against RSS BECAUSE the freed masses were being re-captured by arena retention — the two levers are multiplicative, and any future memory fix must be measured with BOTH active or its effect will be invisible. This is the closeout number for the memory program; residual 1.46GB decomposes as bounded jemalloc live set (DOM/text/XHR/sourcebuffer categories documented in sec1zr-zs) + gst working set — the unaccounted-jemalloc question of sec1zt8 is largely ANSWERED by this measurement: most of the 'unaccounted' mass was arena retention after all.
## sec1zt9 — MEMORY PROGRAM CLOSEOUT: combined eviction+arena-cap measurement (2026-11-26)

Final end-to-end number, r/pics deep scroll 15×1500px = scrollY ~22500, identical protocol across all runs, signed-out feed with video posts:

| config | RSS at depth 22500 |
|---|---|
| baseline (pre-program, growing unbounded) | 7.4GB |
| eviction only (FEATHER_IMGMEM=1) | ~7.2GB |
| arena cap only (MALLOC_ARENA_MAX=2) | 4.2GB |
| **combined (feather shell default config)** | **1.46GB FLAT** |

**−5.9GB, 80% reduction, zero panics, zero playerleaks.** Settled flat (1458→1462→1463MB across minutes). Non-obvious interaction: combined is far better than either lever alone — eviction frees jemalloc image mass fast enough that less churn crosses into glibc arenas, and capped arenas can't retain what little gst churn remains. IMPORTANT nuance for all lanes: eviction-only measurements were nearly useless against RSS BECAUSE the freed masses were being re-captured by arena retention — the two levers are multiplicative, and any future memory fix must be measured with BOTH active or its effect will be invisible. This is the closeout number for the memory program; residual 1.46GB decomposes as bounded jemalloc live set (DOM/text/XHR/sourcebuffer categories documented in sec1zr-zs) + gst working set — the unaccounted-jemalloc question of sec1zt8 is largely ANSWERED by this measurement: most of the 'unaccounted' mass was arena retention after all.

## Devtools server wired into shell-new — diagnostic capability unlock (2026-11-26, macOS lane)

servo's Firefox-protocol devtools server is complete upstream (console/inspector/network/breakpoint actors) but pref-gated and unwired. shell-new now: `devtools` cargo feature + FEATHER_DEVTOOLS=<port> env + ServoDelegate writing token to /tmp/feather-devtools-token and auto-allowing connections. Verified end-to-end: listTabs correct, getTarget exposes ConsoleActor/inspector actors, evaluateJS returns live structured results.

Connection protocol notes: TCP length-prefixed JSON (`len:{json}`); server sends an unsolicited root traits message first — clients must skip it; TabDescriptorActor doesn't handle bare 'attach' — use 'getTarget' → BrowsingContextActor which carries consoleActor/inspector actor names; ConsoleActor evaluateJS works, startListeners returns unrecognizedPacketType (listener names or support incomplete). Client auth: either send {"auth_token":...} as first message or rely on embedder auto-allow.

Memory-investigation rule correction: js_mem_max caps tuned on lightweight sites silently break heavy SPAs — the 192MB cap OOM-ed YouTube boot (uncaught 'out of memory' + interface.rs define_properties unwrap panic killing the pipeline). Now 1GB default + FEATHER_JS_MEM_MAX override. Hidden-stall honest negative: NOT memory-caused — clean 1GB boot still stuck-hidden.

## YouTube replacement chain final form (2026-11-26, macOS lane, round 3 close)

Symptom: search results built but invisible → CONFIRMED MECHANISM: T1429 boot unhides SSR element correctly; T1751 entire ytd-search subtree REPLACED wholesale in post-response re-render, replacement arrives stamped hidden=true; no hidden op ever fires on replacement; crucially the replacement elements are UPGRADED BUT NEVER INITIALIZED (__data absent, dataReady undefined, isAttached=true) — the Polymer data-system init that would clear hidden via property binding never runs on them. Prime suspect direction: deferred/lazy init trigger (yt uses lazy-init heavily — IntersectionObserver-timing or lifecycle-event class). MutationObserver experiment design note: nested removals inside ancestor re-stamps do NOT appear in removedNodes — filter for ancestor-level adds containing the target, not direct node movement.

## CORRECTION — the "never initialized" youtube theory was a framework misread (2026-11-26)

ytd-masthead (fully working) ALSO lacks __data/dataReady — modern YouTube elements don't use the classic Polymer data system at all. The absence of __data on stuck elements is NORMAL, not evidence of failed initialization. Retracted as a mechanism theory. What stands: replacement arrives stamped hidden=true at T1751, no op ever clears it, manual driving of initVisibilityObserver_/flushRenderStamperComponentBindings_/initializeInjection changes nothing without throwing. Lesson: probe framework assumptions against WORKING elements on the same page before theorizing from missing internals.

## PerformanceObserver probe — mostly fine, warning is narrow (2026-11-26, macOS lane)

servo supportedEntryTypes: container, largest-contentful-paint, mark, measure, navigation, paint, resource, visibility-state. observe() accepts every type without throwing (even unsupported ones like longtask/layout-shift — no validation error). The youtube "No valid entry type provided to observe()" warning is therefore NOT a broad PerfObserver gap — likely a specific buffered/option combination; weak lead, deprioritized as boot-telemetry noise unless proven load-bearing.

## YouTube hidden hunt — property-setter probe results (2026-11-26, macOS lane)

Framework manipulates hidden via PROPERTY assignment (`el.hidden =`), not setAttribute — earlier attribute-op logging missed this layer entirely. Hidden-property probe (patched HTMLElement.prototype hidden setter, /tmp/feather-eval-persist.js v-hp) on a fresh search boot:

- ytd-search itself IS unhidden via property write at boot (`setActivePage`, T1457) — page-level logic works.
- Framework helper `qJC` (in ytmainappweb kevlar_base bundle) is the hide/unhide function: e.g. YTD-WATCH-METADATA gets hid=1 then hid=0 ~39ms later, BOTH through qJC — the unhide counterpart mechanism EXISTS and fires for other elements.
- YTD-TWO-COLUMN-SEARCH-RESULTS-RENDERER: hid=1 via qJC at T1469 — never followed by hid=0. The framework actively hides it (not just SSR-stamped) and the clearing qJC call never runs.
- Caveat: replacement-via-innerHTML stamping would bypass the property setter (hidden attribute in parsed markup), so replacement-time stamping is invisible to this probe; identity correlation with mutation log not yet done.
- NEXT LAYER: what calls qJC(hidden=false) for the results renderer in Chrome — get caller chain of qJC by capturing full stack (probe currently stores only frame 2; deepen to full stack) or find the completion signal qJC awaits (search-response processing completion event?). qJC is minified inside ytmainappweb kevlar_base bundle — grep-able in the fetched JS.

## sec1zt10 — YouTube OOM/define_properties theory VERIFIED + fixed (2026-11-26)

Verification of the macOS lane's unifying theory (memory-pressure boot corruption), on Linux:

**Experiment setup**: js_mem_max=32 induced-pressure harness (youtube.com watch boot × 6 per run). NOTE js_mem_max pref is capped at range 1..0x100 (256MB) in script_runtime — values above 256 are IGNORED silently.

**Findings**:
1. **The ceiling lever is a dead end**: servo already sets JSGC_MAX_BYTES=u32::MAX when js_mem_max unset — there is no ceiling to raise. The OOM is genuine allocation failure, not a configurable cap.
2. **Memory pressure reproduces the boot-corruption variance exactly as theorized**: under 32MB cap, boots alternate full/partial ('no-ytplayer' — empty title, incomplete boot) — the stuck-boot symptom reproduced on demand. This CONFIRMS pressure→corruption causal chain.
3. **Panic fixed at BOTH sites** (define_guarded_properties:434 AND define_guarded_methods:418 — same defect class): failure now clears the pending exception + logs `[interface]` warn + leaves interface partially defined. Page degrades, browser survives.
4. **Post-fix survival verified**: 64 uncaught out-of-memory exceptions across 6 boot attempts — all surfaced as page-level JS exceptions, zero panics, process alive throughout. The OOM-death behavior is now correct-by-construction at the interface-definition sites.
5. Caveat: the `[interface]` warning never fired in the induced-pressure runs — the interface-definition path apparently wasn't the failure point under this pressure pattern (OOMs hit script execution instead). The fix is verified by construction + general survival, not by direct observation of that path firing. If YouTube instability persists on memory-constrained devices after this, the next suspect is NON-interface OOM corruption of boot state (page-level, not engine-fixable beyond what browsers normally do).

**Bonus discovery this session**: YouTube is NOT currently bot-walled on the Linux VM — watch pages boot with correct titles and playback telemetry fires (api/stats/qoe POSTs). The wall documented earlier was intermittent or lifted. YouTube is testable on Linux again — update prior assumptions.
## sec1zt10 — YouTube OOM/define_properties theory VERIFIED + fixed (2026-11-26)

Verification of the macOS lane's unifying theory (memory-pressure boot corruption), on Linux:

**Experiment setup**: js_mem_max=32 induced-pressure harness (youtube.com watch boot × 6 per run). NOTE js_mem_max pref is capped at range 1..0x100 (256MB) in script_runtime — values above 256 are IGNORED silently.

**Findings**:
1. **The ceiling lever is a dead end**: servo already sets JSGC_MAX_BYTES=u32::MAX when js_mem_max unset — there is no ceiling to raise. The OOM is genuine allocation failure, not a configurable cap.
2. **Memory pressure reproduces the boot-corruption variance exactly as theorized**: under 32MB cap, boots alternate full/partial ('no-ytplayer' — empty title, incomplete boot) — the stuck-boot symptom reproduced on demand. This CONFIRMS pressure→corruption causal chain.
3. **Panic fixed at BOTH sites** (define_guarded_properties:434 AND define_guarded_methods:418 — same defect class): failure now clears the pending exception + logs `[interface]` warn + leaves interface partially defined. Page degrades, browser survives.
4. **Post-fix survival verified**: 64 uncaught out-of-memory exceptions across 6 boot attempts — all surfaced as page-level JS exceptions, zero panics, process alive throughout. The OOM-death behavior is now correct-by-construction at the interface-definition sites.
5. Caveat: the `[interface]` warning never fired in the induced-pressure runs — the interface-definition path apparently wasn't the failure point under this pressure pattern (OOMs hit script execution instead). The fix is verified by construction + general survival, not by direct observation of that path firing. If YouTube instability persists on memory-constrained devices after this, the next suspect is NON-interface OOM corruption of boot state (page-level, not engine-fixable beyond what browsers normally do).

**Bonus discovery this session**: YouTube is NOT currently bot-walled on the Linux VM — watch pages boot with correct titles and playback telemetry fires (api/stats/qoe POSTs). The wall documented earlier was intermittent or lifted. YouTube is testable on Linux again — update prior assumptions.

## RULED OUT: IntersectionObserver initial-callback hypothesis (2026-11-26, macOS lane)

Minimal repro (/tmp/vtest/iotest.html): servo IO fires the spec-required INITIAL callback correctly for visible (isIntersecting=true), display:none-hidden (false, ratio 0), AND offscreen (false, ratio 0) elements. YouTube's visibilityOptionHidden_ {calledOnce:false, state:hidden} is therefore NOT caused by missing initial IO callbacks — servo IO is spec-correct here. Also established same probe round: el.data ARRIVES on the results container (data-keys=primaryContents) — the data pipeline reaches the element; the failure is strictly downstream in the visibility/state-machine activation. Boot variance confirmed again: vrs=0 on some boots (no video renderers at all) — the stamping itself sometimes never runs. qJC = Polymer-legacy property-effects binding evaluator (ytmainappweb kevlar_base bundle, 9.8MB, fetched to /tmp/ytbase.js — grep-able reference copy).
Symptom: youtube search results never unhide → check: IntersectionObserver initial callback (status: RULED OUT — spec-correct incl. hidden/offscreen initial fire), element data delivery (status: RULED OUT — data arrives with primaryContents), visibility state machine activation trigger (status: PRIME SUSPECT — visOptHidden calledOnce:false, whatever flips it in Chrome never runs; NOT IO-initial-callback).

## YouTube hidden-stall — FINAL MECHANISM LAYER REACHED (2026-11-26, macOS lane)

The binding contract (from kevlar_base bundle source): `<ytd-two-column-search-results-renderer hidden="[[data.hideContents]]" data="[[data.contents.twoColumnSearchResultsRenderer]]">` inside ytd-search's template. hidden is PURELY data-bound.

Live probe on stuck page: ytd-search NOT hidden, data.contents present (full response), data.hideContents UNDEFINED (=falsy). Even a single binding evaluation with current data yields hidden=false. Yet elHiddenAttr=true persists. CONCLUSION: the hidden binding on the post-response replacement element NEVER evaluates — not a timing issue, a dead binding.

PRIME FINAL THEORY (untested, next experiment named): the replacement subtree is created through an innerHTML fallback path rather than Polymer templating — elements arrive upgraded but binding-inert (data arrives via direct property assignment, not binding propagation). In Chrome a FEATURE-DETECTION gate selects the template path (live bindings); servo presumably fails that check. Candidate gates in bundle: window.ShadyDOM detection, HTMLTemplateElement/CE capabilities, 'polymer_yticon_killswitch'-style flags. NEXT EXPERIMENT: grep /tmp/ytbase.js for the render-path feature gates (search: ShadyDOM, template binding instantiation, disable-upgrade) and identify which check servo fails. The scheduler theory is dead: detached-textnode MutationObserver sentinel pattern verified working in servo (motest.html: all 4 patterns FIRED incl. YouTube's exact one). IO innocent. Data pipeline innocent. Everything downstream of the dead binding is explained.

## sec1zt11 — YouTube invisible-results investigation, Linux round: mechanism narrowed to data-binding-never-populates (2026-11-26)

Boot-stability verification first: **8/8 real YouTube search boots succeed post define-panic fix — hidden attr cleared, zero crashes, zero OOMs** (normal memory conditions). The stuck-hidden boot variance documented on macOS does NOT reproduce on Linux at normal memory — consistent with the js_mem_max=192 cap being the macOS corruption source. YouTube is NOT bot-walled on Linux currently — full watch/search pages load, playback telemetry fires.

Mechanism hunt for the invisible-results defect (results in DOM but nothing paints — CONFIRMED still broken on Linux despite hidden being cleared on ytd-search):

**Live measurements** (r/pics-style probe harness on real youtube search): renderer innerHTML fully populated (20KB, real titles) BUT entire chain zero-height; root cause of collapse: `ytd-two-column-search-results-renderer` computed **display:none**; inside it `img src=none` — thumbnails never even got URLs — **the data-binding population never runs on the elements at all**. Text nodes paradoxically measure nonzero range rects inside the display:none subtree (likely a servo range-measurement bug for non-rendered content — minor, noted).

**Ruled out this round (each probe-verified, expensive — do not re-walk)**:
- scheduler.postTask: `window.scheduler` is UNDEFINED in servo BUT servo's UA is Firefox-flavored (`Firefox/153.0` — components/config/prefs.rs UserAgentPlatform) so YouTube serves the FIREFOX codepath, which cannot require postTask (real Firefox lacks it and works). Implementing postTask would be wasted work — coordinator note: NOT unguarded-required, condition for implementation not met.
- IntersectionObserver primitive: fires correctly initial+scroll (test harness in /tmp/mserepro/iotest.html).
- Custom element machinery: constructor+connectedCallback fire correctly for innerHTML insertion, appendChild, AND youtube-style wholesale replaceChildren (cetest.html) — all initialize.
- CE definitions: all registered on the live page (ytd-video-renderer, ytd-app, ytd-masthead all defined).
- JS boot errors: ZERO uncaught exceptions/typeerrors in boots — silent failure, not a thrown error.
- `__data` presence as init discriminator: EVEN THE MASTHEAD (which renders fine) lacks `__data` — it's a Polymer-2-ism modern YouTube doesn't use; macOS lane's init theory partially red-herring. The operative evidence of non-init is img src=none (data never populated), not __data.

**Live suspect chain**: youtubei response processing → framework data-sync into elements never happens for feed elements. The visibility flip (display none→flex on two-column container, hidden clearing) is data-binding-driven — since binding never populates, everything downstream stays collapsed. Next probe directions: (a) does the ytd-search element itself hold response data (.data or framework equiv) — isolates response-processing vs per-element binding; (b) what JSAPI/feature does the data-sync path need — candidates in missing-API class (per cfdcbc39 hydration-stall index): dynamic import()/chunk loading for the framework's lazy data module, or observable/atomic update primitives. NOTE the boot instance DOES get unhidden at T+1952 then replaced — meaning the FIRST element's data-binding DID populate — so response-processing works at least once; the failure is specific to post-replacement re-binding. That narrows it: replacement-time re-hydration trigger, not the data pipeline itself.
## sec1zt11 — YouTube invisible-results investigation, Linux round: mechanism narrowed to data-binding-never-populates (2026-11-26)

Boot-stability verification first: **8/8 real YouTube search boots succeed post define-panic fix — hidden attr cleared, zero crashes, zero OOMs** (normal memory conditions). The stuck-hidden boot variance documented on macOS does NOT reproduce on Linux at normal memory — consistent with the js_mem_max=192 cap being the macOS corruption source. YouTube is NOT bot-walled on Linux currently — full watch/search pages load, playback telemetry fires.

Mechanism hunt for the invisible-results defect (results in DOM but nothing paints — CONFIRMED still broken on Linux despite hidden being cleared on ytd-search):

**Live measurements** (r/pics-style probe harness on real youtube search): renderer innerHTML fully populated (20KB, real titles) BUT entire chain zero-height; root cause of collapse: `ytd-two-column-search-results-renderer` computed **display:none**; inside it `img src=none` — thumbnails never even got URLs — **the data-binding population never runs on the elements at all**. Text nodes paradoxically measure nonzero range rects inside the display:none subtree (likely a servo range-measurement bug for non-rendered content — minor, noted).

**Ruled out this round (each probe-verified, expensive — do not re-walk)**:
- scheduler.postTask: `window.scheduler` is UNDEFINED in servo BUT servo's UA is Firefox-flavored (`Firefox/153.0` — components/config/prefs.rs UserAgentPlatform) so YouTube serves the FIREFOX codepath, which cannot require postTask (real Firefox lacks it and works). Implementing postTask would be wasted work — coordinator note: NOT unguarded-required, condition for implementation not met.
- IntersectionObserver primitive: fires correctly initial+scroll (test harness in /tmp/mserepro/iotest.html).
- Custom element machinery: constructor+connectedCallback fire correctly for innerHTML insertion, appendChild, AND youtube-style wholesale replaceChildren (cetest.html) — all initialize.
- CE definitions: all registered on the live page (ytd-video-renderer, ytd-app, ytd-masthead all defined).
- JS boot errors: ZERO uncaught exceptions/typeerrors in boots — silent failure, not a thrown error.
- `__data` presence as init discriminator: EVEN THE MASTHEAD (which renders fine) lacks `__data` — it's a Polymer-2-ism modern YouTube doesn't use; macOS lane's init theory partially red-herring. The operative evidence of non-init is img src=none (data never populated), not __data.

**Live suspect chain**: youtubei response processing → framework data-sync into elements never happens for feed elements. The visibility flip (display none→flex on two-column container, hidden clearing) is data-binding-driven — since binding never populates, everything downstream stays collapsed. Next probe directions: (a) does the ytd-search element itself hold response data (.data or framework equiv) — isolates response-processing vs per-element binding; (b) what JSAPI/feature does the data-sync path need — candidates in missing-API class (per cfdcbc39 hydration-stall index): dynamic import()/chunk loading for the framework's lazy data module, or observable/atomic update primitives. NOTE the boot instance DOES get unhidden at T+1952 then replaced — meaning the FIRST element's data-binding DID populate — so response-processing works at least once; the failure is specific to post-replacement re-binding. That narrows it: replacement-time re-hydration trigger, not the data pipeline itself.

## sec1zt12 — YouTube invisible-results DEFINITIVE isolation: single-attribute defect (2026-11-26)

Decisive force-removal experiment on Linux, live youtube search page: `ytd-two-column-search-results-renderer` carries **hidden=true NEVER REMOVED** (an earlier probe measuring hidden:false measured the PARENT ytd-search — misleading; the hidden is on the two-column child). Force-removing hidden + display:block: renderer height 0→249px, img src POPULATES with real i.ytimg.com thumbnail URLs, content renders completely — replicating the macOS lane's force-removal-renders-everything on Linux.

**The entire invisible-results defect reduces to ONE attribute**: hidden never cleared on the replacement two-column element. Corrections to prior interpretations: (a) img src=none pre-unhide was LAZY-population-on-visibility, NOT missing data — data binding DID populate the replacement elements; (b) the init theories are dead — everything works except the final attribute flip. The open crux, precisely: data binds on the replacement yet hidden never clears — so hidden is NOT data-binding-driven on the replacement; it's a static SSR stamp whose removal logic fired exactly once on the BOOT element (T+1952ms per macOS) BEFORE hydration swapped in the replacement. In real browsers the ordering must differ (replacement completes before unhide fires, or unhide refires). This is a TIMING/ORDERING race between hydration replacement and the visibility-flip trigger — servo-specific ordering exposes it. Next investigation target: what mechanism fires the T+1952 unhide on the boot element — if it's a one-shot data-ready watcher keyed to element identity, the replacement never re-triggers it; the engine-level question is why hydration ordering differs in servo (custom-elements/reactions timing, or rAF/idle-callback scheduling order — both areas where servo's timing is approximate per sec notes on rIC). Workaround-class fix if root cause stays elusive: none generic — it's a youtube-framework-specific race.

Experiment artifacts: /tmp/ytsession holds a live webdriver session id pattern for reuse (single-session limit workaround — reuse don't recreate); force-flip probe script pattern in session history.

## YouTube hidden-stall — BINDINGS EXONERATED, final layer (2026-11-26, macOS lane)

DECISIVE LIVE PROOF: on the stuck page, `ytd-search.set('data.hideContents', true/false)` toggles the hidden attribute correctly BOTH directions — the Polymer binding system, propagation chain, and qJC evaluator are ALL fully functional in servo, including on the replacement element. Feature-gate/dead-binding theory DEAD.

Final cornered mechanism: the framework step that sets data.hideContents=false after response processing NEVER RUNS in servo. Supporting facts: data.contents present (response data arrived — wholesale data assignment happened), but hideContents is UNDEFINED (never managed). The defect is in post-response navigation-completion logic — the lifecycle step that manages loading visibility state — NOT in any primitive (scheduler/IO/MO/bindings all exonerated with live tests). Boot variance (vrs=0 vs 19) suggests the same completion pipeline partially runs race-dependent amounts.

ONE-LINE VERIFIED MITIGATION: ys.set('data.hideContents', false) unhides results live — usable as a shell-injected shim if a mitigation is ever wanted, but the correct fix is finding why the completion logic doesn't run (suspect: yt-navigate lifecycle handler timing or an exception-free early-exit in the response processor — needs breakpoint-level debugging via devtools server pause/exception hooks, or upstream reading of the navigation completion code path in the kevlar bundle).

SESSION HANDOFF NOTE: this session's context is exhausted at this exact point. Next session: read ytd-search's navigate/response handling in /tmp/ytbase.js (still on disk — 9.8MB bundle), find the hideContents write site, determine its trigger.

## ★ ROOT CAUSE SOLVED: rIC starvation — YouTube hidden-stall final answer (2026-11-26, macOS lane)

PRIMITIVE-LEVEL PROOF (localhost rictest.html + devtools on quiet page): during load frames tick and rIC fires at 0ms. On a SETTLED quiet page, a freshly registered requestIdleCallback does NOT fire even after 4s — servo's rIC implementation piggybacks on the animation-frame tick (we implemented it that way: IdleCallbackTick keepalive), and shell-new only produces frames on input/paint-invalidation. Quiet page → no rAF → rIC starves forever.

COMPLETE UNIFIED MECHANISM for YouTube (and likely the whole SPA class): YouTube/Polymer scheduler `raM` (kevlar_base bundle, ~line 1493596) = `window.requestIdleCallback ? rIC : setTimeout(v,16)` — uses rIC WHEN PRESENT. All post-response data-flush/stamper/visibility work rides on it. Chain: response arrives → flush jobs scheduled via rIC → page quiet (no animation/input) → servo never ticks frames → jobs never run → hideContents never cleared (binding `hidden="[[data.hideContents]]"` verified functional — manual set() toggles both ways), stamper never stamps (explains vrs=0 boot variance as timing races), visibility state machine never activates. Every prior ruled-out layer (IO, MO-sentinel, bindings, crypto, postTask) consistent — the primitives were fine; the SCHEDULING STARVATION was the defect. Irony: pre-rIC servo used the setTimeout fallback (reliable); implementing rIC naively made this WORSE — a trap for partial implementations.

THE FIX (scoped): servo's rIC must fire when the event loop is idle, INDEPENDENT of frame production — spec-shaped idle periods, not rAF piggyback. Implementation site: script_thread event-loop integration (fire idle callbacks after task queues drain / on a fallback timer when no frame tick occurs within ~16ms). Then re-verify: YouTube search results should unhide unassisted. ALSO explains: mouse-nudging sometimes advanced stuck states (input pumps frames), why boot variance is race-like, why hydration "advanced then stalled" patterns across SPAs (F8/F9/F10 class). Symptom index: results-stall/SPA-quiet-stall → check requestIdleCallback frame-piggyback (CONFIRMED CAUSE).

## rIC fix implementation starting point (2026-11-26 — next session read first)

Implementation sites identified: Document::request_idle_callback/run_the_idle_callbacks in components/script/dom/document/document.rs (~lines 2105-2185). Current design: idle callbacks only run inside the animation-frame tick; keepalive via request_animation_frame(AnimationFrameCallback::IdleCallbackTick) — starves when no frames produced.

KEY DISCOVERY: script_thread ALREADY has frame-independent timer infrastructure — `schedule_update_the_rendering_timer_if_necessary(delay)` (components/script/event_loop/script_thread.rs ~line 1088) using TimerEventRequest with a native Box closure callback that sets needs_rendering_update atomic. This proves: native closures CAN be scheduled as timer callbacks without JS handles. THE FIX: when idle callbacks are pending in any document, the script thread should schedule a ~4-16ms timer whose callback forces an idle-callback flush (either by setting needs_rendering_update to force a tick, or directly invoking document.run_the_idle_callbacks for each document with pending callbacks). Simplest correct-shaped version: in request_idle_callback, in addition to the rAF keepalive, post a ScriptThread timer via the window's script-thread access that re-checks/flushes after 16ms; chain while callbacks pending. Everything else (the run logic, deadline semantics) already works — proven correct when ticks occur.

## rIC starvation fix — LANDED + verified primitive, partial YouTube progress (2026-11-26)

Commit 933f453ad on feather-engine: request_idle_callback/run_the_idle_callbacks now poke script_thread.schedule_update_the_rendering_timer_if_necessary(16ms) — frame-independent tick. VERIFIED: late-registered rIC on quiet localhost page fires within 4s (was: never).

YOUTUBE PROGRESS: stamping now completes reliably — vrs=19 with 150 thumbnail imgs consistently (was race-dependent vrs=0/19). BUT hidden persists: data.hideContents still UNDEFINED — the completion write never happens. NEW KEY INSIGHT: Polymer bindings don't propagate undefined — hidden binding never fires without an explicit write; Chrome must explicitly set hideContents (true at SSR/initial then false at completion). The remaining missing piece is the write itself, in navigation-completion logic — not scheduling. NOTE: this page ran entirely on the fixed engine, so completion logic runs but doesn't write hideContents — next suspect: an exception-free early exit or missing API inside the completion handler (devtools unhandledrejection still unimplemented — that's the visibility tool needed).

## sec1zt13 — unhide timing evidence: permanent deterministic failure, UA-independent (2026-11-26)

Aging experiment: youtube search boot sampled at T+5/20/50/80s — hidden stays stamped on ytd-two-column-search-results-renderer the ENTIRE time, renderer height 0 throughout. **Not a late-timing issue — the unhide never fires at all in servo, deterministically.** Also: instance-identity polling (T+5.7→22s) shows a SINGLE persistent element instance — no replacement observed in that window, meaning either replacement completes before T+5.7s or doesn't happen; the two-phase race story from macOS couldn't be confirmed in servo timing. Simpler theory consistent with evidence: the unhide mechanism never runs in servo at all (rather than fires-then-gets-discarded). Chrome-UA spoof test: user_agent=Chrome/131 changes NOTHING (still hidden, still zero-height) — defect is UA-independent, shared framework path, and NOT a UA-gating issue. Open crux unchanged but sharpened: WHAT fires the hidden removal in real browsers — candidates narrowed: rIC-scheduled hydration commit (servo's rIC v1 fires callbacks but with non-spec ordering — IdleCallbackTick pumps at rAF rate continuously, so ordering vs rAF/microtasks is wrong; if youtube's commit is ordered relative to rAF/paint in a way servo violates, the commit could run BEFORE the dependent state exists and no-op silently), or custom-elements reaction timing (though CE primitives probe-verified). Cheapest next experiment: rIC-ordering bisect — locally revert/disable rIC (elements then can't schedule the commit at all — if behavior IDENTICAL with and without rIC, the commit either doesn't use rIC or the v1 ordering is irrelevant; if youtube's commit is rIC-dependent and servo ordering breaks it, disabling rIC should change SOMETHING observable).
## sec1zt13 — unhide timing evidence: permanent deterministic failure, UA-independent (2026-11-26)

Aging experiment: youtube search boot sampled at T+5/20/50/80s — hidden stays stamped on ytd-two-column-search-results-renderer the ENTIRE time, renderer height 0 throughout. **Not a late-timing issue — the unhide never fires at all in servo, deterministically.** Also: instance-identity polling (T+5.7→22s) shows a SINGLE persistent element instance — no replacement observed in that window, meaning either replacement completes before T+5.7s or doesn't happen; the two-phase race story from macOS couldn't be confirmed in servo timing. Simpler theory consistent with evidence: the unhide mechanism never runs in servo at all (rather than fires-then-gets-discarded). Chrome-UA spoof test: user_agent=Chrome/131 changes NOTHING (still hidden, still zero-height) — defect is UA-independent, shared framework path, and NOT a UA-gating issue. Open crux unchanged but sharpened: WHAT fires the hidden removal in real browsers — candidates narrowed: rIC-scheduled hydration commit (servo's rIC v1 fires callbacks but with non-spec ordering — IdleCallbackTick pumps at rAF rate continuously, so ordering vs rAF/microtasks is wrong; if youtube's commit is ordered relative to rAF/paint in a way servo violates, the commit could run BEFORE the dependent state exists and no-op silently), or custom-elements reaction timing (though CE primitives probe-verified). Cheapest next experiment: rIC-ordering bisect — locally revert/disable rIC (elements then can't schedule the commit at all — if behavior IDENTICAL with and without rIC, the commit either doesn't use rIC or the v1 ordering is irrelevant; if youtube's commit is rIC-dependent and servo ordering breaks it, disabling rIC should change SOMETHING observable).

## sec1zt14 — vp9 abort hunt FINAL state + real-YouTube dependency documented (2026-11-26)

Sixth reproduction attempt: extended vp9 seek-churn loop (random seek every 800ms, 171 seeks/2.5min, live decode readyState 4 throughout) — zero panics, process alive. **The vp9 abort is non-reproducible in every harness available on Linux.** Real-YouTube hunting is blocked by a hard dependency: the YouTube player never bootstraps in servo — video element present but src never set, ZERO videoplayback network requests, networkState 0 — the player init is part of the incomplete-boot/visibility defect family (same root class as the hidden-attribute stall). DEPENDENCY CHAIN documented for prioritization: fixing the boot/visibility defect unblocks BOTH the #1-site usability AND the vp9 crash hunt. The vp9 abort mission is closed-exhausted on this lane: negative matrix (5 synthetic harnesses) + extended seek-churn + real-YouTube-blocked-by-boot all documented. Symptom-index final entry: Symptom 'gst vp9 threads crash / abort on youtube' → check: seek-ack teardown race (CONFIRMED FIXED — was real, verified eliminated), playerleak intentional-leak sites (documented, non-crashing), gst decode threads generally (RULED OUT in 6 harnesses), YouTube player bootstrap (blocked upstream by boot-visibility defect — retest there once boot fixed).

## sec1zt15 — rIC starvation FIXED + the cause-verdict correction: insufficient for YouTube visibility (2026-11-26)

The rIC starvation fix is implemented and landed (commit 6744dae2875): pending idle callbacks count as a rendering-update need in Document::needs_rendering_update, making the script-thread timer machinery self-sustaining on quiet pages. Verified mechanically: quiet-page rictest fires immediately post-settle; rIC registered on live youtube fires within 4s. The fix is spec-closer and correct regardless — SPA quiet-stall class improved.

**CRITICAL CORRECTION to the macOS lane's confirmed-cause verdict**: with rIC firing correctly, YouTube search STILL stays hidden (tcHidden:true, display:none, renderers zero-height at T+22s post-fix). Starvation was real but NOT the sufficient cause — the visibility commit either never gets scheduled, or runs and fails silently for another reason. What remains verified-working: the binding itself (ys.set('data.hideContents', false) unhides live per macOS; force-removeAttribute + display:block renders everything incl. thumbnails per sec1zt12). Sharpened open question: the write of hideContents=false (or equivalent visibility-flip data write) never happens — the failure is UPSTREAM of the binding, in the navigation-completion/response-processing logic. Next investigation is framework-level: fetch the kevlar bundle (youtube's ytbase.js), find the hideContents write site, determine its trigger conditions, and probe which one fails in servo. NOTE for that work: webdriver can't inject pre-load — analysis must be static (read the bundle) or use servo devtools protocol. ALSO NOTE: merge-artifact duplication existed in this file from conflict union-merges (cleaned marker lines 2026-11-26; duplicated sec1zt11 body text remains harmless).

## sec1zt16 — YouTube scheduler fully decoded + scheduling ruled out, failure is upstream (2026-11-26)

LIVE PROBE of the scheduler instance on the broken page (ytglobal.schedulerInstanceInstance_): all job queues EMPTY (incl. priority-8), nothing in flight, mode=3 rAF, L=true (using rIC), u=true (rAF available). **Scheduling is definitively NOT the defect — either the visibility-flip job is never scheduled, or it runs and fails silently.** This closes the rIC/scheduling investigation direction permanently.

BUNDLE ANALYSIS (scheduler.js — only 8KB, readable, fetched at /tmp/ytscheduler.js — note /tmp files are ephemeral, refetch if gone: youtube.com/s/desktop/HASH/jsbin/scheduler.vflset/scheduler.js): yt scheduler priorities 0-5+8; mode decision M(): priority-8 jobs with rAF available and page visible run on **requestAnimationFrame** (mode 3), low-priority (1) jobs use rIC {timeout:3000}, mode 2/4/5 use setTimeout variants. Feature-detect: uses rIC only if window.requestIdleCallback exists — meaning pre-rIC servo used reliable setTimeout fallback; implementing rIC naively made things worse (macOS irony confirmed in code).

UNHIDE MECHANISM FOUND (kevlar bundle /tmp/ytbase.js 9.8MB — search 'hideContents'): for ytd-rich-grid-renderer (home; search equivalent follows same pattern): onPageChange schedules a **priority-8 job** whose body runs setMasthead/maybeSetBigYoodle/forceRefreshAndLog/terminateVideoPreview in a try, with `hideContentsForNavigation=false` in the **finally** — the unhide is finally-guarded so even a throwing job body still unhides. Binding: `hidden="[[computeHideContents(hasLoadingContent, hideContentsForNavigation)]]"`. Consequence: since the finally guarantees the flip IF the job runs, and queues are empty (jobs ran or never scheduled), the live suspects narrow to exactly two: (a) onPageChange never fires or isCurrentPage() false — job never scheduled; (b) W-flag logic — if the job runs synchronously-ish via scheduler quirks, onPageChangeJobId handling differs. The single most instrumentable next probe: whether the framework's navigate/response events fire AT ALL on the search page in servo post-fix — the yt-navigate-finish event and whether a data response object reaches the element. NOTE the response itself arrives (200 verified repeatedly) — it's the framework-internal dispatch that's suspect.
||||||| parent of e13779d9a (memory: rIC fix verified — stamping completes, hideContents write is the last layer)

## Temporal proof: hideContents write NEVER happens (11:40 2026-11-26)

Property-write instrumentation on ytd-search.data.hideContents across 40s of settled post-response state: zero writes ever (hcw=[] at T25s and T40s). Not a timing issue — the completion logic that should flip hideContents is either never invoked or exits before the write. Also: eval-console-instruments branch built (BUILDEXIT:0) — shell-new now has FEATHER_EVAL live bridge + console visibility available for next session. Next concrete step for the continuation: implement servo's unhandledrejection event (the missing visibility primitive) OR instrument yt-navigate-finish handlers in the kevlar bundle to find where the search-response completion path diverges. /tmp/ytbase.js still on disk.
||||||| parent of 8af493c268c (memory: sec1zt17 — data bound, flip never writes, cancel-race suspect)

## sec1zt17 — final narrowing: data FULLY BOUND, disconnect is data-arrival → attribute-flip (2026-11-26)

Live probe on stuck search page: `ytd-search` element `.data` is FULLY POPULATED with the real response (responseContext, estimatedResults, contents, trackingParams, header, topbar, onResponseReceivedCommands, targetId) — response processing works end-to-end, binding system alive (ytd-search carries __dataReady/__dataEnabled/__dataPending family — so the Polymer-ish data system IS in use on search elements, partially retracting the modern-framework correction: masthead lacks it but search has it). rIC regression check: CPU cost of the starvation fix negligible (3 ticks/5s on settled page — loop drains correctly, no spin). YET hidden persists. **The complete evidence chain: response arrives (200) → data binds to element (verified populated) → scheduler queues drain (all empty) → but the hidden-flip write never executes.** Remaining suspect space is now tiny: the element-internal logic that translates data-arrival into clearing hidden — either (a) a priority-8 commit job that gets CANCELED by a subsequent navigation/page-change (resetNavigationState cancels onPageChangeJobId — servo-specific double-navigation timing could hit this; queues empty is consistent with cancellation), or (b) the computeHideContents-equivalent binding for search never re-evaluates because the property-effects cascade breaks silently. Probe next: whether a second yt-navigate-start fires AFTER the data-bearing one in servo (double-navigate cancel race — the precise servo-ordering theory). Instrument via webdriver-injected listener post-load then trigger in-page search refinement navigation.
## sec1zt17 — final narrowing: data FULLY BOUND, disconnect is data-arrival → attribute-flip (2026-11-26)

Live probe on stuck search page: `ytd-search` element `.data` is FULLY POPULATED with the real response (responseContext, estimatedResults, contents, trackingParams, header, topbar, onResponseReceivedCommands, targetId) — response processing works end-to-end, binding system alive (ytd-search carries __dataReady/__dataEnabled/__dataPending family — so the Polymer-ish data system IS in use on search elements, partially retracting the modern-framework correction: masthead lacks it but search has it). rIC regression check: CPU cost of the starvation fix negligible (3 ticks/5s on settled page — loop drains correctly, no spin). YET hidden persists. **The complete evidence chain: response arrives (200) → data binds to element (verified populated) → scheduler queues drain (all empty) → but the hidden-flip write never executes.** Remaining suspect space is now tiny: the element-internal logic that translates data-arrival into clearing hidden — either (a) a priority-8 commit job that gets CANCELED by a subsequent navigation/page-change (resetNavigationState cancels onPageChangeJobId — servo-specific double-navigation timing could hit this; queues empty is consistent with cancellation), or (b) the computeHideContents-equivalent binding for search never re-evaluates because the property-effects cascade breaks silently. Probe next: whether a second yt-navigate-start fires AFTER the data-bearing one in servo (double-navigate cancel race — the precise servo-ordering theory). Instrument via webdriver-injected listener post-load then trigger in-page search refinement navigation.

## sec1zt18 — investigation halt state + tooling unlock needed (2026-11-26)

Additional negative data: injected yt-navigate listeners catch NOTHING even when clicking filter chips — and the chip click doesn't even navigate (url unchanged). Whether that's because the page is already wedged or in-page navs don't fire these events in servo is undetermined. THE INVESTIGATION IS NOW TOOLING-BLOCKED: every remaining question requires observing the boot sequence from T+0 (pre-load JS injection), which webdriver fundamentally cannot do. THE UNLOCK for the next session: either (a) servo's devtools protocol — check if it supports Debugger/Runtime instrumentation attachable before navigation completes (servo has devtools feature in build), or (b) mitm proxy (mitmproxy with CA cert installed in servo profile — servo accepts custom CA? unverified) to inject instrumentation into served HTML, or (c) build a servo variant with a pref-gated pre-load user-script injection (small engine feature, ~1-2h, generally useful for all future framework debugging — RECOMMENDED). Summary of the narrowed state: response arrives, data binds, scheduler drains, binding works when manually driven — but the hidden-flip write never executes and the evidence needed to distinguish cancel-race vs cascade-break is inside the unobservable boot window.
||||||| parent of 05a1b9907 (memory: temporal proof — hideContents never written; unhandledrejection is the needed next tool)

## hideContents final chain — command never dispatched (2026-11-26 late)

FULL CONTRACT: ytd-search actionMap registers "yt-show-search-contents-command"→onYtShowSearchContentsCommand→set("data.hideContents",!1). The command string appears NOWHERE else in kevlar_base — not server-sent either: live scan of ys.data shows onResponseReceivedCommands contains ONLY adsControlFlowOpportunityReceivedCommand, and no "showSearchContents" string anywhere in the data. Therefore the command is fired LOCALLY by framework completion logic (app-shell/navigator once content ready) — possibly in one of the 12 other JS chunks the page loads (only base bundle analyzed). The trigger itself never fires in servo — suspect chain: rendering-completion hook that should dispatch it. NEXT: grep the other page chunks for "show-search-contents" or the dynamic dispatch site; alternatively instrument the live dispatch pipeline. IO fully exonerated incl. display:none-ancestor nesting (io2.html — initial false + transitions all correct).
||||||| parent of 7cd7d0d5673 (memory: sec1zt19 — devtools unlock, upgrade myth busted, IO-lazy-init prime suspect)

## sec1zt19 — devtools unlock + near-complete root cause: IO-lazy-init suspect, upgrade myth busted (2026-11-26)

**TOOLING UNLOCKED**: servoshell --devtools=9222 works out of the box (no shell-new build needed — shell-new build skipped deliberately, saving hours). Firefox devtools protocol over RAW TCP (not websocket — connection resets if ws attempted). Client quirks: length-prefixed packets "len:json"; read root hello first; use listTabs → getTarget on tab actor (NOT attach — both tab.attach and BrowsingContext.attach return unrecognizedPacketType); getTarget response frame object contains consoleActor; evaluateJS replies arrive wrapped in {"Ok": {...}} — match by input text; evaluateJS does NOT await promises (write results to window.__x and poll); innerHTML assignments in eval throw TrustedTypes errors on youtube (page enforces TT). Servo actor support: console evaluateJS/evaluateJSAsync yes, listFrames/listWorkers stubs.

**INVESTIGATION RESULTS (all live-probed on stuck youtube search page)**:
1. IntersectionObserver primitive WORKS — test observer on fresh fixed-position element fired immediately (isIntersecting:true). Simple case fine.
2. **Upgrade myth BUSTED**: stuck ytd-two-column-search-results-renderer IS properly upgraded — Object.getPrototypeOf(tc) === customElements.get(...).prototype TRUE. The constructor-identity test (tc.constructor !== def) is a RED HERRING: even freshly createElement'd tc elements show the same pattern (proto correct, constructor resolves to anonymous empty-name class). Definitive upgrade test is PROTO identity, not constructor identity — constructor resolution is weird for these minified classes even in correct operation. Custom element machinery is NOT the defect.
3. connectedCallback pattern: ytd-video-renderer (works) has NO cc; ytd-two-column (stuck) HAS cc; containers ytd-search/ytd-app have cc+ac+rich lifecycle. cc-absence is normal for leaf renderers — YouTube initialization is NOT purely CE-reaction-driven.
4. **KEY FIND: ytd-search prototype has initVisibilityObserver_, registerRenderer, unregisterRenderer_, isRenderer_** — YouTube lazy-initializes renderers via a VisibilityObserver (IntersectionObserver-based). This is the lazy-init path the replacement-chain doc hypothesized. Prime suspect: IO behavior in the specific configuration YouTube uses (elements inside display:none subtrees / zero-size / specific rootMargin) differs servo vs Chrome, OR IO callbacks don't fire correctly for elements that START hidden. NOTE spec subtlety: display:none elements have no boxes → never intersect — if init depends on IO firing while hidden then unhide depends on init, chicken-egg — but Chrome resolves it somehow (perhaps IO fires when visibility flips, or init uses different trigger).
5. Fresh-inserted tc element acquired children:2 spontaneously — some framework machinery (likely MutationObserver-driven stamping) DOES act on inserted elements, but data-init (__data) still never populates.
6. Negative: monkeypatching prototype.connectedCallback post-registration does nothing (spec — reactions use registry-captured callbacks) — don't bother with that instrumentation technique.

**NEXT DECISIVE EXPERIMENT (documented for continuation)**: install IO spy wrapper (pattern in session — wraps window.IntersectionObserver, logs created/callbacks/observes counts) then trigger a TRUE SPA navigation — NOT form.submit (causes full navigation, wipes spy — negative result recorded) — use YouTube internal router: click a visible anchor/link or use the masthead logo SPA behavior. Compare IO fire behavior servo vs expectation during the replacement render. Alternative cheaper static route: fetch youtube's kevlar/kevlarlib bundle and read initVisibilityObserver_ implementation — determine exactly what trigger completes renderer init and what attribute-write clears hidden (the write site question from sec1zt15 is still technically open — the write may BE inside the lazy-init completion).

**Correction to earlier narrowing**: the question is no longer "why doesn't the flip write execute" in the abstract — it is specifically "why doesn't YouTube's visibility-observer-based lazy init complete on the replacement tree". Data binding works (ytd-search.data fully populated), upgrade works, CE reactions work, IO primitive works, scheduler drains. The defect is in the interaction between IO-driven lazy init and servo's IO implementation details OR in whatever event/step YouTube uses to begin that init.
## sec1zt19 — devtools unlock + near-complete root cause: IO-lazy-init suspect, upgrade myth busted (2026-11-26)

**TOOLING UNLOCKED**: servoshell --devtools=9222 works out of the box (no shell-new build needed — shell-new build skipped deliberately, saving hours). Firefox devtools protocol over RAW TCP (not websocket — connection resets if ws attempted). Client quirks: length-prefixed packets "len:json"; read root hello first; use listTabs → getTarget on tab actor (NOT attach — both tab.attach and BrowsingContext.attach return unrecognizedPacketType); getTarget response frame object contains consoleActor; evaluateJS replies arrive wrapped in {"Ok": {...}} — match by input text; evaluateJS does NOT await promises (write results to window.__x and poll); innerHTML assignments in eval throw TrustedTypes errors on youtube (page enforces TT). Servo actor support: console evaluateJS/evaluateJSAsync yes, listFrames/listWorkers stubs.

**INVESTIGATION RESULTS (all live-probed on stuck youtube search page)**:
1. IntersectionObserver primitive WORKS — test observer on fresh fixed-position element fired immediately (isIntersecting:true). Simple case fine.
2. **Upgrade myth BUSTED**: stuck ytd-two-column-search-results-renderer IS properly upgraded — Object.getPrototypeOf(tc) === customElements.get(...).prototype TRUE. The constructor-identity test (tc.constructor !== def) is a RED HERRING: even freshly createElement'd tc elements show the same pattern (proto correct, constructor resolves to anonymous empty-name class). Definitive upgrade test is PROTO identity, not constructor identity — constructor resolution is weird for these minified classes even in correct operation. Custom element machinery is NOT the defect.
3. connectedCallback pattern: ytd-video-renderer (works) has NO cc; ytd-two-column (stuck) HAS cc; containers ytd-search/ytd-app have cc+ac+rich lifecycle. cc-absence is normal for leaf renderers — YouTube initialization is NOT purely CE-reaction-driven.
4. **KEY FIND: ytd-search prototype has initVisibilityObserver_, registerRenderer, unregisterRenderer_, isRenderer_** — YouTube lazy-initializes renderers via a VisibilityObserver (IntersectionObserver-based). This is the lazy-init path the replacement-chain doc hypothesized. Prime suspect: IO behavior in the specific configuration YouTube uses (elements inside display:none subtrees / zero-size / specific rootMargin) differs servo vs Chrome, OR IO callbacks don't fire correctly for elements that START hidden. NOTE spec subtlety: display:none elements have no boxes → never intersect — if init depends on IO firing while hidden then unhide depends on init, chicken-egg — but Chrome resolves it somehow (perhaps IO fires when visibility flips, or init uses different trigger).
5. Fresh-inserted tc element acquired children:2 spontaneously — some framework machinery (likely MutationObserver-driven stamping) DOES act on inserted elements, but data-init (__data) still never populates.
6. Negative: monkeypatching prototype.connectedCallback post-registration does nothing (spec — reactions use registry-captured callbacks) — don't bother with that instrumentation technique.

**NEXT DECISIVE EXPERIMENT (documented for continuation)**: install IO spy wrapper (pattern in session — wraps window.IntersectionObserver, logs created/callbacks/observes counts) then trigger a TRUE SPA navigation — NOT form.submit (causes full navigation, wipes spy — negative result recorded) — use YouTube internal router: click a visible anchor/link or use the masthead logo SPA behavior. Compare IO fire behavior servo vs expectation during the replacement render. Alternative cheaper static route: fetch youtube's kevlar/kevlarlib bundle and read initVisibilityObserver_ implementation — determine exactly what trigger completes renderer init and what attribute-write clears hidden (the write site question from sec1zt15 is still technically open — the write may BE inside the lazy-init completion).

**Correction to earlier narrowing**: the question is no longer "why doesn't the flip write execute" in the abstract — it is specifically "why doesn't YouTube's visibility-observer-based lazy init complete on the replacement tree". Data binding works (ytd-search.data fully populated), upgrade works, CE reactions work, IO primitive works, scheduler drains. The defect is in the interaction between IO-driven lazy init and servo's IO implementation details OR in whatever event/step YouTube uses to begin that init.

## sec1zt20 — IO spec-compliance verified; investigation consolidated for continuation (2026-11-26)

**IntersectionObserver spec matrix (clean measurement after restoring native IO — initial test was contaminated by dead spy wrapper, negative result invalid)**: belowfold rendered element: initial callback fires isIntersecting:false CORRECT; display:none element: initial callback fires isIntersecting:false CORRECT (spec-conformant — display:none elements get initial false callback); visible: fires true CORRECT. **IO initial-callback behavior is fully spec-compliant in servo. IO-lazy-init theory significantly weakened** — if YouTube init waited for initial IO fire it would fire. REMAINING IO question (untested): transition firing when elements inside display:none subtrees become visible — irrelevant though since hidden never clears in the first place.

**Tooling warnings for continuation**: (1) monkeypatching window.IntersectionObserver PERSISTS across same-window SPA navigations and poisons all subsequent IO construction if the state object is wiped — if instrumenting IO, make the wrapper robust to missing state. (2) form.submit() on youtube search causes FULL navigation (wipes instrumentation) — use link clicks for SPA nav. (3) restore native IO from iframe.contentWindow.IntersectionObserver when poisoned.

**Static bundle findings**: ytd-two-column-search-results-renderer class registered with disableElementRegistration:!0, template is trivial (`<div id="primary">` + secondary container with `disable-upgrade$` binding) — the element is a thin shell; hidden-removal write site NOT found in class itself — it's controlled externally (parent template binding or search-lifecycle logic). The write site question remains technically open but the failure is upstream regardless: whatever framework step performs the write never runs.

**INVESTIGATION STATE SUMMARY for continuation** — ruled out with live-verified evidence: CE upgrade machinery (proto-verified correct), IO primitive (spec-compliant), constructor identity (red herring), rIC starvation (fixed, insufficient), scheduler (drains fine), data pipeline (fully bound), response processing (200 + populated). The defect: the framework step that unhides/finalizes the replacement tree never executes. Next most-promising probes in priority order: (1) IO-spy + TRUE SPA nav capturing observer creation/callback pattern during a fresh replacement render (in-progress instrumentation pattern documented in session); (2) compare behavior of the SAME page in servo vs chrome at the framework level: does YouTube in servo even ATTEMPT the unhide (spy on Element.prototype.removeAttribute/attribute writing for 'hidden') — attribute-write spy is EASY to install robustly and would definitively split never-attempted vs attempted-and-failed; (3) static: find the parent-template binding controlling hidden in ytd-search or app-level templates.

## sec1zt21 — DEFINITIVE isolation: exactly ONE write missing, all downstream machinery verified working (2026-11-26)

Force-unhide cascade experiment (live devtools on stuck youtube search): removeAttribute('hidden') on ytd-two-column-search-results-renderer → IO observer on video renderer FIRED (iofires:1), 108 framework mutations cascaded, tc rendered to 7288px height, video renderers populated data (vr.data true AFTER unhide — lazy init was GATED ON VISIBILITY), heights 249px, thumbnails loading, nothing re-hides it. **The entire defect is exactly one missing write: hidden-removal on the replacement element. Everything downstream — IO cascade, visibility-gated lazy data init, rendering, image loading — works perfectly in servo.** This inverts the earlier suspicion: lazy init is not broken; it was correctly waiting for visibility which never came. The missing write is in YouTube's response-completion path (external to the element — class is a thin shell). The investigation question is now fully precise: in Chrome, what code performs the hidden-removal on the post-response replacement element, and why doesn't that codepath execute in servo? Cheap decisive continuation: Element.prototype.removeAttribute + attribute-setter spy on 'hidden' installed BEFORE a true SPA nav (link click, not form submit) will capture whether servo-YouTube even attempts the write vs Chrome attempting it.
||||||| parent of b2eea5eab (memory: yt-show-search-contents-command never dispatched; not in response data — local completion trigger missing)

## ENDGAME STATE: YouTube fully renders — only the command trigger missing (2026-11-26 final)

Direct call ys.onYtShowSearchContentsCommand() → hidden=false, hc=false, layout height 7901px, FULL VISUAL RENDER: thumbnails/titles/chapters/badges all correct (screenshot proof). Raw CustomEvent dispatch does NOT trigger the handler (YouTube actions route through the central command-executor service, not raw DOM events). THE ENTIRE downstream pipeline is functional in servo — the single missing piece is whatever invokes the command through the executor in Chrome. Alpha-shim option now trivially viable: inject the direct handler call post-response. Trigger hunt continuation: find which response-processing/navigation step submits the command to the executor (search the executor service in chunks — command resolution is dynamic, no static string exists; likely the executor resolves a command object constructed by completion logic or embedded in a part of response processing that silently fails in servo — the unhandledrejection tool remains the key visibility need).
||||||| parent of e6a2db59b14 (memory: sec1zt22 — home unhide works, search-specific defect + eval-wedge finding)

## sec1zt22 — CRITICAL discriminator: unhide mechanism WORKS on home page, fails only on search; plus eval-wedge defect (2026-11-26)

**MAJOR narrowing**: SPA-navigated-to-home page renders PERFECTLY in servo — ytd-rich-grid-renderer hidden:false, height 6965px, data bound, 30 rich-item renderers. The hideContents/unhide mechanism is NOT broken in general — it fires correctly for home. The defect is specific to the SEARCH results path (or the initial-load path — discriminator incomplete, see below). Also note: the home page in question was reached via SPA navigation from the wedged search page — SPA nav itself works (logo click), so in-page navigation machinery is functional.

**New defect surfaced — eval-wedge on fresh loads**: after location.href full navigation to youtube home, devtools evaluateJS persistently throws 'Location relevant Document is not same origin-domain with entry settings object' for 30+s across fresh devtools connections — servo document/script state wedges after full navigation in a way it does not after SPA nav. Transient version of this error is normal during navigation, but persistent-for-30s+ suggests either navigation never completing or a script-thread wedge. UNVERIFIED whether page renders visually during the wedge (no screenshot taken). This wedge also blocked the fresh-load-vs-SPA discriminator experiment. **This is itself a serious servo defect candidate affecting all full navigations — possibly the SAME mechanism that breaks initial-load search unhide**: if the initial-load document state is wedged in some subtle way (not fully wedged — most JS runs — but some spec check failing), framework codepaths touching affected state would throw silently. Direction for continuation: reproduce eval-wedge minimally (any full navigation to a simple page?) and find the spec check in servo script/dom — grep for 'same origin-domain' or the spec step in location/document access.

**Instrument survival negative**: attribute-write spy on Element.prototype does NOT survive full navigation (obviously) and the SPA-nav spy capture was pre-empted by the wipe — but the spy install methodology is sound for future SPA-only captures: wrap removeAttribute/setAttribute/hidden-property-setter BEFORE clicking in-page links.
## sec1zt22 — CRITICAL discriminator: unhide mechanism WORKS on home page, fails only on search; plus eval-wedge defect (2026-11-26)

**MAJOR narrowing**: SPA-navigated-to-home page renders PERFECTLY in servo — ytd-rich-grid-renderer hidden:false, height 6965px, data bound, 30 rich-item renderers. The hideContents/unhide mechanism is NOT broken in general — it fires correctly for home. The defect is specific to the SEARCH results path (or the initial-load path — discriminator incomplete, see below). Also note: the home page in question was reached via SPA navigation from the wedged search page — SPA nav itself works (logo click), so in-page navigation machinery is functional.

**New defect surfaced — eval-wedge on fresh loads**: after location.href full navigation to youtube home, devtools evaluateJS persistently throws 'Location relevant Document is not same origin-domain with entry settings object' for 30+s across fresh devtools connections — servo document/script state wedges after full navigation in a way it does not after SPA nav. Transient version of this error is normal during navigation, but persistent-for-30s+ suggests either navigation never completing or a script-thread wedge. UNVERIFIED whether page renders visually during the wedge (no screenshot taken). This wedge also blocked the fresh-load-vs-SPA discriminator experiment. **This is itself a serious servo defect candidate affecting all full navigations — possibly the SAME mechanism that breaks initial-load search unhide**: if the initial-load document state is wedged in some subtle way (not fully wedged — most JS runs — but some spec check failing), framework codepaths touching affected state would throw silently. Direction for continuation: reproduce eval-wedge minimally (any full navigation to a simple page?) and find the spec check in servo script/dom — grep for 'same origin-domain' or the spec step in location/document access.

**Instrument survival negative**: attribute-write spy on Element.prototype does NOT survive full navigation (obviously) and the SPA-nav spy capture was pre-empted by the wipe — but the spy install methodology is sound for future SPA-only captures: wrap removeAttribute/setAttribute/hidden-property-setter BEFORE clicking in-page links.
||||||| parent of fdd3b1eab (memory: endgame — full render proven, only command-executor trigger missing)

## Console visibility live — boot nearly clean, one real error (2026-11-26 final)

eval-console-instruments pipe works. Entire YouTube boot produces only: LegacyDataMixin info x2, "No valid entry type" warning x2, and ONE error: "ResizeObserver loop completed with undelivered notifications". No exceptions/rejections otherwise. The RO-loop error is a NEW untested suspect for the missing command trigger: YouTube layout depends on RO chains; if servo's RO loop-limit drops notifications that Chrome delivers, the layout-completion signal the trigger depends on may never fire. Next cheap test: minimal RO-loop repro comparing servo vs spec behavior (chrome tolerates/delivers across frames; servo may drop).

## unhandledrejection implementation scope + final handoff (2026-11-26 final-final)

ResizeObserver EXONERATED (rotest.html: n=122 callbacks, loop error fires but delivery continues — spec-shaped, same as Chrome). Console pipe shows otherwise-clean boot. unhandledrejection: NO infrastructure exists — needs SM promise rejection tracker hook + PromiseRejectionEvent webidl + firing logic; multi-hour engine feature. It's the correct next investment: the command-trigger failure is either async-invisible (needs this) or a pure logic divergence. Alternative cheaper probe for continuation: compare YouTube's served JS/config in servo vs Chrome — UA-dependent feature flags (_.L(...) killswitches) can gate the command-trigger logic; yt.config_ / EXPERIMENT_FLAGS diff may reveal a flag gate servo fails. yt.config_ is readable live via devtools — look for flags mentioning search_contents/navigation_completion.
||||||| parent of 58815dfeb23 (memory: sec1zt23 — location wedge + failed navigation, prime mechanism suspect)

## sec1zt23 — REFRAMED: location-object wedge + failed navigation — a servo navigation-state defect (2026-11-26)

CRITICAL CORRECTION + finding: the 'same origin-domain' eval error is NOT a document-wide wedge — pure JS evals fine, document.title/documentElement/body all accessible post-error. It is specifically **the Location object access that throws**. AND: after `location.href='https://example.com/'` the page NEVER NAVIGATED — title still 'YouTube', body still youtube's 47 children, minutes later. **Servo defect: location.href assignment during a complex page state can silently fail to navigate AND leaves the Location object itself throwing spec errors on subsequent access.** Mechanism hypothesis: navigation starts (transitions the document's incumbent/settings state) but never completes or aborts, leaving Location's relevant-document check permanently failing. IMPACT ON YOUTUBE THEORY: YouTube's SPA framework constantly reads/writes location/history during response processing — if servo location state wedges during the boot sequence's navigation transitions, framework code touching location throws silently (K() swallows), aborting response completion before the unhide write. **This is now the prime mechanism suspect.** Next steps: (1) minimal repro — does location.href wedge reproduce on simple pages or only youtube-complex ones; (2) grep servo script/dom/location.rs for the relevant-document check and audit navigation-transition state handling; (3) test whether history.pushState (what YouTube SPA actually uses) also wedges. NOTE prior data point consistent: clicking filter chips didn't navigate either (sec1zt18) — possibly the same defect, not 'page already wedged'.
## sec1zt23 — REFRAMED: location-object wedge + failed navigation — a servo navigation-state defect (2026-11-26)

CRITICAL CORRECTION + finding: the 'same origin-domain' eval error is NOT a document-wide wedge — pure JS evals fine, document.title/documentElement/body all accessible post-error. It is specifically **the Location object access that throws**. AND: after `location.href='https://example.com/'` the page NEVER NAVIGATED — title still 'YouTube', body still youtube's 47 children, minutes later. **Servo defect: location.href assignment during a complex page state can silently fail to navigate AND leaves the Location object itself throwing spec errors on subsequent access.** Mechanism hypothesis: navigation starts (transitions the document's incumbent/settings state) but never completes or aborts, leaving Location's relevant-document check permanently failing. IMPACT ON YOUTUBE THEORY: YouTube's SPA framework constantly reads/writes location/history during response processing — if servo location state wedges during the boot sequence's navigation transitions, framework code touching location throws silently (K() swallows), aborting response completion before the unhide write. **This is now the prime mechanism suspect.** Next steps: (1) minimal repro — does location.href wedge reproduce on simple pages or only youtube-complex ones; (2) grep servo script/dom/location.rs for the relevant-document check and audit navigation-transition state handling; (3) test whether history.pushState (what YouTube SPA actually uses) also wedges. NOTE prior data point consistent: clicking filter chips didn't navigate either (sec1zt18) — possibly the same defect, not 'page already wedged'.
||||||| parent of d092a9361 (memory: RO exonerated; unhandledrejection scope; UA-flag probe alternative)

## CORRECTION: unhandledrejection IS fully implemented in servo upstream (2026-11-26)

Earlier claim was wrong. Full chain exists: SM promise_rejection_tracker hook (script_runtime.rs:245) → uncaught_rejections tracking → unhandledrejection event + Console::internal_error reporting. AND console piping shows ZERO rejections across entire YouTube boot. Async-failure theory formally DEAD. Sync errors also zero. Boot is completely clean — the missing command trigger is a pure logic/conditional divergence, not a failure of any kind. Servo spoofs Chrome/131 UA already. Every primitive exonerated. Remaining hypothesis space: (1) response-content divergence — the command may be in Chrome's youtubei response but absent in servo's due to client-capability differences in the POST payload; (2) consumed-and-removed commands — onResponseReceivedCommands may show only unexecuted residue; (3) a client-side conditional branch (capability probe) taking a different path. NEXT BEST PROBE: capture the actual youtubei/v1/search RESPONSE BODY in servo (devtools network actor or mitm) and diff against a Chrome-captured one — decisive for hypothesis 1/2.

## Navigation chain detail + final options (2026-11-26 endgame)

VERIFIED LIVE: yt-page-data-updated + yt-navigate-finish both fire correctly (T2037) — navigation completion machinery works. yt-show-search-contents-command NEVER fires. Bundle analysis: navigate-finish fires in a promise continuation after publishPageData (fEm/Uue at ~3808572 of /tmp/ytbase.js). Response commands resolve via gMl.handleResponse→commandHandler.resolveCommand — gated by commandAllowlist (xQ5). The show-contents command is NOT in servo's response data (only adsControlFlow present). REMAINING DECISIVE TEST (needs fresh session): capture youtubei/v1/search response body in servo AND in Chrome (this mac has Chrome) and diff — if Chrome's response contains the command, servo's client-context in the POST payload diverges (missing capability flags); if neither has it, the trigger is purely client-side render-completion logic inside publishPageData chain and needs breakpoint-level tracing. Cheapest immediate alternative: implement a shell-side shim (auto-inject the direct handler call when data arrives — proven working) as the alpha mitigation while the trigger hunt continues.
||||||| parent of fa21179f982 (memory: sec1zt24 — location theory retracted, devtools artifact)

## sec1zt24 — CORRECTION: location wedge is devtools-eval artifact, page JS unaffected — location theory DEAD (2026-11-26)

Definitive discrimination test: page with inline script accessing location.hostname + history.pushState + location.pathname — title shows LOC-OK:127.0.0.1|PS-OK:/spa-path — **real page JavaScript location access works perfectly, incl. pushState SPA pattern**. The persistent 'same origin-domain' throw is specific to devtools evaluateJS: entry settings object for devtools eval is not the page global, so the spec check correctly throws FOR THE EVAL ONLY. sec1zt23's 'prime mechanism suspect' is RETRACTED — it was instrumentation artifact. (Side findings that stand: location.href full navigation DOES work on simple pages — iana.org nav succeeded; and devtools eval of location.* will always throw — workaround: read location indirectly via document.URL or a page-side mirror.) ALSO: the servo FIXME in location.rs document_if_same_origin is real but only affects not-fully-active edge cases, NOT the youtube defect. BACK TO EVIDENCE: home unhide works (SPA-navigated), search unhide fails (initial loads). The open discriminator is initial-load vs SPA-nav for the SEARCH page specifically.
## sec1zt24 — CORRECTION: location wedge is devtools-eval artifact, page JS unaffected — location theory DEAD (2026-11-26)

Definitive discrimination test: page with inline script accessing location.hostname + history.pushState + location.pathname — title shows LOC-OK:127.0.0.1|PS-OK:/spa-path — **real page JavaScript location access works perfectly, incl. pushState SPA pattern**. The persistent 'same origin-domain' throw is specific to devtools evaluateJS: entry settings object for devtools eval is not the page global, so the spec check correctly throws FOR THE EVAL ONLY. sec1zt23's 'prime mechanism suspect' is RETRACTED — it was instrumentation artifact. (Side findings that stand: location.href full navigation DOES work on simple pages — iana.org nav succeeded; and devtools eval of location.* will always throw — workaround: read location indirectly via document.URL or a page-side mirror.) ALSO: the servo FIXME in location.rs document_if_same_origin is real but only affects not-fully-active edge cases, NOT the youtube defect. BACK TO EVIDENCE: home unhide works (SPA-navigated), search unhide fails (initial loads). The open discriminator is initial-load vs SPA-nav for the SEARCH page specifically.
||||||| parent of 403c3881c (memory: nav events fire; command never dispatches; response-diff test + shim option as the fork)

## hideContents origin refined (2026-11-26 endgame-2)

SSR HTML (curl w/ servo UA) contains NO ytd-search elements at all — bare app shell; entire search page client-rendered from ytInitialData (1.1MB, has results). ytInitialData contains ZERO hideContents and zero showSearchContents — so hideContents is a purely CLIENT-MANAGED navigation-state flag, not server data. KEY DEDUCTION: the hidden=true property write at T1469 (through qJC binding evaluation) means data.hideContents WAS true during boot — the HIDE-side of the machinery executes in servo. Only the completion-side (write false) never happens. So: partial execution confirmed — navigation-start logic runs, completion logic doesn't. Divergence point is strictly inside whatever runs at navigation completion for search pages. The fetch-intercept persist hook is armed on disk (/tmp/feather-eval-persist.js) for the next session to capture the live SPA search response and check for commands in the actual servo-fetched payload.

## Capture hook status (endgame-3): fetch-only patch captured NOTHING — youtubei uses XHR. Hook updated to patch XHR too (in /tmp/feather-eval-persist.js, XHR open/send wrapped, captures len+hasShowCmd+url into window.__capres). Next session: load a results URL fresh (query-param to force re-injection), read window.__capres after 15s — decisive for whether the command exists in servo's response payload at all.

## XHR/fetch capture live results (endgame-4)

Hook works: 104 fetch calls, ZERO xhr (youtubei uses fetch — earlier empty capture was a first-load/embed effect). Captured player endpoint 262KB — hasShowCmd FALSE. Direct-to-results loads use embedded ytInitialData for search (no youtubei/v1/search fetch observed). To capture the actual search payload need SPA in-page navigation (type new query in search box). hasShowCmd false so far consistent with client-side-trigger theory.

## DECISIVE: search payload captured — command NOT server-sent (endgame-5 FINAL)

Live SPA navigation triggered (search-box type + enter works!). Captured actual youtubei/v1/search response: 1.34MB, hasShowCmd FALSE — no showSearchContentsCommand in the real servo-fetched payload. Combined with the command constructor being absent from every JS chunk, the trigger mechanism must be either: (a) present in Chrome's payload only — requires Chrome-side capture comparison, testable on this mac via managed Chrome; or (b) generated by dynamic framework logic that constructs command objects programmatically (command type string never appears literally — kebab-case conversion hides it; re-grep chunks for "showSearchContents" camelCase specifically — earlier searches covered this, absence confirmed). BONUS: SPA search-box typing + enter navigation confirmed working end-to-end in feather.

## ★ REFRAME — the bug may be the erroneous HIDE, not a missing unhide (endgame-6)

Deep scan of fully-processed ys.data: only showDialogCommand + adsControlFlowOpportunity — showSearchContentsCommand absent everywhere in every layer (initial data, SPA payload, processed data, all JS chunks). Since no trigger exists anywhere, reconsider: perhaps in Chrome the hidden write NEVER HAPPENS on direct loads (nothing to hide — no prior contents). The hide at T1469 (qJC binding evaluation with hideContents momentarily true) may be SERVO'S ERROR — data.hideContents incorrectly true during boot. New prime question: what makes hideContents true in servo on a direct load? The binding evaluates [[data.hideContents]] — for it to fire true, some logic SET it true. That set is the defect's origin. Hunt direction flipped: find the hide-write logic (probably in navigation-start/page-data logic misfiring on initial load in servo — e.g. treating initial load as a replacement navigation, possibly because of the boot sequencing or an extra navigation event). KEY supporting evidence: yt-navigate events fired at T2037 — if servo fires an EXTRA/duplicate navigation or page-data event that Chrome doesn't, the hide-during-navigation logic triggers spuriously.
||||||| parent of d64c64ac09a (memory: sec1zt25 — mechanism fully decoded, trigger never fires)

## sec1zt25 — full mechanism chain decoded: the yt-show-search-contents-command action never fires (2026-11-26)

STATIC + LIVE decode of the complete unhide mechanism: ytd-search template binds `hidden="[[data.hideContents]]"` on ytd-two-column-search-results-renderer. The ONLY write that clears it: `onYtShowSearchContentsCommand(){this.set("data.hideContents",!1)}` registered via `actionMap={"yt-show-search-contents-command":"onYtShowSearchContentsCommand"}` in the YtActionHandlerBehavior — actions flow as `yt-action` DOM events carrying detail.actionName, routed by the _.jL singleton router. LIVE VERIFIED on stuck servo page: handler IS registered (typeof function), manual `ys.set('data.hideContents', false)` unhides + full render 7960px — binding chain perfect. Response data (1.66MB) contains NO showSearchContents anywhere (568 other Command refs) and NO hideContents key — the action trigger is purely client-side, fired when response processing completes. BOOT CAPTURE (dispatchEvent spy installed T+7.2s mid-boot): yt-action events DO fire (yt-open-video-preview-action, yt-forward-redux-action-to-live-chat-iframe x8) but yt-show-search-contents-command NEVER fires. The action-routing infrastructure works; the specific trigger never executes. The 'yt-show-search-contents-command' string appears exactly ONCE in ytbase.js (the registration) — so the firing site is either in another jsbin (search controller bundle — bins loaded incl. webcomponents-sd, scheduler, spf, network — full bin list not yet grepped) or constructed dynamically. NEXT: grep all loaded bins for the firing logic, OR compare against Chrome capture of the same event stream to see exact expected firing time/context. Hypothesis space narrowed to: some completion condition in the search response-processing controller never becomes true in servo (e.g. all-renderers-stamped signal, an idle callback, a specific promise chain, or visibility-of-something gate).
## sec1zt25 — full mechanism chain decoded: the yt-show-search-contents-command action never fires (2026-11-26)

STATIC + LIVE decode of the complete unhide mechanism: ytd-search template binds `hidden="[[data.hideContents]]"` on ytd-two-column-search-results-renderer. The ONLY write that clears it: `onYtShowSearchContentsCommand(){this.set("data.hideContents",!1)}` registered via `actionMap={"yt-show-search-contents-command":"onYtShowSearchContentsCommand"}` in the YtActionHandlerBehavior — actions flow as `yt-action` DOM events carrying detail.actionName, routed by the _.jL singleton router. LIVE VERIFIED on stuck servo page: handler IS registered (typeof function), manual `ys.set('data.hideContents', false)` unhides + full render 7960px — binding chain perfect. Response data (1.66MB) contains NO showSearchContents anywhere (568 other Command refs) and NO hideContents key — the action trigger is purely client-side, fired when response processing completes. BOOT CAPTURE (dispatchEvent spy installed T+7.2s mid-boot): yt-action events DO fire (yt-open-video-preview-action, yt-forward-redux-action-to-live-chat-iframe x8) but yt-show-search-contents-command NEVER fires. The action-routing infrastructure works; the specific trigger never executes. The 'yt-show-search-contents-command' string appears exactly ONCE in ytbase.js (the registration) — so the firing site is either in another jsbin (search controller bundle — bins loaded incl. webcomponents-sd, scheduler, spf, network — full bin list not yet grepped) or constructed dynamically. NEXT: grep all loaded bins for the firing logic, OR compare against Chrome capture of the same event stream to see exact expected firing time/context. Hypothesis space narrowed to: some completion condition in the search response-processing controller never becomes true in servo (e.g. all-renderers-stamped signal, an idle callback, a specific promise chain, or visibility-of-something gate).
||||||| parent of aa4392968 (memory: reframe — the erroneous hide may be the bug, not a missing unhide)

## Chrome comparison — DECISIVE END STATE (endgame-7 FINAL)

Chrome headless dump (virtual-time 15s) of identical query: renderer tag has NO hidden attribute, complete binding attrs (use-bigger-thumbs, bigger-thumbs-style=BIG, page-subtype). Servo's stuck element: IDENTICAL completed attributes PLUS hidden="" AND mini-guide-visible="" extra. Interpretation: servo's data pipeline runs to completion identically — every binding resolves — EXCEPT hidden persists. hideContents: 0 occurrences anywhere in Chrome DOM+data, absent in servo data too. The composite evidence now most consistent with: the hide-write itself is spurious/extra in servo (mini-guide-visible extra attribute is a second anomaly pointing at divergent navigation/layout state) OR a binding re-evaluation ordering difference when the data object is reassigned vs mutated. Both hunt directions documented; chrome headless dump method proven reusable (--virtual-time-budget=15000 --dump-dom, /tmp/chromedump.html on disk).

## hidden-write full stack captured (endgame-8)

Fresh-boot hidden setter instrumentation: hid=1 at T1522, stack: vj5 → qJC → _setUnmanagedPropertyToNode. "Unmanaged property" path = Polymer setting bound props on not-yet-upgraded elements during stamping — confirms hidden=true is set AT STAMPING TIME from data.hideContents being true in the data at that moment. So the data the framework assembles for stamping contains hideContents:true (client-generated, hide-while-loading), and in Chrome the same or different flow ends with it cleared. Definitive remaining question is narrow: what clears it in Chrome post-stamping. Persist hook for this capture is in /tmp/feather-eval-persist.js (setter patch, full stacks, element-filtered).

## Shim negative + protocol note (endgame-9 FINAL)

Prototyped auto-shim (500ms interval, fires ys.onYtShowSearchContentsCommand when hidden+data present) — NEVER self-fired across 20s despite hidden=true/vrs=19. Condition never satisfied in-page: likely onYtShowSearchContentsCommand not present as callable on the element instance at poll times (behavior method wiring differs), OR ys.data/.contents timing. Manual direct call DID work earlier in a different boot — variance again. Devtools protocol gotcha: evaluateJS replies sometimes arrive wrapped {"Ok":{...}} vs nested — extraction must handle both; socket noise heavy. Honest close for this session: the shim path is NOT trivially reliable — the element instance method availability itself varies boot-to-boot, which is itself another manifestation of the initialization race the rIC fix partially addressed. Deepest consistent frame: something in the behavior/action wiring finalizes late or never on the results-page elements in servo.
||||||| parent of 07805d5dda8 (memory: sec1zt26 — breakthrough: nav-completion events absent in servo)

## sec1zt26 — BREAKTHROUGH: chrome ground truth — yt-navigate-finish/yt-page-data-updated never fire in servo (2026-11-26)

CHROME COMPARISON CAPTURE (headless chrome + CDP Page.addScriptToEvaluateOnNewDocument — TRUE pre-boot injection, the capability devtools-eval lacks): on a WORKING chrome render of the same youtube search (hidden:false, h:7143): **yt-show-search-contents-command NEVER FIRES IN CHROME EITHER** — the action-based unhide theory is DEAD; that's a legacy/unused path. The events that DO fire in chrome: yt-page-data-fetched:1, yt-page-data-updated:1, yt-navigate-finish:1, yt-update-title:1, yt-renderidom-finished:9, yt-rendererstamper-finished:22. **SERVO CAPTURE COMPARISON: yt-navigate-finish, yt-page-data-fetched, yt-page-data-updated, yt-update-title ALL ABSENT in servo** — the navigation/response-processing completion events never fire. yt-rendererstamper-finished and yt-renderidom-finished DO fire in servo — so rendering machinery runs, but the navigation-completion layer never completes. REVISED MECHANISM UNDERSTANDING: hidden is not cleared by an explicit command — in chrome the post-response re-render naturally stamps the replacement without hidden because data.hideContents is falsy in the response (binding `hidden=\"[[data.hideContents]]\"` evaluates false). The servo replacement being stamped WITH hidden means either the binding evaluated against navigation-in-progress state (hideContentsForNavigation-equivalent stuck true because nav completion never happens) or the servo replacement is not the proper client re-render at all. THE QUESTION IS NOW FULLY PRECISE: why doesn't YouTube's navigation-completion machinery (yt-navigate-finish emission) run in servo? yt-navigate-start comparison pending (was in the chrome stream? not captured — check). Servo navigation machinery suspects: popstate/history events, the navigatefinish trigger waiting on some resource/signal, or the response-processing promise chain breaking. NOTE for future captures: chrome CDP addScriptToEvaluateOnNewDocument is the gold-standard pre-boot injection; use it as ground truth for ANY event-stream question.
## sec1zt26 — BREAKTHROUGH: chrome ground truth — yt-navigate-finish/yt-page-data-updated never fire in servo (2026-11-26)

CHROME COMPARISON CAPTURE (headless chrome + CDP Page.addScriptToEvaluateOnNewDocument — TRUE pre-boot injection, the capability devtools-eval lacks): on a WORKING chrome render of the same youtube search (hidden:false, h:7143): **yt-show-search-contents-command NEVER FIRES IN CHROME EITHER** — the action-based unhide theory is DEAD; that's a legacy/unused path. The events that DO fire in chrome: yt-page-data-fetched:1, yt-page-data-updated:1, yt-navigate-finish:1, yt-update-title:1, yt-renderidom-finished:9, yt-rendererstamper-finished:22. **SERVO CAPTURE COMPARISON: yt-navigate-finish, yt-page-data-fetched, yt-page-data-updated, yt-update-title ALL ABSENT in servo** — the navigation/response-processing completion events never fire. yt-rendererstamper-finished and yt-renderidom-finished DO fire in servo — so rendering machinery runs, but the navigation-completion layer never completes. REVISED MECHANISM UNDERSTANDING: hidden is not cleared by an explicit command — in chrome the post-response re-render naturally stamps the replacement without hidden because data.hideContents is falsy in the response (binding `hidden=\"[[data.hideContents]]\"` evaluates false). The servo replacement being stamped WITH hidden means either the binding evaluated against navigation-in-progress state (hideContentsForNavigation-equivalent stuck true because nav completion never happens) or the servo replacement is not the proper client re-render at all. THE QUESTION IS NOW FULLY PRECISE: why doesn't YouTube's navigation-completion machinery (yt-navigate-finish emission) run in servo? yt-navigate-start comparison pending (was in the chrome stream? not captured — check). Servo navigation machinery suspects: popstate/history events, the navigatefinish trigger waiting on some resource/signal, or the response-processing promise chain breaking. NOTE for future captures: chrome CDP addScriptToEvaluateOnNewDocument is the gold-standard pre-boot injection; use it as ground truth for ANY event-stream question.
||||||| parent of 306713457 (memory: shim negative — handler availability varies; honest close)

## Shim iteration results — ONE iteration from working mitigation (endgame-10 FINAL)

Shim v2/v3 results: fires successfully (T2510/T3115, hidden-after=false confirmed) but final state hidden=true — element gets re-hidden AFTER the early fire. Manual late fire (minutes post-boot) STICKS permanently (screenshot-proven full render). So: an early fire is defeated by the subsequent post-response re-render/replacement; late fire wins. Refire logic doesn't re-trigger — after the first fire the condition never holds again (childList+attribute MO armed, polls 40s — zero further fires). Next-session refinement path: log WHY the condition fails post-first-fire (probe ys.data/onYtShowSearchContentsCommand/hidden individually in the post-fire period), or simpler robust shim: fire repeatedly whenever hidden=true regardless of other conditions (direct ys.set('data.hideContents',false) fallback — also proven working). The working shim is genuinely one iteration away and converts YouTube to unassisted-rendering for alpha while the root-cause ordering divergence (servo applies the re-render later than Chrome / replacement timing) remains the deeper hunt.

## Shim paradox — exact final state for next session (endgame-11)

Final measurements: exactly ONE ytd-two-column-search-results-renderer exists (count=1, hidden, connected). Shim fires once (~T2.5-3s, hafter=false sync read), zero refires despite attribute+childList MO and 45s of polls, zero nofire logs — meaning at every post-first-fire check the element was NOT hidden per the shim's own querySelector, yet the final state is hidden=true on the single element. Interpretations: (a) hidden removal was transient within microtask batches and MO callbacks always delivered in windows where attribute was absent, with re-adds invisible due to batching timing; (b) the binding system re-adds hidden through a path that produces no mutation records (custom-elements upgrade-time attribute application — CFmmm maybe attribute restore during upgrade doesn't generate mutation records in servo? testable: does servo generate attribute mutation records for attributes set during custom element upgrade/clone?). HYPOTHESIS (b) IS TESTABLE cheaply and is a genuine spec-compliance question: attribute changes during upgrade/cloning should generate mutation records per spec. Next session: minimal repro — clone/upgrade an element with attributes, check MO records in servo vs Chrome. If missing, THAT is the engine bug explaining both the paradox and possibly the original defect (YouTube's own observers missing mutations).

## REPLACEMENT THEORY DEAD — clean single-boot capture (endgame-12)

Comprehensive identity-tracked capture (setter patch + MO + polls, one boot): exactly ONE write ever — T1724 SETTER hid=1 on el#1 (already connected), one ATTR record T2028, then the SAME single element stays hidden forever. NO replacement, NO re-hide, NO second element. Earlier replacement observations were boot-variant artifacts. The truth is minimal: hide-while-loading write happens once at stamping; unhide write never happens; everything else completes. Frame: in Chrome either the same hide-then-unhide sequence runs with the unhide trigger firing (trigger unknown — command absent everywhere), or Chrome stamps after data finalization so hideContents is never true. The remaining unexplored machinery: the visibility monitor (WHY()/visibilityObserver) — visOpt{calledOnce:false,state:hidden} persists; possible circular dependency (hidden→never intersects→observer never fires→completion never signaled) that Chrome somehow bootstraps through prescan/initial logic.
||||||| parent of a0968cd0bd5 (memory: sec1zt27 — userscript unlock, unhide fires, defect is replacement re-stamp)

## sec1zt27 — USERSCRIPT UNLOCK + reversal: hidden-removal FIRES in servo, events all fire — defect is replacement re-stamp (2026-11-26)

**TOOLING: servo has --userscripts <dir> BUILT IN** (components/script/dom/userscripts.rs — scripts evaluated when head element parses, TRUE pre-boot injection, no engine work needed). This replaces all the injection hacks — use for all future framework debugging.

**FULL BOOT CAPTURE with userscript injection — reverses the sec1zt26 narrowing**: yt-page-data-fetched FIRES @1928, yt-page-data-updated @2198, yt-navigate-finish @2199, yt-update-title @2162 — the navigation-completion pipeline COMPLETES in servo! AND critically: **HIDDEN-REMOVE on ytd-two-column-search-results-renderer FIRES @1933** — the unhide write DOES execute! Yet final state is hidden:true. Confirmed synthesis with the replacement docs: removal fires on the boot/SSR element, the response-data replacement then arrives stamped WITH hidden, and no second removal fires for the replacement instance. The events all firing means the completion pipeline is NOT the defect. NEW PRECISE QUESTION: in chrome does the replacement element exist at all / does a second removal fire? ANOMALY OBSERVED: every event in servo fires exactly 3x with identical timestamps — either triple-dispatch in youtube logic (servo-specific) or triple-nested spy wrappers (userscript injected 3x?) — discriminate before trusting multiplicities. ALSO NOTE: sec1zt26's 'events absent in servo' was an artifact of post-boot injection (events fire before T+7s when spies could attach) — corrected.
## sec1zt27 — USERSCRIPT UNLOCK + reversal: hidden-removal FIRES in servo, events all fire — defect is replacement re-stamp (2026-11-26)

**TOOLING: servo has --userscripts <dir> BUILT IN** (components/script/dom/userscripts.rs — scripts evaluated when head element parses, TRUE pre-boot injection, no engine work needed). This replaces all the injection hacks — use for all future framework debugging.

**FULL BOOT CAPTURE with userscript injection — reverses the sec1zt26 narrowing**: yt-page-data-fetched FIRES @1928, yt-page-data-updated @2198, yt-navigate-finish @2199, yt-update-title @2162 — the navigation-completion pipeline COMPLETES in servo! AND critically: **HIDDEN-REMOVE on ytd-two-column-search-results-renderer FIRES @1933** — the unhide write DOES execute! Yet final state is hidden:true. Confirmed synthesis with the replacement docs: removal fires on the boot/SSR element, the response-data replacement then arrives stamped WITH hidden, and no second removal fires for the replacement instance. The events all firing means the completion pipeline is NOT the defect. NEW PRECISE QUESTION: in chrome does the replacement element exist at all / does a second removal fire? ANOMALY OBSERVED: every event in servo fires exactly 3x with identical timestamps — either triple-dispatch in youtube logic (servo-specific) or triple-nested spy wrappers (userscript injected 3x?) — discriminate before trusting multiplicities. ALSO NOTE: sec1zt26's 'events absent in servo' was an artifact of post-boot injection (events fire before T+7s when spies could attach) — corrected.
||||||| parent of cab1e7635 (memory: replacement theory dead — single write at T1724, never cleared)

## YouTube hidden-stall ROOT CAUSE — SOLVED to the single wrong bit (endgame-13, FINAL)

Chrome vs servo identical-boot A/B with setter patches (addScriptToEvaluateOnNewDocument on Chrome via CDP ws, persist-hook on servo):
- CHROME: T1268 `SET hid=0 dataHC=nodata` — ONE write, value FALSE, element never hidden, results render.
- SERVO:  T1710 `SET hid=1 dataHC=nodata` — ONE write, value TRUE, element stays hidden forever.
Both: exactly one write through stack `_setUnmanagedPropertyToNode ← qJC (binding evaluator) ← vj5`, at stamping time, while `this.data` does not yet exist. data.hideContents ends `false` in Chrome's data object, `undefined` in servo's (keys=[primaryContents] both). ytInitialData has hideContents=false in both. The yt-show-search-contents-command DOES NOT EXIST in Chrome either — never fires, handler never called — the entire "missing unhide" line was a dead end; Chrome never needs an unhide because it never hides.

**The defect: the binding `hidden="[[data.hideContents]]"` (or sibling computed binding) evaluates at stamp time with the data path absent; the resolved value passed to the property setter is TRUTHY in servo, FALSY in Chrome.** Same polyfill JS, same template — the divergence is therefore in a DOM primitive the evaluation depends on. Suspects, ranked: (1) template stamping writes the literal binding attribute `hidden="[[data.hideContents]]"` to the real DOM before annotation binding replaces it — literal non-empty string = truthy — if servo's stamping/upgrade ordering exposes the literal attribute (or the polyfill reads it via getAttribute during evaluate), the write is truthy. In Chrome the annotation pass runs first / literal never visible. (2) Path resolution returning a truthy sentinel (literal binding text) instead of undefined under a missing-root-path primitive. (3) Boolean attribute reflection coercion during stamp. The write goes through the property SETTER so the truthy value arrives already-coerced — the divergence is upstream in qJC's value computation.

Minimal repro needed: custom element w/ template `hidden="[[data.hideContents]]"` stamped before data set — compare written value servo vs Chrome. Requires webcomponents/polymer harness — alternatively instrument qJC's computed value + intermediate path resolution directly in the live servo page (log what value qJC passes and what the binding source resolved to).

Prior layer results all still valid: rIC starvation fix landed (`933f453ad`, verified); stamping reliable post-fix; every other primitive exonerated; visibility-observer state machine noise; replacement theory dead (some boots show element replacement but endgame-12 clean capture shows single element, single write).
||||||| parent of 40038cbea4b (memory: sec1zt28 — root cause cornered: undefined→true binding write)

## sec1zt28 — ROOT CAUSE CORNERED: binding writes hidden=true for undefined data.hideContents (2026-11-26)

Definitive write-capture with state snapshots (userscript injection): servo sequence: template-parse strips SSR hidden @1973 (state: no data yet), then **propSET hidden=true @1996 while parent ytd-search data EXISTS but hideContents is UNDEFINED**. Chrome comparison for identical capture: remove@1492 then propCLEAR@1498 (false written). **The binding `hidden="[[data.hideContents]]"` evaluates undefined→TRUE in servo vs undefined→FALSE/absent in chrome.** Since the binding framework is identical JS in both browsers, the divergence is in a PRIMITIVE the framework relies on. Leading theory: template binding annotation REGISTRATION fails in servo — the write path was `_setUnmanagedPropertyToNode` (the stamper for UNMANAGED template properties) — i.e. in servo the hidden attribute is treated as unmanaged/static template content and stamped with truthy-string semantics, while in chrome it registers as a managed binding and is excluded from stamping. Suspect primitive areas: servo template parsing producing different attribute representation, or annotation-parse regex/string handling difference, OR custom-element/template lifecycle ordering making the registration miss. NEXT STEP: read the stamper code path in bundle (_parseTemplateNodeAttribute/_setUnmanagedPropertyToNode region, kevlar chunk line ~7473/3644) to determine exactly what makes an attribute managed-vs-unmanaged and probe that primitive in servo. ALSO CONFIRMED ARTIFACT: userscript injects 3x per page (nested wrappers — capture.js:22→capture.js:23 self-chain visible in stacks) — all x3 event duplicates are this artifact, not servo behavior; servo userscript multiplicity itself worth a look (load_script possibly invoked per head mutation).
## sec1zt28 — ROOT CAUSE CORNERED: binding writes hidden=true for undefined data.hideContents (2026-11-26)

Definitive write-capture with state snapshots (userscript injection): servo sequence: template-parse strips SSR hidden @1973 (state: no data yet), then **propSET hidden=true @1996 while parent ytd-search data EXISTS but hideContents is UNDEFINED**. Chrome comparison for identical capture: remove@1492 then propCLEAR@1498 (false written). **The binding `hidden="[[data.hideContents]]"` evaluates undefined→TRUE in servo vs undefined→FALSE/absent in chrome.** Since the binding framework is identical JS in both browsers, the divergence is in a PRIMITIVE the framework relies on. Leading theory: template binding annotation REGISTRATION fails in servo — the write path was `_setUnmanagedPropertyToNode` (the stamper for UNMANAGED template properties) — i.e. in servo the hidden attribute is treated as unmanaged/static template content and stamped with truthy-string semantics, while in chrome it registers as a managed binding and is excluded from stamping. Suspect primitive areas: servo template parsing producing different attribute representation, or annotation-parse regex/string handling difference, OR custom-element/template lifecycle ordering making the registration miss. NEXT STEP: read the stamper code path in bundle (_parseTemplateNodeAttribute/_setUnmanagedPropertyToNode region, kevlar chunk line ~7473/3644) to determine exactly what makes an attribute managed-vs-unmanaged and probe that primitive in servo. ALSO CONFIRMED ARTIFACT: userscript injects 3x per page (nested wrappers — capture.js:22→capture.js:23 self-chain visible in stacks) — all x3 event duplicates are this artifact, not servo behavior; servo userscript multiplicity itself worth a look (load_script possibly invoked per head mutation).

## sec1zt29 — BREAKTHROUGH: the truthy write value is a Trusted-Types policy object (typeof object, stringifies "zClosurez")

Value-level capture on live servo boot (upgraded spy records typeof+String(v)):
```
propSET|object|zClosurez  (t=1931, once effective — x3 artifact)
```
The binding evaluates `data.hideContents` (undefined) but the VALUE assigned to `tc.hidden` is an
OBJECT stringifying to "zClosurez" — truthy — hence hidden=true.

IDENTIFIED in bundle @2458373: the **polymer_resin Trusted Types policy**:
```js
var D={createHTML:()=>"zClosurez", createScript:()=>"/*zClosurez*/", createScriptURL:()=>"about:invalid#zClosurez"};
return typeof trustedTypes!=="undefined" ? trustedTypes.createPolicy("polymer_resin", D) : D
```
When trustedTypes exists, createPolicy returns a real TT policy whose createHTML produces TrustedHTML
objects (typeof object, truthy). The framework's binding write path passes values through a transform
(C7 in the write chain: `C7&&(W=C7(W,q.target,q.kind,k))`) — the resin sanitizer hook.

CHROME contrast: propCLEAR = the assigned value was FALSY (undefined) — chrome's binding evaluation
never produces the object for this path, OR the transform passes undefined through untouched.
SERVO: the object materializes — two candidate primitives:
  (a) servo's trustedTypes.createPolicy/createHTML implementation returns the object even for
      undefined/empty input where chrome returns the input unchanged;
  (b) the binding evaluation in servo feeds a non-undefined sentinel into the transform where
      chrome feeds undefined (evaluation divergence), and the transform wraps it.
Either way the failing primitive is IN THE TRUSTED-TYPES/RESIN TRANSFORM PIPELINE, not the data system.

NEXT PROBES (cheap, decisive):
  1. typeof trustedTypes + trustedTypes.createPolicy behavior in servo vs chrome:
     `trustedTypes.createPolicy('t',{createHTML:s=>s}).createHTML('x')` — typeof of result;
     createHTML(undefined) behavior.
  2. Capture C7's input/output — spy is internal; instead: evaluate `data.hideContents` binding
     semantics manually in-page: compare `(eval of undefined passthrough)` — if TT wrap of undefined
     produces object in servo but undefined in chrome → primitive pinned.
  3. If (a) confirmed: engine fix = match chrome TT semantics (passthrough of non-string input /
     correct undefined handling in createHTML).

Symptom index: YouTube truthy hidden write (object zClosurez) → check Trusted Types createPolicy/
createHTML object-materialization semantics (components/script/dom/trustedtypes*), polymer_resin
policy path in kevlar bundle.

## sec1zt30 — ROOT CAUSE PINNED TO EXACT PRIMITIVE: resin sanitizeDOMValue(undefined) diverges

Direct A/B probe on live pages, identical probe code, same Polymer.sanitizeDOMValue function:
```
input undefined → SERVO: object "zClosurez" TRUTHY   | CHROME: undefined FALSY
input false     → both boolean false
input 'abc'     → both string 'abc'
input null      → both null
```
THE YOUTUBE INVISIBLE-RESULTS DEFECT = this one divergence. Chain: binding `hidden="[[data.hideContents]]"`
evaluates undefined (identical in both — verified ytInitialData/live data have NO hideContents anywhere in
EITHER browser; endgame-13's false-in-chrome claim was WRONG) → sanitizeDOMValue transform → servo returns
the INNOCUOUS_STRING fallback (TrustedHTML object wrapping "zClosurez", truthy) → tc.hidden = truthy →
results hidden forever. Chrome passes undefined through → hidden=false → renders.

TT object materialization itself EXONERATED: createPolicy/createHTML(undefined) behavior byte-identical
in both browsers (truthy objects both). Data layer exonerated identically. rIC/CE/IO/upgrade/scheduler
all previously exonerated. The divergence is INSIDE the resin sanitizer's undefined-handling branch —
same JS, different result ⇒ it depends on a browser primitive that differs. Prime suspects in servo:
  (a) TT type-check primitives the resin safeTypesBridge uses (instanceof TrustedHTML / isUnwrappable
      detection failing on servo's TT object identity) making the sanitizer misroute undefined;
  (b) a typeof/String coercion difference on undefined inside vNk machinery.
NEXT STEP (final): trace the resin z4M inner path (vNk + safeTypesBridge @2458537) — determine which
branch undefined takes in chrome (passthrough) vs servo ($_ fallback), identify the primitive the branch
keys on, probe that primitive directly. HIGHEST-VALUE TARGETS: servo's TrustedTypes implementation
(components/script/dom/trustedtypes*.rs) object identity/type-tag behavior, and resin's isUnwrappable check.

Symptom index: framework sanitizeDOMValue diverges ONLY for undefined input → check Trusted Types
object identity/unwrap detection (isUnwrappable) and TT type-tag primitives in servo.
||||||| parent of 7da7aea6a (memory: write value is zClosurez — Closure expando; resolution walks wrong object)

## sec1zt31 — ROOT CAUSE FINAL: servo's document.all strict-equals undefined — breaks resin's branch

DEFINITIVE A/B primitive probe:
```
                    SERVO      CHROME     spec-correct
typeof document.all   undefined  undefined  undefined   ✓ both
!document.all         true       true       true        ✓ both
document.all == undefined  true  true       true        ✓ both (loose)
document.all === undefined TRUE  FALSE      FALSE       ✗ SERVO WRONG
```
Servo's document.all behaves as if it literally IS undefined (passes typeof/falsy/loose by coincidence)
instead of a real object with the [[IsHTMLDDA]] internal slot. Strict equality object-vs-undefined must
ALWAYS be false per spec — servo returns true, proving document.all is not a proper object.

MECHANISM CHAIN (complete, end to end):
  resin inner sanitizer: vNk returns `(J,D,d,Q) => Q || Q===document.all ? W(J,D,d)(Q) : Q`
  - chrome: undefined input → `undefined || (undefined===document.all→FALSE)` → falsy → PASSTHROUGH
    → sanitizeDOMValue(undefined) returns undefined → hidden write falsy → results render.
  - servo: undefined input → `undefined || (undefined===document.all→TRUE!)` → truthy → sanitizer
    processes it → returns $_ = INNOCUOUS_STRING (TrustedHTML object, truthy) → tc.hidden = object
    → truthy → RESULTS INVISIBLE FOREVER.
YouTube invisible-results root cause = servo's document.all is not a real IsHTMLDDA object.

ENGINE FIX: implement HTMLAllCollection correctly — a real object with [[IsHTMLDDA]] internal slot
making typeof→"undefined", ToBoolean→false, loose==null/undefined→true, but identity/strict-equality
as a normal object. Check components/script/dom/htmlallcollection (or wherever document.all is
implemented) — likely currently returns undefined or a hacked value instead of the spec object.
Verification gate: `document.all === undefined` must be false; YouTube search results render.

Symptom index: ANY framework branch keying on `x === document.all` misfires in servo → check
document.all implementation (IsHTMLDDA object identity). Indexed by mechanism: strict-equality
with undefined incorrectly true for document.all.
## sec1zt32 — document.all implementation in progress (Linux lane) + codegen mechanics decoded

IMPLEMENTING HTMLAllCollection (servo#7396 — never implemented upstream). Files:
- components/script_bindings/webidls/HTMLAllCollection.webidl — interface : HTMLCollection, namedItem getter union
- Document.webidl — `[SameObject] readonly attribute HTMLAllCollection all;` (uncommented)
- components/script/dom/html/htmlallcollection.rs — embeds HTMLCollection rooted at document with
  match-all filter; NamedItem: id-or-name match, single→Element, multiple→live HTMLCollection
- Document::All lazy-cached via MutNullableDom or_init
- html/mod.rs registration

KEY MECHANISM — emulates-undefined for PROXY objects: DOM objects with indexed/named getters are JS
proxies in servo, so JSCLASS_EMULATES_UNDEFINED on the DOMJSClass has NO effect (proxies don't use the
interface class). Solution: mozjs glue NewProxyObject accepts aClass → js::ProxyOptions.setClass.
Added `EMULATES_UNDEFINED_PROXY_CLASS` static in script_bindings/init.rs — bare JSClass carrying only
the EMULATES_UNDEFINED flag; codegen emits `class: Some(&crate::init::EMULATES_UNDEFINED_PROXY_CLASS)`
for HTMLAllCollection's WrapConfig; wrap.rs proxy path now passes config.class through to NewProxyObject
(was hardcoded null). SpiderMonkey reads class flags directly for typeof/truthiness/loose-equality
(verified in jit/MacroAssembler-inl.h branchIfObjectEmulatesUndefined); strict equality untouched.

KEY CODEGEN LEARNING (symptom index — saves hours on any future interface work): cx-ness of generated
trait methods is NOT inferred from webidl types — it's MANUAL per-interface config in
codegen/Bindings.conf (`'cx': [...]`, `'cx_no_gc': [...]`, `'no_gc': [...]` lists). Attributes/methods
not listed generate cx-less trait signatures. HTMLFormControlsCollection entry was the template.
Document::All needed 'All' added to Document's cx list.

## HTMLAllCollection implementation — IN PROGRESS (macOS lane, endgame-15)

sec1zt31 root cause confirmed correct. Implementation landed locally (compiles clean), verification pending:
- `components/script_bindings/webidls/HTMLAllCollection.webidl` — new (length/item/namedItem union, LegacyOverrideBuiltIns; LegacyFactoryFunction intentionally omitted v1)
- `components/script/dom/html/htmlallcollection.rs` — new DOM type wrapping HTMLCollection rooted at document, all-elements filter; namedItem returns Element/union sub-collection per spec
- `components/script_bindings/codegen/codegen.py` — class flags for HTMLAllCollection get `JSCLASS_EMULATES_UNDEFINED` (bit exists in vendored SM 153 as 1<<6, bindgen exposes it — compiled)
- `components/script_bindings/import.rs` — re-export the constant
- `Document.webidl` — `all` uncommented (plain readonly attr, or_init caching like images)
- `document.rs` — all_collection field + All() impl
- `Bindings.conf` — HTMLAllCollection entry (cx for named methods, cx_no_gc for indexed; 'All' added to Document cx list)
- dom/html/mod.rs — module registered
KEY ENGINE FACT: SpiderMonkey implements all IsHTMLDDA semantics (typeof undefined, falsy, loose==undefined true, strict===undefined FALSE) purely from the class flag — no VM changes needed.
Verification gate: `document.all === undefined` must be false in servo, typeof document.all "undefined", !document.all true, document.all==undefined true; then YouTube results render unassisted.

## HTMLAllCollection round 2 — the proxy class path (endgame-16)

CRITICAL STRUCTURAL FACT: servo implements interfaces with indexed/named getters as **JS Proxies** (CreateProxyHandler + NewProxyObject in wrap.rs), not plain objects — so the init_domjs_class flags path never applies to them. typeof was still "object" after round 1 because the proxy object got the default proxy class (`ptr::null()` class arg in wrap.rs NewProxyObject). SM mechanism confirmed in vendored source: EmulatesUndefined() unwraps WrapperObjects then reads getClass()->emulatesUndefined() — proxies CAN carry custom classes (ProxyOptions.setClass). Fix plumbed: `HTML_ALL_COLLECTION_PROXY_CLASS` static in script_bindings/utils.rs (flags=JSCLASS_EMULATES_UNDEFINED, verified=64 in bindgen jsapi.rs), codegen emits it for HTMLAllCollection's proxy WrapConfig, wrap.rs passes config.class into NewProxyObject. Verification gate unchanged: typeof document.all === "undefined" etc.

## HTMLAllCollection VERIFIED — YouTube alpha gate DOWN (endgame-17, THE fix)

Primitive verification (docall.html): typeof document.all="undefined" ✓, !document.all=true ✓, document.all==undefined=true ✓, **document.all===undefined=FALSE** ✓ (the critical bit), instanceof Object=true ✓ — full spec IsHTMLDDA semantics correct.
END-TO-END VERIFICATION: youtube.com/results?search_query=techno+mix renders full search results UNASSISTED — no shim, no manual unhide. Video entries, thumbnails, sponsored slots all visible. The multi-session hunt (hydration stall → rIC starvation → hidden attribute → resin sanitizer → document.all) is CLOSED.
KNOWN RESIDUALS: (1) devtools evaluateJS accessing document.all properties crashes/brands-fail — grip serialization or brand-check interaction with the custom proxy class, devtools-only path, in-page semantics correct; (2) LegacyFactoryFunction omitted (document.all callable form unimplemented — essentially unused on real web); (3) namedItem multiple-match sub-collection path untested. Follow-ups non-blocking.
Implementation notes for upstream port: JSCLASS_EMULATES_UNDEFINED exists in vendored SM 153 (=64); proxies accept custom classes via NewProxyObject class arg; servo previously passed ptr::null() for all DOM proxies — the plumbing added in wrap.rs is generic (any proxy interface can now carry a custom class).

## HTMLAllCollection FINAL — stable + fully verified (endgame-18)

Crash root cause found+fixed: the v1 static class had null cOps/ext/oOps — proxy objects NEED SpiderMonkey's proxy class ops (proxy_Finalize/ProxyObject::trace/objectMoved + ProxyObjectOps for all property access). Fix: clone the exported `js::ProxyClass` (JS_PUBLIC_DATA, available in bindgen as js::jsapi::ProxyClass) and OR JSCLASS_EMULATES_UNDEFINED into flags — `html_all_collection_proxy_class()` in script_bindings/utils.rs (lazy OnceLock, SyncClass wrapper for Send/Sync). Final verification all green: typeof=undefined, falsy, loose-eq true, strict-eq false, instanceof Object true; in-page len/indexed/namedItem/keys all work (len=5, i0=HTML, nm=object, keys=7); devtools-path eval access works; NO crashes; YouTube results render fully unassisted (channel cards, shelves, sponsored). The multi-session YouTube invisible-results defect chain is CLOSED at the engine level.

## Post-fix breadth verification (endgame-19)

Sites re-tested after the document.all fix on the stable build: YouTube results+SPA+watch-chrome ✓, X.com login page ✓, reddit (botwall renders — attestation class) ✓, **old.reddit.com login renders fully (previously BLANK in the pre-alpha ledger — legacy document.all gating confirmed as a real-site impact class)** ✓, wikipedia ✓ no regression, github ✓ no regression. Legacy IE-detection branching (`if (document.all)` / `===` comparisons) is a broad real-web pattern — this fix likely unblocks a long tail of legacy-gated rendering defects beyond YouTube.

## Playback regression check (endgame-20): Rick Astley watch page — full chrome + ad pre-roll playing, sidebar recommendations, no regression from document.all fix. YouTube is now end-to-end functional in feather except attestation-botwall videos.

## namedItem verification (endgame-21): multi-match returns HTMLCollection ✓, single-match Element semantics correct (test artifact confirmed), empty-string null ✓, indexed item() identity consistent ✓. Collection surface fully verified.
Verification gate once built: document.all === undefined must be FALSE, typeof "undefined", falsy,
document.all.length == element count, document.all('id') lookup; then YouTube search results render.
||||||| parent of e51a7d10a (memory: namedItem surface verified)
## HTMLAllCollection implementation — IN PROGRESS (macOS lane, endgame-15)

sec1zt31 root cause confirmed correct. Implementation landed locally (compiles clean), verification pending:
- `components/script_bindings/webidls/HTMLAllCollection.webidl` — new (length/item/namedItem union, LegacyOverrideBuiltIns; LegacyFactoryFunction intentionally omitted v1)
- `components/script/dom/html/htmlallcollection.rs` — new DOM type wrapping HTMLCollection rooted at document, all-elements filter; namedItem returns Element/union sub-collection per spec
- `components/script_bindings/codegen/codegen.py` — class flags for HTMLAllCollection get `JSCLASS_EMULATES_UNDEFINED` (bit exists in vendored SM 153 as 1<<6, bindgen exposes it — compiled)
- `components/script_bindings/import.rs` — re-export the constant
- `Document.webidl` — `all` uncommented (plain readonly attr, or_init caching like images)
- `document.rs` — all_collection field + All() impl
- `Bindings.conf` — HTMLAllCollection entry (cx for named methods, cx_no_gc for indexed; 'All' added to Document cx list)
- dom/html/mod.rs — module registered
KEY ENGINE FACT: SpiderMonkey implements all IsHTMLDDA semantics (typeof undefined, falsy, loose==undefined true, strict===undefined FALSE) purely from the class flag — no VM changes needed.
Verification gate: `document.all === undefined` must be false in servo, typeof document.all "undefined", !document.all true, document.all==undefined true; then YouTube results render unassisted.

## HTMLAllCollection round 2 — the proxy class path (endgame-16)

CRITICAL STRUCTURAL FACT: servo implements interfaces with indexed/named getters as **JS Proxies** (CreateProxyHandler + NewProxyObject in wrap.rs), not plain objects — so the init_domjs_class flags path never applies to them. typeof was still "object" after round 1 because the proxy object got the default proxy class (`ptr::null()` class arg in wrap.rs NewProxyObject). SM mechanism confirmed in vendored source: EmulatesUndefined() unwraps WrapperObjects then reads getClass()->emulatesUndefined() — proxies CAN carry custom classes (ProxyOptions.setClass). Fix plumbed: `HTML_ALL_COLLECTION_PROXY_CLASS` static in script_bindings/utils.rs (flags=JSCLASS_EMULATES_UNDEFINED, verified=64 in bindgen jsapi.rs), codegen emits it for HTMLAllCollection's proxy WrapConfig, wrap.rs passes config.class into NewProxyObject. Verification gate unchanged: typeof document.all === "undefined" etc.

## HTMLAllCollection VERIFIED — YouTube alpha gate DOWN (endgame-17, THE fix)

Primitive verification (docall.html): typeof document.all="undefined" ✓, !document.all=true ✓, document.all==undefined=true ✓, **document.all===undefined=FALSE** ✓ (the critical bit), instanceof Object=true ✓ — full spec IsHTMLDDA semantics correct.
END-TO-END VERIFICATION: youtube.com/results?search_query=techno+mix renders full search results UNASSISTED — no shim, no manual unhide. Video entries, thumbnails, sponsored slots all visible. The multi-session hunt (hydration stall → rIC starvation → hidden attribute → resin sanitizer → document.all) is CLOSED.
KNOWN RESIDUALS: (1) devtools evaluateJS accessing document.all properties crashes/brands-fail — grip serialization or brand-check interaction with the custom proxy class, devtools-only path, in-page semantics correct; (2) LegacyFactoryFunction omitted (document.all callable form unimplemented — essentially unused on real web); (3) namedItem multiple-match sub-collection path untested. Follow-ups non-blocking.
Implementation notes for upstream port: JSCLASS_EMULATES_UNDEFINED exists in vendored SM 153 (=64); proxies accept custom classes via NewProxyObject class arg; servo previously passed ptr::null() for all DOM proxies — the plumbing added in wrap.rs is generic (any proxy interface can now carry a custom class).

## HTMLAllCollection FINAL — stable + fully verified (endgame-18)

Crash root cause found+fixed: the v1 static class had null cOps/ext/oOps — proxy objects NEED SpiderMonkey's proxy class ops (proxy_Finalize/ProxyObject::trace/objectMoved + ProxyObjectOps for all property access). Fix: clone the exported `js::ProxyClass` (JS_PUBLIC_DATA, available in bindgen as js::jsapi::ProxyClass) and OR JSCLASS_EMULATES_UNDEFINED into flags — `html_all_collection_proxy_class()` in script_bindings/utils.rs (lazy OnceLock, SyncClass wrapper for Send/Sync). Final verification all green: typeof=undefined, falsy, loose-eq true, strict-eq false, instanceof Object true; in-page len/indexed/namedItem/keys all work (len=5, i0=HTML, nm=object, keys=7); devtools-path eval access works; NO crashes; YouTube results render fully unassisted (channel cards, shelves, sponsored). The multi-session YouTube invisible-results defect chain is CLOSED at the engine level.

## Post-fix breadth verification (endgame-19)

Sites re-tested after the document.all fix on the stable build: YouTube results+SPA+watch-chrome ✓, X.com login page ✓, reddit (botwall renders — attestation class) ✓, **old.reddit.com login renders fully (previously BLANK in the pre-alpha ledger — legacy document.all gating confirmed as a real-site impact class)** ✓, wikipedia ✓ no regression, github ✓ no regression. Legacy IE-detection branching (`if (document.all)` / `===` comparisons) is a broad real-web pattern — this fix likely unblocks a long tail of legacy-gated rendering defects beyond YouTube.

## Playback regression check (endgame-20): Rick Astley watch page — full chrome + ad pre-roll playing, sidebar recommendations, no regression from document.all fix. YouTube is now end-to-end functional in feather except attestation-botwall videos.
