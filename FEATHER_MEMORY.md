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
||||||| parent of 551002bcb5e (docs: MSE seek design + symptom index (§1t))

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
||||||| parent of 2eb676bd68b (docs: fmp4 indexing + corrected suspect ranking (§1v))

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
||||||| parent of e17213bdb (FEATHER_MEMORY: webgl-spoof experiment verdict — nondeterministic, attribution ambiguous)

## Symptom: claude.ai turnstile — nondeterministic pass/loop with full real stack + WebGL string spoof
→ check: WebGL RENDERER/VENDOR masked strings (status: spoofed locally to ANGLE-Metal/Google-Inc. values, verified live — NOT PUSHED, experiment only; verdict: insufficient alone, cold-state still loops), cf_clearance cookie hypothesis (status: plausible — the one observed pass occurred with accumulated storage present; a clearance from background-running challenge machinery can't be ruled out, so attribution to the spoof is ambiguous), turnstile internal machinery (status: runs DEEP — service worker + caches + IndexedDB + localstorage all registered under challenges.cloudflare.com origin; the payload executes fully rather than dying on API checks), verdict class (status: nondeterministic — passed once after minutes of runtime, loops other times; server-side risk scoring on aggregate signals means remaining tells are statistical/behavioral + TLS, a different work class than API presence).
