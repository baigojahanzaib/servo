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
