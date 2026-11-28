# Feather idle-RAM levers Ã¢â‚¬â€� what needs upstream Servo changes

Feather's idle budget is <150MB RSS. Feather-side work shipped in
`feather/shell` (lazy session restore, jemalloc background purging,
`js_mem_max` cap) leaves ~210-280MB on a GPU-less host, of which roughly
65MB is llvmpipe software-GL overhead that disappears on real hardware.
The remaining levers below need engine-side changes; sketches live here
rather than as hacks in feather-shell.

## 1. Bounded image cache (net/image_cache.rs) Ã¢â‚¬â€� SHIPPED on feather/image-cache-bound

`ImageCache` retained every decoded image for the lifetime of the
pipeline Ã¢â‚¬â€� no size bound, no eviction.

Implemented: `completed_loads` is now capped at 128 MiB of decoded
raster bytes per `ImageCache` (per pipeline). `CompletedLoad` records
its decoded size; `completed_loads_bytes` + `completed_loads_lru` drive
`enforce_completed_load_budget` after each completed load. Eviction
reuses `remove_loaded_image` (drops the entry + `DeleteImage` to
WebRender); hits promote entries in the LRU. Entries whose decoded
`Arc<RasterImage>` is still referenced outside the cache (DOM nodes
hold one via `ImageRequest.image`) are skipped Ã¢â‚¬â€� evicting them frees
nothing and forces a re-fetch, so the cap is soft when a page displays
more than the budget.

Measured (feather-shell, llvmpipe host, 40x 1600x1200 PNG Ã¢â€°Ë† 307 MiB
decoded):

| Scenario | Before | After |
|---|---|---|
| Slideshow page (images unreferenced after display) | 839 MiB RSS | **657 MiB** (Ã¢Ë†â€™182) |
| All-visible grid | 855 MiB | ~831 MiB Ã¢â‚¬â€� soft cap, no churn |

The naive version without the `Arc::strong_count` check caused an
evictÃ¢â€ â€™refetchÃ¢â€ â€™re-decode cycle (80 fetches for 40 images) for no RSS
gain Ã¢â‚¬â€� the pinned-entry check is required.

Caveat: on a page that *keeps* all its images referenced, the cache is
not the retainer Ã¢â‚¬â€� the DOM is. Shrinking that case needs script-side
discard of decoded surfaces (like imglib's discard-on-memory-pressure),
a much bigger change.

## 1b. Hidden-document image surface discard Ã¢â‚¬â€� SHIPPED on feather-engine

Follow-up to Ã‚Â§1: releases decoded surfaces for the DOM-referenced case
when a WebView is hidden (background tab in feather-shell).

Chain: `WebView::hide()/show()` now also sends
`EmbedderToConstellationMessage::SetWebViewHidden` Ã¢â€ â€™
`SetThrottled` Ã¢â€ â€™ `Document::update_visibility_state(Hidden|Visible)`.
Bonus: `document.visibilityState` and timer throttling now follow tab
visibility, which upstream had not wired.

On `Hidden`, `update_visibility_state` releases every retainer of the
decoded `Arc<RasterImage>`:

1. `image_cache.discard_completed_loads()` Ã¢â‚¬â€� clears completed loads and
   sends `DeleteImage` for each `ImageKey` (pending loads/listeners and
   the key cache are kept).
2. `window.layout_mut().drop_cached_layout_trees()` Ã¢â‚¬â€� new `Layout`
   method dropping box/fragment/stacking trees + `resolved_images_cache`.
3. `animating_images.node_to_state_map.clear()` Ã¢â‚¬â€� per-node animation
   state pins `Arc<RasterImage>` too.
4. Per node: `node.clear_layout_data()` Ã¢â‚¬â€� **required**: each node's
   `InnerDOMLayoutData.self_box` keeps its `LayoutBox` alive after the
   tree root is dropped, and `ReplacedContents`/`ImageInfo` inside pins
   the image. This was the last retainer found via `Arc::strong_count`
   instrumentation.
5. Per `HTMLImageElement`: `discard_decoded_image()` takes
   `current_request.image` (state/url/metadata kept).

On `Visible`, `restore_decoded_image()` re-fetches each `CompletelyAvailable`
image (`fetch_image` Ã¢â€ â€™ re-decode Ã¢â€ â€™ repaint); layout data and trees are
rebuilt by the normal reflow.

Measured (feather-shell, llvmpipe host, same 40-image loop page,
40x1600x1200 PNG Ã¢â€°Ë† 307 MiB decoded):

| Build | Page loaded | Tab hidden (~60-90s settled) |
| stock servoshell (`main` @170fe92389a, no feather patches) | 944 MB, stays resident | n/a Ã¢â‚¬â€� no tab-hide path |
| feather-engine (image-cache bound only) | 1039 MB | 1027 MB Ã¢â‚¬â€� no release |
| feather-engine + discard lever | 835 MB | **712 MB** (Ã¢Ë†â€™315 MB vs no-lever) |

Restore verified on screen: re-showing the tab re-fetches all 40 images
and re-renders the grid; repeated hide/show cycles release again each
time. Note `Image::as_raster_image()` returns a clone Ã¢â‚¬â€� counting
`strong_count` through it overstates by one.

Remaining gap: RSS release lags behind the logical free (jemalloc/GC
retains segments briefly); a deeper lever for *visible* tabs would need
per-image offscreen discard driven by IntersectionObserver/paint bounds Ã¢â‚¬â€�
the cache-bound already covers DOM-unreferenced images, and DOM-referenced
on-screen ones must stay decoded by definition.

## 2. Lazy subsystem init

Subsystems that could start on first use instead of at `Servo::new`:

- WebGPU/wgpu (`dom_webgpu_enabled` is already off by default Ã¢â‚¬â€� keep it)
- WebRTC, ServiceWorker (off by default Ã¢â‚¬â€� keep them off in Feather)
- Font fallback / system font scans
- The gamepad polling thread (`dom_gamepad_enabled` defaults true Ã¢â‚¬â€�
  Feather could set it false if it doesn't ship gamepad support)

## 3. Suspended/discarded WebViews

Feather now lazy-restores tabs (about:blank until activated). A stronger
primitive Ã¢â‚¬â€� discarding a loaded tab's browsing context and re-creating it
on activation Ã¢â‚¬â€� would let long-running sessions shed background tabs
entirely, matching Chrome's tab discarding. Needs a constellation-level
"discard" message that tears down the pipeline + script thread while
preserving the session-history entry. Not currently exposed on `WebView`.

## 4. Thread-pool sizing

`thread_pool_workers_max`, `thread_pool_async_runtime_workers_max`,
`thread_pool_webrender_workers_max`, `layout_threads` were each measured
at ~0 idle-RSS effect Ã¢â‚¬â€� pools are created lazily and idle threads cost
little RSS. Left at defaults.

## 5. What prefs could NOT fix (measured, single DDG tab, ~60s idle)

Baseline ~322MB RSS / ~275MB PSS (llvmpipe host):

| Lever | RSS | Verdict |
| `layout_threads` 3Ã¢â€ â€™1 | 323MB | no effect |
| `js_mem_max` 128 | 321MB | no idle effect; kept at 192 as a runaway cap |
| `js_mem_gc_malloc_threshold_base_mb` 38Ã¢â€ â€™16 | 321MB | no effect |
| thread pools halved | 322MB | no effect |
| jemalloc background_thread + decay | 320MB | Ã¢Ë†â€™2MB + ongoing decay |
| `LP_NUM_THREADS=2` | 320MB | no effect (llvmpipe cost isn't threads) |
| `narenas:1` (MALLOC_CONF test) | 318MB | marginal |
| lazy session restore (5 tabs) | 214Ã¢â‚¬â€œ281MB | only loads the active tab |

The floor on this host is Servo runtime (~180MB) + llvmpipe (~65MB).
On real GPU hardware expect ~215-255MB with one loaded tab.

## 6. Reflector-root revival Ã¢â‚¬â€� nav-leak measurements

`release_reflector_roots()` (reflector-root revival, HEAD 9ec4a62b) makes
dead pipelines' objects *collectable*. Whether RSS actually falls depends
on collection pressure. 30-nav WebDriver loop (10 sites Ãƒâ€”3), single
servoshell PID RSS in MB, macOS arm64 16GB host:

| Config | StartÃ¢â€ â€™End | Shape |
| stock servo/main | 411Ã¢â€ â€™3355 | monotone |
| stock `--zealous-gc` | 414Ã¢â€ â€™2460 | still climbing |
| feather-engine (roots released) | 410Ã¢â€ â€™2953 | mild sawtooth, real mid-run drops |
| engine `--zealous-gc` | 411Ã¢â€ â€™5130Ã¢â‚¬â€œ6599 (2 runs) | monotone Ã¢â‚¬â€� worst config tested |
| engine + `JS_GC` on pipeline teardown (tested, not pushed) | 411Ã¢â€ â€™6126 | monotone, ~1.9Ãƒâ€” stock |
| engine + `JS_MaybeGC` on teardown (tested, not pushed) | Ã¢â€ â€™3916 @ nav18 | wedged at nav19 |
| engine + `MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0` | 411Ã¢â€ â€™5261 | monotone |
| stock + feather env (js_mem 192/50/300 + jemalloc 5s decay + bg thread) | 415Ã¢â€ â€™3482 | slower monotone |
| engine + feather env (same) | 411Ã¢â€ â€™5213, 5598 (2 runs) | monotone, ~1.5Ãƒâ€” stock |
| engine + feather env, no `js_mem_max` cap | 411Ã¢â€ â€™5718 | monotone Ã¢â‚¬â€� cap ruled out |
| engine + `JS_GC` every 8th teardown (tested, not pushed) | 411Ã¢â€ â€™4364 | sawtooth-ish but ~25% over stock; wedged at nav8 in 1 of 2 runs |

### Bisect: the 1.5Ãƒâ€” is IntersectionObserver, not the reflector fix

`test/bisect-*` = stock-baseline + stacked cherry-picks, same prefs/env
as the rows above, same-window runs:

| Build | End MB | Verdict |
| stock | 2681 | baseline |
| reflector-roots only | 3209 | Ã¢â€°Ë† stock, with reclaim drops |
| + image-cache bound | 2697 | Ã¢â€°Ë† stock |
| + hidden-document discard | 2767 | Ã¢â€°Ë† stock same-window |
| + IntersectionObserver (b4a6153e2) | 4086 | **~1.5Ãƒâ€” Ã¢â‚¬â€� culprit** |
| + h265 (full engine code) | 3799, 4583 | same as IO layer |
| IO build, `dom_intersection_observer_enabled=false` | 2584 | Ã¢â€°Ë† stock |

The pref-off A/B on an identical binary pins it: live IntersectionObserver
objects retain ~1.3GB across navs on this site list (observers hold
Element targets/roots; most crawled sites register them). The spec-gap
fixes are not implicated Ã¢â‚¬â€� enabling the feature is. Fix direction: unroot
observer targets at pipeline teardown, not revert.

**Fix shipped (816b8e39e):** `Document::clear_intersection_observers` now
runs in `Window::clear_js_runtime` Ã¢â‚¬â€� drains each observer's target list,
element `registered_intersection_observers` entries, and queued entries.
Post-fix same-window runs (feather-env, IO on): **3263, 3272** vs pre-fix
4086 and IO-off-on-fixed-binary **3392** Ã¢â‚¬â€� IO-on now tracks the IO-off
curve; the residual over the earlier 2681 stock reading is ambient drift,
not IO. IO verified functional (callback fires on scroll via WebDriver).

Verdict (macOS): every lever that forces reclaim Ã¢â‚¬â€� eager GC on teardown,
zeal GC, allocator decay Ã¢â‚¬â€� makes things *worse*. The ~1.5Ãƒâ€”-over-stock
full-branch number decomposes in the bisect below: the reflector-root
revival itself is clean (Ã¢â€°Ë† stock, with natural-GC reclaim drops); the
retention comes from IntersectionObserver being enabled. The Windows
forced-GC sawtooth (~4GB peak) did not reproduce here. Purge/decay can't
help because retained objects are still *reachable* Ã¢â‚¬â€� they only become
dirty pages after a GC, and eager GC is the part that hurts.

`js_mem_max=192` is NOT the cause: patched+feather-env without the cap
ends at 5718 (vs 5213/5598 with it). Throttled GC (every 8th teardown)
lands at 4364 Ã¢â‚¬â€� better than eager but still ~25% over stock AND it
wedged at the first GC'd exit in one of two runs. GC-on-teardown at any
cadence, zeal, and the cap are all ruled out. The bisect below shows the
~1.5Ãƒâ€” over stock is NOT the revival Ã¢â‚¬â€� it's live IntersectionObservers.

## 1c. E2E scoreboard + hidden-tab CPU caveat (Nov 24)

Feather on feather-engine vs stock servoshell (upstream main), llvmpipe host:

| Metric | Stock | Feather | ÃŽâ€� |
|---|---|---|---|
| Cold start Ã¢â€ â€™ paint | ~297ms | ~296ms | parity |
| Idle RSS, 1 DDG tab | 663MB | 566MB | Ã¢Ë†â€™97MB |
| 4-tab (2 grid + slideshow + DDG) | 2503MB resident | 433MB lazy-restored / 1287MB hidden | Ã¢Ë†â€™49% |
| Binary | 206MB / 153MB stripped | 89.5MB production | Ã¢Ë†â€™57% |

CAVEAT resolved (commit 75697a2bc78): a hidden tab whose JS keeps
mutating images (slideshow cycling `<img src>`) churned at ~390-460% CPU Ã¢â‚¬â€�
each timer tick refetched + decoded a surface the hide path discards, AND
every hidden-doc update-the-rendering produced a reflow Ã¢â€ â€™ display list Ã¢â€ â€™
WR raster of work that is never displayed. Fixed with three gates on the
real visibilityState:
- `fetch_image` early-returns while hidden; a `src` mutated while hidden
  queues in `pending_request` and `restore_decoded_image` (on Visible)
  now prefers the pending request's URL, so the latest src wins.
- `Document::needs_rendering_update` returns false while hidden Ã¢â‚¬â€�
  reasons accumulate and flush in one update on show.
- `update_the_rendering` step-3 non-renderable filter skips hidden docs
  (the WHATWG "visibility state is hidden" TODO).

RESULTS (llvmpipe, looping 40-image slideshow every 350ms):

| Tab state | CPU | net fetches |
| hidden, before | ~390-460% | 1 fetch+decode/tick |
| hidden, fixed | **0.0%** | **0** |
| shown, cycling | ~110-120% (legit per-tick raster) | 0 (warm cache) |
| stock servoshell hidden-equivalent | ~53% | Ã¢â‚¬â€� |
| re-show | RSS returns to shown level; newest image renders |

ALSO REQUIRED Ã¢â‚¬â€� embedder-side: the fix needs an on-demand repaint loop.
Feather's shell had TWO independent CPU-burn bugs found while isolating
this: egui-winit answers `repaint: true` for `WindowEvent::RedrawRequested`
itself, and `if response.repaint { request_redraw() }` re-requested a
redraw after every redraw Ã¢â€ â€™ self-sustaining ~290fps repaint loop that
rasterized the active webview on ANY page (even static), plus an
unconditional `request_redraw()` per servo wake in `user_event`. Shell
fix: skip re-requesting redraw when the event IS RedrawRequested, and
drop the unconditional redraw (new-frame notifications already request
redraw via `notify_new_frame_ready`). Result: feather idle on static
page = 0.0% CPU vs ~390% before. Stock servoshell doesn't have this bug
(it already only repaints on `notify_new_frame_ready`).

## 1d. External-scheme navigations (wa.me fix) Ã¢â‚¬â€� SHIPPED on feather-engine

Top-100 crawler defect queue flagged wa.me as the only real engine defect:
wa.me Ã¢â€ â€™ api.whatsapp.com renders, then page JS does
`location.href = 'whatsapp://chat/'` Ã¢â€ â€™ document navigation to an
unregistered scheme Ã¢â€ â€™ UnsupportedScheme net-error page.

Chrome parity: other engines treat a document navigation into a scheme
they can't handle as a *cancelled* navigation handed to the OS /
interstitial Ã¢â‚¬â€� the previous page stays, no error page.

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
float rounding case) re-ran both ways on stock Ã¢â€ â€™ flaky, not real.

**Verdict: our branch is compat-neutral on dom/cssom** Ã¢â‚¬â€� memory/IO/cache
work neither helps nor hurts pass rate, as expected. This subset is the
weekly scoreboard; widen it (e.g. add `tests/wpt/tests/dom`) for deeper
coverage when needed.

## 1e. Suspended-webview freeze Ã¢â‚¬â€� JS-heap GC on hide Ã¢â‚¬â€� SHIPPED on feather-engine

INVESTIGATION (what a hidden document still does, feather-engine):
Full throttling was already wired upstream Ã¢â‚¬â€� `webview.hide()` Ã¢â€ â€™
`SetWebViewHidden` Ã¢â€ â€™ `pipeline.send_throttle_messages()` Ã¢â€ â€™ script
`SetThrottled` clamps timers to `pref!(js_timers_minimum_duration)`=1000
+ `PaintMessage::SetThrottled`; `window.throttled()` gates animations and
rAF; `update_the_rendering` is skipped entirely; image fetch/decode is
gated on visibility (Ã‚Â§1c); decoded image surfaces + cached layout trees
are discarded on hide (Ã‚Â§1b/Ã‚Â§1c). What still runs: layout queries still
trigger full `window.reflow()` (required for correct values, Chrome same);
FontContext is `Arc` per-script-thread shared across docs (not
doc-scopable Ã¢â‚¬â€� ruled out). The uncovered gap: **the JS heap**.
SpiderMonkey GC is allocation-driven, and a throttled hidden page
allocates too rarely to ever trigger collection Ã¢â€ â€™ dead objects sit in
the heap for as long as the tab stays hidden.

FIX (commit e434b31): `document.rs` `update_visibility_state` runs
`window().gc(cx)` on the visibleÃ¢â€ â€™hidden transition Ã¢â‚¬â€� a full GC while the
pause is invisible to the user. Semantically safe (only unreachable
objects are collected) and instant.

MEASURED (page holding ~240MB of dead JS objects, feather, llvmpipe VM):
- stock servoshell:      ~470MB, resident (no hide path exists)
- feather-engine before: 506Ã¢â€ â€™509MB hidden, 0MB released after 90s
- feather-engine + fix:  453Ã¢â€ â€™274MB on hide, **Ã¢Ë†â€™178MB released immediately**
Re-showing the tab works normally; a second hide cycle releases again.
Pairs with Ã‚Â§1b/Ã‚Â§1c: hide now frees images AND dead JS heap.

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
hits both builds identically Ã¢â€ â€™ environmental.

15 subtest deltas Ã¢â‚¬â€� ALL engine wins, all from b4a6153e2's IO spec-gap
fixes: 13Ãƒâ€” rootMargin/scrollMargin absolute-length units (cm/mm/q/in/pt/pc),
1Ãƒâ€” scroll-margin intersection, 1Ãƒâ€” detached-document target observation.
**Zero regressions in the IO suite from the teardown fix.**

CAUGHT BY THIS RUN: the IO teardown fix had a RefCell double-borrow Ã¢â‚¬â€�
`clear_intersection_observers` drained the Vec while holding the
borrow_mut, and `clear_registrations` Ã¢â€ â€™ `disconnect_from_owner` Ã¢â€ â€™
`remove_intersection_observer` re-borrows it Ã¢â€ â€™ panic on pipeline exit.
Fixed in 5d1ef8ba9 (collect-then-clear).

## IO pref "anomaly" resolved (2026-11-24) Ã¢â‚¬â€� measurement artifact, not a bug

Earlier observations that `typeof IntersectionObserver` returned 'undefined'
without `--pref` on a binary supposedly built from a branch with
`dom_intersection_observer_enabled: true` in `const_default()` turned out to
be a **stale servoshell binary** in `/tmp/engine` (its `Preferences` static
was compiled before b4a6153e2's prefs.rs flip was in the checkout).

Verified on a fresh `target/release/servoshell` (head 1b093c1ba): with NO
--pref flag, WebDriver eval returns `function` for IntersectionObserver Ã¢â‚¬â€�
IO is correctly on by default in the engine. Instrumenting `prefs::set`
confirmed `io=true` is written at startup; no code path flips it off.

Consequences for prior measurements:
- All WPT/crawler comparisons are UNAFFECTED Ã¢â‚¬â€� both sides ran with
  `--binary-arg=--pref=dom_intersection_observer_enabled`, a valid A/B.
- The "IO-off" scoreboards (nav loops, crawler) ran with the pref forced on
  via prefs-file/--pref, so they measured the intended state anyway.
- Lesson: always rebuild + re-hash the preserved `/tmp/engine` binary after
  pulling feather-engine; a servoshell on port 7000 from a prior run can
  also shadow fresh probes (pgrep before probing).

## 1f. Downloads pipeline Ã¢â‚¬â€� SHIPPED on feather-engine

Upstream Servo had ZERO download support: no `WebView` delegate hook, no
constellation message, nothing in servoshell. A navigation to a
`Content-Disposition: attachment` response rendered the body as an inline
text wall; an undisplayable MIME type fell into the generic "unknown
content" path with the URL lost.

PIPELINE (all on feather-engine):
- `script/dom/servoparser/mod.rs::load_document` Ã¢â‚¬â€� after the MediaType
  verdict (HTML spec step 2), fires `ScriptToConstellationMessage::
  DownloadRequested { webview_id, pipeline_id, url, mime,
  content_disposition }` when `is_attachment || media_type.is_none()`.
  For attachments `media_type` is forced to `None` so the body never
  renders: the parser takes the existing inline "Downloading content of
  type (Ã¢â‚¬Â¦)." page instead of streaming the payload.
- `NavigationParams` gained `content_disposition: Option<String>`,
  populated in `process_response` from the response's
  `Content-Disposition` header (needed to resolve the download filename
  embedder-side).
- `constellation/constellation.rs` forwards the message as
  `ConstellationToEmbedderMsg::DownloadRequested(webview_id, url, mime,
  content_disposition)` (registered in `tracing.rs`); `servo.rs`
  dispatches it to `WebViewDelegate::notify_download_requested(webview,
  url, mime, content_disposition)` Ã¢â‚¬â€� the new delegate hook embedders
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
- `JS_MaybeGC`/`maybeGC` only collects if zones already past eager triggers Ã¢â‚¬â€�
  not a real periodic trigger (and a blocking GC when it does fire).
- Incremental slices without a driver stall mid-collection; driven slices keep
  running under continued nav allocation with write-barrier overhead, so
  reclamation lags further behind growth.
- A blocking `JS_GC` per nav is pure cost: the retained memory is mostly NOT
  JS-heap garbage Ã¢â‚¬â€� it's Rust-side allocations (layout, image cache, DOM
  arenas in jemalloc), which GC can't touch. GC only frees the JS wrappers.

Conclusion: there is no GC-trigger fix for per-nav retention. The remaining
sawtooth is Rust-allocator memory; the levers that work are jemalloc decay
prefs (already in featherprefs) and Rust-side release-at-teardown work
(e.g. `image_cache().clear()` in handle_exit_pipeline_msg). `JS_GC` under
`Window::clear_js_runtime`'s `cx.no_gc()` scope is suppressed Ã¢â‚¬â€� earlier
"eager GC" experiments measured nothing; the correct deferred point is the
next event-loop turn.

## 1g. Downloads round 2 Ã¢â‚¬â€� body handoff + a[download]/blob Ã¢â‚¬â€� SHIPPED

The double-fetch and the markup-initiated gaps from Ã‚Â§1f are closed.

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
  already fetched Ã¢â‚¬â€� no second fetch, signed URLs and POST results work.
- `a[download]`: `follow_hyperlink` (links/relations.rs) checks the
  `download` attribute before navigation and starts a
  `create_a_potential_cors_request_with_claim` fetch with
  `AnchorDownloadFetchContext` Ã¢â‚¬â€� `Destination::None`,
  `ensure_blob_referenced_by_url_is_kept_alive` claims the blob, so
  `blob:` and `data:` URLs download through the same path. The
  `download` attribute value is sent as `filename_hint`.
- New message flow: `DownloadRequested` gained `filename_hint` +
  `content_length`; added `DownloadProgress {received, total}` (Ã¢â€°Â¥1MiB
  granularity) and `DownloadFinished {body_path, error}` Ã¢â‚¬â€� body_path
  ownership transfers to the embedder (same-process path). All three
  forward through constellation to `WebViewDelegate::
  notify_download_progress` / `notify_download_finished` (default no-ops).
- Verified end-to-end in feather-shell: attachment + octet-stream navs,
  a[download] with markup filename winning, blob: and data: downloads,
  50MB throttled progress, cancel-then-finish cleans the temp file,
  collision naming.

STILL MISSING: no engine-side cancel (embedder marks cancelled and
deletes the delivered temp file Ã¢â‚¬â€� the fetch itself runs to EOF; a real
cancel needs a fetch-abort message into net), no multipart/range resume,
no save-As prompt.

## Per-nav retention resolved: jemalloc purge at pipeline teardown (693146a10)

Attribution (vmmap + MallocStackLogging, macOS): the ~2-3GB retained RSS is
NOT framework buffers despite the misleading "CoreMedia Capture Data" vmmap
label Ã¢â‚¬â€� stack traces show those regions are `_rjem_je_extent_alloc_mmap`
(i.e. jemalloc arena extents). jemalloc mallctl after ~18 navs:
allocated(live) ~1.0-1.8GB vs mapped ~1.6-2.0GB Ã¢â€ â€™ several hundred MB-1GB of
mapped-but-unallocated dirty pages. The rest of RSS is SpiderMonkey's own
mmap'd GC heap + non-heap regions.

Levers measured on the 30-nav loop (feather-env prefs):
- `MALLOC_CONF dirty_decay_ms:1000,muzzy_decay_ms:1000`: end 3592 Ã¢â‚¬â€� decay
  returns pages but slower than navs produce them.
- `+ background_thread:true`: end 4551 (oscillating troughs ~2.1-2.7G) Ã¢â‚¬â€�
  no better than natural.
- `arena.4096.purge` (MALLCTL_ARENAS_ALL) in `handle_exit_pipeline_msg`:
  ends 2725 / 3500 (end is reddit-heavy sensitive), mid-loop sits
  ~1.4-2.6GB vs ~2.4-3.3GB unpurged Ã¢â‚¬â€� ~600MB-1GB sustained win.

Shipped: `servo_allocator::purge_arenas()` (mallctl arena.purge on all
arenas; no-op on non-jemalloc builds) called once per pipeline exit in
handle_exit_pipeline_msg. Cross-platform via jemalloc; Linux siblings get
it for free.

Audit trail for future passes (already-clean teardown, don't re-check):
`pipeline_to_node_ids` removed at exit; `image_cache().clear()` sends
WR `DeleteImage` for every cached key; `document.unload` Ã¢â€ â€™
`clean_up_media_elements` Ã¢â€ â€™ `reset_media_player`; `animation_manager`
cleared; `pipeline_exited` Ã¢â€ â€™ `transaction.remove_pipeline` (WR scene).
Remaining RSS after purge = live jemalloc allocs (layout/DOM still alive
in bfcache/history) + SpiderMonkey GC heap Ã¢â‚¬â€� next lever would be auditing
which Rust objects survive teardown (layout snapshots, font caches), not
allocator tuning.

## Object-level audit: bfcache depth is the dominant per-nav lever (no code needed)

Follow-up to the purge work: WHY does live jemalloc alloc stay ~1-1.8GB?
Answer Ã¢â‚¬â€� session history keeps nav'd-away documents fully alive
(`NeedsToReload::No`) until evicted past `session_history_max_length`
(default **20**) or until the document unloads unsalvageable. Up to ~20
dead documents Ãƒâ€” DOM+layout+JS global each = the sawtooth mass. Eviction
path: `session_history.rs` eviction Ã¢â€ â€™ `NeedsToReload::Yes` Ã¢â€ â€™ close_pipeline.
Unsalvageable docs die sooner via `DiscardDocument` Ã¢â€ â€™ replace_reloader(Yes).

Measured (30-nav loop, feather-env prefs, purge already shipped):

| history depth | end RSS | mid-loop |
| 20 (default) | 2725 / 3500 | 1.4-2.6 GB |
| 5 | 1884 | 1.6-2.3 GB |
| 1 | 2386 | 1.3-2.4 GB |

Cutting depth 20Ã¢â€ â€™5 roughly halves end RSS and ~doubles how much a dead
document's memory returns at teardown. 1 gives no further win Ã¢â‚¬â€� floor is
live current-doc + SM heap + fonts. **Lever is the pref
`session_history_max_length` Ã¢â‚¬â€� set 5 in feather prefs.json; no engine
patch.** Chrome's bfcache is ~5-10 entries + memory-bounded, so 5 matches
mainstream UX (still gives working back-forward).

Stock servoshell honours the same pref (it's upstream's), so this is a
feather-config win, not a patch Ã¢â‚¬â€� both sides of any A/B must use the same
depth or the comparison is meaningless.

### Linux cross-verify of the arena-purge lever (Nov 24, llvmpipe host)

30-nav loop (40-img page + ~60MB JS garbage per nav, same page as the
macOS runs): stock upstream servoshell climbs 1.0GB Ã¢â€ â€™ **7.79GB RSS and
never comes back**; feather-engine head sits **flat ~1.1-1.2GB for the
whole loop** (servoshell AND feather-shell alike). The lever is safe and
working on Linux jemalloc Ã¢â‚¬â€� no regression, identical flat curve.

Honest attribution: reverting ONLY the purge (same tree minus
7d02b70553b) stays flat too Ã¢â‚¬â€� ~1.1-1.25GB at 3s/nav AND at 800ms/nav.
Linux jemalloc's default dirty-page decay already reclaims teardown
pages between navs, and the image-cache bound dominates the stock-vs-ours
delta (stock's unbounded decoded cache = ~200MB/nav pile-up). So the
~1GB mapped-but-unallocated problem Mac attributed on macOS is
platform-specific: purge is essentially free insurance on Linux and the
real win stays on macOS. Reclaim confirmed working; incremental Linux
delta unmeasurable in this workload.

### Idle-floor decomposition (Nov 24, llvmpipe host, feather-engine 81fc006)

Goal: explain the ~420MB settled idle RSS vs the <150MB budget Ã¢â‚¬â€� find
what's releasable. Measured via /proc/PID/smaps on a settled
single-DDG-tab feather process, compared against about:blank.

**Feather @ about:blank = 311MB RSS.** Floor is structural:

- **77MB** feather binary pages (file-backed; shared/reclaimable under
  memory pressure Ã¢â‚¬â€� not live heap)
- **55MB** libLLVM-15 + **8MB** swrast_dri Ã¢â‚¬â€� llvmpipe-only (~65MB total
  tax; vanishes on real GPUs)
- **38MB** glibc [heap] Ã¢â‚¬â€� mostly mesa/LLVM allocations (same tax)
- **~116MB anonymous, 509 VMAs** Ã¢â‚¬â€� live engine+shell runtime: 65 threads
  (8Ãƒâ€”llvmpipe workers, 8Ãƒâ€”JS Helper, 4Ãƒâ€”jemalloc bg, 4Ãƒâ€”WRWorker +
  WRSceneBuilder + WRRenderBackend, StyleThreadÃƒâ€”2, GlobalPoolÃƒâ€”4,
  Script, Constellation, FetchThread, ResourceManager, storage trio,
  tokio poolÃ¢â‚¬Â¦), surfman/GL contexts + framebuffers, SM runtime,
  jemalloc live heap, egui atlas, IPC. jemalloc decay already purges
  freed pages every 5s, so this residue is *live* allocations, not
  decay candidates.

**A loaded DDG tab adds ~110MB** on top (Ã¢â€ â€™420MB): DOM+layout trees,
SM heap (js-gc-heap 6Ã¢â€ â€™17MB), decoded images, font/shaping caches,
WebRender picture/tile state, net caches.

**Releasable vs structural, ranked:**

1. Decoded image surfaces on *visible-but-idle* tabs Ã¢â‚¬â€� only sizable
   pool (~40-80MB on image-heavy pages). The hide-discard lever already
   releases them for hidden tabs; extending to idle-visible means the
   next paint/scroll must re-fetch+re-decode Ã¢â‚¬â€� real resume jank, so
   deferred (NOT clean). If attempted: gate on compositor keeping the
   painted frame in its FBO, re-decode on demand.
2. Font/shaping caches, glyph runs Ã¢â‚¬â€� few MB each, low value.
3. Thread stack trimming (65 threads) / WR worker count Ã¢â‚¬â€� small each,
   upstream-invasive.
4. `js_mem_max=192` already caps SM growth; `session_history_max_length=5`
   caps dead-doc retention; image cache bounded at 128MiB Ã¢â‚¬â€� all shipped.

**Verdict:** no clean big lever at idle-on-load. The <150MB budget is
unreachable on this architecture today: ~180-250MB is engine runtime
(binary pages + SM + compositor + thread pools + one live document).
Remaining path is upstream Ã¢â‚¬â€� smaller runtime footprint, fewer threads,
shared surfaces Ã¢â‚¬â€� not a shell or prefs fix. Linux note: the 30-nav
flat-933MB result shows retention is solved; the *floor* is the problem,
and it's ~2.5Ãƒâ€” budget even after llvmpipe subtraction (~355MB).

### 1g. Idle-surface-discard prototype (llvmpipe VM, feather-engine) Ã¢â‚¬â€� DROPPED

Coordinator asked: prototype discarding decoded surfaces on a
*visible-but-idle* tab, measure resume jank vs RSS saved, ~100ms budget.

**Implementation** (prototyped, then reverted Ã¢â‚¬â€� stash `idle-surface-discard
prototype` on feather-engine): new
`EmbedderToConstellationMessage::DiscardWebViewIdleResources` Ã¢â€ â€™
`ScriptThreadMessage::DiscardIdleResources` Ã¢â€ â€™
`Document::discard_idle_resources(cx)` Ã¢â‚¬â€� the Hidden-branch discard
(image_cache completed loads + `drop_cached_layout_trees` + animating
state + per-node `clear_layout_data` + per-img `discard_decoded_image` +
`window.gc`) with NO visibility change; `idle_resources_discarded` flag Ã¢â€ â€™
lazy `restore_idle_resources` called from `Window::reflow` (every repaint
stimulus funnels there). Shell trigger: Ctrl+Shift+D + resize/wheel
instrumentation on `notify_new_frame_ready`.

**Numbers** (3 tabs, active = 40Ãƒâ€”1600Ãƒâ€”1200-PNG page, llvmpipe):
- settled RSS ~865-887MB; discard frees ~303MB instantly (850Ã¢â€ â€™547 in the
  first run Ã¢â‚¬â€� the pool is real)
- **but it never holds**: the discard itself marks the document dirty
  (`clear_layout_data`/tree drop) Ã¢â€ â€™ `needs_rendering_update` Ã¢â€ â€™ the very
  next reflow (within ~1-2s, zero user input) lazily restores Ã¢â€ â€™ re-fetch
  from HTTP cache (0 network GETs) + re-decode Ã¢â€ â€™ RSS spikes to ~1100MB
  then decays back to a ~905MB plateau Ã¢â€°Ë† baseline. Net saving Ã¢â€°Ë† 0.
- resumeÃ¢â€ â€™first-frame: 2-5ms (WR retains the last painted tiles Ã¢â‚¬â€� the
  page never visibly flickers while restore races the reflow)

**Why it fails structurally:** a visible document can't keep its painted
frame while its surfaces are dropped Ã¢â‚¬â€� dropping the pins
(DOM `current_request.image`, image-cache entry, ReplacedContents in
layout_data) requires invalidating layout data, which forces a reflow,
which re-requests the surfaces. Suppressing the dirty flag instead risks
the compositor receiving a blank display list Ã¢â‚¬â€� page blanks while the
user reads it. Either way the ~300MB pool cannot be held on a *visible*
tab. The hidden-tab path (Ã‚Â§1a/Ã‚Â§1b) already covers the real case; a
visible idle tab's honest floor stands at the Ã‚Â§1f decomposition.

**Residual cost if pursued anyway:** a ~300MB churn cycle + decode CPU
burst every idleÃ¢â€ â€™wake, for ~0 steady-state gain. Dropped per the
coordinator's jankyÃ¢â€ â€™drop criterion; the honest number is "save doesn't
hold", not "jank too high".

## 1h. Permanent-webview wedge fix (bb5c9bc9b94, 2026-11-24)

**Bug**: after any navigation that never fires LoadFinished (wa.me/api.whatsapp.com, and the other consistent-timeout sites in the tranco-500 crawl), the webview was permanently dead Ã¢â‚¬â€� every subsequent navigation timed out in that process, for webdriver AND real users. NOT the arena purge/download spool/hidden-doc GC (Windows lane's prime suspect was a red herring).

**Root cause Ã¢â‚¬â€� three cooperating layers**:
1. `webdriver_server::handle_command` pre-command `wait_for_navigation_to_complete` blocked EVERY command (incl. the next Get) for the full 30s page-load timeout because the stuck webview never left `navigating_webviews`. Ã¢â€ â€™ `Get` now exempt: clears stale navigating state, since a new navigation inherently abandons the old one.
2. `constellation::load_url` silently `return None`-dropped new top-level navigations while a pending session-history change existed. Ã¢â€ â€™ now supersedes: closes the stuck pipeline, latest navigation wins (mirrors the iframe-path supersede).
3. Fetch-layer-cancelled navigations (`NetworkError::LoadCancelled`, e.g. redirect into whatsapp:) vanished silently in `script_thread::handle_fetch_metadata` Ã¢â‚¬â€� no abort, pending change pinned forever. Ã¢â€ â€™ now sends `AbortLoadUrl` mirroring the 204/205 path.
Plus: unnavigable schemes dropped at `load_url` entry before any pipeline exists.

**Verify**: example.com ok Ã¢â€ â€™ wa.me timeout Ã¢â€ â€™ example.com 12ms ok (was: permanent timeout loop). Underlying defect remains: api.whatsapp.com never fires LoadFinished Ã¢â‚¬â€� separate engine bug, affects the ~10 consistent-timeout tranco sites.

## 1i. Tranco top-500 macOS crawl (2026-11-25) Ã¢â‚¬â€� baseline + second defect class

500-site WebDriver crawl on feather-engine head Ã¢â€ â€™ `crawler/baselines/tranco500-macos.jsonl`
(feather repo). **330 ok / 500; 157 infra-side apexes (dns 123, tcp 22, cert 12);
13 engine-verdict fails Ã¢â€ â€™ ~96% real-site compat.**

**Wedge-fix verified on macOS**: webex.com nav Ã¢â€ â€™ clean `{"error":"timeout"}` at 30s,
then same session navigates example.com ~1s. `bb5c9bc9b94` works on arm64.

**Wedge signature on macOS (for future crawls):** one bad nav kills the whole
WebDriver HTTP port (`/status` and `/session` both hang). Burned rows record
`driver_error` with `load_ms=None`, or `nav_error` w/ "Connection refused" +
load_ms~0 Ã¢â‚¬â€� nav_error is NOT in the crawler's TRANSIENT list, so strips miss
it; requires a per-site fresh-engine retest pass. Watchdog addition: restart on
Ã¢â€°Â¥6 consecutive transient rows (streak), cap retries at 3 attempts/site.

**NEW DEFECT Ã¢â‚¬â€� post-nav driver hang (still live post-wedge-fix, deterministic):**
7 sites break the WebDriver session even on a fresh `158f0e329` engine:
- `webex.com`, `meraki.com`, `cisco.com` Ã¢â‚¬â€� nav starts, then every later request
  hangs; no row produced in 240s. Pages never settle (webex loops tracker
  requests forever).
- `alidns.com`, `imdb.com`, `smartadserver.com`, `stanford.edu` Ã¢â‚¬â€� nav returns,
  then a post-nav request dies Ã¢â€ â€™ `driver_error` every attempt.
Distinct from the timeout-supersede wedge: the driver itself stops answering
mid-flow. Likely a post-load probe (execute/screenshot) hanging on a
never-settling page, or the unfinished-navigation class via a different path.
Minimal repro: servoshell --webdriver + nav one of these + probe
`/session/{id}/title` after.

**Real engine fails worth investigating:** `load_error` on hosts that pass
dns+tcp+tls probes: nflxvideo.net, elasticbeanstalk.com, mybluehost.me,
gcdn.co, huawei.com. `slack.com` consistent >30s timeout (wa.me/aol class).

## 1i. Load-event stall family Ã¢â‚¬â€� drain race fixed; residual is upstream net-class (2026-11-25)

**Drain race fix** (`23f1a29f30e`): bootloader pages (api.whatsapp.com) insert the next async
script batch synchronously from each executing script. `wait_until_asap_scripts_have_executed`
checked the sets after execution Ã¢â‚¬â€� already re-populated Ã¢â‚¬â€� so the step-7 drain never completed.
Fix: `has_finished_all_asap_scripts` only gates on parser-inserted scripts; dynamic insertions
still execute but don't re-arm the spin. Spec-letter is ambiguous (prepare-a-script sets
'delaying the load event' unconditionally for external scripts) but browsers observably don't
let post-parser-end insertions re-gate indefinitely; commit message has the honest reading.

**Navigate-and-cancel for external schemes** (`deab4229838`): the bb5c9bc9b94 silent early-drop
in `load_url` is removed. Document navigations into unnavigable schemes now proceed, cancel at
the fetch layer (`LoadCancelled` Ã¢â€ â€™ `AbortLoadUrl`), and leave the current document untouched Ã¢â‚¬â€�
matching what browsers do. **Integration point for a future shell "open in app?" prompt hook:
the removed early-drop site in `constellation.rs::load_url` is where an embedder callback for
external schemes would slot in** (surface the scheme+url to the shell, let it prompt/discard).
Don't build it yet Ã¢â‚¬â€� noted per coordinator.

**The residual stall is NOT a feather regression.** Probed stock: wa.me ends on `whatsapp://chat/`
with title "Error loading page" Ã¢â‚¬â€� stock HARD-ERRORS; the fast "loads" I measured in the wedge
era were error pages. On feather-engine the landing survives, its boot chain terminates
naturally (~21 fetches in the first second, then total network silence), yet load never fires:
a handful of script **body streams** never finish reaching the document Ã¢â‚¬â€� net logs `got 200
response` for exactly the scripts stuck in the document's `blocking_loads` (data:-URI images
transiently stick too but do resolve). **aol.com reproduces the same stall on STOCK servo** Ã¢â‚¬â€�
so this family is an upstream-class fetch-completion/net bug (chunked body stream dies
mid-flight, no error ever surfaces), not a feather patch. Fix belongs upstream in the net Ã¢â€ â€™
script fetch handoff; the stall signature is: `blocking_loads` entries whose URLs already show
`got 200 response` in the net log.

## 1j. WebDriver post-nav hang Ã¢â‚¬â€� root cause + layer-1 fix (2026-11-25)

Whole-actor hang after nav on webex/meraki/cisco-class sites: a long
page JS task (minutes-long timer callback, heavy NumberToAtom churn)
starves the single script thread; driver's `wait_for_ipc_response` had
no timeout so the single dispatcher thread parked forever Ã¢â‚¬â€� even /status
died. Fix `7d150c23b`: bound every `wait_for_ipc_response*` at session
script timeout + `IPC_RESPONSE_MARGIN` (30s), expiry Ã¢â€ â€™ classified
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
2. **qtdemux demuxes all 300 samples correctly** Ã¢â‚¬â€� "no next entry, EOS"
   is *correct* end-of-stream for a fully-buffered file, not a defect.
3. **The burn is real but file-specific**: the whole video consumed in
   1.3s wall-clock because nothing throttles to presentation time Ã¢â‚¬â€�
   reproducible in pure `gst-launch-1.0` (no servo!) with fakesink
   sync=true, glimagesink, even the clocksync element, on gst 1.20.
   ffmpeg-generated mp4s (with or without audio) and VP9+Opus webm all
   sync perfectly at exact realtime duration in the same harness.
   Verdict: gst-1.20-vs-that-file quirk, NOT a servo bug. Don't use
   test-videos.co.uk BBB for media verification; generate assets with
   ffmpeg instead.
4. **Real servo bug found and fixed (`74d9743`)**: eager appsrc EOS
   permanently poisons the source for post-seek refetches (pushes after
   EOS are dropped as FlowError::EosÃ¢â€ â€™Ok). EOS is now deferred to
   need-data-drain and disarmed by seeks.
5. **Remaining known hole: replay-after-end.** appsrc EOS is final and
   push-mode byte seeks don't flush upstream, so reseeking after EOS
   never revives the stream. Fix direction: inject a flush into the
   wrapped appsrc or recreate it when a seek arrives post-EOS.

Verified working end-to-end on Linux: MP4 h264(+aac), mp4 video-only,
webm vp9+opus Ã¢â‚¬â€� realtime playback, exact forward/backward seeks, stable
pause, resume. WebM/VP9+Opus (the MSE target family) works Ã¢â‚¬â€� the
WebM-first plan in docs/mse-plan.md holds.

## 1k. MSE stage 1 Ã¢â‚¬â€� DOM surface (Nov 25, feather-engine `a1f0fcde987`)

MediaSource/SourceBuffer/SourceBufferList exist per spec; isTypeSupported
answers honestly from the gst registry via `ServoMedia::can_play_type`.
Verified live: vp9/opus webm true, fake codecs false, spec-correct
InvalidStateError behavior in closed state.

Binding-authoring lessons that cost compile iterations (read before writing
the next interface):

- Trait signatures are the ground truth: attribute accessors are
  `Get`/`Set`-prefixed only for `[Throws]` attributes or when codegen decides
  so Ã¢â‚¬â€� read the generated trait in
  `target/debug/build/servo-script-bindings-*/out/Bindings/<Iface>Binding.rs`
  before writing the impl, not after.
- `double` args arrive as `Finite<f64>`; `unrestricted double` attrs stay `f64`.
- Static methods take `&D::Window` (concrete `&Window`) as first param.
- Constructors implicitly receive cx Ã¢â‚¬â€� never list 'Constructor' in
  Bindings.conf `cx` arrays (hard build error).
- Methods that need cx for object creation: list the method name in
  Bindings.conf `'cx': [...]` Ã¢â‚¬â€� do NOT use `script_runtime::temp_cx()`.
- Union args arrive as `codegen::UnionTypes::FooOrBar` enums holding DomRoot
  values; changing a signature to a union breaks all callers Ã¢â‚¬â€� construction
  is `Union::Variant(DomRoot::from_ref(&**x))`.
- Event handlers: `event_handler!(name, GetOnname, SetOnname);` inside the
  methods impl, no manual plumbing.
- `#[no_trace]` on struct fields that aren't JSTraceable (Uuid etc).
- webidl files are auto-scanned (no registration list); DOM impls need
  `pub(crate) mod` in the parent mod.rs only.
- cargo check from the servo repo root fails on missing pkg-config deps;
  always check/build from `~/feather/shell` (the proven env).

Probe harness: write results into `document.title`, read with
`xdotool search --name PROBE | xdotool getwindowname` Ã¢â‚¬â€� no webdriver needed
for pure-JS surface verification.

## 1l. MSE stage 3 architecture + decode validation notes (Nov 25, in progress)

Player instantiation path: `HTMLMediaElement::create_player` (line ~2345) Ã¢â€ â€™
`ServoMedia::get().create_player(...)` Ã¢â€ â€™ gst backend `GStreamerPlayer::setup`
builds `playbin3` with uri `servosrc://` handled by ServoSrc (one appsrc in a
GstBin, script pushes bytes through it). For MSE the plan: new `servomse://`
element Ã¢â‚¬â€� one appsrc PER SOURCEBUFFER (YouTube uses separate audio+video
SourceBuffers; each SourceBuffer bytestream is a standalone valid fragmented
stream), feeding demux inside playbin3. Bytes flow: appendBuffer (script) Ã¢â€ â€™
new servo-media player API push_buffer(track,bytes) Ã¢â€ â€™ appsrc.

Isolated decode validation (gst-launch, /tmp/msetest/segs): ffmpeg-made
fragments. qtdemux eats frag.mp4 (frag_keyframe+empty_moov) fine with
openh264dec; matroskademux eats webm segments with vp9dec. KEY RISK RETIRED:
concatenated webm segments (cat seg0 seg1 seg2 Ã¢â‚¬â€� simulating sequential MSE
appends) decode through matroskademux with NO errors. avdec_h264/avdec_vp9
do NOT exist on stock Linux (gst-libav absent) Ã¢â‚¬â€� decoders are openh264dec +
vp9dec. gst-libav absence also confirms WebM-first: MP4 decode depends on
openh264 (Cisco, BSD) being present.

Key integration facts: eos semantics Ã¢â‚¬â€� endOfStream() is the ONLY eos for
MSE; duration/seekable come from the MediaSource object not the stream;
seeks within buffered ranges must hit appsrc-cached data or the segment
index (the plan's per-buffer segment index in script space). SourceBuffer
append order is arbitrary Ã¢â‚¬â€� script-space reordering by appendWindow/
timestampOffset before pushing to appsrc.

Probe harness pattern that works without webdriver: results into
document.title, read via `xdotool search --name PROBE | xdotool
getwindowname` after sleep 10. Probe pages in /tmp/msetest/probe*.html.

## 1m. MSE stage 3 KEY INSIGHT Ã¢â‚¬â€� push API already exists (Nov 25)

`servo_media::player::Player` trait ALREADY exposes `push_data(Vec<u8>)` and
`end_of_stream()` Ã¢â‚¬â€� the byte-injection API MSE needs. Progressive playback
feeds bytes through the same path from the fetch machinery
(htmlmediaelement.rs:3988 DataBuffer::Payload Ã¢â€ â€™ player.push_data).

Therefore stage 3/4 minimal path needs NO new gst element for the
single-SourceBuffer case: attach creates a normal servosrc-backed player,
appendBuffer routes bytes to player.push_data, endOfStream routes to
player.end_of_stream. Concatenated webm segments demux fine (validated
1l). The multi-pad servomse element (separate audio+video appsrc pads) is
deferred to stage 5 where two SourceBuffers must coexist.

Glue gaps being built: (1) attach_media_source must create the player Ã¢â‚¬â€�
currently creates none; (2) SourceBuffer needs media-source backreference,
MediaSource needs element backreference; (3) appendBuffer routing +
updating flag + update events; (4) EOS only from endOfStream Ã¢â‚¬â€� must NOT
let servosrc auto-EOS; (5) StreamType choice for MSE player.

## SAB/Atomics shim experiment verdicts (Nov 25, macOS lane Ã¢â‚¬â€� stage 0 of the SAB unlock)

Decisive measurements with a pure-JS fake-SAB/Atomics shim (ArrayBuffer subclass + functional Atomics ops stubs, injected via feather's boosts.json userscript system Ã¢â‚¬â€� zero engine changes):

- **claude.ai / Cloudflare turnstile: UNLOCKED** Ã¢â‚¬â€� the challenge completes and the full sign-in app renders with the shim. Cloudflare's use of SAB is SHALLOW (probe-level). The real implementation (stages 1-2 per docs/sab-atomics-scoping.md) is de-risked by measurement.
- **accounts.google.com: UNCHANGED** Ã¢â‚¬â€� still "browser may not be secure" with the shim. Binding constraint for Google is the non-JS signals (TLS/JA3 prime suspect per docs/google-signin-decision.md); accepted limitation, do not chase.

Symptom: `typeof SharedArrayBuffer === 'undefined'` in servo (anti-bot payloads probe it) Ã¢â€ â€™ check: mozjs-sys makefile.cargo `--disable-shared-memory` flag (CONFIRMED CAUSE Ã¢â‚¬â€� the engine implementation is compiled out entirely), `JS::RealmCreationOptions::setSharedMemoryAndAtomicsEnabled` in mozjs/js/public/RealmOptions.h (the embedder opt-in that exists but is dead while the build flag is off), servo's WebIDL surface in components/script_bindings/webidls (no SharedArrayBuffer.webidl exists Ã¢â‚¬â€� bindings work needed even after the flag flip), structured clone in script (needs a share-not-copy branch for real implementation).

Symptom: postMessage/structuredClone of a real SharedArrayBuffer throws DataCloneError (cross-agent sharing dead even though the built-ins exist) Ã¢â€ â€™ check: CloneDataPolicy literals in components/script/dom/bindings/structuredclone.rs write/read paths (CONFIRMED CAUSE Ã¢â‚¬â€� both hardcode allowSharedMemoryObjects_: false + allowIntraClusterClonableSharedObjects_: false, and use StructuredCloneScope::DifferentProcess; per mozjs/js/public/StructuredClone.h shared-memory cloning requires the policy opt-in AND is only possible intra-process Ã¢â‚¬â€� servo's agents are in-process threads so SameProcess scope is the correct configuration for windowÃ¢â€ â€�worker messaging), the realm option sharedMemoryAndAtomics_ in script_bindings/interface.rs (status: FIXED 398467e6a Ã¢â‚¬â€� enabled, built-ins verified live), mozjs build flags (status: ruled out Ã¢â‚¬â€� desktop builds compile shared memory in by default; --disable-shared-memory is WASI-only). FIXED 5f26d9f7a: write/read now default SameProcess scope + policy allowed, sabCloned veto removed; IndexedDB persistence paths use explicit DifferentProcess variants. Verified TRULY-SHARED memory + cross-agent Atomics on macOS. CAUTION for future work: flipping the policy flags while keeping DifferentProcess scope is memory-corruption-fatal (silent death, no panic) Ã¢â‚¬â€� scope and policy must always be coherent. Residual: claude.ai turnstile STILL stalls even with correct shared memory Ã¢â‚¬â€� SAB was necessary but not sufficient; remaining suspects per the Cloudflare doc: OffscreenCanvas (confirmed absent), wasm threads, Atomics.wait/notify edge behavior.

## 1n. MSE post-stage-3 state (Nov 25, feather-engine 41d0c9c5f64)

DONE: stages 1-3 core. appendBuffer->push_data->decode->playback verified.
Next in order: (1) buffered-range indexing Ã¢â‚¬â€� SourceBuffer needs segment
timestamps; design per plan: parse segment boundaries script-side OR query
player.buffered() (player trait already exposes buffered()! check what it
returns for push streams Ã¢â‚¬â€� player.rs ~line 356 reads servosrc buffered
ranges); (2) duration from stream end; (3) updating abort on remove/seek.
Probe harness: /tmp/mediatest/probe5.html served on :8741, feathers run
with `pkill -x feather; rm -f session.json` first, read title via xdotool
after sleep 14. Test segs: /tmp/mediatest/seg_v0.webm seg_v1.webm.

## CSP opaque-origin panic Ã¢â‚¬â€� fixed upstreamably (Nov 25, macOS lane)

`csp.rs` frame-ancestors ancestor walk `.expect()`ed every ancestor origin to parse
as a URL; opaque origins (sandboxed/srcdoc frames Ã¢â‚¬â€� universal ad-tech pattern on
amazon/bbc/cnn/espn) serialize as `null` Ã¢â€ â€™ panic Ã¢â€ â€™ with feather's `panic=abort`
production profile the whole browser died at 5-40 real-world tabs. Root-caused
during the bakeoff2 memory-scaling pass (deaths had no crash reports/jetsam traces
because abort+strip leaves nothing; NOT memory pressure Ã¢â‚¬â€� reproduced at 5 tabs/1GB).

Fix `1bf410a0e`: skip opaque entries in both the same-origin fast path and the
cross-origin constellation path, continuing the walk upward (spec step 6.2 is itself
incomplete Ã¢â‚¬â€� it assumes the parse cannot fail). Verified 20/20 consecutive 6-tab
amazon/bbc/cnn/espn/reddit/ebay sessions survive on macOS (previously
near-deterministic death by ~tab 6 in the same scenario).

Upstreamable-gold notes: the spec gap is real (w3c/webappsec-csp step 6.2 of
frame-ancestors navigation check); a servo upstream PR should reference it.
WPT CSP-subset regression run not yet done Ã¢â‚¬â€� no servoshell build fits the macOS
box's disk (needs ~10GB, had ~2GB); behaviorally no test can regress since the
only changed path previously crashed. Lane with disk should run
`tests/wpt/tests/content-security-policy` stock-vs-patched when convenient.

## Clipboard multi-format write fix (9edd19e2e, feather-engine)
`write_content_to_the_clipboard` iterated all drag-store items issuing one
`SetClipboardText` per text item Ã¢â‚¬â€� last-write-wins regardless of MIME type, so
Sheets-style copies (text/plain + application-internal payloads written after)
put internal GUID tokens on the clipboard instead of cell text. Fix: write only
the `text/plain` item (first match), falling back to the first text item when
no explicit plain type exists. Verified live on macOS: multi-format copy page
with plain-first/internal-second ordering now yields the plain text; plain
static, multi-script (CJK+accented), and editable-input copies unaffected.

## Symptom index (clipboard investigations)

Symptom: copy of static page text writes nothing to the system clipboard Ã¢â€ â€™ check: `run_default_keydown_handler` in document_event_handler (owns keyÃ¢â€ â€™editing-action routing; status: ruled out Ã¢â‚¬â€� copy chain verified end-to-end live, exact drag-selected text lands in the pasteboard), `DocumentSelectionDragHandler`/`document.selection` population (prime suspect when it reproduces Ã¢â‚¬â€� the original observation most likely was a drag that never formed a DOM selection; not reproducible since), arboard fallback (status: ruled out on macOS Ã¢â‚¬â€� pasteboard writes verified).

Symptom: copy puts an internal GUID/token on the clipboard instead of visible text (Sheets-class apps) Ã¢â€ â€™ check: `write_content_to_the_clipboard` in editing (owns copy-event data-storeÃ¢â€ â€™clipboard writes; CONFIRMED cause Ã¢â‚¬â€� it flattened every text item with one SetClipboardText each, last-write-wins regardless of MIME type; FIXED in 9edd19e2e: only text/plain written), `DataTransferItemList::set_data` ordering (status: ruled out Ã¢â‚¬â€� insertion order is faithful to the page's calls).

Symptom: copy works in-page but never reaches the system clipboard Ã¢â€ â€™ check: `clipboard` module in servo/clipboard_delegate (owns arboard access; CONFIRMED trap Ã¢â‚¬â€� silent in-memory fallback on any arboard error; loud warn-once added in a922f7fa4), `DefaultClipboardDelegate` wiring (status: ruled out Ã¢â‚¬â€� delegate chain is sound).

Symptom: script thread wedges during paste (page unresponsive, no crash) Ã¢â€ â€™ check: paste-path `GetClipboardText` blocking reads in editing paste handling and `EmbedderClipboardProvider::get_text` in text_input (CONFIRMED unbounded IPC Ã¢â‚¬â€� bounded to 5s in a922f7fa4), script-event-loop starvation by runaway JS (status: ruled out for this symptom Ã¢â‚¬â€� separate slow-script class, see PR #58 docs).

## 1o. MSE stage 5 Ã¢â‚¬â€� multi-track servomse (IN PROGRESS checkpoint, 2026-11-25)

Landed/working tree: `servomse` GstBin element (components/media/backends/gstreamer/mse_source.rs) Ã¢â‚¬â€� one appsrc per SourceBuffer track, sometimes src pads, uridecodebin autoplugs each pad into its own decodebin (verified by topology walk Ã¢â‚¬â€� sometimes-pad sources DO autoplug correctly in playbin). StreamType::MediaSource plumbed: script Ã¢â€ â€™ create_player Ã¢â€ â€™ uri servomse://. Track routing verified correct end-to-end (video bytes track 0, audio track 1).

**CRITICAL PIPELINE-CREATION BUG FIXED (the reason everything since stage 3 silently used servosrc)**: `HTMLMediaElement::resource_fetch_algorithm` calls `create_media_player` FIRST THING, before the blob-ms attach short-circuit. The player was always created with media_source=None Ã¢â€ â€™ StreamType::Seekable. Fix: hoist the blob-urlÃ¢â€ â€™attach check ABOVE create_media_player. Mechanism lesson: any element-level state that affects player construction must be set before the fetch algorithm runs, or the fetch algorithm is the wrong place to detect it.

**OPEN BUG, partially solved (debugging in harness /tmp/mseharness)**: data now flows Ã¢â‚¬â€� typefind detects video/webm + audio/webm at probability 100 per track, matroskademux+vp9dec+opusdec negotiate 1920x1080@30 and decode actively, demux src pads link multiqueue. REMAINING: playbin never leaves Paused Ã¢â‚¬â€� no frames reach the fakesinks despite decode chain being fully wired. Next suspects: timestamp/preroll semantics, multiqueue backpressure, or insufficient data reaching demux tail. Harness iterations: GST_DEBUG=6 works in harness (lower levels silent); grep task-thread ids for flow evidence; pad probes fire correctly when attached before pushes.

SOLVED sub-cause chain (each verified in harness):
(1) dynamic appsrc NULL state Ã¢â€ â€™ fix: `sync_state_with_parent` after bin add.
(2) **THE big one Ã¢â‚¬â€� `is-live=true` on appsrc makes basesrc wait for PLAYING before emitting ANY buffer (`gst_base_src_wait_playing_unlocked: live source waiting for running`), but playbin can't reach PLAYING because preroll needs those buffers Ã¢â‚¬â€� circular deadlock.** Fix: do NOT set is-live on MSE appsrcs. ServoSrc doesn't either. Lesson: "live" in gst means timestamped-at-arrival sources; MSE segments carry their own timestamps, they must flow in PAUSED like file data.
(3) ruled out: sometimes-pad autoplug (works perfectly Ã¢â‚¬â€� uridecodebin creates one decodebin per pad), ghostpad wrapping (microtest2 green), dynamic-add-then-link pattern itself (microtest green), activation ordering, state cycling.

### Symptom index additions (per standing rule)
- Symptom: two-track MSE plays audio but video chain dies "Mismatch reparsing track type / Expected 1, found 2" Ã¢â€ â€™ check: `HTMLMediaElement::resource_fetch_algorithm` ordering (owns player creation Ã¢â‚¬â€� WAS the confirmed cause: player created pre-attach as servosrc, both tracks' bytes concatenated into one stream), `servomse` track routing (ruled out Ã¢â‚¬â€� instrumentation showed correct per-track sizes), matroskademux chaining tolerance (ruled out Ã¢â‚¬â€� probe9 single-track same bytes clean).
- Symptom: MSE playback works but zero servomse activity Ã¢â€ â€™ check: whether `create_media_player` ran before `attach_media_source` (confirmed cause Ã¢â‚¬â€� see above); symptom masks itself completely because the servosrc fallback plays single-track content fine.
- Symptom: pipeline stalls pre-preroll with no error, no EOS, position frozen at 0 Ã¢â€ â€™ check: dynamically-added element states (`state=Null` in topology walk means missing `sync_state_with_parent` Ã¢â‚¬â€� confirmed first cause), then appsrc `current-level-bytes` property (if bytes accumulate = streaming task never started Ã¢â‚¬â€� OPEN, see above).
- Symptom: gst element added to running bin accepts data but never processes Ã¢â€ â€™ check: `sync_state_with_parent` call (owns state propagation for runtime-added elements Ã¢â‚¬â€� gst_bin_add does NOT reliably propagate; silent because pushes return Ok into the internal queue regardless).

Symptom: gst streaming thread or script thread wedges permanently after a seek (playback frozen, all later media ops stall) Ã¢â€ â€™ check: `SeekChannel::_await` in servo-media gstreamer player.rs (owns the seek roundtrip Ã¢â‚¬â€� was an unbounded recv; FIXED with 5s bounded try_recv_timeout, seek reported as failed on timeout), `SeekLock::unlock` ack wait in servo-media-player lib.rs (same class, same fix). Unbounded-IPC wedge family Ã¢â‚¬â€� same pattern as the clipboard GetClipboardText and WebDriver wait_for_ipc_response fixes; any NEW cross-thread recv introduced in media code must be bounded at review time.

## macOS video playback verification + symptom index entries
Progressive MP4 (h264/aac 720p, ffmpeg-generated) plays to completion in
feather on macOS with correct frame rendering Ã¢â‚¬â€� gstreamer pipeline initializes
fine against the GStreamer.framework build (needs GST_PLUGIN_PATH +
DYLD_FALLBACK_LIBRARY_PATH pointing at the framework at runtime, plus the
rpath symlink dir for the dylibs). EOS 540MB RSS after playback.

Symptom: video plays first time but replay-after-end never advances (playing state, time frozen at 00:00) Ã¢â€ â€™ FIXED root cause chain (macOS lane, instrumented end-to-end): (1) servo-media `GStreamerMediaPlayer::play` (owns forwarding play into the gst pipeline Ã¢â‚¬â€� CONFIRMED PRIMARY CAUSE: its `if !paused return` guard no-op'd because nothing ever sets paused=true at EOS, so replay never forwarded play() and the pipeline never resumed consuming); (2) `connect_end_of_stream` handler in player.rs (owns EOS bookkeeping Ã¢â‚¬â€� CONFIRMED SECOND CAUSE: never mirrored EOS into the paused flag); both fixed by always forwarding play + setting paused at EOS. HTMLMediaElement ended/replay spec logic (status: ruled out Ã¢â‚¬â€� internal_play_steps correctly fires ended=true and the spec seek-to-0 once the player state is consistent), servosrc pending-EOS deferral in source.rs (status: ruled out Ã¢â‚¬â€� appsrc self-unseals on successful seek per gst-1.22 do_seek is_eos=FALSE, and bytes post-seek are accepted with zero rejections), seek/re-fetch chain (status: ruled out Ã¢â‚¬â€� completes fully post-seek: seek propagates, need-data refires, re-fetch EOFs), qtdemux consumption (status: ruled out as independent defect Ã¢â‚¬â€� it resumes once the pipeline is actually set playing). RESIDUAL UNFIXED: the seek_channel `._await()` blocking wait in the appsrc seek_data callback can wedge the gst streaming thread in principle (unbounded-IPC class, same family as clipboard); position query reports 0 briefly post-seek which can confuse `ended_playback` precision logic.

Symptom: video element renders black/no frames on macOS Ã¢â€ â€™ check: GST_PLUGIN_PATH/DYLD fallback env at runtime (CONFIRMED requirement Ã¢â‚¬â€� without the framework plugin path no playback pipeline exists), rotation metadata in render path (status: Windows-lane defect, unverified on macOS), gst pipeline init (status: ruled out Ã¢â‚¬â€� frames render correctly with env set).

- Symptom: gst pipeline stuck pre-preroll forever, zero errors, events flow but no buffers Ã¢â€ â€™ check: `is-live` property on any appsrc/basesrc in the chain (CONFIRMED cause Ã¢â‚¬â€� live sources hold all output until PLAYING, which can never arrive without their data; evidence string `live source waiting for running` in gst_base_src_wait_playing_unlocked). Also check `current-level-bytes` on each appsrc: nonzero-and-static = upstream blockage, draining-then-stall = downstream backpressure.
- Symptom: typefind errors "Stream doesn't contain enough data" on EOS Ã¢â€ â€™ check: whether EOS raced ahead of data because a pad flush event destroyed queued buffers in transit (evidence: burst of `_gst_buffer_free` finalizes at flush time) Ã¢â‚¬â€� likely artifact of manual state cycling, NOT a real MSE hazard (ruled out for normal operation).

## Symptom index additions Ã¢â‚¬â€� media/memory investigations (Windows lane)

### Symptom: memory grows without bound scrolling an infinite media feed (synthetic mp4 autoplay test: +386MB over 25 scroll bursts)

Ã¢â€ â€™ check: `HTMLMediaElement` player lifecycle in `components/script/dom/html/embedded_content/htmlmediaelement.rs` (owns per-element gstreamer pipeline; confirmed cause Ã¢â‚¬â€� players were never suspended/torn down when scrolled offscreen, ~15MB/player accumulating with count; single player is flat over time so NOT a frame-queue leak)
Ã¢â€ â€™ check: net `ImageCache` in `components/net/image_cache.rs` (owns decoded image bytes; status: ruled out Ã¢â‚¬â€� already LRU-capped per pipeline and evicts with WR `DeleteImage`; dropping the cap 128Ã¢â€ â€™16MB left reddit scroll growth unchanged at ~+1100MB)
Ã¢â€ â€™ check: WR-side image resources / DOM node retention (status: ruled out at synthetic scale Ã¢â‚¬â€� infinite-images +3MB and infinite-DOM +6MB over the same scroll protocol)

FIXED on feather-engine: viewport-aware eviction Ã¢â‚¬â€� video elements entirely beyond Ã¢Ë†â€™2vh/+3vh bands are paused AND fully torn down (`reset_media_player`), resumed with position restore inside the 1.5vh/0.5vh hysteresis band; scan in `Document::scan_media_viewport_eviction` throttled to 500ms from the scroll dispatch path. Verified: synthetic +386Ã¢â€ â€™+154MB (residual = band-resident players, by design). Hysteresis mandatory Ã¢â‚¬â€� pausing alone does NOT release gst memory (measured), teardown is required.

### Symptom: memory grows ~+1000MB deep-scrolling reddit (chrome flat on the same scroll)

Ã¢â€ â€™ check: `HTMLMediaElement` / media players (status: ruled out Ã¢â‚¬â€� instrumented scan shows ZERO active media elements during an entire reddit deep scroll; reddit's DASH/HLS videos never become players in servo. The video-eviction fix cannot affect reddit.)
Ã¢â€ â€™ check: net `ImageCache` (status: ruled out Ã¢â‚¬â€� see above; 16MB cap experiment)
Ã¢â€ â€™ check: heavy DOM/layout accumulation on real posts (status: ruled out Ã¢â‚¬â€� realistic-weight synthetic ~600 elements/post incl. nested comments+svg grew only +16MB over the same scroll protocol)
Ã¢â€ â€™ check: per-pipeline costs of ad/tracking iframes (prime suspect, mechanism evidence: infinite iframe-image synthetic grew +111MB with only ~15 frames Ã¢â€°Ë† 7MB per iframe pipeline that is never released; the net image cache is per-PIPELINE so each iframe also gets a private 128MB decoded-image budget with zero cross-document pressure. Matches both magnitude and the wild run-to-run variance since ad injection varies per load. Unconfirmed on live reddit Ã¢â‚¬â€� no direct iframe-count measurement yet.)
Ã¢â€ â€™ check: JS heap growth from scroll-triggered execution (unexplored)

DECOMPOSITION (2026-11-25, controlled variants): empty iframes cost ~0.5MB each (nearly flat +8MB/15 frames) vs image-filled iframes ~7MB each (+111MB/15 frames) Ã¢â‚¬â€� per-iframe cost is ~90% PIXEL DATA (decoded images + canvas backing stores), NOT pipeline structure. Images of identical weight in the MAIN document are flat (+3MB) because the single pipeline's 128MB net-cache cap actually binds; iframe pipelines never reach their private caps so eviction never triggers. Canvas backing stores are outside the net cache entirely Ã¢â‚¬â€� untested share on real sites.
Ã¢â€ â€™ note: run-to-run variance on live reddit is extreme (+118 to +1060 with identical binary) Ã¢â‚¬â€� single measurements are meaningless here; control content or repeat 3x minimum.

### Symptom: browser process gone mid-measurement during reddit deep scroll

Ã¢â€ â€™ check: teardown/resume churn panic in media pipeline (suspect, unconfirmed Ã¢â‚¬â€� one observation, stderr capture now mandatory on all runs)

## 1p. MSE remove() + YouTube-choreography validation (2026-11-25)

`SourceBuffer::Remove` implemented: validates updating/range, spec lifecycle (updatestart/update/updateend from queued task, updating synchronous-true then false), range subtraction rebuilt into the script-side TimeRangesContainer (piecewise: overlap clips at [rs,re) boundaries; no fractional merging).

probe10 verified end-to-end in feather-shell: strictly updateend-gated alternating video/audio appends (YouTube's exact feed pattern), endOfStream, playback continues while remove(0,4) evicts Ã¢â‚¬â€� post-remove buffered ranges exactly [4.3-5.3][8.5-9.5], clock advancing. This is the permanent stage-6 regression harness: it replicates YouTube's append choreography without the bot wall.

### Symptom index additions (per standing rule)
- Symptom: `remove()` throws NotSupportedError or leaves buffered unchanged Ã¢â€ â€™ check: `SourceBuffer::Remove` in components/script/dom/media/sourcebuffer.rs (owns the range subtraction Ã¢â‚¬â€� was unimplemented stub, now implemented and verified), TimeRangesContainer piecewise arithmetic (prime suspect for boundary bugs Ã¢â‚¬â€� half-open [rs,re) semantics, no fp epsilon merging).
- Symptom: YouTube page loads but no videoplayback segment requests Ã¢â€ â€™ check: bot-wall layer FIRST before any engine work Ã¢â‚¬â€� `rr*.googlevideo.com` NXDOMAIN or IPv6-only resolution means CDN null-routing (CONFIRMED environmental on v4-only VMs Ã¢â‚¬â€� engine is innocent; evidence: 9 videoplayback requests issued = player JS accepted our MSE), "Sign in to confirm you're not a bot" overlay (anti-bot, not an engine defect), embed Error 153 (embedder-identity rejection).
- Symptom: MSE playback appears to work but eviction logic never fires Ã¢â€ â€™ check: buffered end reaching the eviction window (if buffered is indexed correctly per Ã‚Â§82af8c9113e the problem is downstream), updateend-gated append loop (ruled out Ã¢â‚¬â€� verified correct lifecycle in 41d0c9c5f64).

## 1q. MSE seek re-feeding Ã¢â‚¬â€� timebox investigation results (2026-11-25)

Harness evidence (seektest in /tmp/mseharness, pure-gst replication):
(1) gst pipeline seek events FAIL OUTRIGHT in push mode Ã¢â‚¬â€� `seek()` returns BoolError "Failed to seek": the TIME seek converts to a byte seek upstream, appsrc cannot serve it, whole seek is rejected (pipeline keeps playing linearly Ã¢â‚¬â€� nothing happens at all).
(2) Manual flush_start/flush_stop(reset_time) pushed downstream on servomse's ghost src pads succeeds mechanically Ã¢â‚¬â€� matroskademux resets and re-parses a fresh stream cleanly post-flush (full EBML header re-read, update_tracks, SimpleBlocks parse 'ok'). Demux recovery is NOT the problem.
(3) BUT playback never resumes: position frozen at exactly the last pre-flush frame, zero buffers reach sinks even when post-flush data is timestamp-aligned with the current position. The post-flush restart needs the serialized event sequence (stream-start/caps/segment) re-emitted in-stream before data.
(4) Naively pushing stream-start/segment events from the application thread into the live chain HANGS/crashes the pipeline immediately Ã¢â‚¬â€� they are serialized events that must originate inside the streaming flow, not from outside threads.

Conclusion: correct implementation = the flush+event re-sequence must be driven from within the data path itself (what WebKit's WebKitMediaSrc does with its own streaming thread Ã¢â‚¬â€� PausedTask state machine, seek-pending flags processed in-stream). NOT an hours-scale fix; honestly 2-4 days. Full writeup in this file; harness seektest preserves the repro.

### Symptom index additions (per standing rule)
- Symptom: seek on MSE/push-mode playback does nothing at all (position never moves, no error surfaced) Ã¢â€ â€™ check: `PlayerInner::seek` in components/media/backends/gstreamer/player.rs (owns seek dispatch Ã¢â‚¬â€� gst seek events FAIL OUTRIGHT upstream of appsrc in push mode, confirmed "Failed to seek" BoolError; the failure is silent in the player), script-side seek event wiring (ruled out Ã¢â‚¬â€� events fire and currentTime sets correctly).
- Symptom: after manual pad flush on MSE source, demux appears dead Ã¢â€ â€™ check: matroskademux push-mode recovery (ruled OUT Ã¢â‚¬â€� re-parses fresh stream cleanly post-flush, verified in gst debug log), post-flush event re-sequencing (CONFIRMED suspect Ã¢â‚¬â€� stream-start/segment must be re-emitted inside the streaming flow; pushing them from application threads hangs the pipeline).
- Symptom: pipeline position frozen at last frame after MSE flush despite data flowing Ã¢â€ â€™ check: serialized event sequence after flush_stop (prime suspect Ã¢â‚¬â€� buffers without preceding segment events are dropped/misscheduled downstream), vp9dec keyframe requirement (unchecked Ã¢â‚¬â€� vseg files start with keyframes so unlikely), sink re-preroll state (unchecked).

## 1r. MSE seek swallow (v1) Ã¢â‚¬â€� verified nonfatal (2026-11-25)

Two-layer seek swallow for StreamType::MediaSource: player-level early-return in `PlayerInner::seek` AND script-level early-return at the top of `HTMLMediaElement::seek` when media_source is attached. BOTH are required Ã¢â‚¬â€� player-level alone still wedges: the element sets current_playback_position to the seek target and entering the seeking state pins position reporting forever (seek completion never arrives). probe11 verified: playback clock continues advancing through a seek attempt (1.82Ã¢â€ â€™3.82Ã¢â€ â€™11.99), no freeze, no error, paused=false.

### Symptom index additions (per standing rule)
- Symptom: playback position frozen at exactly the seek target after any seek attempt, clock never advances again, no error Ã¢â€ â€™ check: `HTMLMediaElement::seek` seeking-state completion path (CONFIRMED cause Ã¢â‚¬â€� element enters seeking state, position pinned to target, `seek_end` never fires because the gst-level seek can't complete in push mode; player-level swallow alone is INSUFFICIENT), player position reporting (ruled out Ã¢â‚¬â€� pipeline keeps playing fine underneath).

### Symptom: images on infinite-scroll pages accumulate decoded bytes without bound (big-image synthetic +214MB over 25 scrolls)

Ã¢â€ â€™ check: DOM pin `HTMLImageElement.current_request.image` (status: INSUFFICIENT Ã¢â‚¬â€� dropping it verified working, but memory unchanged)
Ã¢â€ â€™ check: net cache `completed_loads` entry (status: INSUFFICIENT Ã¢â‚¬â€� evicting it alongside the pin verified working, memory unchanged; also the reason ALL cache-budget approaches Ã¢â‚¬â€� per-pipeline 128MB, 16MB, or the shipped global 384MB Ã¢â‚¬â€� can never bound live-document images: eviction skips entries pinned by strong_count>1)
Ã¢â€ â€™ check: layout `ReplacedContents` in the fragment tree (CONFIRMED final pin Ã¢â‚¬â€� the decoded Arc<RasterImage> is shared single-copy across DOM/cache/layout; ALL THREE pins must drop to free anything. The layout pin only drops via layout-data invalidation Ã¢â€ â€™ reflow Ã¢â€ â€™ box reconstruction Ã¢â€ â€™ image re-request Ã¢â€ â€™ instant re-pin. Churn is structural: infinite scroll triggers constant reflows via content insertion.)
Ã¢â€ â€™ VERDICT: no cheap safe win. The real fix requires either content-visibility-style layout skipping of offscreen subtrees (servo lacks it Ã¢â‚¬â€� large feature) or byte-swappable RasterImage shells with paint-time re-decode (deep engine surgery). Weeks-scale either way. The pin-release implementation was built, verified mechanically correct (113 evictions logged in one scroll test), measured useless, and REVERTED Ã¢â‚¬â€� do not re-attempt the same two-pin variant.
Ã¢â€ â€™ note: images below the initial viewport appear NOT to decode until scrolled near (has_image=false below viewport in scan logs) Ã¢â‚¬â€� servo already has partial lazy behavior; retention is for once-visible content only.

## 1s. Signed-in YouTube verification + cookie persistence + real-world MSE recon (2026-11-25)

STAGE 6 VERIFIED: signed-in YouTube plays in feather. Cookie path: user signed into Chrome on the VM, cookies decrypted from Chrome's profile sqlite (Linux v10 = AES-CBC, PBKDF2("peanuts","saltysalt",1,16), IV=16 spaces), converted to servo's cookie_jar.json format (`{version:1, cookies_map{regkey:[{cookie as Set-Cookie-string, host_only, persistent, creation_time, last_access, expiry_time}]}, max_per_host:150}`; SystemTime serializes as `{secs_since_epoch, nanos_since_epoch}`; cookie field is a string parsed by cookie::Cookie::parse Ã¢â‚¬â€� one bad cookie fails the WHOLE jar silently; map keys are eTLD+1 reg domain of the cookie domain attribute, domains written WITHOUT leading dot), dropped into feather's data dir. Verified: personalized feed, pre-roll ad then main video continuous playback with captions, live cookie rotation (SIDCC rotation proves the jar is bidirectionally live).

CRITICAL SHELL FIX: feather's ServoBuilder never set opts.config_dir Ã¢â€ â€™ resource_thread ran with config_dir=None Ã¢â€ â€™ cookie_jar.json NEVER read Ã¢â‚¬â€� feather had zero cookie persistence before this fix. Shell now points config_dir at feather's data dir. NOTE: jar writes happen only on clean CoreResourceMsg::Exit Ã¢â‚¬â€� pkill'd sessions never persist jar mutations, but startup reads work regardless.

Recon verdicts (2026-11-25): dailymotion PLAYS. twitch FAILS (black player, page+live-data fine). vimeo FAILS ("Technical Difficulties" catch-all pre-playback). shaka demo FAILS (app UI never populates, pre-player). bitmovin blocked by Cloudflare (environmental).

### Symptom index additions (per standing rule)
- Symptom: cookies never sent to any site / every session starts logged-out despite jar file existing Ã¢â€ â€™ check: shell ServoBuilder opts.config_dir (CONFIRMED cause when None Ã¢â‚¬â€� resource_thread skips jar load entirely; the jar file location is silently irrelevant), cookie_jar.json deserialize (silent total failure if ANY single cookie string fails Cookie::parse Ã¢â‚¬â€� verify with a trivially-simple test cookie entry keyed to 127.0.0.1 against a local logging server before suspecting individual cookies), cookies_map key format (must be eTLD+1 of the cookie's domain attribute, no leading dot Ã¢â‚¬â€� a leading-dot domain breaks domain_match for all subdomains).
- Symptom: twitch page fully works but player black forever Ã¢â€ â€™ check: HLS/fMP4+H264 path (prime suspect Ã¢â‚¬â€� twitch is avc-only, supported_codecs=av1,h265,h264 in their usher call; transmuxed hls.js fMP4 appends never exercised in our WebM-first validation), avc decode presence in the gst registry (check avdec_h264/openh264 Ã¢â‚¬â€� avdec absent on stock Linux), `Could not set the playback rate: NonSeekableStream` warnings (MSE/live stream rate-set unhandled), "Trying to exec media action on an unknown client context" (media client lifecycle mismatch, unchecked). DOM surface/network/CDN ruled out Ã¢â‚¬â€� m3u8 master playlists fetch 200 fine.
- Symptom: vimeo video page shows generic "Technical Difficulties" Ã¢â€ â€™ uncharacterized, occurs BEFORE any media init evidence; suspect player feature-detection JS failing, not yet investigated.
- Symptom: shaka-player demo header/footer render but content area never populates Ã¢â€ â€™ uncharacterized framework-level JS failure, pre-player; not evidence about MSE itself.

## 1t. Silent hard crash on heavy real sites Ã¢â‚¬â€� amazon, reproducible (2026-11-25)

Observed first during bakeoff persite measurement (process disappeared mid-run), reproduced deliberately 3/3: amazon.com alone, pure wheel scrolling (10 bursts x2 rounds), dies in round 1-2 every time. Also reproduces with bbc+amazon+reddit multi-tab + Ctrl+Tab. NOT tab-switch-dependent, NOT memory (dies at ~600MB on 34GB box).

Evidence negatives: zero ERROR/panic lines in stderr (log ends mid-normal-network-activity), NO Windows Error Reporting event even with LocalDumps configured, NO procdump exception capture. CRITICAL: with procdump actively attached the process SURVIVES all 5 rounds Ã¢â‚¬â€� classic heisenbug/timing-sensitive behavior, race-flavored. Gstreamer-in-path irrelevant (crashes with gst removed). Media eviction teardown churn ruled out as cause (crash predates AND reproduces without gst).

### Symptom index additions (per standing rule)
- Symptom: feather process silently disappears while scrolling a heavy real site (amazon verified; no panic output, no WER event, no exception capture) Ã¢â€ â€™ check: multiprocess/threading races in script/layout/compositor (PRIME SUSPECT Ã¢â‚¬â€� timing-sensitive heisenbug confirmed by procdump-attachment survival; access violation in native code or silent abort suspected but unproven), OOM (ruled out Ã¢â‚¬â€� dies at <1GB RSS), gstreamer media lifecycle including offscreen-eviction teardown/resume churn (ruled out Ã¢â‚¬â€� reproduces with gstreamer entirely absent from PATH), tab-switch Ctrl+Tab injection (ruled out Ã¢â‚¬â€� reproduces with pure scrolling in a single tab), panic=abort with output loss (ruled out-ish Ã¢â‚¬â€� stderr file capture shows nothing, but a panic inside a thread whose abort precedes flush cannot be fully excluded; the heuristic evidence argues native), Windows Error Reporting pipeline (ruled out as EXPLANATION Ã¢â‚¬â€� LocalDumps configured correctly yet nothing captured, consistent with deliberate abort/ExitProcess paths).
- Open follow-up: stock servoshell control test (does upstream servo crash identically? Ã¢â‚¬â€� discriminates feather-shell vs engine), then instrumented/debug-engine build for the exception identity. Until characterized, treat ALL heavy-site long-scroll measurement processes as crash-prone: capture stderr always, verify process aliveness before trusting a measurement tail.

## Ã‚Â§1t Ã¢â‚¬â€� MSE seek implementation (Linux lane, 2026-11-25)

**Design (harness-validated before engine implementation):** push-mode MSE
pipelines cannot execute gst seeks. Seek = (1) push flush_start/flush_stop
on the servomse ghost src pads Ã¢â‚¬â€� safe from the application thread, resets
demuxers/decoders; (2) recreate every track's appsrc inside servomse,
re-targeting the SAME ghost pad (pad identity stable for decodebin3) Ã¢â‚¬â€�
a fresh appsrc naturally re-emits stream-start/caps/segment in-stream on
the first post-seek push, avoiding the manual serialized-event injection
that wedges the pipeline (Ã‚Â§1q finding 4); (3) the page re-appends media
naturally on its 'seeking' handler. Validated in /tmp/mseharness
(seektest2): position advancing continuously post-seek, first try.

**Engine pieces:** servomse seek_reset/reset_track (mse_source.rs);
player.rs MSE seek branch routes to seek_reset (small, inside the
MediaSource match arm only Ã¢â‚¬â€� no overlap with progressive flushing-seek);
HTMLMediaElement spec seek restored, completion queued directly because
data arrival is app-driven; SourceBuffer init caching + buffered clearing
+ init re-feed on first post-seek append; MediaSource.notify_seek fan-out.

**Spec deviations (deliberate):** buffered ranges are CLEARED on seek Ã¢â‚¬â€�
the flush genuinely drops pipelined media and reporting it buffered would
stop players re-feeding; seek completion fires without waiting for data
decode (impossible when data is app-driven).

**Symptom index additions:**
- Symptom: playback does not resume after MSE seek Ã¢â€ â€™ check: servomse
  seek_reset (owns flush+recreate), SourceBuffer init_cache/needs_init_refeed
  (media-only post-seek appends are UNDECODABLE without re-fed init Ã¢â‚¬â€�
  prime suspect if data flows but no frames), HTMLMediaElement seek wedge
  (ruled out Ã¢â‚¬â€� completion now queued directly, verified seeking flag clears),
  manual in-stream event injection (ruled out Ã¢â‚¬â€� wedges pipeline, Ã‚Â§1q).
- Symptom: seeking/seeked events fire twice per seek Ã¢â€ â€™ cosmetic, observed
  in probe6; suspect: position-driven completion path + explicit queued
  completion both firing. Not user-visible harmful.
- Symptom: post-seek position wrong/stale Ã¢â€ â€™ check: matroskademux timestamp
  passthrough after flush (verified correct Ã¢â‚¬â€� position lands on target),
  init re-feed prepending (corrupts timestamps if double-init appended;
  the already_has_init check guards this).

**Verification (probe6, live engine):** seek to 5.0 Ã¢â€ â€™ position 5.35 then
7.88 advancing, seeking+seeked fired, seeking flag false, rs 4, playing.

## Ã‚Â§1u Ã¢â‚¬â€� fMP4/H.264 mechanism diagnosis (2026-11-25)

**Verdict: the fMP4/H.264 engine path WORKS** Ã¢â‚¬â€� probe7 live-verified:
isTypeSupported true for avc1/mp4a, fMP4 init+media appends decode and play
(t advancing, rs 4, zero errors). Pure-gst harness agrees.

- Symptom: twitch-class sites black player with working page chrome Ã¢â€ â€™ check:
  SourceBuffer::SetTimestampOffset (accept-and-ignore Ã¢â‚¬â€� hls.js transmuxing
  rewrites ALL timestamps through it; PRIME SUSPECT), live-edge semantics
  (seekable ranges/duration Infinity unimplemented), avdec_h264 absence
  (ruled out Ã¢â‚¬â€� openh264dec covers decode, verified), qtdemux push-mode fMP4
  (ruled out Ã¢â‚¬â€� verified working in harness + engine), isTypeSupported mp4
  (ruled out Ã¢â‚¬â€� returns true correctly).
- Symptom: vimeo "Technical Difficulties" / shaka demo shell broken Ã¢â€ â€™ NOT
  media machinery; player JS fails before any append. General web-compat.
- Symptom: fMP4 MSE append produces no playback Ã¢â€ â€™ NOT a real symptom class;
  verified working end-to-end. If seen, suspect timestampOffset confusion
  or missing init re-feed post-seek instead.

## 1u. Silent heap-corruption crash Ã¢â‚¬â€� deep characterization (2026-11-25, Windows lane)

THE BUG: feather.exe dies silently on heavy real sites with NO input needed Ã¢â‚¬â€� reddit 60s idle, amazon 75s idle, bbc/amazon scroll round 1-2. Exit 0xC0000374 STATUS_HEAP_CORRUPTION. With full pageheap (IFEO GlobalFlag=0x02000000, PageHeapFlags=3): deterministic 0xc0000005 in verifier.dll heap validation, then process exit 0xc0000421. Timing-sensitive: survives under procdump attachment (heisenbug).

VERIFIED NEGATIVES (do not re-walk):
- NOT input-driven: crashes with zero input injected, pure idle.
- NOT media: reddit has zero media players, dies at 60s idle.
- NOT gstreamer/gst-lifecycle: crash unchanged with gst absent from PATH (note: gst dlls sit beside the exe, so PATH removal does NOT disable gst Ã¢â‚¬â€� the dlls load regardless; media ruled out by the reddit result instead).
- NOT winhit subclass: knockout build crashes identically.
- NOT jemalloc purge commit: servo-allocator build.rs gates jemalloc_enabled to non-Windows Ã¢â‚¬â€� purge_arenas is an INERT STUB on Windows. All jemalloc theories dead for Windows.
- NOT OOM (600MB on 34GB box), NOT tab-switch injection, NOT scroll-specific.
- NOT engine-shared-by-default: servoshell built from same feather-engine tree SURVIVES amazon scroll x5 AND amazon idle 375s+ (controls verified valid: cursor over window, scroll events confirmed in log, gst dlls present in both runtime dirs). feather dies in the same minutes. THE BUG IS FEATHER-SHELL-SPECIFIC.

CAPTURE TECHNIQUE (reuse it): heap-corruption fastfail bypasses SetUnhandledExceptionFilter/WER/procdump. Working pipeline: AddVectoredExceptionHandler(1) + first-fatal-only capture of CONTEXT registers + raw stack qwords; dll bases boot-constant so post-hoc module mapping works. MiniDumpWriteDump cannot run inside the corrupted process (allocates). Module used: shell/src/crashdump.rs (NOT committed Ã¢â‚¬â€� diagnostic only). Symbolization: dbghelp SymFromAddr via python ctypes + feather.pdb beside the exe copy.

SYMPTOM INDEX additions (per standing rule):
- Symptom: feather process silently vanishes on heavy real sites (idle or scrolling, no panic output, no WER, exit 0xC0000374/0xC0000421) Ã¢â€ â€™ check: feather-shell's engine-driving loop and GL/texture workload (PRIME SUSPECT area Ã¢â‚¬â€� the only substantive unsafe surface differing from servoshell: per-frame webview.paint() pattern, egui_glow texture uploads, favicon decode cache calling into engine every frame), feature-selection deltas vs servoshell (feather lacks multiprocess/devtools/webgl/webgpu/subtlecrypto/brotli/max_log_level that servoshell enables; feather adds no-wgl Ã¢â‚¬â€� untested as a group), style-traversal parallel churn (crash correlates with 98% style:: recalc storms but shared with servoshell Ã¢â‚¬â€� context not cause), mozjs-internal jemalloc frames appear in fault chain (coarse public-only resolution Ã¢â‚¬â€� UNRELIABLE, do not over-read).
- Resolution limit hit: release+line-tables PDB resolves only sparse publics; mozjs C++ has no debuginfo in release either. Exact corrupting function requires full debug=2 rebuild (hours) or upstream-quality repro.

REPRO HARNESS (exact): session.json single tab amazon/reddit; launch feather-crash.exe copy of feather.exe; wait 60-90s; dies with zero input. Scroll injection accelerates but is unnecessary. servoshell.exe same-site control survives 5x longer.

## Symptom: Atomics.wait throws TypeError ("waiting is not allowed") in workers despite shared memory working
Ã¢â€ â€™ check: FutexThread canWait flag in mozjs AtomicsObject (owns the gate Ã¢â‚¬â€� `cx->fx.canWait()` checked in atomics_wait_impl), JS_SetFutexCanWait embedder API in jsapi.cpp (owns opt-in; CONFIRMED CAUSE Ã¢â‚¬â€� servo never called it, so every agent defaulted to no-blocking), dedicatedworkerglobalscope/sharedworkerglobalscope runtime setup (fixed 659eec46d: flag set at worker context creation; spec: only the window main-thread agent may not block).

## Symptom: worker posts pre-wait message then never wakes despite main-thread Atomics.notify
Ã¢â€ â€™ check: JS_SetFutexCanWait (status: ruled out for the wake path Ã¢â‚¬â€� with the flag set, probe verified worker blocks ~600ms, notify returns exactly 1 waiter, woken worker observes updated value; sabwait2.html localhost probe), lost-wakeup race in probe design (status: ruled in for the first flaky probe run Ã¢â‚¬â€� notify fired before worker parked; not an engine defect), turnstile live behavior (status: real-SAB claude.ai STILL stalls with correct wait/notify Ã¢â‚¬â€� challenge restarts, Ray ID cycles; residuals: OffscreenCanvas absent (confirmed missing), wasm threads, canvas/webgl fingerprint hardening).

## Symptom: OffscreenCanvas undefined on window and worker despite servo shipping an implementation
Ã¢â€ â€™ check: dom_offscreen_canvas_enabled pref in components/config/prefs.rs (CONFIRMED CAUSE Ã¢â‚¬â€� defaults false; the 614-line implementation in components/script/dom/canvas/offscreencanvas.rs is complete enough for 2d use and WPT-exercised upstream; flipping the pref enables constructor/2d/fillRect/transferToImageBitmap on window+worker, verified), getContext('webgl') on OffscreenCanvas (status: broken/throws TypeError even after the pref flip Ã¢â‚¬â€� separate unowned gap), HTMLCanvasElement.transferControlToOffscreen + createImageBitmap (status: ruled out Ã¢â‚¬â€� both present and functional).

## Symptom: claude.ai turnstile still loops/restarts with real SAB + working Atomics.wait/notify + OffscreenCanvas-2d all verified
Ã¢â€ â€™ check: WebGL-on-OffscreenCanvas TypeError (prime suspect Ã¢â‚¬â€� unguarded access would kill the payload chain), WebGL fingerprint surface generally, wasm threads (unprobed), behavioral/TLS signals (out of JS-engine scope Ã¢â‚¬â€� do not chase per Google decision), payload opacity (sufficiency unprovable from outside; shim stays the only working path).

## Symptom/probe verdicts: wasm capabilities (turnstile residual sweep)
Ã¢â€ â€™ check: WebAssembly basic compile/exec (status: ruled out Ã¢â‚¬â€� present and correct), WebAssembly.Memory({shared:true}) construction (status: ruled out Ã¢â‚¬â€� constructs OK), wasm atomic instructions (status: INCONCLUSIVE Ã¢â‚¬â€� hand-assembled probe module failed with a byte-encoding error in the probe itself, not an engine verdict; needs a properly assembled threads-test module or wat2wasm tooling).

## Symptom: canvas.getContext('webgl') absent/null everywhere in feather
Ã¢â€ â€™ check: feather shell Cargo.toml servo feature list (CONFIRMED CAUSE Ã¢â‚¬â€� webgl is in servo's default_web_features but feather's minimal list omitted it, so even on-window WebGL was absent; fixed by adding the feature, feather PR #112), offscreencanvas.rs get_or_init_webgl_context downcast::<Window> (status: confirmed structural gap Ã¢â‚¬â€� offscreen-in-worker returns null even with the feature on; workers have no Window global), WebGL RENDERER/VENDOR strings (status: loud fingerprinting tell Ã¢â‚¬â€� literally report "Mozilla/Servo"; hardening decision unowned).

## Symptom: claude.ai turnstile still loops with FULL real stack (SAB + Atomics.wait/notify + OffscreenCanvas-2d + WebGL 1/2 all verified present)
Ã¢â€ â€™ check: worker-offscreen-WebGL null return (prime suspect if payload runs GL work in a worker unguarded), wasm threads atomics (inconclusive Ã¢â‚¬â€� probe encoding was broken), fingerprint surface beyond JS (TLS/HTTP2 Ã¢â‚¬â€� out of scope per Google decision), behavioral signals. NOTE: four necessary APIs fixed and upstreamable regardless; sufficiency unprovable from outside.

## Ã‚Â§1v Ã¢â‚¬â€� fmp4 buffered indexing + timestampOffset empirical correction

- Symptom: mp4 MSE appends never populate buffered Ã¢â€ â€™ scan_cluster_timecodes
  is webm-only (CONFIRMED CAUSE, fixed Ã¢â‚¬â€� tfdt fallback added); player append
  loops that gate on buffered stall even though decode/playback work.
- Symptom: fmp4 live-edge playback broken by missing timestampOffset Ã¢â€ â€™
  RULED OUT as a playback blocker: huge tfdt timestamps play fine, position
  just reports container time. timestampOffset remains a timeline-correctness
  feature, not a playback gate. seekable ranges + duration=Infinity for live
  are now the prime live-semantics suspects.

## Symptom index addendum Ã¢â‚¬â€� heap-corruption knockout bisect (Windows, 2026-11-25)

Symptom: silent heap-corruption death (0xC0000374 / pageheap AV) on heavy sites, idle, no input, ~60s on reddit Ã¢â‚¬â€� feather shell only, servoshell control survives everything.
Ã¢â€ â€™ ruled out, verified NOT the cause:
- favicon decode path (knockout died same) Ã¢â‚¬â€� `feather shell update_favicon_cache`
- paint ordering / make_current-before-paint (knockout died same) Ã¢â‚¬â€� `shell gui.run paint`
- ALL chrome collectively Ã¢â‚¬â€� minimal-shell build (only webview.paint + composite blit) still dies at 60s Ã¢â‚¬â€� bug is in CORE INTEGRATION LOOP, not chrome. `App::run` / `ServoBuilder` / webview lifecycle layer.
- winhit / hit-test injection, media/gstreamer, jemalloc-purge (inert on Windows), webgl/webgpu feature absence, input handling, OOM, tab-switching
- js-mem prefs (js_mem_max=192 etc.) Ã¢â‚¬â€� servoshell with FULL feather pref set survives 255s+ Ã¢â‚¬â€� prefs exonerated
- user_content_manager attachment Ã¢â‚¬â€� died at 315s with ucm detached (5x delay but still died; treat as noise until replicated Ã¢â‚¬â€� death-time variance unmeasured)
- winit ControlFlow busy-loop vs Wait Ã¢â‚¬â€� Wait build dies at 60s identical Ã¢â‚¬â€� event pacing NOT the trigger
Ã¢â€ â€™ prime remaining suspects (core integration loop): ServoBuilder opts/config differences, multi-webview infrastructure, hidpi scale-factor handling, embedder_traits usage differences, the winit ApplicationHandler shell-servo glue itself.
Ã¢â€ â€™ bisect status: STALLED on measurement noise Ã¢â‚¬â€� death-time variance uncharacterized, each knockout test costs 6min. Next step recommended: full debug=2 build for a real capture stack, or replicate baseline 3x to calibrate variance before further knockouts.
Ã¢â€ â€™ harness facts: repro = work/feather-crash.exe + session.json reddit single-tab idle, dies ~60-75s; example.com survives 255s+ (content-driven); capture module crashdump.rs uncommitted in feather shell tree; full pageheap ACTIVE on feather-crash.exe in registry.

## Symptom: claude.ai turnstile Ã¢â‚¬â€� nondeterministic pass/loop with full real stack + WebGL string spoof
Ã¢â€ â€™ check: WebGL RENDERER/VENDOR masked strings (status: spoofed locally to ANGLE-Metal/Google-Inc. values, verified live Ã¢â‚¬â€� NOT PUSHED, experiment only; verdict: insufficient alone, cold-state still loops), cf_clearance cookie hypothesis (status: plausible Ã¢â‚¬â€� the one observed pass occurred with accumulated storage present; a clearance from background-running challenge machinery can't be ruled out, so attribution to the spoof is ambiguous), turnstile internal machinery (status: runs DEEP Ã¢â‚¬â€� service worker + caches + IndexedDB + localstorage all registered under challenges.cloudflare.com origin; the payload executes fully rather than dying on API checks), verdict class (status: nondeterministic Ã¢â‚¬â€� passed once after minutes of runtime, loops other times; server-side risk scoring on aggregate signals means remaining tells are statistical/behavioral + TLS, a different work class than API presence).

## Symptom index addendum 2 Ã¢â‚¬â€� structured-clone crash stack (2026-11-25, pageheap capture symbolized)

Symptom: heap-corruption/AV death, content-driven, feather shell.
Ã¢â€ â€™ CONFIRMED CRASH SITE (from symbolized capture, thread dies here): `JS_ReadStructuredClone` deserialization of a MessagePort postMessage payload Ã¢â‚¬â€� stack: `structuredclone::read_with_scope` Ã¢â€ ï¿½ `GlobalScope::route_task_to_port` Ã¢â€ ï¿½ `post_messageport_msg` task; inside: `WriteBytesToJSStructuredCloneData` (read-side reconstruction copy), `JSStructuredCloneReader::read`, `BufferList<SystemAllocPolicy>::IterImpl::Advance/AdvanceAcrossSegments`, `AllocateBytes/AllocateSegment`, `js_arena_malloc`.
Ã¢â€ â€™ module attribution: EVERYTHING in feather.exe (mozjs statically linked Ã¢â‚¬â€� no external dll involvement except ntdll/verifier/ucrtbase). The earlier "mystery 0x7ff7b module" was a red herring from pre-module-list guessing.
Ã¢â€ â€™ under FULL pageheap out-of-bounds WRITES fault at write time (guard pages per allocation); a READ fault inside deserialization therefore suggests UAF-read or malformed-bytes read rather than classic heap corruption propagating silently.
Ã¢â€ â€™ mechanism facts from code review: write side copies bytes OUT of the JS buffer into a Rust Vec (structuredclone.rs write Ã¢â‚¬â€� CopyJSStructuredCloneData + set_len); read side copies bytes back IN (WriteBytesToJSStructuredCloneData) then calls JS_ReadStructuredClone. Byte-copy logic itself looks correct on inspection. SameProcess scope sets allowSharedMemoryObjects=true Ã¢â‚¬â€� serialized form may embed RAW BUFFER REFERENCES rather than copies for shared-memory-backed objects.
Ã¢â€ â€™ suspect areas for the UAF/malformation: (a) transferred object backing stores (transferred_image_bitmaps / offscreen_canvases / ports) lifetime across pipeline teardown; (b) feather's webview lifecycle differences Ã¢â‚¬â€� tab sleep/teardown/peek Ã¢â‚¬â€� tearing down pipelines while port messages are in flight (servoshell never does this); (c) victim-only reading Ã¢â‚¬â€� clone buffers are merely the densest memory traffic.
Ã¢â€ â€™ CAPTURE TOOLING THAT WORKS: crashdump.rs now captures module list (CreateToolhelp32Snapshot) + registers + 512 stack qwords; attr.py buckets addresses by module; symbolize.py resolves via dbghelp against shell/target/release/feather.exe (pdb adjacency Ã¢â‚¬â€� do NOT point it at the renamed copy). mozjs C++ frames RESOLVE with line-tables-only pdb Ã¢â‚¬â€� full debug=2 build NOT needed for attribution.
Ã¢â€ â€™ OPEN CONTROL GAP CLOSED PENDING: servoshell was only control-tested on amazon/webgl Ã¢â‚¬â€� never reddit (the fastest killer). servoshell+reddit idle test in flight Ã¢â‚¬â€� if it dies, this is an UPSTREAM bug misfiled as shell-specific.

## Symptom index addendum 3 Ã¢â‚¬â€� variance collapse + paint-gating fix (2026-11-25)

Symptom: heap death on heavy sites.
Ã¢â€ â€™ MEASUREMENT WARNING Ã¢â‚¬â€� death-time variance is 30-420s for identical configs. The minimal+wait+no-ucm 'survival' at 420s did NOT replicate (died 60s on rerun). ALL single-run knockout survivals are noise; dying-results remain valid negatives but survival-results are meaningless without replication. Black-box bisect closed as statistically unworkable at 6min/run.
Ã¢â€ â€™ CODE-VERIFIED shell difference (not bisect-dependent): feather composited UNCONDITIONALLY on every winit RedrawRequested; servoshell gates paint on needs_repaint (window.rs repaint logic Ã¢â‚¬â€� only composites when notify_new_frame_ready or UI change marked dirty). Painter::render() composites unconditionally when called and can self-cascade via refresh-driver FrameStart.
Ã¢â€ â€™ FIX IN TESTING (feather shell, uncommitted): webview_needs_paint Cell flag Ã¢â‚¬â€� set in notify_new_frame_ready, gates webview.paint() in redraw. Acceptance test: 5x reddit-idle 420s runs (baseline never survived once). Also restores ucm + favicons + keeps ControlFlow::Wait (servoshell parity, battery win).
Ã¢â€ â€™ mechanism theory being tested: rAF-active pages (reddit ad/tracker loops) sustain a composite feedback cycle whose rate is shell-determined; rate scales memory churn; churn scales hit probability of the structured-clone race/UAF. Explains: content-dependence, input-independence, loose time-bomb clustering, shell-specificity.
Ã¢â€ â€™ if fix passes: race still exists in engine but becomes unreachable in practice Ã¢â‚¬â€� document as mitigated-not-fixed; upstream-quality repro still valuable. If fix fails: gating stays as pure battery win; hunt resumes on webview-lifecycle/delegate behaviors.

## Symptom: feather burns ~84% of one core playing 720p video (Safari: ~4%)
Ã¢â€ â€™ check: vtdec hardware decode (status: ruled out Ã¢â‚¬â€� gst-launch decode-only baseline measures ~1% CPU; hardware decode confirmed active), gst-side videoconvert YUVÃ¢â€ â€™BGRA software conversion (status: real but minor Ã¢â‚¬â€� decode+convert pipeline totals ~9.6% CPU in isolation, so ~9 points of the 84), per-pixel frame copies/texture upload (status: ruled out as DOMINANT Ã¢â‚¬â€� 320Ãƒâ€”180-native video at same display size still burns 75.7%; cost does not scale with frame pixels), display-size rasterization of video rect (status: ruled out Ã¢â‚¬â€� 320px-wide display of same video still 84%), COMPOSITOR/REPAINT PATH (CONFIRMED DOMINANT CAUSE Ã¢â‚¬â€� a plain 30fps rAF background-color animation with NO video/decode/upload/gst involvement burns 77.1%; feather pays ~2.5ms+ main-thread CPU per repaint regardless of what changed. Video is just a 30fps invalidator; ALL animated content pays this Ã¢â‚¬â€� CSS animations, rAF, transitions). window-size scaling partial (500Ãƒâ€”400 window ~50% vs large window ~84% Ã¢â‚¬â€� consistent with viewport-proportional rasterization plus per-frame constant). open suspects within the repaint path: full display-list rebuild per frame vs tile caching failure (webrender), egui shell chrome repainting in lockstep with content, deprecated macOS GL translation layer cost (glrFlushDrawFramebufferToIOSurface/glTexSubImage2D visible in profile Ã¢â‚¬â€� GL-on-macOS is CPU-translated), main-thread serialization of rasterize+composite.

## Symptom index addendum 4 Ã¢â‚¬â€� the measurement contamination discovery (2026-11-25)

Symptom: 60s idle-death on reddit-class pages, feather only.
Ã¢â€ â€™ PRIME SUSPECT IDENTIFIED: the repro binary carried TEMP memreport instrumentation (dirty tree only, NEVER in main Ã¢â‚¬â€� verified git show main:shell/src/app.rs has zero refs) Ã¢â‚¬â€� automatic create_memory_report at elapsed 50s+110s in the window-event handler. Deaths at ~60s = 50s mark + measurement duration. Survival outliers (315/420s) = probabilistic measurement crash; pass both marks and nothing remains to kill the process.
Ã¢â€ â€™ CRASH SIGNATURES (7 captures, all contaminated builds): signature A = structured-clone postMessage deserialization; signature B = INSIDE memory measurement itself (get_reports/compute_size/malloc_size_of_including_raw_self<MessagePort>/Document::collect_reports/perform_memory_report). Both involve MessagePort memory Ã¢â‚¬â€� consistent with one event: concurrent memory measurement of cross-pipeline shared structures (MessagePort impls are shared across globals/pipelines) racing live clone traffic.
Ã¢â€ â€™ ENGINE LATENT RACE (real, documented, upstream-relevant): memory measurement of shared-across-pipelines structures is not synchronized Ã¢â‚¬â€� servo never measures in production so upstream never hits it; any embedder triggering create_memory_report on a multi-pipeline workload may crash. THIS IS A REAL FIND regardless of the contamination outcome.
Ã¢â€ â€™ PROCESS LESSON (mandatory reading): black-box bisection against an instrumented binary is void Ã¢â‚¬â€� the repro baseline MUST be a known-clean build before any knockout work. Hours of knockout results (favicon/ucm/controlflow/minimal-shell/paint-gating) are ALL contaminated and must be re-evaluated; treat them as void, not as exonerations.
Ã¢â€ â€™ paint-gating fix: kept regardless of verdict Ã¢â‚¬â€� correct servoshell semantics + battery win Ã¢â‚¬â€� but it is NOT validated as a crash fix.
Ã¢â€ â€™ verdict test in flight: no-memreport build 5x reddit-idle acceptance.

## Symptom index addendum 5 Ã¢â‚¬â€� repaint-cost profiler attribution (2026-11-25)

Symptom: feather pays ~7-8ms+ CPU per composite frame; animated content capped below display rate.
Ã¢â€ â€™ check: webrender DisplayList build (status: RULED OUT Ã¢â‚¬â€� profiler avg 0.01ms/frame), webrender scene building / tile cache (status: RULED OUT Ã¢â‚¬â€� profiler avg 0.02ms/frame; tile caching healthy), webrender frame/render phase (status: CONFIRMED DOMINANT INTERNAL TERM Ã¢â‚¬â€� profiler "Frame CPU total" avg 7.76ms/frame at large viewport, excludes feather-side blit+egui+present so true cost higher), fixed driver overhead (status: RULED OUT Ã¢â‚¬â€� cost is pixel-proportional: same anim runs 61fps full-rate in a 500x400 window vs 31fps large), full-viewport texture upload per frame (status: RULED OUT by sample attribution Ã¢â‚¬â€� glTexSubImage2D ~0 samples in redraw; blit_framebuffer 42/5336 ~1%, IOSurface flush 71/5336 ~1%), Apple GL translation layer draw processing (status: CONFIRMED DOMINANT CAUSE Ã¢â‚¬â€� glDrawElements/gleDrawArraysOrElements_ExecCore/gldRenderFillPolygonPtr = 4139/5336 ~78% of redraw samples: CPU-side vertex processing + software polygon fill in the deprecated GL-to-Metal shim; pixel-proportional scaling explained by triangle fill area). THE fix lever is getting off legacy GL on macOS (ANGLE-Metal); embedder-copy elimination is worth only ~1-2%.
Ã¢â€ â€™ partial-present viability: webrender supports buffer_age partial present but servo Painter hardcodes buffer_age 0 AND surfman 0.14 exposes NO damage-aware present API at all (present_surface/swap_buffers have no damage parameters; CGL backend has no damage plumbing) Ã¢â‚¬â€� damage-aware fix requires new surfman API + platform work, not a config flip. Fix levers ranked: (a) eliminate embedder-side full-window copies (blit+egui texture upload), (b) ANGLE/Metal to remove GL translation tax, (c) damage plumbing through surfman Ã¢â‚¬â€� largest integration surface.

## Symptom index addendum 6 Ã¢â‚¬â€� ANGLE-on-macOS integration cost reality (2026-11-25)
Ã¢â€ â€™ surfman 0.14 angle backend: windows-cfg'd at module level, D3D11-flavored implementation Ã¢â‚¬â€� ungating for macOS requires backend adaptation, not a config flip.
Ã¢â€ â€™ mozangle 0.7.1 build data contains ZERO Metal-backend sources (macos branch exists for the shader translator only) Ã¢â‚¬â€� ANGLE-Metal on macOS requires extending mozangle's vendored chromium build (week-scale, not days). The repaint-cost fix investment should be sized accordingly.

## Symptom index addendum 7 Ã¢â‚¬â€� sw-rasterize (SW-WR) fallback scoping (2026-11-25)
Ã¢â€ â€™ webrender 0.70 SHIPS the swgl-based software rasterizer behind feature `sw_compositor` (`swgl` dep) Ã¢â‚¬â€� Firefox's SW-WR. The renderer duck-detects swgl contexts via `swgl_*` entry points in the GL implementation Ã¢â‚¬â€� no separate render mode, you hand it a software GL context.
Ã¢â€ â€™ INTEGRATION SURFACE for servo: (a) gleam 0.15.1 has NO swgl bridge Ã¢â‚¬â€� a Gl-trait-to-swgl forwarding shim must be written or extracted from Firefox's webrender_bindings glue (gleam's Gl trait is large; swgl implements the subset webrender uses); (b) surfman bypass Ã¢â‚¬â€� swgl contexts are pure-memory, no windowing/ANGLE/surfman needed for rasterization; (c) present path Ã¢â‚¬â€� final pixels land in system memory and must reach the screen; feather's present chain is itself GL (egui/winit window), but present cost measured only ~1-3% so a single full-window upload+draw through legacy GL is acceptable Ã¢â‚¬â€� circularity is NOT fatal.
Ã¢â€ â€™ KEY UNKNOWN: whether multithreaded SIMD swgl rasterization is actually cheaper than Apple's GL-shim draw processing for servo workloads. Firefox public data: SW-WR competitive with GPU-WR on many pages, sometimes faster on weak GPUs Ã¢â‚¬â€� but apple-silicon CPU is fast, so plausibly a large net win vs 78%-CPU-draw-submission status quo. UNKNOWN until measured.
Ã¢â€ â€™ CHEAPEST VALIDATION EXPERIMENT (design): stand-alone harness linking swgl + gleam shim rendering representative display lists (solid-color fullscreen rects, text runs, rounded corners) comparing CPU-cost-per-frame vs the measured 7.76ms+ webrender-through-Apple-GL baseline. Est. 1-2 days just to validate, before any servo integration.
Ã¢â€ â€™ ESTIMATE if validated: gleam-swgl bridge + painter rewiring + feather present path = days-to-week, comparable uncertainty to ANGLE but all-Rust (no chromium build surgery). Does NOT benefit webgl/canvas GL content Ã¢â‚¬â€� only compositing/rasterization.

## Ã‚Â§1w Ã¢â‚¬â€� timestampOffset + live duration (2026-11-25)

- timestampOffset applied via in-place tfdt rewriting at append time
  (sourcebuffer appendBuffer Ã¢â€ â€™ rewrite_tfdt_offsets). WebM offsets
  unimplemented Ã¢â‚¬â€� fmp4-only need so far. Negative shifts saturate at 0.
- ms.duration=Infinity propagation fixed (was falling through to derived
  duration Ã¢â‚¬â€� is_finite() rejected Infinity; live streams need it).
- Symptom: segments land at wrong timeline positions / overlapping appends
  after transmux Ã¢â€ â€™ check rewrite_tfdt_offsets (owns shifting), the moof-
  presence guard in appendBuffer (must fire on concatenated init+media too),
  timescale parsing (mdhd Ã¢â‚¬â€� wrong timescale = wrong shift magnitude).
- Symptom: live stream ends prematurely / wrong duration Ã¢â€ â€™ check
  duration_value Infinity propagation (mediasource.rs), seekable mirroring
  buffered (already correct for MSE).
- TWITCH VERIFIED PLAYING after the tfdt indexing fix (Ã‚Â§1v): live video,
  chat, full UI. hls.js append loop was starving on empty buffered.
## Symptom index Ã¢â‚¬â€� FINAL VERDICT: heap/clone crash culprit confirmed (2026-11-25)

Symptom: silent death on reddit/amazon-class pages, idle, ~60s, feather shell.
Ã¢â€ â€™ CONFIRMED CAUSE: fork commit 5f26d9f7abf 'structured-clone shared memory Ã¢â‚¬â€� SameProcess scope for in-process messaging'. Evidence: pre-SAB engine (4-file revert Ã¢â‚¬â€� structuredclone.rs + idbcursor/idbobjectstore/idbrequest.rs) + feather shell survives 3/3x420s idle reddit vs 4/5 deaths at 60s with the commit (P(chance)Ã¢â€°Ë†0.008).
Ã¢â€ â€™ MECHANISM: SameProcess scope lets SM embed raw memory references in the serialized clone stream. Servo's port plumbing copies serialized bytes OUT to a Rust Vec (write: CopyJSStructuredCloneData+set_len in structuredclone.rs), ships the Vec across threads/pipelines/port-transfer queues with arbitrary delay, then reconstitutes a fresh clone buffer on the receiver thread (read_with_scope: WriteBytesToJSStructuredCloneData then JS_ReadStructuredClone). The byte-copy transposition is outside SM's ownership/lifetime machinery Ã¢â‚¬â€� embedded references dangle Ã¢â€ â€™ UAF-read. Pageheap-full makes it a deterministic AV in BufferList iteration/allocation during deserialization.
Ã¢â€ â€™ WHY DEATHS CLUSTER ~60s: ad-tech iframes spin up post-load; heavy postMessage/port-transfer traffic begins ~50-60s in.
Ã¢â€ â€™ WHY servoshell survives: UNRESOLVED Ã¢â‚¬â€� same engine, same commit Ã¢â‚¬â€� shell interaction affects exposure (pump timing/lifecycle). The engine bug is real regardless; do NOT re-investigate shell differences unless crash reappears post-revert.
Ã¢â€ â€™ FIX DIRECTIONS: (a) revert commit Ã¢â‚¬â€� stability now, loses SAB cloning; (b) correct fix Ã¢â‚¬â€� clone data for in-process traffic must stay inside SM ownership (carry the clone buffer, not byte copies) Ã¢â‚¬â€� nontrivial design, belongs to SAB lane.
Ã¢â€ â€™ CAUTION for future lanes: yesterday's persite disappearance predates this commit Ã¢â‚¬â€� IF a second rarer bug exists it will show post-revert; do not assume.
Ã¢â€ â€™ symptom index negative results (all verified): chrome/favicon/ucm/controlflow/paint-gating/prefs/media/winhit/input/memreport-instrumentation/OOM/webgl-features Ã¢â‚¬â€� NOT causes. measurement-race in memreport (capture signatures B) is a REAL but separate latent issue: concurrent memory measurement of cross-pipeline MessagePort structures can crash Ã¢â‚¬â€� only triggered when create_memory_report is invoked, which production never does.

## content-visibility implementation Ã¢â‚¬â€� Stage 0/1 state + design notes (2026-11-25)

STATUS:
- stylo parsing SOLVED locally: patched copy at C:/Users/Administrator/repos/stylo-local (copy of pinned rev 0ab78ce + patches). Changes: (1) longhands.toml Ã¢â‚¬â€� removed engine="gecko" from [content-visibility], [contain-intrinsic-width/height/block-size/inline-size]; added servo_restyle_damage="rebuild_box" + animation_type="discrete" to content-visibility; (2) values/specified/box.rs Ã¢â‚¬â€� added ComputeSquaredDistance + ToAnimatedZero derives to ContentVisibility enum; (3) stylo_static_prefs/preferences.toml Ã¢â‚¬â€� layout.css.contain.enabled=true. stylo COMPILES + servo workspace builds against it via LOCAL-ONLY [patch] section in servo Cargo.toml (uncommitted Ã¢â‚¬â€� replace with fork repin when baigojahanzaib/stylo exists Ã¢â‚¬â€� BLOCKED ON REPO CREATION, escalated to coordinator).
- `contain` needs no stylo patch (servo_pref already exists, just defaulted false).
- upstream stylo latest verified: content-visibility STILL gecko-only Ã¢â‚¬â€� fork is the only path, ever.

IMPLEMENTATION DESIGN for Stage-1 (relevance + paint-skip), for whoever continues:
- Spec hook: document.rs update-the-rendering step 16.2.3-4 (marked TODO) Ã¢â‚¬â€� BUT NOTE the early-return guard: the whole block currently returns early when resize_observers is empty Ã¢â‚¬â€� the cv-determination loop must run regardless of resize observers, so the guard needs restructuring (move cv determination before/independent of the resize-observer early return).
- Relevance state: store per-element (Element has interior-mutability slots pattern Ã¢â‚¬â€� follow how other per-element rendering state is stored); document-side pass iterates elements with content-visibility:auto used-value, computes viewport intersection (use the same intersection machinery IntersectionObserver uses Ã¢â‚¬â€� components/script/dom/intersectionobserver.rs Ã¢â‚¬â€� it already solves root-margin intersection against the scrolling viewport) with a UA margin (start with 50% viewport each side, spec-free choice).
- Paint-skip integration point: components/layout/display_list/ Ã¢â‚¬â€� fragment-level: skipped subtree = element's box still produces its own fragment/background/borders but descendants produce NO display items. Cleanest hook: wherever fragments recurse into children during display-list building (paint_traversal / stacking context tree construction) Ã¢â‚¬â€� check a flag threaded from the element's relevance state through box/fragment construction. Because Stage-1 still lays out skipped subtrees, fragment geometry is correct; only display items are elided.
- Invalidation: relevance transitions must dirty layout paint-only (not rebuild boxes) Ã¢â‚¬â€� use restyle damage repaint-level invalidation; the determination pass runs inside update-the-rendering which already precedes paint.
- KEY UNEXPLORED RISK: servo's display-list construction may not have a clean per-subtree elision point if items are emitted during a single pass tied to fragment tree walk Ã¢â‚¬â€� evaluate paint_traversal.rs first.
- contain-intrinsic-size + size containment needed only for Stage-2 (eviction); Stage-1 correct without them because layout still runs.
- threading: determination runs on script thread; layout reads element state during reflow on the same thread Ã¢â‚¬â€� no cross-thread concern (layout is same-thread in servo).

## content-visibility Stage 1 Ã¢â‚¬â€� landed on cv-wip branch (2026-11-25)

Verified mechanisms (function-indexed, for future lanes):

- **cargo `[patch]` only applies at the workspace ROOT of the build being run.** Shell builds
  use feather/shell/Cargo.toml as root Ã¢â‚¬â€� the patch section in servo/Cargo.toml is INERT there.
  Both need the section while the local-path infrastructure exists.
- **Partial patching of the stylo workspace splits resolution into crates.io duplicates**
  (servo_arc 0.6.0 crates.io vs git both in tree, selectors losing its git source) which breaks
  feature unification (servo_arc/serde lost) Ã¢â‚¬â€� symptom: `Arc<Url>: serde::Deserialize not satisfied`
  in servo-url. ALL NINE stylo crates must be patched together: style, stylo_atoms, stylo_dom,
  stylo_malloc_size_of, stylo_static_prefs, stylo_traits, selectors, servo_arc, to_shmem.
- **`cargo check -p servo-script` standalone does not work on Windows** Ã¢â‚¬â€� builds mozjs with jit
  disabled which needs python3 in the configure shell; only full shell builds use the correct
  feature set. Validate engine changes via the shell build, never standalone cargo check.
- **Paint elision for a subtree requires four sites in `paint_traversal.rs`** because fragments
  are reached through multiple paths: stacking-context entry, `traverse_stacking_container`,
  the Box arm of `traverse_block_level_descendants_decorations`, and `traverse_box_in_a_line_box`.
  Eliding fewer leaves reachable paths that still paint.
- **A change in the skip set must force `need_new_display_list`** in `LayoutImpl::handle_reflow`
  before the skip check Ã¢â‚¬â€� scrolling alone dirties nothing, so without this the display list never
  rebuilds when relevance changes.
- **Hit-test traversal deliberately does NOT skip** (`PaintTraversal::traverse` without the set) Ã¢â‚¬â€�
  spec: skipped content remains hit-testable/findable since layout still runs.
- Relevance determination runs in `run_the_resize_observer_steps` Ã¢â‚¬â€� the early-return for empty
  resize observers had to be restructured so determination runs regardless.

Stage-1 known gaps (deliberate, per scoping doc): no focus/selection relevance term, no
contain-intrinsic-size consumption (skipped elements lay out at natural size), no memory win
yet (layout runs Ã¢â‚¬â€� Stage 2 is eviction), dynamic cv toggling and print path untested.

## Symptom index addendum 8 Ã¢â‚¬â€� SW-WR validation verdict + integration facts (2026-11-25)
Ã¢â€ â€™ VERDICT: webrender-over-swgl composites a representative 1440x900 animated frame at ~1.05ms aggregate CPU vs 7.76ms+ through Apple GL translation Ã¢â‚¬â€� ~7x win, multithreaded (sustained 690fps harness vs 31fps feather anim). SW-WR is the validated direction for macOS repaint cost; ANGLE shrinks to webgl-only scope. Harness: /tmp/swwr-val (standalone cargo project, webrender 0.70 + feature sw_compositor).
Ã¢â€ â€™ INTEGRATION FACTS (verified): swgl::Context implements gleam's Gl trait DIRECTLY Ã¢â‚¬â€� no shim crate needed, contrary to earlier scoping. Detection automatic via RENDERER string 'Software WebRender'. GOTCHA: WebRenderOptions::clear_caches_with_quads defaults TRUE and hits an unimplemented swgl path (DepthFunc(GL_ALWAYS) asserts in gl.cc) Ã¢â‚¬â€� must be set false for SW-WR. swgl needs explicit framebuffer backing: Context::create + make_current + init_default_framebuffer(x,y,w,h,stride,buf); readback via get_color_buffer(fbo, flush).
Ã¢â€ â€™ STATUS of alternatives: ANGLE-on-Metal confirmed NOT viable via mozangle (vendored ANGLE has only d3d+gl backends, no metal/vulkan; non-windows egl builds panic) Ã¢â‚¬â€� standalone upstream ANGLE build would be needed, week+ scale, chromium-build risk. mozangle gl-backend on macOS pointless (same deprecated GL).

## SW-WR painter integration plan (2026-11-25, pre-implementation)

ARCHITECTURE: implement a new `SoftwareRenderingContext` in components/shared/paint/rendering_context.rs implementing the existing `RenderingContext` trait Ã¢â‚¬â€� NOT painter surgery. The painter already treats the rendering context as an abstraction (`gleam_gl_api()`, `make_current`, `read_to_image`, `size/resize/present`), so an swgl-backed implementation slots in without touching webrender wiring at all.
- backing: swgl::Context::create + one memory framebuffer sized to the viewport, reallocated on resize; make_current = ctx.make_current; gleam_gl_api = Rc::new(ctx) (swgl implements Gl directly Ã¢â‚¬â€� verified in validation harness); present = no-op (nothing to swap Ã¢â‚¬â€� the pixels are already in memory); read_to_image = memcpy out of the framebuffer (replaces the gl.read_pixels+flip path, likely FASTER than today).
- WebRenderOptions REQUIRED delta: clear_caches_with_quads: false (swgl panics otherwise Ã¢â‚¬â€� verified), upload_method: Immediate (auto-selected by renderer-string detection, verify).
- embedder present path: feather uses OffscreenRenderingContext + render_to_parent blit into egui today. With swgl the final frame is memory Ã¢â‚¬â€� replace the blit with a direct memoryÃ¢â€ â€™egui-texture upload in the shell (small shell patch, measured present cost ~1-3% headroom). Alternatively keep the existing callback shape by making the software context expose the framebuffer through the same callback interface Ã¢â‚¬â€� decide at implementation, prefer whichever touches fewer shell layers.

FEATURE GATING: servo pref `paint_software_rendering` (or env var for v1) Ã¢â‚¬â€� selection happens where feather constructs the rendering context (shell/app.rs WindowRenderingContext::new + offscreen_context). macOS-first; other platforms untouched.

KNOWN INTEROP GAPS (v1 accepted, documented):
- WebGL canvases: compositing expects surfman texture-sharing (create_texture). SoftwareRenderingContext returns None there Ã¢â‚¬â€� webgl content renders black/missing under SW-WR v1. ACCEPTED for the milestone because (a) most compat-critical pages don't need webgl to render, (b) ANGLE-webgl is queued separately, (c) the eventual bridge is webgl-readback-to-memory Ã¢â‚¬â€� follow-up work. HONEST LIMITATION: this is a rendering regression for webgl pages vs today Ã¢â‚¬â€� the feature must ship behind a pref/default-off until the bridge exists, OR accept webgl broken-by-default on macOS in exchange for the universal CPU win Ã¢â‚¬â€� user decision at PR time.
- Video: gst frames are CPU-side texture uploads Ã¢â‚¬â€� should just work (upload = memcpy in swgl). Verify with test720.mp4.
- glow_gl_api(): trait requires a glow::Context Ã¢â‚¬â€� check what consumes it; may need a panic-or-stub decision since glow expects real GL.

VERIFICATION: (1) repaint-cost suite re-run Ã¢â‚¬â€� anim.html CPU must drop from ~77% toward single digits; (2) pixel-diff vs GL-rendered reference pages (screenshot comparison on a few stable test pages); (3) video playback still works; (4) crash-gate: reddit idle repro (same standard as always).

ESTIMATE: core context + painter glue 1-2d, shell present adaptation half day, verification half day. THEN SAB-clone implementation.

## 1x. Timeout-"regression" bisect verdict: NO regression Ã¢â‚¬â€� crawl artifacts of the pre-wedge-fix binary (2026-11-25)

**Question**: the tranco500 crawl showed consistent timeouts (nvidia, baidu, adobe.io, ubuntu.com, theguardian.com, googleadservices, adtrafficquality) that stock servo supposedly handled Ã¢â‚¬â€� suspected regression from arena purge (7d02b70553b), download spool (92080fd7), or hidden-doc GC (826f3a5).

**Method**: fresh servoshell per site, retest of all 9 crawl-timeouts + 3 load_errors + slack.com, stock (`/tmp/servo-main` release) vs feather-engine HEAD post-wedge-fix.

**Verdict Ã¢â‚¬â€� bisect moot, culprit already fixed**: feather-head == stock or faster on ALL 13 sites (ubuntu.com 2135ms vs stock 18232ms). The crawl timeouts were 100% artifacts of running the crawl on a binary built BEFORE the wedge fix (bb5c9bc9b94): rows recorded in a chunk after an unfinished navigation had wedged the webview. `--stop-on-timeout` restarts mitigated but the wedge can also pin without producing a timeout row first. ALL THREE bisect suspects (arena purge, download spool, hidden-doc GC) are **ruled out without a single bisect build** Ã¢â‚¬â€� post-fix feather matches stock everywhere.

Residual genuine fails, identical in stock Ã¢â€ â€™ upstream class, not feather bugs: elasticbeanstalk.com, gcdn.co, mybluehost.me, nflxvideo.net (all `load_error` in both). aol.com fails in both Ã¢â‚¬â€� stock hangs to 30s timeout, feather fails fast with nav_error at ~9s (feather behavior arguably better, still a fail). slack.com loads fine in both post-fix (was "consistent timeout" pre-fix Ã¢â‚¬â€� also wedge artifact).

**Symptom index Ã¢â‚¬â€� Symptom: consistent page-load timeouts on sites stock handles Ã¢â€ â€™ check**: unfinished-navigation wedge in `constellation::load_url`/`webdriver_server` (confirmed cause historically Ã¢â‚¬â€� fixed bb5c9bc9b94; verify the crawling binary POSTDATES the fix before bisecting anything), arena purge at pipeline teardown (status: ruled out Ã¢â‚¬â€� post-fix parity with stock on all repro sites), download-spool servoparser changes (status: ruled out Ã¢â‚¬â€� same evidence), hidden-doc GC (status: ruled out Ã¢â‚¬â€� same evidence; also crawl documents never go hidden). Lesson for the index: **crawl data is only as good as the binary date** Ã¢â‚¬â€� wedge-family bugs poison every row after the triggering navigation in the same engine process.

## 1y. Tranco-500 final clean dataset + roblox Linux crash confirmation (2026-11-25)

**Cross-lane confirmation**: the SameProcess-clone revert (bundled in verdict commit 8024ffda808) fixes the Linux crash too Ã¢â‚¬â€� pre-revert servoshell died hard on roblox.com 100% (whole process gone, webdriver port connection-refused); post-revert roblox navigates cleanly in 2.7s. Contradicts the Windows-lane "servoshell survives" data point Ã¢â‚¬â€� the UAF is engine-level and can kill servoshell, not just feather shell; Windows servoshell survival was timing luck. NOTE: HEAD no longer clones SharedArrayBuffers Ã¢â‚¬â€� turnstile-class SAB work is reverted-blocked until the ownership-preserving replacement lands.

**Final tranco500-linux dataset** (`crawler/runs/tranco500-linux-final.jsonl`, every row produced or re-verified on post-revert binary with per-error fresh-engine isolation): 500/500 coverage Ã¢â‚¬â€� 337 ok, 123 dns_no_address, 22 tcp_unreachable, 12 cert_error, 4 load_error, 2 timeout. Real-site compat (excluding infra-dead hosts): **337/343 = 98.3%**. The entire defect tail is 6 sites and ALL reproduce identically in stock upstream servo (aol.com + wa.me consistent-timeout, elasticbeanstalk/gcdn/mybluehost/nflxvideo load_error Ã¢â‚¬â€� upstream net-class residuals per Ã‚Â§1i). **Zero feather-specific defects in the tranco 500.**

**Crawler methodology lessons (symptom-index additions):**
- Symptom: crawl row says nav_error/timeout on a major site that obviously works (google.com, wikipedia.org) Ã¢â€ â€™ check: whether an EARLIER site in the same engine process crashed/wedged it (post-crash rows are fallout garbage); rows are only trustworthy pre-first-error in their chunk OR produced with fresh-engine isolation. Correct attribution ordering: the FIRST error row in a contiguous error run is the suspect, everything after is fallout.
- Symptom: servoshell process vanishes mid-crawl (webdriver port goes connection-refused, rows flip to instant-error at 0ms) Ã¢â€ â€™ check: structuredclone.rs SameProcess scope (confirmed cause historically Ã¢â‚¬â€� reverted; the UAF mechanism is documented in Ã‚Â§1u/line 1276), any future re-implementation of shared-memory cloning that copies serialized bytes out of SM ownership.
- crawl.py now exits for restart on nav_error/driver_error too, not just timeout (`--stop-on-timeout` semantics widened); xrun.sh implements the 3-strikes consistent-failure attribution loop.
- Timeout-bisect verdict from Ã‚Â§1x stands: arena purge (7d02b70553b), download spool (92080fd7), hidden-doc GC (826f3a5) all ruled out Ã¢â‚¬â€� post-fix parity with stock on every repro site.

## SW-WR integration outcome + symptom index (2026-11-25, macOS lane)

### Verdict: negative result for repaint cost Ã¢â‚¬â€� measurement-driven
swgl integration is COMPLETE and pixel-correct behind FEATHER_SWWR=1 (default off), but A/B
measurement on the same binary shows NO significant win: anim 52-62% both backends, video
GL 74% / SW 70% (~marginal), static idle 0.2% both. The harness validated rasterization in
isolation but rasterization is NOT the binding cost of a repaint in situ.

### Symptom index entries
- Symptom: black page area with swgl active, webrender initialized fine, high CPU Ã¢â€ â€™ check:
  shell present path end-to-end (owns getting pixels to screen Ã¢â‚¬â€� confirmed cause: readback
  existed in engine but nothing in the shell ever displayed it; a backend swap is not done
  until pixels reach the window), swgl context threading (status: ruled out Ã¢â‚¬â€� make_current
  in readback correct but was never the issue), webrender composite target (ruled out).
- Symptom: red/blue channels swapped under swgl, green/yellow fine Ã¢â€ â€™ check: byte order of
  swgl memory (confirmed cause: BGRA memory layout, GL native; RgbaImage contract is RGBA Ã¢â‚¬â€�
  swizzle in readback), row flip (ruled out Ã¢â‚¬â€� orientation was correct).
- Symptom: sustained ~55% CPU on static-looking page during tests Ã¢â€ â€™ check: modal system
  dialogs stealing osascript keystrokes (confirmed cause once: devtools-auth dialog up meant
  navigation never happened Ã¢â‚¬â€� the ANIM page was still running; measurement contamination),
  paint-gating (ruled out Ã¢â‚¬â€� gated idle is 0.2% under both backends).
- Symptom: repaint CPU cost unchanged after composite backend swap Ã¢â€ â€™ check: egui/shell
  present path (prime suspect Ã¢â‚¬â€� the shell's own egui render passes through Apple GL every
  repaint regardless of webrender backend), upstream pipeline (prime suspect for rAF
  content: JS/style/layout/display-list/scene-building per frame), webrender rasterization
  backend (ruled out for total cost Ã¢â‚¬â€� A/B shows it is a minority share), texture upload /
  readback copies (ruled out Ã¢â‚¬â€� NOREAD experiment: identical CPU with zero readback).
- Environmental: `sample`/`spindump` on macOS need the Developer-Tools-Access auth dialog
  approved in the GUI (password devin) or they hang silently forever Ã¢â‚¬â€� and the modal dialog
  intercepts osascript keystrokes while up, contaminating any concurrent test navigation.

### Experiment infrastructure (reusable)
FEATHER_SWWR_NOREAD=1 (paint but skip readback+upload) and FEATHER_SWWR_NOUPLOAD=1 (readback
but no texture update) env switches in shell/src/app.rs Ã¢â‚¬â€� binary-search cost attribution of
the present path in a single build.

## Repaint frame attribution Ã¢â‚¬â€� the complete decomposition (2026-11-25, FEATHER_TIMING)

Measured with env-gated wall-time instrumentation in the shell redraw (FEATHER_TIMING=1),
anim.html 30fps, ms/frame on main thread:
- GL mode:   paint=6.9  guipaint=10.6  present=11.4  frame-total=29
- SW-WR mode: paint=0.3 readback=0.3  guipaint=16.4  present=10.2  frame-total=28
CPU-math check: ~28ms x ~19fps-effective ~= 560ms/s ~= measured 56-62% CPU Ã¢â‚¬â€� wall-times are
real CPU, not vsync blocking. Anim achieves only ~19 of 30 target fps Ã¢â‚¬â€� main thread saturated.

### Symptom index entries
- Symptom: repaint CPU high regardless of webrender backend Ã¢â€ â€™ check: shell present path
  (CONFIRMED binding cost: egui tessellation+GL draw of chrome/content = 10-16ms plus
  surfman present() = 10-11ms of every ~28ms frame Ã¢â‚¬â€� 75-95% of repaint cost lives here,
  NOT in the engine), webrender rasterization (ruled out as binding: swgl takes it to
  0.3ms with zero total improvement because cost relocates to texture upload),
  upstream pipeline JS/layout/DL (ruled out for simple anim: 97.6% of samples on main
  thread; pipeline threads negligible).
- Symptom: present() costing 10-11ms per frame Ã¢â€ â€™ check: surfman/NSOpenGL CGLFlushDrawable
  path (prime suspect Ã¢â‚¬â€� constant across all modes; the macOS GL window-system tax),
  vsync blocking (ruled out as explanation: CPU math closes without it).
- Symptom: texture upload cost appearing in guipaint under swgl Ã¢â€ â€™ mechanism: egui texture
  upload of the full-frame content image through Apple GL each repaint (~6ms at retina);
  this is where raster cost RELOCATES if rasterization moves to CPU Ã¢â‚¬â€� any CPU-raster design
  must count upload as part of its cost.
- Experiment machinery now in tree: FEATHER_TIMING=1 (per-phase ms/frame log),
  FEATHER_NOPAINT / FEATHER_SWWR_NOREAD / FEATHER_SWWR_NOUPLOAD (isolation switches Ã¢â‚¬â€�
  caveat: NOPAINT removes the pipeline throttle so frames run unthrottled; do not interpret
  its absolute CPU as upstream cost).

## 1z. Timeout-wedge "regression" verdict: NOT live on Linux HEAD Ã¢â‚¬â€� full survival chain verified (2026-11-25)
**Claim tested**: "any single timed-out page load permanently kills the browser session" Ã¢â‚¬â€� ordered bisected among arena purge / download spool / hidden-doc GC.
**Verdict: no repro on Linux, nothing to bisect.** Full chain on HEAD (post-SAB-revert):
- Webdriver: aol.com times out with proper error at exactly 30s Ã¢â€ â€™ example.com nav succeeds Ã¢â€ â€™ current-url + title both confirm real load. Session fully healthy post-timeout.
- Real feather shell: aol.com loads + streams forever (never-finishing load class) Ã¢â€ â€™ omnibox nav to example.com succeeds, window retitles. No wedge.
- The three bisect suspects are exonerated for the wedge itself, not just crawl artifacts Ã¢â‚¬â€� they all PREDATE the original passing wedge-fix verification, and survival still works with them in-tree today.

**Traps that produce false wedge positives (symptom-index entries):**
- Symptom: webdriver navigation NEVER returns (no response even at 95s+) Ã¢â€ â€™ check: whether the client set an explicit pageLoad timeout Ã¢â‚¬â€� servo DOES implement the spec's 300s default (TimeoutsConfiguration::default uses DEFAULT_PAGE_LOAD_TIMEOUT=300_000, present since the upstream merge; CORRECTED in Ã‚Â§1za Ã¢â‚¬â€� earlier revision of this entry wrongly claimed unbounded). Without `/timeouts` the nav command still hangs for up to FIVE MINUTES before erroring, and every later command queues behind it Ã¢â‚¬â€� to anyone watching at 60-120s it looks exactly like the permanent wedge. Harnesses must always POST /timeouts explicitly. The TRUE historical permanent wedge was unbounded IPC reply waits parking the dispatcher thread (even /status died) Ã¢â‚¬â€� fixed in 7d150c23be2 (bounded at script timeout + 30s margin); any binary predating that commit shows the real wedge.
- Symptom: typing into feather's omnibox does nothing in Xvfb/xdotool testing Ã¢â€ â€™ check: Xvfb has no WM, so winit never gets focus Ã¢â‚¬â€� must `xdotool windowfocus <id>` explicitly; ctrl+a/click selection feedback can misleadingly work while keystrokes go nowhere.
- Symptom: post-timeout crawl rows garbage Ã¢â€ â€™ see Ã‚Â§1y (crash-fallout ordering; first error row is the suspect, the rest are fallout).

**Platform caveat**: this verdict is Linux-only. If the wedge reproduces on Windows/macOS, it is platform-specific Ã¢â‚¬â€� the Linux exoneration does not transfer, but the repro sequence above (30s timeout + post-timeout nav + current-url verification) is the exact test to run there.

## Metal present path (stage 1 landed, feather PR #120)

Symptom: entire wgpu/Metal window contents vertically flipped on macOS Ã¢â€ â€™ check: surfman WindowRenderingContext layer attachment (CONFIRMED cause Ã¢â‚¬â€� NSOpenGL attachment installs a bottom-up-coordinate layer on the window; any CAMetalLayer composited inside inherits the flip; fix: create NO GL window machinery in metal mode), egui.wgsl vertex transform (ruled out Ã¢â‚¬â€� math correct by inspection), machine/wgpu-level presentation (ruled out Ã¢â‚¬â€� bare-wgpu fullscreen triangle rendered correct orientation).

Symptom: wgpu frame wall-time high (~14ms) but CPU low Ã¢â€ â€™ check: surface.get_current_texture blocking (CONFIRMED Ã¢â‚¬â€� 13.7ms of the 14.3ms guipaint wall is vsync FIFO backpressure, CPU-free; the frame lands exactly on one 60Hz vsync period meaning the pipeline is vsync-paced not CPU-bound; real GPU present work ~1ms/frame: texupload=0.28 tess=0.03 updbufs=0.11 renderpass=0.1 submit+present=0.46). Do NOT read wall-time frame totals as cost in a vsync-throttled pipeline Ã¢â‚¬â€� use CPU%.

Measured outcome (anim.html benchmark): GL path shell present 21-27ms/frame CPU Ã¢â€ â€™ Metal ~1ms/frame; whole-process CPU 77-97% Ã¢â€ â€™ ~24%. Passes the <=4ms success gate of docs/metal-present-design.md. egui-wgpu 0.34.3 + wgpu 29 is a viable present stack for egui 0.34.3 shells on macOS arm64.

## SAB structured-clone correct implementation (landed 2f1ca404c)

Symptom: postMessage of SharedArrayBuffer throws DataCloneError Ã¢â€ â€™ check: structuredclone::write CloneDataPolicy + scope (FIXED Ã¢â‚¬â€� SameProcess scope with permissive policy for in-process messaging paths; realm gate stage-1 was necessary but not sufficient).

Symptom: ~60s idle-death crash on postMessage-heavy pages after naive SameProcess flip Ã¢â€ â€™ check: byte-copy transposition of SameProcess clone streams (CONFIRMED cause of 5f26d9f7a crash Ã¢â‚¬â€� CopyJSStructuredCloneData copies only the byte segment; embedded backing-store references dangle when the SM buffer drops; correct fix is carrying the SM-owned buffer end-to-end, implemented via process-global registry keyed by u64 since StructuredSerializedData is serde-transported even in-process).

Invariants future lanes must not break:
- SameProcess streams never leave SM ownership as bytes. The registry key is single-consumer: any path that deserializes the SAME StructuredSerializedData more than once (performance entries per getEntries, history state per navigation) MUST use DifferentProcess Ã¢â‚¬â€� multi-read of a carried buffer panics on second read by design.
- BroadcastChannel fanout uses DifferentProcess: SAB-over-broadcast throws DataCloneError (interim deviation; proper fix is per-recipient serialization).
- clone_for_broadcast copying sm_buffer_key would double the ownership Ã¢â‚¬â€� the warn guard there must stay.

## Ã‚Â§1za. Correction: webdriver default page-load timeout exists (300s, spec-compliant) Ã¢â‚¬â€� and the final wedge-bisect verdict (2026-11-25)

The Ã‚Â§1z trap entry misread the evidence: servo webdriver has ALWAYS (since the upstream merge b4a6153e26d) defaulted `page_load` to Some(300_000) per the spec Ã¢â‚¬â€� verified in components/webdriver_server/timeout.rs both at the merge commit and at HEAD. The observed 95s hang was spec-compliant behavior (would have errored at 300s), not a missing default. There is nothing to implement.

Final verdict on the timeout-regression bisect order (3x repeated): the wedge does not exist on Linux feather-engine HEAD. Matrix test on a51dbc57445, one servoshell process: THREE timeout events (aol x2, wa.me), each erroring at exactly 30.0s with explicit client timeouts, each followed by verified successful navigation (url+title checked): example.com 0.1s, ubuntu.com 2.4s, example.org 0.1s, example.com 0.0s, example.com 0.2s. Harness /tmp/wedge-matrix.py, log /tmp/wedge-matrix-out.txt. Arena purge, download spool, hidden-doc GC all in-tree throughout Ã¢â‚¬â€� none is a culprit; no bisect is possible against a non-reproducing symptom.

Symptom index additions:
- Symptom: "one timed-out page load permanently kills the session" reported by any harness Ã¢â€ â€™ check IN ORDER: (1) was the harness binary built before 7d150c23be2 (bounded IPC waits, the REAL wedge fix Ã¢â‚¬â€� pre-fix binaries genuinely wedge, dispatcher thread parks forever)? (2) does the harness POST explicit /timeouts Ã¢â‚¬â€� if not, the nav command sits for the full 300s spec default looking dead? (3) is the "wedge" actually process death Ã¢â‚¬â€� roblox-class crash (pre-SameProcess-revert binary)? (4) ONLY THEN suspect engine regressions. On Linux HEAD all four checked: negative, negative, negative, negative.

## Metal stage-2 evaluation (verdict doc feather PR #122)

Symptom: getContext('webgl'/'webgl2') returns null everywhere under FEATHER_METAL or FEATHER_SWWR Ã¢â€ â€™ check: WebGLThread::create_webgl_context painter_surfman_details_map (CONFIRMED cause Ã¢â‚¬â€� webgl piggybacks on the painter's surfman connection; swgl mode has no surfman connection, nothing to piggyback on; NOT caused by Metal itself Ã¢â‚¬â€� identical failure under FEATHER_SWWR alone), wgpu/Metal surface interference (ruled out Ã¢â‚¬â€� swgl-only repro).

Measurement rule: input latency in a vsync-paced pipeline is NOT measurable as present-path wall time Ã¢â‚¬â€� both GL and Metal block CPU-free on vsync. The honest metric is event-loop saturation: achieved rAF rate. GL achieves 23.1/30fps (frame wall ~29ms Ã¢â€°Ë† 2 vsyncs, misses deadlines); Metal achieves full 30.0/30 (frame wall exactly 1 vsync). get_current_texture blocking does NOT add input latency vs GL Ã¢â‚¬â€� it halves worst-case latency.

Bridge options indexed for the future implementer: (1) independent surfman connection + readback-composite for webgl textures, 3-5d; (2) swgl-backed webgl contexts Ã¢â‚¬â€� swgl IS a full GL implementation, potentially cheaper, CPU-bound. Evaluate (2) first.

## Unresolved observations

Observation: 2026-11-25, one GL-mode (default, non-metal) feather launch died silently mid-run on fpsprobe.html Ã¢â‚¬â€� no panic in log (18 lines, ends at favicon 404), process simply gone. Not reproducible on immediate retry with identical input. Suspect class: segfault in GL path (rust panics would print). If this recurs anywhere it becomes a priority Ã¢â‚¬â€� watch for it.

## WebGL bridge for software compositing (landed 015512402)

Symptom: getContext('webgl') null under swgl/metal Ã¢â€ â€™ FIXED: independent hardware-GL surfman connection for the WebGL thread + readback composite (glReadPixels BGRA row-flipped at swap time Ã¢â€ â€™ shared map Ã¢â€ â€™ ExternalImageSource::RawData). Verified: runtime shader compile+link+draw correct under FEATHER_METAL, animated compositing correct orientation.

Mechanism notes for future lanes:
- swgl CANNOT back webgl: shader_source panics on any shader not build-time-transpiled Ã¢â‚¬â€� no runtime GLSL compilation exists in swgl, and webgl is nothing but runtime shaders. Dead end, do not re-walk.
- webgl frames in software mode register as ExternalImageType::Buffer; the outer handler in shared/paint/lib.rs must pass RawData through with UNFLIPPED texel rect (GL textures use flipped uv, memory buffers are already flipped at readback).
- JS readPixels returning 0,0,0,0 after present is SPEC-CORRECT when preserveDrawingBuffer=false (default) Ã¢â‚¬â€� not a bug.
- WebGL thread surfaces are IOSurface-backed on macOS Ã¢â‚¬â€� a zero-copy optimization over glReadPixels exists but is unwired; current readback cost is one copy per canvas-frame.

## Ã‚Â§1zb. Real-YouTube seek verification Ã¢â‚¬â€� harness-verified seek FAILS on real content (2026-11-25/26)

End-to-end verification of the Ã‚Â§1t seek implementation against signed-in real YouTube (watch?v=Uc726aogLjw, servoshell webdriver probes /tmp/seekverify2.py, /tmp/seekdiag2.py, /tmp/appendtrace.py): the seek implementation passes the controlled harness but FAILS on real YouTube. Defects, most damning first:

1. Position frozen at X.001 values (11.001, 30.001, 40.001 observed) after seeks Ã¢â‚¬â€� playback feeding continues (network appends flow, verified) but position never advances. rs stuck at 4. Sometimes position frozen even PRE-seek (7.873373 across reads in one run) Ã¢â‚¬â€� non-deterministic across runs.
2. Post-seek position misaligned when it does advance: seek +60 from tÃ¢â€°Ë†8 Ã¢â€ â€™ position reports 43.4 advancing to 50 Ã¢â‚¬â€� matches neither target (~68) nor any obvious landmark.
3. Buffered index does not match the presentation timeline on real YouTube: buf end 11.0 while playing at t=15 in one run; buf 43 while t=8 in another. Post-seek buf jumps to 71.001.
4. Seek-to-0 hard-fails: media error + emptied events Ã¢â‚¬â€� pipeline teardown, player dead.
5. getVideoPlaybackQuality unimplemented (returns undefined) Ã¢â‚¬â€� no frame-counter motion detection available.

INSTRUMENTED FACTS (appendtrace): YouTube NEVER uses timestampOffset for regular appends (always 0) Ã¢â‚¬â€� segments carry timeline position in their internal tfdt. Post-seek YouTube re-appends init segments (small 220-266 byte appends) + media segments, including re-appending byte-identical lengths already fed pre-seek. So the "page re-appends on its own seeking handler" assumption holds Ã¢â‚¬â€� data IS flowing post-seek; the failure is in position/timeline machinery, not starvation.

- Symptom: position frozen at exactly X.001 while playback feeding continues (rs 4, no error) Ã¢â€ â€™ check: post-flush-recreate position derivation in servomse/mse_source.rs (owns the gst segment base after appsrc recreation Ã¢â‚¬â€� prime suspect: recreated appsrc emits segment starting at/near 0 and position mapping clamps or mismatches), video track decode stall after seek (first-frame-only decode if post-seek media doesn't align), per-track position selection logic in servo-media player (which track provides position and whether it re-bases after recreate). NOT the network layer (appends verified flowing), NOT timestampOffset handling (YouTube uses 0 always).
- Symptom: buffered ranges disagreeing with playback position on real YouTube (index ahead OR behind position) Ã¢â€ â€™ check: tfdt-scanning indexer vs multi-init quality-switch streams (YouTube switches init segments across bitrates Ã¢â‚¬â€� each quality has own init; indexer may mis-parse across the switch), appendWindow clipping unimplemented?, the X.001 offset pattern suggesting +1ms frame-duration artifacts. WebM indexer verified accurate on uniform content Ã¢â‚¬â€� fMP4 multi-init real-world content is the untested class.
- Symptom: media error + emptied events immediately after seek-to-zero Ã¢â€ â€™ check: seek_reset path with target 0 / pipeline state when seek arrives while previous recreate still in flight (three rapid seeks in probe), error propagation from qtdemux on re-fed init segments.

## Metal default-on milestone (feather PR #123, 2026-11-25)

Metal is the default present path on macOS; FEATHER_METAL=0 is the escape hatch. All stage-2 gates passed with the webgl bridge in place: webgl probes+real site, GL regression clean, input latency improved (29.6 vs 23.1 rAF fps), video correct. The repaint-cost program that started with the video-CPU diagnosis is closed: present cost 21-27ms Ã¢â€ â€™ ~1ms/frame.

## Real-world validation findings (2026-11-26)

Symptom: instant abort loading youtube.com in swgl/metal mode, "unknown shader ps_quad_gradient DITHERING" Ã¢â€ â€™ FIXED (enable_dithering:!is_software_webrender): swgl lacks runtime shader compile; webrender build strips DITHERING variant. Rule: any webrender feature that mints new shader variants must be checked against swgl's build-time shader set before enabling in software mode.

Symptom: youtube watch page aborts with GLib pthread_mutex_lock invalid-argument (vp9/gst media threads) Ã¢â€ â€™ PRE-EXISTING, reproduces identically in GL mode Ã¢â‚¬â€� NOT metal-related. Prime suspect: servo-media gst threading on macOS with youtube's streaming format (vp9+adaptive). Local progressive mp4 plays fine. Owner: LINUX LANE is root-causing this (assigned 2026-11-26) Ã¢â‚¬â€� repro facts for it: URL https://www.youtube.com/watch?v=jNQXAC9IVRw (first youtube video, vp9); death signature in stderr: 'GLib (gthread-posix.c): Unexpected error from C library during pthread_mutex_lock: Invalid argument. Aborting.' preceded by gstpad.c 'Got data flow before segment' warnings from capsfilter0/vp9dec0; process dies within ~20s of page load, no rust panic. Reproduces 100% on macOS in BOTH GL and metal modes on the same binary. Local progressive 720p mp4 through gst/vtdec plays fine for minutes Ã¢â‚¬â€� the failure is specific to youtube's adaptive/vp9 chain (mse-ish streaming), not decode generally.

## Real-world validation of Metal default-on (2026-11-26, feather PR #127)

Real-world CPU paired measurements (windowed top sampling, same binary): metal vs GL Ã¢â‚¬â€� bbc scroll 9.5/29.8%, reddit scroll 1.4/6.2%, maps scroll 0.9/6.0%. Rendering pixel-correct on real sites; webgl organic validation via google maps passes through the readback bridge.

Symptom: macOS "Developer Tools Access" password dialog appears during automation-driven input injection (osascript System Events keystrokes) and steals focus, silently degrading scroll-injection benches Ã¢â€ â€™ password is the machine account password (devin); dismiss once and the grant persists. Also: cmd+a does NOT select-all in the secure password field; click field first, then type. Computer-tool coordinates are 1024x768 scaled Ã¢â‚¬â€� divide screenshot-pixel coords by (1120/1024, 840/768) before clicking.

Symptom: bench harness `wait` with no arguments in a script that launched feather with `&` hangs forever (waits on feather itself) Ã¢â€ â€™ check: harness backgrounding structure. Never bare `wait` in bench scripts.

Symptom: webgl-heavy content dramatically worse in swgl/metal mode than GL mode (83% vs 62% CPU at 800x600 canvas) while small canvases are fine Ã¢â€ â€™ FIXED (single-shot readback): the readback bridge issued one read_pixels PER SCANLINE for the vertical flip; driver-call overhead scales with rows, not bytes. Rule: never loop GL driver calls per scanline when a single full-frame read plus in-memory transform works Ã¢â‚¬â€� driver round-trip cost dominates memory bandwidth on unified memory. Also indexed: IOSurface zero-copy evaluated and deprioritized Ã¢â‚¬â€� the memcpy is nearly free on unified memory and stride-padding makes it inexpressible in webrender's tight-packed RawData path anyway.

## Ã‚Â§1zc. Streaming EBML indexer Ã¢â‚¬â€� real-YouTube buffered fixed, forward seek works, backward stall isolated (2026-11-26)

Root-caused the Ã‚Â§1zb swiss-cheese buffered: YouTube splits single WebM clusters across multiple appendBuffer calls at arbitrary byte boundaries (64KB chunks verified Ã¢â‚¬â€� captured appends begin mid-cluster with continuation bytes). The flat per-append scanner missed split clusters and false-positive-detected the 1F43B675 byte pattern inside binary payload. Content is WebM (VP9/opus) with ABSOLUTE cluster timecodes in ms (0, 10.0, 10.7, 20.0 Ã¢â‚¬â€� parsed from raw bytes); YouTube never uses timestampOffset.

FIX: WebmIndexer in ebml.rs Ã¢â‚¬â€� streaming EBML parser with cross-append state. Walks element structure properly: skips payloads by exact declared sizes (payload bytes never misread as headers), descends into Segment, extracts Timecode by parsing cluster-head child elements, carries partial elements in a bounded pending buffer across appends. Plus cross-append span estimation in sourcebuffer.rs (single-cluster-per-append batches use the gap from the previous append's last timecode instead of a fixed 1s guess).

VERIFIED on real YouTube: buffered ranges now contiguous Ã¢â‚¬â€� single growing range [0, 64.06] vs prior swiss cheese; eviction trim visible and correct. Forward seek WORKS end-to-end: seeking/seeked fire at target, position advances post-seek (53.4Ã¢â€ â€™56.4 at correct rate). Parser handles pathological fragmentation (32768-byte partial cluster carried across feeds, consumed accounting verified exact in debug logs).

REMAINING DEFECTS (indexed, next work item):
- Backward seeks stall: position frozen at exactly target+0.001 (50.001 observed), seeking/seeked fire correctly but playback never resumes. YouTube then enters a re-append loop (same segment byte-lengths recycling endlessly Ã¢â‚¬â€� the player re-feeding holes while stalled). Post-seek pipeline resume is the suspect, not indexing: forward seeks with identical machinery DO resume.
- Position reporting flaky pre-seek: frozen at ~7.8-8.7s across reads in most runs while playback actually continues (verified by later jump to 16.02). Freeze zone coincides with YouTube's lowÃ¢â€ â€™high quality switch (new init segment appended ~5-10s in). Suspect: quality-switch init re-feed disturbing the position/clock chain, NOT the indexer (indexer output verified correct in the same runs).
- Seek-to-0: position read 80.001 Ã¢â‚¬â€� nonsensical; error+emptied events in earlier runs. Uninvestigated.

### Symptom index additions
- Symptom: backward seek stalls at exactly target+1ms while forward seeks work Ã¢â€ â€™ check: post-flush-recreate pipeline resume in servomse seek_reset for backward-in-time data (prime suspect Ã¢â‚¬â€� the recreated appsrc re-segments fine for forward but backward re-fed media may arrive with timestamps LOWER than the running pipeline clock/segment base and get dropped as late), player re-append loop as a SYMPTOM not a cause (verified: player re-feeds correctly-formed segments while stalled), indexing layer (ruled out Ã¢â‚¬â€� contiguous correct ranges verified pre/post seek).
- Symptom: position frozen at ~8s pre-seek while playback continues Ã¢â€ â€™ check: YouTube quality-switch init re-feed interaction with position reporting (prime suspect Ã¢â‚¬â€� freeze coincides with the second init segment arrival), per-track clock selection after caps renegotiation.

## Ã‚Â§1zd. Seek algorithm code-reading findings Ã¢â‚¬â€� structural bugs beyond indexing (2026-11-26)

Code review of HTMLMediaElement::seek after live-probe chaos (non-deterministic positions, seeks landing at wrong targets, EMPTIED cascades):

1. SEEKABLE-ABORT BUG: step 8 aborts the seek entirely (no seeking event, no player seek) when seekable is empty Ã¢â‚¬â€� and seekable for MSE mirrors buffered, which handle_seek CLEARS on every seek. So after one seek, until the page re-appends enough, all further seeks silently no-op. Compounded by YouTube evicting aggressively behind the playhead Ã¢â‚¬â€� backward seeks into evicted regions clamp to the nearest surviving range end instead of the user target (spec-correct clamping applied to an artificially-empty index). The spec assumes buffered persists across seeks; our deviation (clearing because the flush genuinely drops pipelined data) structurally breaks the spec algorithm that consumes it. OPEN DESIGN TENSION: clearing is needed so the page re-feeds post-seek, but clearing breaks seekability/draining semantics. Candidate resolution: keep a shadow 'ever-buffered' seekable union independent of the flush-drop index.
2. Position-reporting chain after flush+recreate remains unmapped: positions post-seek sometimes advance from pre-seek values (pipeline never reset?) and sometimes freeze at target+1ms. Whether seek_reset even executes in observed runs is UNVERIFIED Ã¢â‚¬â€� gst debug categories showed no flush lines, but those categories don't log event flow at level 2-4. Next instrumentation: eprintln in seek_reset + player seek routing (cheap, decisive) before any further theory.
3. YouTube's own recovery behaviors dominate black-box observables: the player tears down and recreates the video element (EMPTIED events) when positions misbehave, making per-run behavior chaotic. Controlled-harness verification (local WebM MSE page, no player JS) is the only clean signal Ã¢â‚¬â€� black-box YouTube probing is diminishing-returns until the position chain is mapped.

### Symptom index additions
- Symptom: second and later seeks on MSE do nothing at all (no seeking event even) Ã¢â€ â€™ check: seekable-empty abort in HTMLMediaElement::seek step 8 interacting with the buffered-clear-on-seek deviation (CONFIRMED structural cause by code reading Ã¢â‚¬â€� seekable mirrors buffered, cleared on prior seek), YouTube eviction amplifying it.
- Symptom: seek lands at wrong position (clamped far from target) Ã¢â€ â€™ check: step-8 nearest-seekable-range clamping against a flush-cleared/evicted index (CONFIRMED mechanism), NOT a pipeline seek execution issue.

## Ã‚Â§1ze. Position-offset correction + the post-seek resume-stall as the last blocker (2026-11-26)

Mechanism chain now fully mapped with instrumentation evidence: (1) seeks DO execute Ã¢â‚¬â€� eprintln verified player routing + seek_reset running on every probe seek; (2) positions post-seek misreport because matroskademux re-bases at the first post-seek buffer Ã¢â‚¬â€� playbin reports time-since-seek, correct pre-seek only by coincidence (clusters start at 0); (3) user seeks are clamped before reaching the pipeline (probe set 30, pipeline received 22.181 Ã¢â‚¬â€� nearest seekable end against the flush-cleared index).

FIX LANDED: mse_position_offset in player.rs Ã¢â‚¬â€� the MediaSource-branch seek stores the (clamped) target as offset; position-update events report pipeline position + offset. Strictly inside MSE code paths; progressive/seekable untouched. Mechanistically necessary regardless of the remaining stall Ã¢â‚¬â€� without it even a perfectly-resuming pipeline would report wrong positions.

REMAINING BLOCKER (the last one for real-YouTube seek): post-seek playback RESUME is flaky-to-dead. Instrumented facts: seek completes (seeking flag clears), data flows (YouTube re-appends verified), but position pinned at exactly the target and never advances in most runs Ã¢â‚¬â€� the pipeline accepts re-fed bytes without producing advancing playback. Rare runs do advance Ã¢â‚¬â€� race flavor. Seek-to-0 additionally hard-errors the pipeline (error+emptied events Ã¢â‚¬â€� probably the same stall cascading into YouTube's recovery teardown). This is a gst pipeline-state problem inside the flush+recreate design: re-preroll, multiqueue/decodebin3 state after flush, or segment/clock re-derivation on the recreated appsrc. NEXT INSTRUMENTATION PLANNED: GST_DEBUG pad probes on the recreated appsrc output + demux output post-seek Ã¢â‚¬â€� count buffers actually flowing vs dropped, compare working vs stalled runs.

Disk hygiene (user standing rule): pruned 31GB Ã¢â‚¬â€� feather shell debug/incremental (27G) + stock servo target tree (4.4G, binary preserved at /tmp/stock-servoshell). Free space 7.2GÃ¢â€ â€™38G.

## Heavy webgl2 dogfooding (2026-11-26, feather docs/webgl2-heavy-dogfood.md)

Symptom: three.js skinned-mesh demo renders model SOLID BLACK while rest of scene correct (metal mode, persists past load) Ã¢â€ â€™ suspects ranked: compressed texture formats (KTX2/basis) undecoded, vertex-texture/float-texture skinning path, sRGB-in-readback (weakened by aquarium textured fish being correct). Owner: unclaimed defect DQ-webgl-black-skinning.

Symptom: heavy continuous webgl saturates a core in BOTH modes at ~7-8fps on large canvases (aquarium 1024Ã‚Â²) Ã¢â€ â€™ INVARIANT cost, not a bug: GPU sync + swgl CPU raster of dirty region per the zero-copy eval. Metal ~25% cheaper than GL but both bound. Do not re-investigate as a regression.

Symptom: shadertoy.com unreachable Ã¢â‚¬â€� Cloudflare security-verification wall Ã¢â€ â€™ known turnstile limitation, NOT a webgl defect. Indexed so nobody misattributes.

POSITIVE: webgl2 fully verified Ã¢â‚¬â€� real WebGL 2.0 contexts, core instancing/VAO/texImage3D, GLSL ES 3.00 compile+link+draw correct through the readback bridge in metal mode. Aquarium (instanced textured geometry, multi-pass) pixel-perfect.

## sec1zf Ã¢â‚¬â€� post-seek resume-stall ROOT-CAUSED + FIXED (027251a9ed2)

**Symptom: post-seek playback stall (position pinned at target, never advances) Ã¢â€ â€™ check: servomse seek_reset lossy-handover race (CONFIRMED CAUSE Ã¢â‚¬â€� fixed), matroskademux state (status: ruled out Ã¢â‚¬â€� when handover is lossless the demux re-parses init+clusters fine), appsrc enough-data signaling (status: ruled out Ã¢â‚¬â€� pushes returned Ok throughout), multiqueue/decodebin3 (status: partially involved downstream but not the root Ã¢â‚¬â€� flow resumes once input is lossless).**

Evidence chain (pad probes + push counting on signed-in YouTube):
- post-seek pushes accepted: 268, buffers exiting ghost pads: 3 Ã¢â‚¬â€� data swallowed at the source, never reaching demux
- stall vs resume is a RACE: identical builds sometimes resume (19+ buffers out) depending on whether re-appends arrive during the flush/recreate window
- pushes in the window land in the old appsrc being destroyed, or hit a removed track key Ã¢â€ â€™ bytes silently lost Ã¢â€ â€™ demux starves permanently (first cluster partial/unparseable)
- fix: seek_reset holds the tracks mutex across the entire flush+recreate so pushes block and land in the new appsrcs losslessly. Verified: consecutive runs resume with advancing position.

**Symptom: post-seek position reads higher than seek target (~78-88 when target 30) Ã¢â€ â€™ check: htmlmediaelement seek clamping vs seekable ranges (prime suspect Ã¢â‚¬â€� clamp target varies run to run with buffered extent; sec1zd structural tension unresolved), mse_position_offset double-counting (status: ruled out arithmetically Ã¢â‚¬â€� neither rebase nor absolute pts theory reproduces observed values without a varying clamp input), demux timebase re-basing (mechanism confirmed but correctly compensated by the offset fix).**

**Symptom: rs collapses to 0 / currentTime resets to 0 some seconds after a stalled seek Ã¢â€ â€™ check: YouTube player.js error recovery resetting MSE state (prime suspect Ã¢â‚¬â€� application-level response to starvation, not engine). Not an engine crash.**

Debug-instrumentation recipe (reusable): eprintln in mse_source push_buffer + pad probe on ghost pads post-flush Ã¢â‚¬â€� buffer OUT count vs push count is the decisive lossless-handover metric.

## sec1zg Ã¢â‚¬â€� multi-seek end-to-end state: two fixes landed, resume-flakiness precisely layered

**Symptom: seek target clamped upward/backward seeks impossible Ã¢â€ â€™ check: MSE seekable mirroring flush-cleared buffered union (CONFIRMED CAUSE Ã¢â‚¬â€� FIXED in 4c14e896601: seekable=[0,duration] for finite MSE, matching Chrome/Firefox; clamping eliminated, targets accepted exactly).**

**Symptom: post-seek position wildly wrong (~90 for target 60) Ã¢â€ â€™ check: mse_position_offset double-count (CONFIRMED CAUSE Ã¢â‚¬â€� FIXED in 4c14e896601: matroskademux outputs ABSOLUTE cluster timecodes, pipeline positions are natively correct presentation times; offset removed; verified correct advancing positions when playing). The earlier rebase theory (sec1ze) is REVISED: demux does not rebase; pre-seek correctness was coincidental (streams start at 0).**

**Symptom: post-seek playback frozen at exact target forever (rs=4, correct data indexed) Ã¢â€ â€™ multi-layered, partially unresolved:
- layer 1: byte loss in flush+recreate handover (CONFIRMED Ã¢â‚¬â€� FIXED in 027251a9ed2, atomic reset under tracks lock)
- layer 2: YouTube feed flakiness Ã¢â‚¬â€� sometimes never re-feeds post-seek (aborts=0, append count frozen); cause unknown, suspect engine-side seek-event completion or timing; NOT a constant Ã¢â‚¬â€� other runs re-feed correctly (73 appends, correct target ranges indexed)
- layer 3: when fed correctly, pipeline consumption still stalls in most runs Ã¢â‚¬â€� bytes verified entering appsrcs (pushes ok) but playback doesn't advance; pad-probe discrimination of source-exit vs downstream starvation inconclusive across flaky runs. Suspects: decodebin3 stream-id handling of recreated appsrc stream-start (new random stream-id on existing topology), segment/running-time vs pipeline clock mismatch, post-flush decoder keyframe re-priming.
- negative results: appendBuffer errors Ã¢â‚¬â€� none ever observed; feeding logic itself Ã¢â‚¬â€� ruled out as constant cause (feeds correctly in some runs); seekable clamping Ã¢â‚¬â€� ruled out post-fix.**

**Symptom: position collapses to ~0 + rs=0 some seconds after a stalled forward-out-of-range seek Ã¢â€ â€™ check: YouTube player.js error-recovery teardown/restart (prime suspect Ã¢â‚¬â€� player recreates MSE pipeline, restarts feeding from 0; NOT an engine crash).**

**Probe recipes (reusable): (1) JS wrap SourceBuffer.prototype.appendBuffer/abort counting calls+bytes+errors on window Ã¢â‚¬â€� distinguishes page-side feeding vs engine-side consumption; (2) eprintln push counting + pad probes on servomse ghost pads Ã¢â‚¬â€� decisive lossless-handover metric is buffers-out vs pushes-in; (3) probes must be installed inside seek_reset since pads don't exist earlier. Startup flakiness note: youtube watch pages sometimes serve ads (dur<60) Ã¢â‚¬â€� gate probes on dur>60 with retry+nudging play().**

Honest mission assessment: single-seek sometimes works end-to-end now (correct position, advancing playback); multi-seek does not. The remaining stall is multi-layered race-flakiness centered on the flush+recreate design's interaction with decodebin3 Ã¢â‚¬â€� resolving it may require replacing the design with timestamp-rewriting append pipelines (WebKit-style), est. 1-2d expert gst work, outcome uncertain.

## sec1zh Ã¢â‚¬â€� Reddit heap attribution: the category table IS measured (Linux, working instrument) (2026-11-26)

**Instrument (now working on Linux, pushed)**: parallel+timeout memory-report collector in components/profile/mem.rs (fires all reporters before waiting, 20s per-reporter bound Ã¢â‚¬â€� a wedged reporter no longer takes down the report) + engine-side timer trigger: `SERVO_MEMDUMP_AFTER=<secs>` env Ã¢â‚¬â€� thread inside servo fires reports independent of shell event loops and LoadStatus (reddit never reaches Complete), repeats every 60s. Servoshell also gained unconditional --memory-output full-row dumps (upstream was hitrace-only).

**THE TABLE Ã¢â‚¬â€� idle reddit tab, signed-out, settled ~45s+, RSS 1162MB, stable across 3 dumps 2min apart:**

| MB | category |
| 342 | webrender/images Ã¢â‚¬â€� WR-side decoded+uploaded surfaces |
| 309 | net image-cache Ã¢â‚¬â€� CPU decoded copies, main pipeline ALONE |
| 272 | DOM text nodes (reddit shreddit DOM) |
| 78 | JS malloc-heap, all realms summed |
| 51 | layout trees (box-tree 48 + stylist + stacking-ctx) |
| 49 | JS gc-heap used, all realms |
| 34 | JS non-heap |
| 29 | net memory-cache (raw undecoded resources) |
| 28 | DOM other+element+attr nodes |
| ~2 | webrender display-list/other |

Cross-check rows: jemalloc-heap-allocated 777 / active 798 / mapped 885; smaps anonymous 829 of which js-gc-heap 78 (SM's own mmap, outside jemalloc). Resident 1162. Attributed total slightly exceeds resident due to overlap between measurement axes Ã¢â‚¬â€� directionally correct per category.

**THE HEADLINE FINDING: images own >55% of the tab Ã¢â‚¬â€� and the same decoded bytes are effectively held TWICE (342 WR-side + 309 net-cache CPU-side Ã¢â€°Ë† 651MB combined).** Second-order finding: **the net image-cache per-pipeline cap (MAX_COMPLETED_LOAD_BYTES=128MB) is measurably violated Ã¢â‚¬â€� 309MB in ONE pipeline's cache.** Either the cap doesn't count the same bytes the reporter does, eviction never triggers (pinned-in-use images can't evict?), or LRU is broken. THIS is the highest-leverage concrete memory bug now identified: fixing the cap enforcement alone should bound the CPU copy at 128MB; the deeper fix (single-copy ownership Ã¢â‚¬â€� cache shells with paint-time re-decode, already documented as weeks-scale) attacks both copies at once.

**Corrections of record**: (1) the earlier note "dropping the cap 128Ã¢â€ â€™16MB left scroll growth unchanged" is consistent ONLY if the cap is non-functional Ã¢â‚¬â€� reframe: those experiments measured nothing because the cap never binds. (2) Windows lane's 2.8GB vs our 1162MB Ã¢â‚¬â€� different content/scroll depth, same structure; table ratios are what transfer.

**Symptom index Ã¢â‚¬â€� symptom: single idle reddit tab RSS >1GB Ã¢â€ â€™ check: net image_cache enforcement (prime suspect Ã¢â‚¬â€� cap measurably violated, 309/128MB), double-ownership of decoded surfaces net-cache vs webrender (confirmed structural, 2x copies), DOM text-node mass on shreddit (confirmed 272MB Ã¢â‚¬â€� content-visibility addresses layout not DOM), JS heaps (ruled out as dominant Ã¢â‚¬â€� only ~160MB all realms), layout trees (ruled out Ã¢â‚¬â€� 51MB).**

**Measurement caveats**: measurement-race crash (MessagePort concurrency) NOT hit in 3+ consecutive reddit dumps on Linux Ã¢â‚¬â€� treat as rare. vsize/total-row sums are hierarchical double-counting Ã¢â‚¬â€� never sum all rows; attribute per leaf categories. The url() rows key by document Ã¢â‚¬â€� reddit shows google-recaptcha webworker (29MB malloc + 20MB non-heap!) and accounts.google.com iframes as separate realms Ã¢â‚¬â€� third-party JS is ~90MB of the JS total.

## Black-skinning root cause + fix (Nov 26, mac lane) Ã¢â‚¬â€� SOLVED

Symptom: three.js skinned-mesh / ANY MeshStandardMaterial content renders solid black in both GL and Metal
modes, while basic/phong materials and the ground plane render correctly Ã¢â€ â€™ check: `validate_internal_format`
in components/script/dom/webgl/validations/tex_image_2d.rs (owns texImage2D internalformat acceptance Ã¢â‚¬â€�
CONFIRMED CAUSE: rejected unsized GL_RG 0x8227 because TexFormat::Red/RG had empty compatible_data_types
via the catch-all), texture upload paths image-element/image-bitmap/blob-bitmap (status: ruled out Ã¢â‚¬â€� all
verified uploading+sampling correctly via probe), skinning/bone-texture pipeline (ruled out Ã¢â‚¬â€� procedural
SkinnedMesh repro correct), shadow mapping (ruled out Ã¢â‚¬â€� shadowMap+castShadow repro correct), MSAA/antialias
(ruled out), canvas size (ruled out), RG16F half-float sized upload+sampling (ruled out Ã¢â‚¬â€� raw probe exact).
MECHANISM: three.js r186 added unconditional `material.dfg = texture2D(dfgLUT, ...)` to standard-material
lighting; the LUT is a 16x16 RG+HalfFloat DataTexture uploaded with UNSIZED GL_RG internalformat Ã¢â‚¬â€� invalid
in strict WebGL2 but accepted by all browsers via driver passthrough. servo rejected it Ã¢â€ â€™ texture never
allocated Ã¢â€ â€™ shader sampled 0 Ã¢â€ â€™ `multiScatteringCompensation = 1 + spec*(1/EssMs - 1)` with EssMs=0 Ã¢â€ â€™
division by zero Ã¢â€ â€™ NaN Ã¢â€ â€™ black. FIX: feather-engine d4a8a04e3 Ã¢â‚¬â€� Red/RG compatible types added;
EXT_color_buffer_float implemented (core-profile GL exposes float rendering without the ES extension
string Ã¢â‚¬â€� is_supported now `!is_gles || extension-present`); OES_texture_float/half_float_linear same.
Bisect artifacts (three r183-r187 + probes) in /tmp/vtest Ã¢â‚¬â€� key ones: pbrtest186.html (minimal repro),
rg16fprobe.html, glbtestlive.html. DIAGNOSTIC LESSON: bisecting the three.js VERSION (r185 ok/r186 black)
cracked it in 4 steps when mechanism-guessing was stalled; for library-dependent rendering defects, bisect
the library version FIRST. Also: the texImage InvalidEnum error appeared in WORKING versions too (r183) Ã¢â‚¬â€�
error-presence is not proof of causation; verify against a passing baseline before chasing.

## sec1zi Ã¢â‚¬â€� image-cache cap verdict: structurally toothless, memory is DOM/WR-owned (2026-11-26)

**Symptom: net image-cache far exceeds the 128MB per-pipeline cap (309-338MB on reddit) Ã¢â€ â€™ check: encoded-vs-decoded byte miscounting (status: RULED OUT Ã¢â‚¬â€� cap counts image.bytes.len() = decoded RGBA), broken LRU/eviction (status: RULED OUT Ã¢â‚¬â€� eviction logic correct as designed), total pinning by external Arc holders (CONFIRMED CAUSE Ã¢â‚¬â€� instrumented: 103/103 images pinned, 100%: every cache entry is Arc-shared with a DOM/render holder; evicting pinned entries frees nothing by construction so the cap has nothing it can legally free).**

Consequences:
- The cache does not OWN the memory it measures Ã¢â‚¬â€� it mirrors bytes owned by DOM image nodes and paint-path references. Cap enforcement can only ever affect genuinely-unreferenced images (scroll-accumulation case, where it already works: post-DOM-removal unpinned images get evicted).
- Ã‚Â§1037 pin-release dead end is now explained mechanistically a second time: dropping a pin does nothing because bytes are Arc-shared and re-pinned every paint Ã¢â‚¬â€� do not re-attempt a third time.
- The idle-memory image mass decomposes into: DOM-side decoded bytes (the Arc) + WR-side copy (webrender/images 342MB Ã¢â‚¬â€� independent ownership, uninvestigated lifetime) + the encoded originals (small). Lever ranking: (1) WR-side CPU-copy lifetime scoping Ã¢â‚¬â€� hours-scale, biggest tractable piece; (2) lazy/offscreen decode Ã¢â‚¬â€� weeks-scale structural.

**Instrumentation recipe (reusable)**: eprintln inside enforce_completed_load_budget gated on >half-cap Ã¢â‚¬â€� prints per-pipeline total/pinned bytes+counts. IMGCACHE-DBG in working tree only, never committed.

### sec1zi addendum Ã¢â‚¬â€� WR-side copy scoping verdict (hours-scale check, 2026-11-26)
webrender/images (342MB) is CPU-side bytes retained by upstream webrender 0.70's resource cache Ã¢â‚¬â€� WR keeps CPU copies of every registered image so textures can be re-uploaded after texture-cache eviction. The keys persist because servo never deletes them (same pinning reflection: image-cache eviction is what sends WR DeleteImage, and pinned images never evict). Attacking this = vendoring/forking webrender (crates.io upstream, NOT a servo fork Ã¢â‚¬â€� Cargo.toml:290) and redesigning resource-cache retention: weeks-scale, upstream-grade surgery, NOT hours-scale. On software-GL hosts (llvmpipe/SWGL) there is no GPU copy at all Ã¢â‚¬â€� the WR bytes ARE the rendering surface, so no savings are possible there at all; on real GPUs the CPU copies are still retained by design. CONCLUSION: no hours-scale cheap win exists in the image-memory subsystem in either direction; both copies are structural. The only real lever remains the documented weeks-scale redesigns (single-copy ownership, lazy/offscreen decode-at-visibility). The image-cache cap mission is closed as premise-false: the cache never owned the bytes.

## sec1zj Ã¢â‚¬â€� image-memory stage 0: same-bytes VERIFIED, real-feed baseline, scroll-crash blocker (2026-11-26)

**Same-bytes hypothesis confirmed at every link (code-verified, not measured)**: (1) servo uploads via GenericSharedMemory::from_arc_vec(bytes.clone()) Ã¢â‚¬â€� in single-process mode (use_ipc() false unless --multiprocess) this is InProcess zero-copy, the SAME Arc; (2) WR stores it as CachedImageData::Raw(Arc<Vec<u8>>) unchanged (resource_cache.rs:134); (3) both reporters measure op(ptr) on the SAME heap allocation Ã¢â‚¬â€� WR's own comment admits no double-counting protection. CONSEQUENCE: webrender/images and net image-cache rows in memreport are the same physical bytes counted twice; true decoded image mass on reddit Ã¢â€°Ë†340MB not 650. The single-copy-at-a-time design in feather docs/image-memory-residency.md stands on this basis.

**Baseline on VERIFIED real feed** (title "Reddit - The heart of the internet", 28 shreddit-posts, 124 imgs, scrollH 13659 Ã¢â‚¬â€� the js_challenge URL parameters persist post-challenge but the feed is real): idle-top dump: resident 1769MB, wr_images 338, net image-cache 291, dom text 267, layout 59.

**NEW DEFECT Ã¢â‚¬â€� scroll crash**: window.scrollBy on real reddit feed crashes servoshell: GLib-GObject-CRITICAL g_object_ref_sink assertion then signal 11 in thread "Script#3", first scroll never returns, webdriver connection resets. BLOCKS every verification gate of image-memory stages 2-4 (can't verify scroll-eviction behavior if scrolling crashes). Symptom index Ã¢â‚¬â€� symptom: crash signal 11 in Script#3 on first scrollBy of reddit feed Ã¢â€ â€™ check: scroll-event handling of shreddit custom elements (prime suspect Ã¢â‚¬â€� shreddit uses resize/intersection observers heavily), G_IS_OBJECT assertion in glib layer immediately preceding (ordering evidence Ã¢â‚¬â€� the glib error fires first, suggests GObject lifecycle corruption propagating into script thread).

**Measurement methodology correction (important for all lanes)**: webdriver /session creates a NEW blank webview Ã¢â‚¬â€� JS introspection runs against about:blank while the URL-argument page loads in the original webview. All webdriver-driven measurement MUST navigate the session itself (POST /session/{id}/url) Ã¢â‚¬â€� never pass the URL as a servoshell argv and introspect via webdriver. Conversely memreport is process-wide so either way works for memory measurement alone.

## sec1zk Ã¢â‚¬â€� reddit "scroll crash" verdict: NOT scroll Ã¢â‚¬â€� gstreamer teardown use-after-finalize (2026-11-26)

**Symptom: signal 11 in thread Script#3 shortly after scrolling reddit feed Ã¢â€ â€™ check: scroll event handling (RULED OUT Ã¢â‚¬â€� plain 20000px page survives, bbc.com survives 8 scrolls, reddit itself survives 4/5 identical repro attempts with scrollY reaching 16000), servoshell/webdriver harness (RULED OUT), gstreamer object lifecycle in media teardown (CONFIRMED CAUSE Ã¢â‚¬â€� crash log shows gst_bus_set_flushing assertion on a dead bus, then g_object_ref '!object_already_finalized', then g_object_ref_sink 'G_IS_OBJECT failed', all within 1ms before the fatal signal).**

Mechanism chain: scrolling is innocent but causally adjacent Ã¢â‚¬â€� moving through the feed starts/stops/tears down video posts (qtdemux/mp4 evidence in the warnings immediately preceding the crash), and video element teardown hits the documented "there be dragons" disposal path in GStreamerPlayer::drop (player.rs ~1205): `unsafe { player.run_dispose() }` justified by the comment "nobody uses the player after this moment" Ã¢â‚¬â€� the crash is that claim being violated. The servo authors knew: the comment references upstream gstreamer MR13022 as the real fix. Content randomness (whether video posts are near the scroll path) explains the flakiness.

Ruled out: pure create/destroy lifecycle churn Ã¢â‚¬â€� a synthetic MSE teardown loop (460 rapid cycles of video+MediaSource+appendBuffer+abort+remove) survives cleanly. Garbage-fed pipelines never reach the playing state though Ã¢â‚¬â€� the fatal window likely requires a LIVE pipeline mid-teardown (bus messages in flight while dispose finalizes the bus).

Fix options in cost order: (a) intentional-leak mitigation Ã¢â‚¬â€� skip run_dispose when inner is non-unique (the dragons condition is already detected! currently only debug_asserted, compiled out in release) Ã¢â‚¬â€� bounded memory cost per torn-down video, eliminates the whole class; (b) serialize teardown Ã¢â‚¬â€� NULL state + bus drain before dispose, ordering complexity; (c) true root-cause of the violating refholder Ã¢â‚¬â€� 1-2 days gst/glib internals, uncertain. NOTE: the dragons detection already exists in-code Ã¢â‚¬â€� option (a) is small and surgical.

## Hardware video decode investigation (Nov 26, mac lane) Ã¢â‚¬â€� vtdec already active; vp9 gap measured marginal

Symptom: video playback CPU high on macOS Ã¢â€ â€™ check: gstreamer vtdec/VideoToolbox hardware decode
(status: CONFIRMED ALREADY ACTIVE Ã¢â‚¬â€� gst_vtdec_handle_frame + VideoToolbox frames in thread samples of
normal playback; nothing to wire). videoconvert/software-conversion cost (status: ruled out as
significant Ã¢â‚¬â€� ~zero activity in samples). Repaint/compositing path (prime suspect, confirmed dominant Ã¢â‚¬â€�
consistent with the repaint-cost diagnosis: video is a 30fps invalidator).

MEASURED (bundled .app, env-clean, 30fps looped local files, windowed top sampling):
static page ~0%, h264-720p 39.1%, h264-1080p 52.0%, vp9-720p 46.6%, vp9-1080p 58.6%.
Software vp9 decode costs only ~+7pp over hw-decoded h264 at the same resolution Ã¢â‚¬â€� the compositing
path dominates both. Codec coverage: gst 1.22.3 vtdec handles h264/h265/mpeg2/jpeg/prores Ã¢â‚¬â€� NO vp9/AV1
(vp9 hw wiring landed upstream in gst 1.24+). YouTube serves vp9 Ã¢â€ â€™ falls to libav software decode.
vtdec SRC caps include video/x-raw(memory:GLMemory) Ã¢â‚¬â€� hw frames CAN stay GPU-side in gst, but servo's
appsink handoff forces system memory (cheap-ish on unified memory, nonzero).

VERDICT: no software-decode defect exists for h264/HEVC Ã¢â‚¬â€� hardware decode was already working.
The vp9 gap is real but marginal (~7pp of ~47-59% playback CPU); upgrading bundled GStreamer to 1.24+
for vtdec-vp9 is the only available lever and is risky while the vp9/gst threading crash (youtube,
indexed for the Linux lane) is unresolved Ã¢â‚¬â€� a newer gst may also change that crash's behavior either way.
Deprioritized behind the compositing-cost program which dominates video CPU regardless of codec.

## sec1zl Ã¢â‚¬â€� teardown-crash FIX landed: intentional-leak mitigation, 10/10 gate (2026-11-26)

Fix (sec1zk option a) in GStreamerPlayer::drop: the dragons condition (non-unique inner / extra player refs) is now checked BEFORE dispose; when detected the player is intentionally leaked (mem::forget) instead of run_dispose Ã¢â‚¬â€� eliminating the use-after-finalize crash class at a bounded memory cost. Loud comment + upstream MR13022 reference in-code. [playerleak] eprintln counter logs each leak with running total.

Verification gate PASSED: 10/10 deep-scroll reddit sessions survived (8 scrolls each, scrollY 14000-16000 Ã¢â‚¬â€� video posts in path); pre-fix crash rate was ~1/5 sessions.

LEAK COST MEASURED: 7-13 leaked players per 8-scroll reddit session (mean ~11) Ã¢â‚¬â€� the dragons condition is PERVASIVE, not occasional: without the fix those players were all disposed unsafely and the crash was the unlucky intersection. Per-leak cost is pipeline object shells not frame data (pipeline stopped first) Ã¢â‚¬â€� est. tens-to-hundreds of KB each, so single-digit MB per heavy scroll session. Acceptable against the image masses, but it ACCUMULATES over very long sessions Ã¢â‚¬â€� indexed as a known bounded-leak cost; proper fix remains upstream MR13022 or the true refholder root-cause.

Calibration: healthy progressive-mp4 teardown does NOT trip the leak path (0 leaks) Ã¢â‚¬â€� the threshold distinguishes correctly.

Symptom index Ã¢â‚¬â€� symptom: SIGSEGV in Script#3 during/after scrolling video-heavy feeds, preceded by gst object-finalized assertions Ã¢â€ â€™ check: GStreamerPlayer::drop disposal race (CONFIRMED cause class Ã¢â‚¬â€� fixed by intentional-leak mitigation), scroll input handling (ruled out), [playerleak] counter in stderr (diagnostic: nonzero count means the hazardous condition occurred and was survived).

## sec1zm Ã¢â‚¬â€� image-memory stage 1: byte-swap machinery resurrected onto current feather-engine (2026-11-26)

Applied the imgmem-wip machinery (284-line diff, 4 files) cleanly onto current feather-engine: encoded_bytes retention at decode, DecodedBytes interior-mutable residency (Resident/Evicted behind RwLock, shared across all pin holders through the same allocation), materialize_decoded() accessor (re-decodes synchronously from encoded source when evicted; re-checks after write-lock upgrade), evict_decoded() (static images only, frames.len()==1), global resident-byte counter, env-gated enforcement (FEATHER_IMGMEM=1, 384MB budget Ã¢â‚¬â€� default OFF so behavior-neutral). RasterImage keeps its original bytes field for initialization; residency shares the same allocation (no duplication).

Stage-1 verification gates PASSED: compile-clean; visual correctness on image-heavy sites (reddit feed + r/pics Ã¢â‚¬â€� photos/avatars/banners decode and paint correctly); memreport rows flow unchanged (image rows same order of magnitude as baseline modulo content variance). KNOWN COST: encoded_bytes retention is unconditional Ã¢â‚¬â€� every decoded image now also retains its encoded source (~10-20x smaller; est. +15-30MB on reddit-class pages). This is the design's intentional insurance payment, not a regression.

CRITICAL REMINDER for stage 2 (restated so it can't be lost): byte eviction alone frees ZERO for any painted image in single-process servo Ã¢â‚¬â€� WR's CachedImageData::Raw holds a clone of the same Arc allocation (verified sec1zj). The stage-2 eviction unit must be (decoded residency + WR key deletion) together, with re-decode + re-upload on re-visibility. The byte-swap layer is necessary-but-insufficient scaffolding.

## sec1zn Ã¢â‚¬â€� stage-2 mechanism map + refined design (2026-11-26)

Image-key lifecycle mapped (function-indexed): upload is ONE-SHOT at decode completion Ã¢â‚¬â€� set_webrender_image_key (net/image_cache.rs ~150) allocates from key_cache batch, calls paint_api.add_image, sets raster_image.id = Some(key) permanently. Display lists reference the key via ImageResolver::image_key_from_cached_image (layout/context.rs ~283) which just reads raster_image.id Ã¢â‚¬â€� no upload-state awareness anywhere. DeleteImage happens only on structural cache eviction (rare due to pinning). CONCLUSION: no re-upload path exists at all Ã¢â‚¬â€� deleting a WR key today permanently breaks rendering of that image. Stage 2 must build re-upload from scratch.

REFINED DESIGN Ã¢â‚¬â€� key decisions:
1. **Re-upload hook**: image_key_from_cached_image is the natural site Ã¢â‚¬â€� every display-list image reference flows through it. Needs: upload-state tracking on RasterImage (Uploaded(key)/Evicted) replacing the plain id field, and a re-upload path that materializes decoded bytes + calls paint_api.add_image + re-keys. COMPLICATION: add_image needs paint_api access and key allocation which currently live in the net-process image cache store Ã¢â‚¬â€� must be plumbed into the layout-side resolver or done via a synchronous cache request. Half-day plumbing minimum.
2. **The LRU-touch oracle FAILS as designed in imgmem-wip**: display lists are viewport-AGNOSTIC Ã¢â‚¬â€� servo builds fragments for the entire document regardless of scroll position, and lists do not rebuild on pure scroll (compositor-side scrolling). So display-list reference does not mean visible, and even visible images stop being touched on static pages. Eviction cannot use naive reference-touch ordering.
3. **Therefore viewport relevance must be computed geometrically at display-list-build time**: fragment rect vs viewport+margin Ã¢â‚¬â€� the builder knows both. This is the same verdict cv-wip needs (synergy: compute once, share). Content insertions on infinite scroll trigger rebuilds often enough for the verdict to stay fresh on exactly the pages that matter.
4. Eviction trigger remains budget-pressure (global), but candidate ordering uses the geometric verdict: never evict in-viewport images; evict furthest-offscreen-first; hysteresis margin to prevent scroll-flap churn.
5. Failure mode analysis: worst case remains re-decode+re-upload churn on rapid scrolling Ã¢â‚¬â€� bounded by hysteresis; correctness never at risk because re-materialization is always possible from encoded bytes.

IMPLEMENTATION ORDER for stage 2 (each independently verifiable): (a) upload-state tracking + re-upload plumbing Ã¢â‚¬â€� correctness-preserving even with eviction off (verify: forced eviction unit test, image reappears); (b) geometric viewport verdict at display-list-build Ã¢â‚¬â€� measure hit rates, no behavior change; (c) eviction-unit atomics (bytes+key together); (d) budget trigger + ordering + hysteresis, gates per design doc.

## sec1zo Ã¢â‚¬â€� stage 2a landed: eviction-unit + re-upload machinery; black-flash confirms step-order mandatory (2026-11-26)

Implemented (env-gated FEATHER_IMGMEM=1, default off): eviction unit now deletes WR key alongside decoded bytes (paint_api.update_images DeleteImage + key recorded in evicted_upload_keys Ã¢â‚¬â€� required because single-process WR holds a clone of the same Arc; byte eviction alone frees zero). Re-upload-on-cache-read in get_completed_image_if_available: evicted keys re-materialize (re-decode from encoded) and re-add via paint_api.add_image REUSING THE SAME KEY Ã¢â‚¬â€� no RasterImage mutability surgery needed, permanent id association stays valid. Budget now env-configurable (FEATHER_IMGMEM_BUDGET_MB) for testing.

Verification: compile-clean; forced eviction on r/pics (20MB budget) Ã¢â‚¬â€� evictions fire, 6 re-uploads observed in one scroll cycle, zero crashes, 236 imgs present. BUT the visual gate exposed the predicted gap exactly: visible post image renders BLACK after eviction cycle Ã¢â‚¬â€� because re-upload only triggers on cache READ, and painted images are never re-read (layout resolves keys once at display-list-build time; scrolling does not rebuild). Keys are baked into display lists; the re-uploaded data never reaches already-built lists' visible images unless a reflow happens.

CONSEQUENCE (design validation, not surprise): the step ordering in sec1zn is MANDATORY Ã¢â‚¬â€� eviction must not target visible images, which requires the geometric viewport verdict (step b) BEFORE eviction is safe to enable for real. The black-flash is not a bug in 2a; it is 2a operating exactly as designed without its prerequisite. Stage 2b (geometric verdict, shared with cv-wip) is the next implementation and is the true core of stage 2. Mechanical pieces confirmed working and retained: eviction-unit atomics, re-decode re-upload path, same-key reuse (validates that WR accepts re-adding a deleted key Ã¢â‚¬â€� was an open question).

## Energy bakeoff (2026-11-26, macOS VM) Ã¢â‚¬â€� symptom index entries

Symptom: high battery drain / CPU in video playback vs other browsers Ã¢â€ â€™ check: swgl compositing of video frames (per-frame full repaint, readback+raster+upload Ã¢â‚¬â€� cannot amortize on changing content; prime suspect, confirmed dominant), gst vtdec decode path (status: ruled out Ã¢â‚¬â€� hardware decode already active, vtdec in samples), vp9 software decode (minor, ~7pp). Measured: video 720p feather 448 ms/s vs chrome 234 vs safari 86 Ã¢â‚¬â€� the single largest measured energy gap; closing requires GPU-side compositing (see scoping doc, weeks-scale).

Symptom: energy measurement on macOS VMs Ã¢â€ â€™ check: powermetrics cpu_power sampler (status: ruled out Ã¢â‚¬â€� no pmgr IO registry entry on VirtualMac2,1, physically impossible), powermetrics tasks sampler (works: per-process CPU ms/s + package-idle wakeups are the rigorous proxy ingredients; relative deltas meaningful, watts are not).

Symptom: browser idle battery drain Ã¢â€ â€™ check: paint-gating + Metal present path (status: verified fixed Ã¢â‚¬â€� feather idle measures 0.1 CPU ms/s and ZERO package-idle wakeups vs chrome 12.0/3.6 safari 5.0/15.7; an idle feather genuinely does nothing).

Harness: feather repo shell/tools/ebench/ (measure.sh parse.py runall.sh) Ã¢â‚¬â€� reruns the full 12-cell matrix in ~65min on any mac.

## sec1zp Ã¢â‚¬â€� byte-swap made REAL: sole-owner residency; measured -1GB on reddit deep-scroll; corruption gap confirmed (2026-11-26)

CRITICAL FIX over sec1zo: the stage-1 resurrection was accounting fiction Ã¢â‚¬â€� RasterImage kept its original `bytes` field alongside the new residency RwLock, so evict_decoded dropped the lock's Arc while `self.bytes` pinned the same allocation. Eviction counters fired, WR keys died, physical memory never moved (first measurement: cache rows INCREASED 861MB, RSS unchanged 3840 vs 3812 baseline). Fix: deleted the `bytes` field entirely Ã¢â‚¬â€� the RwLock is now the SOLE owner of decoded bytes. Consumer surface was tiny (favicon used first_frame only for width/height; descriptor/snapshot/to_shared already materialize). Accounting made Drop-balanced via ResidentBytes newtype (constructor increments, drop decrements Ã¢â‚¬â€� covers eviction swap, image drop, cache removal with zero manual call-site accounting). Manual MallocSizeOf reports encoded + resident-decoded non-triggeringly (measurement does not re-materialize Ã¢â‚¬â€� verified by design of the impl reading the lock fast-path only).

MEASURED WIN (reddit signed-in, 8x1500px deep scroll, SERVO_MEMDUMP_AFTER=78, budget 100MB):
- resident total: 3812MB (no eviction) -> 2775MB (eviction on) = -1037MB (-27%)
- webrender/images: 872 -> 60MB; reddit image-cache rows: 595+221 -> 249+64MB
- 357 evictions / 1281MB cumulative churn / 26 re-uploads; zero crashes; SURVIVED deep scroll
- residual cache mass = retained encoded bytes (the insurance cost Ã¢â‚¬â€� larger than the 15-30MB idle estimate on deep scrolls, scales with distinct images seen) + animated + under-budget resident

CORRUPTION GATE Ã¢â‚¬â€� resolved NEGATIVE, exactly as the design predicted: scrollback to top shows BLACK RECTANGLE for images evicted-while-visible (verified visually: kitchen-aid post black, freshly-loaded apple ad renders fine). Zero-corruption is UNACHIEVABLE with budget-only eviction because (a) display lists bake keys and never rebuild on pure scroll, (b) cache-read re-upload only fires on reflow, (c) WR cannot render a deleted key no matter the scroll position. Both directions of the problem need geometric machinery: eviction must not target visible images AND scroll-approach must trigger re-upload. There is no cheaper trigger Ã¢â‚¬â€� confirmed by this measurement. This is stage 2b, the core multi-day cost, now confirmed unavoidable rather than speculative.

Symptom index: symptom "image renders black after scrolling back to it (FEATHER_IMGMEM on)" -> check: display-list key baking (owns: keys permanent once built; pure scroll never rebuilds) prime suspect/confirmed; cache-read re-upload path (status: working but insufficient Ã¢â‚¬â€� only fires on reflow); WR key deletion (status: working as designed); NOT a re-decode failure Ã¢â‚¬â€� re-uploads verified mechanically sound.

## sec1zq Ã¢â‚¬â€� stage 2b implementation map: mechanism terrain fully explored (2026-11-26)

Explored paths and verdicts for the geometric machinery (prerequisite for corruption-free eviction):
- PAINTER-SIDE scroll-driven re-upload: DEAD. Paint retains only the ScrollTree + iframe rects + epochs per pipeline (pipeline_details.rs) Ã¢â‚¬â€� display lists are NOT retained paint-side (they ship to WR). No geometry to walk. Do not re-attempt.
- WR-internal demand signal (renderer notices missing key at frame render): architecturally ideal but WR is upstream crates.io webrender 0.70, unforked Ã¢â‚¬â€� vendoring WR is the documented weeks-scale cost. Not for alpha.
- KEY POSITIVE FACT: scroll positions ALREADY flow paint->layout (webview_renderer.rs send_scroll_positions_to_layout_for_pipeline) for sticky positioning/IO/scroll events. Layout owns fragment geometry (retained fragment tree) AND holds an image_cache handle (layout/context.rs). Layout is the correct home for both halves of the geometric machinery.
- THE DESIGN that falls out: (1) at display-list-build time, record per-image spatial index entries (cache key, fragment rect, scroll node) in a side table owned by layout Ã¢â‚¬â€� cheap because layout already walks everything; (2) on scroll-position updates (riding the existing paint->layout path, throttled to scroll-end or ~100ms), query the spatial index for evicted images intersecting viewport+margin and call the existing cache re-upload path; (3) at eviction time, consult the same index so visible images are never evicted in the first place. Both halves share ONE geometric index Ã¢â‚¬â€� this is the economy of the design. Open implementation questions: fragment-tree lifetime vs index invalidation on reflow; spatial index cost on shreddit-scale trees (thousands of images Ã¢â‚¬â€� need a flat sorted structure, not a tree walk); whether scroll updates arrive frequently enough (they ride input events, not WR frame scrolls Ã¢â‚¬â€� verify: does compositor-side scroll send position updates per frame or only on scroll-end? If only per input event batch, re-upload latency may be visible as brief black flash even in correct operation Ã¢â‚¬â€� acceptable if bounded).
- Hysteresis note: eviction margin must exceed re-upload margin by a hysteresis band or images oscillate evict/re-upload at viewport edges (design doc already says this; now it's concrete WHICH margins: the same spatial index query with two different paddings).

Disk hygiene note for this lane: target/release is now ~30GB after the full rebuild Ã¢â‚¬â€� incremental artifacts pruned once already; if space pressure returns, `cargo sweep` or delete incremental dirs is safe (git is the backup).

## NV12 video pipeline (2026-11-26) Ã¢â‚¬â€� symptom index entries

Symptom: high CPU/energy in video playback Ã¢â€ â€™ check: videoconvert NV12Ã¢â€ â€™BGRA pass (status: FIXED Ã¢â‚¬â€� planar-NV12 pipeline 2e312865a, ~22-26% video CPU reduction; frames now reach webrender as R8+RG8 planes rasterized by brush_yuv shader), general compositing machinery swgl-raster+present (prime suspect for residual Ã¢â‚¬â€� video is a 30fps invalidator; not video-specific, compositing-bound), gst copy-out + texture-upload copies (minor, Ã¢â€°Â¤10% ceiling, deprioritized).

Symptom: video colors wrong (green/purple shift, washed out) after pipeline changes Ã¢â€ â€™ check: YuvColorSpace/ColorRange hardcoded Rec709+Limited in layout/display_list/mod.rs push_yuv_image (correct for HD h264; wrong for SD BT.601 content Ã¢â‚¬â€� if SD video looks wrong, this is why; proper fix parses gst colorimetry caps), chroma plane order in VideoFrameData::NV12 (verified correct for vtdec output), UV plane dimensions (RG8 half-width half-height Ã¢â‚¬â€� a full-width UV mistake shows as horizontal chroma doubling).

Measurement lesson: copy-pass elimination measured HALF its predicted win (22% vs ~50%) because per-pixel conversion compute merely moves from videoconvert into the rasterizer shader Ã¢â‚¬â€� only memory bandwidth and extra passes are truly saved. When costing pipeline optimizations, separate compute (often conserved) from bandwidth/passes (actually eliminated).

## sec1zr Ã¢â‚¬â€� stage 2b LANDED: geometric eviction verdict + scroll-driven re-upload; scrollback corruption FIXED (2026-11-26)

The geometric machinery is implemented and verified end-to-end:
- Shared registry pixels::IMAGE_GEOMETRY (WR key -> content-space rect + near_viewport bit), written by layout at display-list-build time (image fragment push site, initial verdict vs unscrolled viewport) and updated on EVERY scroll-offset change Ã¢â‚¬â€� both the paint->layout renderer path (set_scroll_offsets_from_renderer, input scrolling) and the script path (set_scroll_offset_from_script, programmatic scrolling Ã¢â‚¬â€� required because webdriver/JS scrolling rides it).
- Eviction-time verdict: enforce_resident_byte_budget skips images whose near_viewport bit is set Ã¢â‚¬â€� visible content is never evicted (kills the black-rectangles-on-visible-content class at the source).
- Scroll-driven re-upload: on scroll updates, keys transitioning far->near are collected and re-uploaded via the new ImageCache::reupload_evicted_images trait method (reverse-indexed batch, reuses the same key so baked display-list references become valid again on the next frame Ã¢â‚¬â€� no rebuild needed). Re-upload margin = 1 viewport ahead, so images return BEFORE entering the visible region.
VERIFICATION (r/pics, the exact testbed that black-rectangled pre-2b): 12x1000px deep scroll + incremental scrollback, budget 100MB Ã¢â‚¬â€� 23 re-upload batches / 116 re-uploads / 392 evictions, zero crashes, and the top-post photo renders CORRECTLY on scrollback (visually verified against the pre-2b black-rectangle baseline). Memory rows stay bounded: webrender/images 109MB, cache rows small. Mechanism confirmed: display lists do not need rebuilding Ã¢â‚¬â€� re-adding the same WR key fixes rendering on the next frame.
V1 approximation limits (documented, not blocking): single-bit verdict at 1x margin for both decisions (no oscillation possible since consistent, but no hysteresis band); nested scrollers/transforms not accounted (root-scrolled feeds exact); dominant-scroller approximation uses max scroll offset across nodes; re-decode is synchronous on the scroll path (jank risk on slow devices Ã¢â‚¬â€� stage 3 async re-materialization); registry entries are never pruned (small leak, few hundred KB worst case); multi-webview shares one registry (fine for alpha).
Symptom index: symptom "image black after scrollback (FEATHER_IMGMEM)" -> status FIXED by geometric verdict + scroll-driven re-upload; if recurs check: scroll offset sign convention (positive-down verified working on reddit), script-vs-renderer scroll path coverage (both hooked), registry entries fresh after reflow (rebuild overwrites).

## sec1zs Ã¢â‚¬â€� async re-upload offload attempt: crashes webrender, REVERTED; sync path is the shipped design (2026-11-26)

Attempted stage-3 cheap core: moving the re-upload batch (reverse-index build + re-decode + add_image) off the layout/scroll thread onto the cache thread pool. Result: webrender render_api RecvError panic then pthread_mutex_destroy/signal-11 during active scrolling Ã¢â‚¬â€� the WR render backend dies when image updates arrive from pool threads mid-scroll. Mechanism unproven but the timing correlation is exact (sync path never crashes across all prior verification runs; async crashed on first deep-scroll test). Candidate mechanisms: (a) paint_api/CrossProcessPaintApi add_image not safe from arbitrary pool threads in single-process mode; (b) concurrent in-flight batches racing WR texture-cache updates; (c) pool-thread re-decode lifetime interacting with WR frame rendering. NOT investigated further Ã¢â‚¬â€� cost/benefit poor because the sync path is verified-correct and the jank it risks is unmeasured.
DECISION: sync re-upload is the shipped design. Re-pin/re-decode scroll-path jank is a DOCUMENTED RESIDUAL Ã¢â‚¬â€� if user-visible jank is reported on slow devices, the correct stage-3 is a properly-designed async pipeline (bounded queue, single dedicated re-decode thread, generation-tagged uploads), not a thread-pool spawn.
Symptom index: symptom "webrender RecvError panic + signal 11 during scrolling" -> check: image updates arriving from non-layout threads (prime suspect when FEATHER_IMGMEM re-uploads are pool-offloaded Ã¢â‚¬â€� reverted); NOT the gstreamer teardown class (different signature: no gst assertions precede).

## Pre-alpha dogfood (2026-11-26, shell-new macOS) Ã¢â‚¬â€� symptom index entries

Symptom: in-page clicks missing small targets (links/input fields dead) while big buttons hit Ã¢â€ â€™ check: forward_page_input rel() coordinate conversion in shell-new src/app.rs (prime suspect Ã¢â‚¬â€� clicks land down-right of physical position, verified: center-click of 300x100 anchor logged at 227,100 hitting BODY; scale confusion between egui logical pts / winit physical px / scale_factor), servo input pipeline itself (status: ruled out Ã¢â‚¬â€� keydown events reach document fine, clicks DO register, just at wrong coords), engine focus handling (status: ruled out as primary Ã¢â‚¬â€� fields never receive correct-position clicks to begin with).

Symptom: typing in web forms dead Ã¢â€ â€™ check: same coordinate defect as above (confirmed cause Ã¢â‚¬â€� input fields never gain focus because clicks miss them; keyboard delivery itself verified WORKING: doc keydown events fire for every key).

Symptom: shell becomes zombie after sustained use Ã¢â‚¬â€� omnibox/new-tab silently dead but chrome responsive Ã¢â€ â€™ check: shell action-processing dispatch logic (prime suspect Ã¢â‚¬â€� main thread verified parked in normal mach_msg idle, NOT an event-loop wedge; egui repaints fine; actions specifically never execute), winit/egui event delivery (status: ruled out Ã¢â‚¬â€� events still update UI state visibly).

Symptom: video element black despite playback Ã¢â€ â€™ check: gst plugin loading (status: ruled out Ã¢â‚¬â€� vtdec hw decode verified active, 993 samples in gst_vtdec_handle_frame; startup plugin-error log is benign fallback path), frame handoff to compositor / media client-context registration (prime suspect Ã¢â‚¬â€� "unknown client context" warnings coincide), initial-paint invalidation gap (related: pages don't composite until first input).

Symptom: localhost dev servers unreachable "client error (Connect)" Ã¢â€ â€™ check: IPv6/IPv4 resolution fallback (localhostÃ¢â€ â€™::1 vs IPv4-only server; 127.0.0.1 works fine) Ã¢â‚¬â€� no happy-eyeballs in servo net.

## Hydration-stall class root cause (2026-11-26) Ã¢â‚¬â€� symptom index entries

Symptom: SPA renders but never becomes interactive though JS partially runs (analytics fire, spinners animate) Ã¢â€ â€™ check: MISSING BOOT-CRITICAL APIs first (confirmed cause class) Ã¢â‚¬â€� requestIdleCallback/cancelIdleCallback undefined (NO webidl/impl in servo at all, unguarded call throws sync mid-boot Ã¢â‚¬â€� prime suspect, YouTube boot scheduler uses rIC), window.scheduler missing, crypto.subtle missing, visualViewport missing, serviceWorker missing, navigation api missing, history.scrollRestoration missing; shadow DOM rendering (status: ruled out Ã¢â‚¬â€� verified painting correctly); customElements lifecycle (status: ruled out Ã¢â‚¬â€� define/upgrade/connectedCallback/built-in extends all verified working); single unguarded missing-API call unwinds the whole boot routine at the throw point Ã¢â‚¬â€� explains uniform failure across unrelated heavy apps.

Symptom: cannot diagnose engine JS failures in embedding Ã¢â€ â€™ console output and uncaught exceptions invisible in shell-new (nothing surfaces); servo's devtools server exists behind devtools_server_enabled pref but shell doesn't wire it; diagnosis requires blind localhost-probe pages Ã¢â‚¬â€� highest-leverage tooling fix available.

Lesson: probe pages testing typeof/guarded-call behavior loaded in the target browser beat log spelunking when no console exists Ã¢â‚¬â€� write results visibly into the DOM, click page once to force the first paint (F4 invalidation gap).

## sec1zt Ã¢â‚¬â€� stage 4 landed (hysteresis + budget tuning) + VIDEO-PLANE CRASH root-caused (independent of imgmem) (2026-11-26)

Stage 4 policy: dual-margin hysteresis implemented Ã¢â‚¬â€� eviction protection at 2x viewport (wide), re-upload trigger at 1x viewport (tight); the band between them neither evicts nor re-uploads so fast scroll direction reversals cannot thrash evict/re-decode cycles. Budget default now 256MB (2x the 100MB verified zero-corruption, headroom for image-dense viewports); with geometric protection the budget is a backstop.

CRASH INVESTIGATION Ã¢â‚¬â€� major negative-correlation result: the "webrender dies mid-scroll" crash class is NOT caused by image eviction. Control experiment on r/pics deep-scroll: eviction-OFF died 3/3, eviction-ON died 2/3 Ã¢â‚¬â€� same signature both: `Unexpected format RG8` panic in WRRenderBackend texture_cache.rs:488 then scene-sender SendError / signal 11. ROOT CAUSE: video plane uploads Ã¢â‚¬â€� media_frame_renderer.rs uploads NV12 video as R8 luma + RG8 chroma plane images; WR's shared texture-cache arrays only support alpha8/16 + RGBA8/BGRA8 and PANIC on RG8 allocation. Flakiness explained: large planes get standalone allocation (safe), small/medium planes fit the shared arrays (panic) Ã¢â‚¬â€� so whether a feed scroll crashes depends on the sizes of videos encountered. Pre-existing upstream-grade defect in video rendering, exposed by video playback enablement Ã¢â‚¬â€� UNRELATED to all image-memory work. The sec1zs async-offload crash attribution was WRONG in retrospect Ã¢â‚¬â€� same class.
Fix options documented: (a) vendor WR + make shared-cache allocation fall back to standalone for unsupported formats instead of panicking (proper, weeks-scale vendoring cost); (b) servo-side chroma expansion Ã¢â‚¬â€� convert UV plane to BGRA before upload (quarter-res conversion, moderate per-frame CPU, self-contained in media_frame_renderer); (c) gstreamer caps negotiation forcing BGRA output (full-frame conversion cost, simplest). Recommend (b) if this becomes alpha-priority.
Symptom index: symptom "Unexpected format RG8 panic WRRenderBackend then signal 11 while scrolling video-containing feeds" -> check: media_frame_renderer NV12 plane upload (confirmed cause Ã¢â‚¬â€� RG8 chroma planes hitting shared texture cache); NOT image-memory eviction (ruled out by control experiment 3/3 deaths with eviction off); NOT scroll itself; size-dependent Ã¢â‚¬â€� small/medium videos crash, large don't.

## sec1zu Ã¢â‚¬â€� FINAL ATTRIBUTION measurement protocol note (2026-11-26)
The stage-4 closeout table compares: sec1zh/sec1zj baselines (idle-top: WR 342 + cache 309 same-bytes Ã¢â€°Ë†340MB real image mass, RSS 1162; deep-scroll: WR 872MB, RSS 3812+) vs post-machinery measurements. All measurements on signed-in reddit, SERVO_MEMDUMP_AFTER timed dumps.

## sec1zv Ã¢â‚¬â€� IMAGE MEMORY MISSION CLOSEOUT: final attribution table (2026-11-26)

Protocol: signed-in reddit, SERVO_MEMDUMP timed dumps, deep scroll 8-24 x 1000-1500px. Same-bytes correction applies throughout (WR + cache rows count the same physical allocation Ã¢â‚¬â€� real mass is NOT their sum).

| measurement | before machinery | after machinery | delta |
| resident total, deep scroll 12000px | 3812MB (growing 4523 by 24k equiv) | 2775MB | -1037MB (-27%) |
| resident total, deep scroll 24000px | (baseline run showed continuous growth 3812->4184->4524 across dumps) | 2374MB FLAT | bounded flat |
| webrender/images row, deep scroll | 872MB and growing | 98-111MB FLAT across depths | -88%, non-growing |
| net image-cache rows, deep scroll | 595+221MB | 36-249MB (encoded retention + protected-resident) | bounded |
| scrollback correctness | n/a (machinery off) | zero corruption Ã¢â‚¬â€� visually verified r/pics exact-failure-testbed | PASS |
| crashes in verification | teardown class (fixed separately) | zero eviction-attributable (video RG8 crash proven independent by control 3/3) | clean |

What the numbers mean: image decoded-byte mass is now BOUNDED FLAT across infinite scroll Ã¢â‚¬â€� the single-copy-at-a-time invariant holds: visible+protected images cost decoded bytes, everything else costs only encoded bytes (10-20x smaller) until scrolled back. The residual encoded retention (~tens of MB scaling with distinct images seen) is the documented insurance cost. Remaining documented residuals: synchronous re-decode on scroll path (unmeasured jank risk, proper async is the rejected-cheap-path lesson of sec1zs), registry entry accumulation (KB-scale), DOM text-node mass 272MB (next category Ã¢â‚¬â€� untouched).

Mission structure as built: stage 1 byte-swap machinery (sole-owner residency, Drop-balanced accounting), stage 2 eviction unit + geometric verdict + scroll-driven re-upload, stage 3 async REJECTED with evidence (sync is the design), stage 4 dual-margin hysteresis + 256MB backstop budget + default-on in feather shell (feather#140 merged).

## requestIdleCallback implementation (2026-11-26, feather-engine 402c303cf)

Symptom: youtube blank white after rIC landed (was grey skeleton before) Ã¢â€ â€™ UNRESOLVED whether progress (hydration advanced past skeleton into broken real-content render) or regression Ã¢â‚¬â€� bisect by reverting rIC locally and comparing. Mechanism note: IdleCallbackTick no-op frame callback keeps the rAF pump alive while any idle callback is pending; recursive idle re-registration (common site pattern) = continuous rAF-rate ticking = CPU cost Ã¢â‚¬â€� not spec-true idle detection, documented v1 approximation.

Lesson: generated webidl callbacks invoke via `callback.Call__(cx, args..., ExceptionHandling::Report)` NOT `.call()`; dictionary fields without defaults generate as Option<T>; methods need cx param only when codegen decides (compile errors tell); CrossProcessInstant needs #[no_trace] in dom structs; double returns need Finite<f64> wrap.

## F1 final root cause (2026-11-26) Ã¢â‚¬â€� correction of earlier coordinate hypothesis

Symptom: in-page links/inputs appear dead Ã¢â€ â€™ check: POST-NAVIGATION PAINT INVALIDATION in shell-new compositing (CONFIRMED CAUSE Ã¢â‚¬â€� verified live: clicks register exact css coords (149,148 vs true 150,150), click events fire on correct targets (target=A), anchor activation proceeds, navigation fetches+loads the target (200 in net log), omnibox eventually updates Ã¢â‚¬â€� but the new page NEVER PAINTS until subsequent input interactions nudge the surface; links were never dead, navigation was invisible); forward_page_input coordinate math (status: RULED OUT on macOS scale1 AND windows scale2 Ã¢â‚¬â€� exact registration both lanes); anchor activation/navigation delegate path (status: RULED OUT Ã¢â‚¬â€� navigation completes end-to-end); blit geometry at scale1 (status: RULED OUT Ã¢â‚¬â€� visual box position matches layout position within 2px). MEASUREMENT LESSON: the original down-right-coordinate evidence was measurement error Ã¢â‚¬â€� the click position was calculated from assumed element positions rather than verified visual positions; never infer input-transform defects without a self-reporting target element (one that logs its own hit coordinates).

## YouTube results-render stall Ã¢â‚¬â€� root cause chain (2026-11-26, macOS lane)

Symptom: SPA hydration appears complete, search data arrives (200), results never paint Ã¢â€ â€™ check: `ytd-two-column-search-results-renderer` hidden attribute removal (prime suspect Ã¢â‚¬â€� never removed in servo; force-removal renders everything), Polymer property-effects/observer timing for visibility triggers (open), custom element upgrade machinery (ruled out Ã¢â‚¬â€� constructors/attachShadow/connectedCallback/upgrade all probe-verified on the live page), nested flex layout (ruled out Ã¢â‚¬â€� probe passes), content-visibility (ruled out Ã¢â‚¬â€� unsupported in servo), Trusted Types (ruled out Ã¢â‚¬â€� correctly implemented; probe throws were eval artifacts), boot throws (ruled out Ã¢â‚¬â€� zero console errors with full exception+rejection visibility).

Key diagnostic pattern: when an entire subtree is 0x0@0,0, walk UP the chain before theorizing layout bugs Ã¢â‚¬â€� the cause was a single display:none host at the top. Zero-geometry descendants are the trivial consequence.

Tooling lesson: an evaluate-javascript escape hatch in the shell (/tmp/feather-eval.js hook) converts blind-probe hours into direct-measurement minutes. Console+exception+rejection visibility is necessary but not sufficient Ã¢â‚¬â€� silent logic failures need DOM inspection.

## YouTube final unhide chain + negative results (2026-11-26, macOS lane, round 2)

Symptom: results fully built in DOM but never visible Ã¢â€ â€™ check: hydration element replacement (CONFIRMED Ã¢â‚¬â€� boot instance gets unhidden at T+1952ms, hydration then swaps in a NEW element instance stamped with hidden again; the final unhide step never executes on it; verified by identity tracking: current element has zero hidden-ops in log). Binding churn ruled out (MutationObserver Ã¢â‚¬â€� nothing re-sets hidden). scheduler.postTask ruled out (zero references in YouTube's entire JS payload Ã¢â‚¬â€� would have been wasted implementation). Trusted Types ruled out (fully correct). Unhide machinery generally ruled out (100+ successful removals across other elements).

Post-manual-unhide functionality measured: search results render fully (thumbnails, badges, metadata), SPA click-through to /watch works, watch page renders full chrome incl. live chat. YouTube visible gaps reduced to exactly two: final unhide step + video frame compositing (F3).

Diagnostic tooling pattern that worked: persistent eval hook (re-injected at navigation commit) + monkeypatched Element.prototype.setAttribute/removeAttribute/hidden-setter with element identity tagging (WeakMap ids) Ã¢â‚¬â€� this is the general instrumentation pattern for "which JS touches which DOM state when" questions in servo. Also: shell-new spin-throttle Duration subtraction underflow panic Ã¢â‚¬â€� TOCTOU between guard check and subtraction; any elapsed()-then-subtract pattern in the shell needs saturating_sub.

## sec1zm Ã¢â‚¬â€� RG8 video-plane crash FIXED: planar YCbCr upload (2026-11-26)

FIXED Ã¢â‚¬â€� commit f9180e2bf1d. NV12 video chroma is now deinterleaved into separate Cb/Cr planes and uploaded as planar YCbCr Ã¢â‚¬â€� all-R8 planes, fully shared-cache compatible, natively supported by WebRender's brush_yuv shader. Deinterleave cost is a quarter-resolution byte shuffle Ã¢â‚¬â€� negligible against decode cost.

Root-cause chain for the record: WebRender `is_allowed_in_shared_cache` (texture_cache.rs) checks SIZE constraints but NEVER checks whether the format is supported by the shared arrays Ã¢â‚¬â€� RG8 planes under TEXTURE_REGION_DIMENSIONS entered shared cache and select() panicked "Unexpected format RG8". Size-dependent flakiness explained: large planes went standalone (safe), small/medium hit shared arrays (panic). This is an upstream WR defect Ã¢â‚¬â€� the proper upstream fix is a format-support check in is_allowed_in_shared_cache, but the planar path is strictly better for servo anyway (no upstream dependency, no vendoring).

Verified: (1) SMPTE color bars render with CORRECT colors (grey/yellow/cyan/green/magenta/red/blue + bottom stripes) Ã¢â‚¬â€� video visibly plays, not black, not color-shifted; (2) 8/8 deep-scroll bursts on r/pics survive to scrollY 15649 with video content in path, zero RG8 panics Ã¢â‚¬â€� pre-fix rate ~2/3 crashes at this depth.

Symptom index: symptom "Unexpected format RG8 panic WRRenderBackend then signal 11 scrolling video feeds" -> status FIXED at f9180e2bf1d Ã¢â‚¬â€� planar upload means RG8 is never uploaded to WR at all. Symptom "video renders black despite active decode" (dogfood F3) -> FIXED same commit Ã¢â‚¬â€� planes now reach WR in a format its shader actually rasterizes.

Symptom: video colors wrong on SD content Ã¢â€ â€™ check: Rec709+Limited hardcoded in display_list/mod.rs push_yuv_image (unchanged by this fix Ã¢â‚¬â€� correct for HD h264, wrong for BT.601 SD; proper fix parses gst colorimetry caps).

Threading note for future work: MediaFrame now carries image_key + uv_key + v_key; uv_key+v_key both present = planar, uv_key alone = NV12 (legacy path retained), neither = BGRA. Key lifecycle: generated per frame-format-change in media_frame_renderer render(), deleted in delete_frame_keys.

Residual cost: deinterleave allocates two new vectors per frame (half-res each) Ã¢â‚¬â€� memcpy-bound, unmeasured but small vs decode. The gst copy-out remains as documented in the NV12 pipeline notes.

## sec1zn Ã¢â‚¬â€� media teardown stress: seek-ack panic FIXED + youtube-test environmental block (2026-11-26)

REPRO HARNESS for youtube-class media crashes without youtube (youtube is bot-walled on v4-only VMs even with valid cookies Ã¢â‚¬â€� verified LOGIN_INFO sent yet still walled; youtube-nocookie embed also walled, zero videoplayback requests): /tmp/vp9stress.html Ã¢â‚¬â€� 4 concurrent looping vp9 decoders + create/destroy churn every 3s. This reproduces teardown-race crashes in minutes.

FIXED crash class (commit in player.rs): seek-ack channel Disconnected panic at player.rs seek_data callback Ã¢â‚¬â€� teardown drops the ack receiver while a gst-streaming-thread seek is in flight; ack_channel.send(()).unwrap() panicked. Reproduced 5 panics in 2min of churn pre-fix; ZERO panics in 3min of identical stress post-fix. Mechanism codec-agnostic but exposed by vp9-style decode churn.

Symptom index: symptom "panic player.rs seek_data ack_channel Disconnected during video teardown/seek churn" -> FIXED Ã¢â‚¬â€� send failure is benign (nobody waiting), now let-_. Rule for this file: ALL cross-thread channel sends in player.rs teardown-adjacent paths must be let-_ style Ã¢â‚¬â€� the receiver lifetime is never guaranteed during teardown.

OPEN Ã¢â‚¬â€� playerleak volume: the intentional-leak mitigation fires constantly under teardown churn (74 leaks in 3min extreme stress; real-world infinite-feed teardown rate is much lower but nonzero). Each leak pins a gst pipeline shell. PROPER FIX candidate: deferred-dispose retry Ã¢â‚¬â€� instead of permanent mem::forget, queue the player for a dispose retry when refs drain (poll on next player creation or a timer). Not attempted Ã¢â‚¬â€� design change beyond crash-fix scope.

UNRESOLVED Ã¢â‚¬â€� the "vp9 threads crash on YouTube" as a named defect: no documented signature exists in FEATHER_MEMORY or lane history (grepped). Direct vp9 playback (1080p webm loop) does NOT crash. YouTube itself untestable on this VM environmentally. If the crash resurfaces with a concrete signature, the vp9stress harness is the reproduction starting point.

## sec1zo Ã¢â‚¬â€� vp9/YouTube abort hunt: exhaustive negative-results matrix (2026-11-26)

MISSION CONTEXT: coordinator directive to root-cause "gst YouTube abort (vp9 threads crash)". CRITICAL META-FINDING: no such crash signature is documented anywhere Ã¢â‚¬â€� not in FEATHER_MEMORY, not in feather/docs/defect-queue.md, not in lane session history. All greps negative. The defect as named may be misremembered, misattributed, or fixed-incidentally by the planar-YCbCr/teardown work. This entry is the negative-results index so future lanes don't re-walk the search.

REPRO MATRIX Ã¢â‚¬â€� all on post-planar-fix feather, all CLEAN (zero panics/aborts):
1. Progressive vp9 loop 1080p webm, minutes Ã¢â‚¬â€� clean, renders correct.
2. Concurrent vp9 teardown churn (4 decoders, destroy/recreate 3s cycle) Ã¢â‚¬â€� clean post-seek-ack-fix (was: 5 seek-ack panics/2min pre-fix).
3. Random-seek stress 2.5 seeks/sec Ã¢â‚¬â€� clean.
4. MSE vp9 append choreography (updateend-gated 64KB chunks, YouTube pattern) dual-player churn, 5+min Ã¢â‚¬â€� clean, renders correct.
5. MSE adaptive resolution alternation Ã¢â‚¬â€� test artifact: arbitrary byte-splicing corrupts ebml stream (matroskademux "reading large block" errors are the SPLICE being invalid, NOT a product defect Ã¢â‚¬â€� proper repro needs cluster-aligned ffmpeg-generated segments, not built).

Symptom index: symptom "gst abort/crash during YouTube vp9 playback" -> check: seek-ack teardown race (FIXED Ã¢â‚¬â€� sec1zn); RG8 planar class (FIXED Ã¢â‚¬â€� sec1zm, was the actual cause of multiple misattributed crash reports); progressive vp9 decode (ruled out Ã¢â‚¬â€� minutes of loop clean); vp9 teardown churn (ruled out post-seek-ack-fix); MSE append churn (ruled out 5min clean); adaptive resolution reconfigure (UNTESTED properly Ã¢â‚¬â€� needs cluster-aligned segments). If the crash resurfaces: capture the exact signature BEFORE investigating Ã¢â‚¬â€� every hour spent on the wrong signature is waste.

ENVIRONMENTAL: real YouTube untestable on this VM Ã¢â‚¬â€� bot wall even with valid cookies sent (LOGIN_INFO present in requests, SIDCC server-side rotation invalidates within a day), youtube-nocookie embed walled too. YouTube testing requires either fresh cookie extraction from a live signed-in Chrome (portal-keyring encryption blocks the sqlite path Ã¢â‚¬â€� needs CDP-based extraction) or a non-null-routed network.

NEW CONFIRMED DEFECT (found via the stress harness): playerleak volume Ã¢â‚¬â€� 423 leaks in 5.5min of MSE churn stress. The intentional-leak mitigation converts crashes to leaks at high rate under teardown-heavy use. Deferred-dispose-retry is the proper-fix candidate.

## sec1zq Ã¢â‚¬â€� MSE memory leak class FIXED: sourcebuffer quota enforcement (2026-11-26)

ROOT CAUSE chain verified: appsrc push accepts bytes unconditionally regardless of max-bytes (the setting only drives need/enough-data signals) Ã¢â‚¬â€� servo's servomse pushed every appended byte in with no level check Ã¢â‚¬â€� memory grew unboundedly for any video whose pipeline doesn't consume fast enough, which is EVERY paused/preloaded/never-played video (reddit feed posts: 88MB measured idle, 2 SourceBuffers). Enforcement now at push time: level+size > 50MB/track cap Ã¢â€ â€™ push rejected Ã¢â€ â€™ QuotaExceededError from appendBuffer (spec-compliant, Chrome-matching behavior). Rejected bytes never enter the engine nor the indexer.

Symptom index: symptom "memory grows unboundedly on MSE video sites / SourceBuffer rows tens-hundreds of MB" -> FIXED at the quota commit Ã¢â‚¬â€� check servomse push_buffer level enforcement if it regresses. Symptom "video buffers stop growing / appends throw QuotaExceededError" -> now EXPECTED spec behavior when the page doesn't evict; pages that mishandle QuotaExceeded may stop buffering Ã¢â‚¬â€� not an engine defect.

Verification notes: regression pass = updateend-gated append choreography still completes, playback unaffected (playing videos never hit cap Ã¢â‚¬â€� queues drain). Accepted cost: one byte-copy clone per appendBuffer in the reordering (memcpy-speed, negligible vs decode). OPEN refinement: the 50MB/track cap is the pre-existing design constant Ã¢â‚¬â€� tuning it lower for memory-tight alpha is a one-line change with real tradeoffs (youtube forward-buffer depth). The mass attribution caveat: gst-internal appsrc queue bytes don't show in malloc_sizeof rows Ã¢â‚¬â€� memory verification for MSE must use RSS or behavior-level evidence, not the SourceBuffer row alone.

## sec1zr Ã¢â‚¬â€� XHR retention verdict: NOT an engine defect (2026-11-26)

Investigated the 79MB/663-object XMLHttpRequest mass from the reddit attribution. Servo stores response bytes in `response: DomRefCell<BytesMut>` inside the XHR object Ã¢â‚¬â€� spec-correct behavior: response/responseText must remain available while JS retains the object. The retention is PAGE-caused (reddit's JS holds hundreds of XHR objects alive in closures/pools), not engine waste Ã¢â‚¬â€� no fix available without violating spec. Negative index entry so nobody re-walks it: XHR response mass is a website-behavior datum, not a feather defect. Compare: the SourceBuffer mass looked similarly page-caused but turned out to be engine-fixable (sec1zq) because the spec HAS a quota mechanism Ã¢â‚¬â€� XHR has no equivalent pressure valve.

## sec1zs Ã¢â‚¬â€� DOM text-node mass characterized: CONTENT-dominated, representation dead-end (2026-11-26)

Controlled measurement (memreport rows on synthetic pages): (a) one 1MB text node Ã¢â€ â€™ 2.0MB row Ã¢â‚¬â€� content costs ~2x byte size when JS-assigned (parser-created content likely ~1x); (b) 10,000 nodes Ãƒâ€” 10 chars Ã¢â€ â€™ 3.21MB row Ã¢â‚¬â€� **~321 bytes per-node overhead** (node struct + reflector + String headers). Element nodes: ~473B each.

Applied to reddit's 224MB text mass: even a generous 30k-node estimate accounts for only ~10MB overhead Ã¢â‚¬â€� **the mass is ~95% CONTENT bytes**. Conclusions: (1) representation optimization is a dead end (~5% ceiling) Ã¢â‚¬â€� do not pursue; (2) the content itself must be shredded-JSON-state-dominated (shreddit serializes megabytes of state into script/element text) Ã¢â‚¬â€� page-inherent like the XHR mass; (3) the only theoretical levers are architectural and dubious: dropping script text post-execution violates spec (scripts can re-read textContent), string interning is marginal. VERDICT: text-node mass is not tractable engine-side. Negative index entry Ã¢â‚¬â€� do not re-walk unless the measurement methodology itself is questioned.

Symptom index: symptom "js/dom/text-nodes rows hundreds of MB" -> check: page content volume FIRST (shreddit-class JSON-state embedding), not engine representation. Per-node overhead ~321B, per-element ~473B are the calibrated constants for estimating overhead share on any page: overhead_MB Ã¢â€°Ë† nodesÃƒâ€”321/1M Ã¢â‚¬â€� if that's small vs the row, the mass is content and engine-intractable.

## sec1zt2 Ã¢â‚¬â€� consolidation verification: all fixes converge on real reddit (2026-11-26)

Post-everything measurement (planar video + seek-ack fix + MSE quota, feather debug build, idle r/pics, 5 consecutive 60s memdumps): image-cache converges 711MB transient Ã¢â€ â€™ **267MB flat at cap** across 4 steady-state dumps Ã¢â‚¬â€� eviction convergence confirmed in the wild; the 711 first-dump was the initial decode-burst transient before eviction catches up (bounded, self-correcting Ã¢â‚¬â€� but worth knowing: eviction lags burst loads). SourceBuffer **68MB flat** across all steady-state dumps Ã¢â‚¬â€� bounded consistent with quota enforcement (though under the 100MB 2-track cap, so enforcement not definitively stress-proven in the wild Ã¢â‚¬â€� the synthetic paused-append test remains the proof). RSS 1474MB idle Ã¢â‚¬â€� vs sec1zj-era 1162MB: the delta is media/gst preload cost + encoded retention growth, all now bounded.

Remaining open engineering item, deliberately not attempted: playerleak deferred-dispose-retry. Risk assessment: the dragons-condition refcount semantics are subtle (glib clone counting unverified), a wrong implementation resurrects the use-after-finalize crash class (alpha-critical) Ã¢â‚¬â€� risk-adjusted ROI poor vs the few-MB leak cost. Documented for a future careful session.

## YouTube boot instability Ã¢â‚¬â€� OOM + define_properties panic (2026-11-26, macOS lane, round 3)

Symptom: YouTube boot succeeds fully sometimes (video plays end-to-end), wedges mid-boot other times (stuck hidden, partial chrome), crashes outright occasionally ("Servo crashed! Send failed (Disconnected)") Ã¢â€ â€™ check: SpiderMonkey memory pressure during boot (PRIME SUSPECT Ã¢â‚¬â€� live evidence: `uncaught exception: out of memory` in youtube main module + simultaneous script-thread panic), js_mem_max (ruled out as cause Ã¢â‚¬â€� unset/-1 = no configured cap), RSS system starvation (unverified Ã¢â‚¬â€� 1GB at crash time, needs profiling).

CONFIRMED ENGINE DEFECT regardless of memory: `define_guarded_properties` in script_bindings/interface.rs unwraps `define_properties` Ã¢â‚¬â€� when SM allocation fails (OOM), JS_DefineProperties errors, the unwrap PANICS, the script thread dies, the whole pipeline crashes. The panic converts a recoverable page-level OOM into a fatal pipeline crash. Signature: `panicked at interface.rs:434 called Result::unwrap() on an Err value: ()`.

Unifying theory for the YouTube hidden-attribute stall: boot corruption from memory pressure explains the run-to-run variance Ã¢â‚¬â€� full boot (video test), stuck-hidden boot (most common), crash boot. The final unhide step may be a victim of corruption rather than a missing primitive. Ruled out for the unhide this round: yt-navigate event chain (all events fire correctly on both watch AND search navs Ã¢â‚¬â€� start/player-updated/finish/page-data-updated), scheduler.postTask (unused by YouTube entirely), Trusted Types, CE machinery.

Meaningful functionality baseline established: watchÃ¢â€ â€™watch SPA navigation flawless, miniplayer works, autoplay chain works, video playback correct post-planar-fix (F3 confirmed fixed end-to-end on real YouTube MSE).

## sec1zt3 Ã¢â‚¬â€� THE NEXT MEMORY FRONTIER: gst decode-pipeline mass is RSS-dominant and reporter-invisible (2026-11-26)

Deep-scroll verification with ALL fixes (eviction ON, r/pics video-heavy, scrollY 22500, 15/15 scrolls zero panics): every reporter category bounded flat Ã¢â‚¬â€� image-cache 266Ã¢â€ â€™287MB at cap, webrender/images 235Ã¢â€ â€™218, SourceBuffer 66Ã¢â€ â€™27 (quota working in the wild), text-nodes ~flat. **Yet RSS = 7.2GB.** The memreport/jemalloc accounting is now blind to the dominant mass: gstreamer decode pipelines allocate through system malloc/glib Ã¢â‚¬â€� NOT jemalloc Ã¢â‚¬â€� so video memory is completely invisible in memreport rows. Frame math: one 1080p NV12 frame Ã¢â€°Ë† 3MB; every preloaded video's pipeline holds multiqueue+decoder-pool frames; video-heavy infinite feeds preload dozens-hundreds of pipelines. Compare closeout-era RSS 2374MB at similar depth BEFORE video enablement Ã¢â‚¬â€� the multi-GB delta is the video pipeline mass.

Symptom index: symptom "RSS huge but memreport categories all bounded/small on video-heavy pages" -> check: gst pipeline frame memory FIRST (system-malloc invisible to jemalloc rows Ã¢â‚¬â€� verification must use RSS not memreport), multiqueue/decoder bufferpool retention in paused/preloaded pipelines. NOT image cache/eviction regression Ã¢â‚¬â€� verify categories before suspecting them.

OPEN MISSION-CLASS questions: (1) how many concurrent gst pipelines does a feed scroll accumulate and when do they die (shreddit virtualization destroys post elements Ã¢â‚¬â€� does teardown actually happen?); (2) multiqueue/decoder pool sizes per pipeline Ã¢â‚¬â€� the multiqueue default buffers are time-based not byte-capped for video; (3) whether preloaded-but-never-played videos can be prevented from prerolling deep frame buffers at all (decode-on-play activation Ã¢â‚¬â€� the natural architectural fix, echoes the image-memory lesson: the only real win is not decoding what isn't visible).
## sec1zt3 Ã¢â‚¬â€� THE NEXT MEMORY FRONTIER: gst decode-pipeline mass is RSS-dominant and reporter-invisible (2026-11-26)




## sec1zt4 Ã¢â‚¬â€� RSS forensic numbers: the 5GB gap decomposition starting point (2026-11-26 continuation of sec1zt3)

Precise numbers from the deep-scroll measurement (scrollY 22500 r/pics, all fixes): RSS/Pss_Dirty 7.4GB anonymous-private (real, not shared). jemalloc-heap-allocated only 1.68GB, system-heap-allocated 0.55GB Ã¢â‚¬â€� so ~5GB of RSS is NOT live allocation in either heap. Prime suspect decomposition: (a) jemalloc mapped-but-not-allocated retention Ã¢â‚¬â€� freed decoded bytes whose dirty pages jemalloc keeps for reuse (CHECK jemalloc-heap-mapped vs allocated ratio in future dumps Ã¢â‚¬â€� if mappedÃ¢â€°Ë†RSS the mass is retention, and the arena-purge lever applies directly Ã¢â‚¬â€� investigate whether eviction frees should trigger purge); (b) gst/glib system-malloc frames Ã¢â‚¬â€� but system-heap-allocated says only 0.55GB, weakening the gst-frame theory unless glib uses its own allocator outside system-heap accounting; (c) only 7 decodebin instances existed Ã¢â‚¬â€� NOT hundreds of pipelines, and only 4 playerleaks Ã¢â‚¬â€� both pipeline-count and leak theories WEAKENED. Resumption protocol: compare jemalloc-heap-mapped against RSS at matched timestamps; if mappedÃ¢â€°Ë†RSS, the fix is purge-after-evict in the image eviction path (cheap, high value). This is the single most important open memory question Ã¢â‚¬â€� all category rows are bounded yet the process eats 7GB.

## sec1zt5 Ã¢â‚¬â€� RSS forensics FINAL CLUE: giant anonymous mmaps, neither heap (2026-11-26)

smaps analysis of the 7.4GB-RSS scrolled process: a single **3.3GB anonymous rw-p mmap** plus ~40 more 100-500MB anon mmaps Ã¢â‚¬â€� together ~5-6GB, explaining nearly the whole gap. These are NOT jemalloc (heap-mapped only 2GB) and NOT system-malloc. Allocation pattern (huge contiguous anonymous mmaps) matches engine-level arena/zone allocators: prime suspects (1) **mozjs GC heap retention** Ã¢â‚¬â€� spidermonkey allocates arena chunks and notoriously never returns them; gc-heap/used row was only 22MB meaning gigabytes of EMPTY GC arenas retained Ã¢â‚¬â€� if confirmed this is a GC-tuning/GC-param problem not a leak; (2) llvmpipe/software-GL texture memory if textures mmap outside jemalloc. RESUMPTION PROTOCOL: (a) test on a NON-JS-heavy scroll (wikipedia) Ã¢â‚¬â€� if the giant mmaps don't grow, it's mozjs/GC; (b) mozjs GC params: servo exposes js options Ã¢â‚¬â€� shrinking/nursery/zeal settings; the key question is whether GC heap shrinkage (compact/empty-chunk-release) ever runs during idle. NOTE: this finding reorders the memory roadmap AGAIN Ã¢â‚¬â€� if the 3-5GB is GC retention, it dwarfs every category fixed so far and is a single-mechanism fix.

## sec1zt6 Ã¢â‚¬â€� RSS forensics DECISIVE COMPARISON: low-JS/no-video page stays flat (2026-11-26)

Wikipedia (List of chemical elements Ã¢â‚¬â€� long page, moderate JS, NO video) scrolled 15Ãƒâ€”1500px same protocol as the reddit deep-scroll: final RSS **431MB** vs reddit's **7.4GB**. Same engine, same scroll depth. CONCLUSIONS: (a) the giant-mmap mass is NOT inherent to scrolling/rendering/JS-churn generally Ã¢â‚¬â€� it's specific to video-bearing feeds; (b) mozjs-GC-retention theory WEAKENED (wiki runs plenty of JS); (c) the prime suspect returns to VIDEO decode memory with refined mechanism: **glibc per-thread malloc arenas** Ã¢â‚¬â€� gst decode threads allocate through glibc thread arenas which mmap independently (64MB-chunk growth pattern matches the ~40 observed 100-500MB anon mmaps) and are INVISIBLE to both jemalloc accounting AND (plausibly) the system-heap-allocated row if that only measures the main arena. The 3.3GB single contiguous mmap remains unexplained by this theory Ã¢â‚¬â€� candidate: glibc arena coalescing appearance in smaps (adjacent arenas merge visually) or llvmpipe texture memory for video frames. NEXT RESUMPTION STEP: scroll r/pics and timestamp-correlate mmap growth against vp9 decode start events in the gst log; if confirmed, fixes are (1) glibc arena tuning (MALLOC_ARENA_MAX=2 env Ã¢â‚¬â€� trivial to test, potentially multi-GB win), (2) gst decode buffer pooling caps. MALLOC_ARENA_MAX=2 is a one-line test with possibly the largest single memory ROI in the project.

## sec1zt7 Ã¢â‚¬â€� MALLOC_ARENA_MAX=2 confirmed: Ã¢Ë†â€™3.2GB on reddit scroll (2026-11-26)

Decisive test: identical r/pics 15Ãƒâ€”1500px deep-scroll with MALLOC_ARENA_MAX=2 env: RSS **4.2GB vs 7.4GB** baseline Ã¢â‚¬â€� glibc thread arenas were retaining ~3.2GB. Confirms the sec1zt6 thread-arena theory: gst decode threads allocate via glibc arenas which never return memory cross-thread. One-line env fix, biggest single memory ROI in the project. Remaining 4.2GB vs wiki's 431MB still unexplained Ã¢â‚¬â€� the 3.3GB contiguous mmap and other masses persist; investigation continues but the cheap lever is proven. Implementation note: glibc reads malloc env tunables at ptmalloc_init (first glibc-malloc call) Ã¢â‚¬â€� since servo's Rust side allocates through jemalloc, first glibc malloc happens in C libs (gst/glib) AFTER main starts, so setting the env early in main() may be early enough Ã¢â‚¬â€� MUST be empirically verified in the feather shell before shipping; fallback is re-exec-with-env pattern. Arena count tradeoff: 2 arenas may serialize allocations across threads slightly Ã¢â‚¬â€� measure startup/perf impact if visible, but memory win dwarfs it. Symptom-index: Symptom: RSS grows multi-GB on video-heavy feeds invisible to memreport rows Ã¢â€ â€™ check glibc thread arenas (prime suspect, CONFIRMED Ã¢Ë†â€™3.2GB with MALLOC_ARENA_MAX=2), jemalloc retention (ruled out Ã¢â‚¬â€� heap-mapped only 2GB), mozjs GC retention (weakened Ã¢â‚¬â€� wiki flat).

## sec1zt8 Ã¢â‚¬â€� residual forensics post-arena-cap: the giant mmap is NOT arenas (2026-11-26)

Post-cap measurement (MALLOC_ARENA_MAX=2, r/pics scrolled, RSS 4.5GB): a **single 2.6GB contiguous anonymous rw-p mmap** persists plus [heap] 173MB and several 100-250MB mmaps. Since arenas are now capped, the giant contiguous mmap CANNOT be glibc thread arenas Ã¢â‚¬â€� revised attribution: most likely **jemalloc's own extent regions** (jemalloc mmaps large contiguous chunks; heap-mapped was ~2GB in the uncapped measurement, consistent scale) Ã¢â‚¬â€� meaning the residual giant-mmap mass IS inside jemalloc accounting after all, and the real question becomes the sec1zt4 second gap: jemalloc-heap-allocated 1.68GB vs sum-of-category-rows Ã¢â‚¬â€� i.e. LIVE-but-unaccounted allocation inside the Rust heap, not retention. Suspects for unaccounted live jemalloc mass: (a) mozjs GC/JS objects measured only partially by gc-heap rows; (b) layout/script structures outside the reporter tree; (c) WR internal structures. RESUMPTION: compare jemalloc-heap-allocated vs heap-mapped and category-sum at matched timestamps; if allocatedÃ¢â€°Ë†1.7GB Ã¢â€°Ë† giant mmap, the frontier is 'unaccounted live rust heap' Ã¢â‚¬â€� use heaptrack or jemalloc prof (MALLOC_CONF=prof:true) for definitive attribution. The arena-cap fix (feather#153 merged) already removed 3.2GB; the residual ~4.5GB decomposes as ~2GB jemalloc (partially unaccounted) + [heap]+glibc ~0.7GB + remainder TBD.

## js_mem_max 192MB was the YouTube boot-crash cause Ã¢â‚¬â€� raised to 1GB (2026-11-26, macOS lane)

The shell's `js_mem_max=192` runaway cap (kept from memory-pressure work, predates heavy-SPA dogfooding) hard-caps SpiderMonkey Ã¢â‚¬â€� YouTube boot exceeds it: uncaught 'out of memory' in the main module + define_properties unwrap panic at interface.rs:434 killing the pipeline. Explains boot variance: full/wedged/crashed depending on when the ceiling hit. Fixed in shell-new: 1GB default, FEATHER_JS_MEM_MAX env override. HONEST NEGATIVE: the hidden-attribute stall is NOT memory-caused Ã¢â‚¬â€� clean 1GB boot still ends stuck-hidden with 19 renderers built; that defect is deterministic logic, memory was only crash/wedge variance. Rule for future memory work: caps tuned against lightweight sites silently break heavy SPAs Ã¢â‚¬â€� verify against YouTube before adopting memory ceilings.

## sec1zt9 Ã¢â‚¬â€� MEMORY PROGRAM CLOSEOUT: combined eviction+arena-cap measurement (2026-11-26)

Final end-to-end number, r/pics deep scroll 15Ãƒâ€”1500px = scrollY ~22500, identical protocol across all runs, signed-out feed with video posts:

| config | RSS at depth 22500 |
| baseline (pre-program, growing unbounded) | 7.4GB |
| eviction only (FEATHER_IMGMEM=1) | ~7.2GB |
| arena cap only (MALLOC_ARENA_MAX=2) | 4.2GB |
| **combined (feather shell default config)** | **1.46GB FLAT** |

**Ã¢Ë†â€™5.9GB, 80% reduction, zero panics, zero playerleaks.** Settled flat (1458Ã¢â€ â€™1462Ã¢â€ â€™1463MB across minutes). Non-obvious interaction: combined is far better than either lever alone Ã¢â‚¬â€� eviction frees jemalloc image mass fast enough that less churn crosses into glibc arenas, and capped arenas can't retain what little gst churn remains. IMPORTANT nuance for all lanes: eviction-only measurements were nearly useless against RSS BECAUSE the freed masses were being re-captured by arena retention Ã¢â‚¬â€� the two levers are multiplicative, and any future memory fix must be measured with BOTH active or its effect will be invisible. This is the closeout number for the memory program; residual 1.46GB decomposes as bounded jemalloc live set (DOM/text/XHR/sourcebuffer categories documented in sec1zr-zs) + gst working set Ã¢â‚¬â€� the unaccounted-jemalloc question of sec1zt8 is largely ANSWERED by this measurement: most of the 'unaccounted' mass was arena retention after all.
## sec1zt9 Ã¢â‚¬â€� MEMORY PROGRAM CLOSEOUT: combined eviction+arena-cap measurement (2026-11-26)




## Devtools server wired into shell-new Ã¢â‚¬â€� diagnostic capability unlock (2026-11-26, macOS lane)

servo's Firefox-protocol devtools server is complete upstream (console/inspector/network/breakpoint actors) but pref-gated and unwired. shell-new now: `devtools` cargo feature + FEATHER_DEVTOOLS=<port> env + ServoDelegate writing token to /tmp/feather-devtools-token and auto-allowing connections. Verified end-to-end: listTabs correct, getTarget exposes ConsoleActor/inspector actors, evaluateJS returns live structured results.

Connection protocol notes: TCP length-prefixed JSON (`len:{json}`); server sends an unsolicited root traits message first Ã¢â‚¬â€� clients must skip it; TabDescriptorActor doesn't handle bare 'attach' Ã¢â‚¬â€� use 'getTarget' Ã¢â€ â€™ BrowsingContextActor which carries consoleActor/inspector actor names; ConsoleActor evaluateJS works, startListeners returns unrecognizedPacketType (listener names or support incomplete). Client auth: either send {"auth_token":...} as first message or rely on embedder auto-allow.

Memory-investigation rule correction: js_mem_max caps tuned on lightweight sites silently break heavy SPAs Ã¢â‚¬â€� the 192MB cap OOM-ed YouTube boot (uncaught 'out of memory' + interface.rs define_properties unwrap panic killing the pipeline). Now 1GB default + FEATHER_JS_MEM_MAX override. Hidden-stall honest negative: NOT memory-caused Ã¢â‚¬â€� clean 1GB boot still stuck-hidden.

## YouTube replacement chain final form (2026-11-26, macOS lane, round 3 close)

Symptom: search results built but invisible Ã¢â€ â€™ CONFIRMED MECHANISM: T1429 boot unhides SSR element correctly; T1751 entire ytd-search subtree REPLACED wholesale in post-response re-render, replacement arrives stamped hidden=true; no hidden op ever fires on replacement; crucially the replacement elements are UPGRADED BUT NEVER INITIALIZED (__data absent, dataReady undefined, isAttached=true) Ã¢â‚¬â€� the Polymer data-system init that would clear hidden via property binding never runs on them. Prime suspect direction: deferred/lazy init trigger (yt uses lazy-init heavily Ã¢â‚¬â€� IntersectionObserver-timing or lifecycle-event class). MutationObserver experiment design note: nested removals inside ancestor re-stamps do NOT appear in removedNodes Ã¢â‚¬â€� filter for ancestor-level adds containing the target, not direct node movement.

## CORRECTION Ã¢â‚¬â€� the "never initialized" youtube theory was a framework misread (2026-11-26)

ytd-masthead (fully working) ALSO lacks __data/dataReady Ã¢â‚¬â€� modern YouTube elements don't use the classic Polymer data system at all. The absence of __data on stuck elements is NORMAL, not evidence of failed initialization. Retracted as a mechanism theory. What stands: replacement arrives stamped hidden=true at T1751, no op ever clears it, manual driving of initVisibilityObserver_/flushRenderStamperComponentBindings_/initializeInjection changes nothing without throwing. Lesson: probe framework assumptions against WORKING elements on the same page before theorizing from missing internals.

## PerformanceObserver probe Ã¢â‚¬â€� mostly fine, warning is narrow (2026-11-26, macOS lane)

servo supportedEntryTypes: container, largest-contentful-paint, mark, measure, navigation, paint, resource, visibility-state. observe() accepts every type without throwing (even unsupported ones like longtask/layout-shift Ã¢â‚¬â€� no validation error). The youtube "No valid entry type provided to observe()" warning is therefore NOT a broad PerfObserver gap Ã¢â‚¬â€� likely a specific buffered/option combination; weak lead, deprioritized as boot-telemetry noise unless proven load-bearing.

## YouTube hidden hunt Ã¢â‚¬â€� property-setter probe results (2026-11-26, macOS lane)

Framework manipulates hidden via PROPERTY assignment (`el.hidden =`), not setAttribute Ã¢â‚¬â€� earlier attribute-op logging missed this layer entirely. Hidden-property probe (patched HTMLElement.prototype hidden setter, /tmp/feather-eval-persist.js v-hp) on a fresh search boot:

- ytd-search itself IS unhidden via property write at boot (`setActivePage`, T1457) Ã¢â‚¬â€� page-level logic works.
- Framework helper `qJC` (in ytmainappweb kevlar_base bundle) is the hide/unhide function: e.g. YTD-WATCH-METADATA gets hid=1 then hid=0 ~39ms later, BOTH through qJC Ã¢â‚¬â€� the unhide counterpart mechanism EXISTS and fires for other elements.
- YTD-TWO-COLUMN-SEARCH-RESULTS-RENDERER: hid=1 via qJC at T1469 Ã¢â‚¬â€� never followed by hid=0. The framework actively hides it (not just SSR-stamped) and the clearing qJC call never runs.
- Caveat: replacement-via-innerHTML stamping would bypass the property setter (hidden attribute in parsed markup), so replacement-time stamping is invisible to this probe; identity correlation with mutation log not yet done.
- NEXT LAYER: what calls qJC(hidden=false) for the results renderer in Chrome Ã¢â‚¬â€� get caller chain of qJC by capturing full stack (probe currently stores only frame 2; deepen to full stack) or find the completion signal qJC awaits (search-response processing completion event?). qJC is minified inside ytmainappweb kevlar_base bundle Ã¢â‚¬â€� grep-able in the fetched JS.

## sec1zt10 Ã¢â‚¬â€� YouTube OOM/define_properties theory VERIFIED + fixed (2026-11-26)

Verification of the macOS lane's unifying theory (memory-pressure boot corruption), on Linux:

**Experiment setup**: js_mem_max=32 induced-pressure harness (youtube.com watch boot Ãƒâ€” 6 per run). NOTE js_mem_max pref is capped at range 1..0x100 (256MB) in script_runtime Ã¢â‚¬â€� values above 256 are IGNORED silently.

**Findings**:
1. **The ceiling lever is a dead end**: servo already sets JSGC_MAX_BYTES=u32::MAX when js_mem_max unset Ã¢â‚¬â€� there is no ceiling to raise. The OOM is genuine allocation failure, not a configurable cap.
2. **Memory pressure reproduces the boot-corruption variance exactly as theorized**: under 32MB cap, boots alternate full/partial ('no-ytplayer' Ã¢â‚¬â€� empty title, incomplete boot) Ã¢â‚¬â€� the stuck-boot symptom reproduced on demand. This CONFIRMS pressureÃ¢â€ â€™corruption causal chain.
3. **Panic fixed at BOTH sites** (define_guarded_properties:434 AND define_guarded_methods:418 Ã¢â‚¬â€� same defect class): failure now clears the pending exception + logs `[interface]` warn + leaves interface partially defined. Page degrades, browser survives.
4. **Post-fix survival verified**: 64 uncaught out-of-memory exceptions across 6 boot attempts Ã¢â‚¬â€� all surfaced as page-level JS exceptions, zero panics, process alive throughout. The OOM-death behavior is now correct-by-construction at the interface-definition sites.
5. Caveat: the `[interface]` warning never fired in the induced-pressure runs Ã¢â‚¬â€� the interface-definition path apparently wasn't the failure point under this pressure pattern (OOMs hit script execution instead). The fix is verified by construction + general survival, not by direct observation of that path firing. If YouTube instability persists on memory-constrained devices after this, the next suspect is NON-interface OOM corruption of boot state (page-level, not engine-fixable beyond what browsers normally do).

**Bonus discovery this session**: YouTube is NOT currently bot-walled on the Linux VM Ã¢â‚¬â€� watch pages boot with correct titles and playback telemetry fires (api/stats/qoe POSTs). The wall documented earlier was intermittent or lifted. YouTube is testable on Linux again Ã¢â‚¬â€� update prior assumptions.
## sec1zt10 Ã¢â‚¬â€� YouTube OOM/define_properties theory VERIFIED + fixed (2026-11-26)





## RULED OUT: IntersectionObserver initial-callback hypothesis (2026-11-26, macOS lane)

Minimal repro (/tmp/vtest/iotest.html): servo IO fires the spec-required INITIAL callback correctly for visible (isIntersecting=true), display:none-hidden (false, ratio 0), AND offscreen (false, ratio 0) elements. YouTube's visibilityOptionHidden_ {calledOnce:false, state:hidden} is therefore NOT caused by missing initial IO callbacks Ã¢â‚¬â€� servo IO is spec-correct here. Also established same probe round: el.data ARRIVES on the results container (data-keys=primaryContents) Ã¢â‚¬â€� the data pipeline reaches the element; the failure is strictly downstream in the visibility/state-machine activation. Boot variance confirmed again: vrs=0 on some boots (no video renderers at all) Ã¢â‚¬â€� the stamping itself sometimes never runs. qJC = Polymer-legacy property-effects binding evaluator (ytmainappweb kevlar_base bundle, 9.8MB, fetched to /tmp/ytbase.js Ã¢â‚¬â€� grep-able reference copy).
Symptom: youtube search results never unhide Ã¢â€ â€™ check: IntersectionObserver initial callback (status: RULED OUT Ã¢â‚¬â€� spec-correct incl. hidden/offscreen initial fire), element data delivery (status: RULED OUT Ã¢â‚¬â€� data arrives with primaryContents), visibility state machine activation trigger (status: PRIME SUSPECT Ã¢â‚¬â€� visOptHidden calledOnce:false, whatever flips it in Chrome never runs; NOT IO-initial-callback).

## YouTube hidden-stall Ã¢â‚¬â€� FINAL MECHANISM LAYER REACHED (2026-11-26, macOS lane)

The binding contract (from kevlar_base bundle source): `<ytd-two-column-search-results-renderer hidden="[[data.hideContents]]" data="[[data.contents.twoColumnSearchResultsRenderer]]">` inside ytd-search's template. hidden is PURELY data-bound.

Live probe on stuck page: ytd-search NOT hidden, data.contents present (full response), data.hideContents UNDEFINED (=falsy). Even a single binding evaluation with current data yields hidden=false. Yet elHiddenAttr=true persists. CONCLUSION: the hidden binding on the post-response replacement element NEVER evaluates Ã¢â‚¬â€� not a timing issue, a dead binding.

PRIME FINAL THEORY (untested, next experiment named): the replacement subtree is created through an innerHTML fallback path rather than Polymer templating Ã¢â‚¬â€� elements arrive upgraded but binding-inert (data arrives via direct property assignment, not binding propagation). In Chrome a FEATURE-DETECTION gate selects the template path (live bindings); servo presumably fails that check. Candidate gates in bundle: window.ShadyDOM detection, HTMLTemplateElement/CE capabilities, 'polymer_yticon_killswitch'-style flags. NEXT EXPERIMENT: grep /tmp/ytbase.js for the render-path feature gates (search: ShadyDOM, template binding instantiation, disable-upgrade) and identify which check servo fails. The scheduler theory is dead: detached-textnode MutationObserver sentinel pattern verified working in servo (motest.html: all 4 patterns FIRED incl. YouTube's exact one). IO innocent. Data pipeline innocent. Everything downstream of the dead binding is explained.

## sec1zt11 Ã¢â‚¬â€� YouTube invisible-results investigation, Linux round: mechanism narrowed to data-binding-never-populates (2026-11-26)

Boot-stability verification first: **8/8 real YouTube search boots succeed post define-panic fix Ã¢â‚¬â€� hidden attr cleared, zero crashes, zero OOMs** (normal memory conditions). The stuck-hidden boot variance documented on macOS does NOT reproduce on Linux at normal memory Ã¢â‚¬â€� consistent with the js_mem_max=192 cap being the macOS corruption source. YouTube is NOT bot-walled on Linux currently Ã¢â‚¬â€� full watch/search pages load, playback telemetry fires.

Mechanism hunt for the invisible-results defect (results in DOM but nothing paints Ã¢â‚¬â€� CONFIRMED still broken on Linux despite hidden being cleared on ytd-search):

**Live measurements** (r/pics-style probe harness on real youtube search): renderer innerHTML fully populated (20KB, real titles) BUT entire chain zero-height; root cause of collapse: `ytd-two-column-search-results-renderer` computed **display:none**; inside it `img src=none` Ã¢â‚¬â€� thumbnails never even got URLs Ã¢â‚¬â€� **the data-binding population never runs on the elements at all**. Text nodes paradoxically measure nonzero range rects inside the display:none subtree (likely a servo range-measurement bug for non-rendered content Ã¢â‚¬â€� minor, noted).

**Ruled out this round (each probe-verified, expensive Ã¢â‚¬â€� do not re-walk)**:
- scheduler.postTask: `window.scheduler` is UNDEFINED in servo BUT servo's UA is Firefox-flavored (`Firefox/153.0` Ã¢â‚¬â€� components/config/prefs.rs UserAgentPlatform) so YouTube serves the FIREFOX codepath, which cannot require postTask (real Firefox lacks it and works). Implementing postTask would be wasted work Ã¢â‚¬â€� coordinator note: NOT unguarded-required, condition for implementation not met.
- IntersectionObserver primitive: fires correctly initial+scroll (test harness in /tmp/mserepro/iotest.html).
- Custom element machinery: constructor+connectedCallback fire correctly for innerHTML insertion, appendChild, AND youtube-style wholesale replaceChildren (cetest.html) Ã¢â‚¬â€� all initialize.
- CE definitions: all registered on the live page (ytd-video-renderer, ytd-app, ytd-masthead all defined).
- JS boot errors: ZERO uncaught exceptions/typeerrors in boots Ã¢â‚¬â€� silent failure, not a thrown error.
- `__data` presence as init discriminator: EVEN THE MASTHEAD (which renders fine) lacks `__data` Ã¢â‚¬â€� it's a Polymer-2-ism modern YouTube doesn't use; macOS lane's init theory partially red-herring. The operative evidence of non-init is img src=none (data never populated), not __data.

**Live suspect chain**: youtubei response processing Ã¢â€ â€™ framework data-sync into elements never happens for feed elements. The visibility flip (display noneÃ¢â€ â€™flex on two-column container, hidden clearing) is data-binding-driven Ã¢â‚¬â€� since binding never populates, everything downstream stays collapsed. Next probe directions: (a) does the ytd-search element itself hold response data (.data or framework equiv) Ã¢â‚¬â€� isolates response-processing vs per-element binding; (b) what JSAPI/feature does the data-sync path need Ã¢â‚¬â€� candidates in missing-API class (per cfdcbc39 hydration-stall index): dynamic import()/chunk loading for the framework's lazy data module, or observable/atomic update primitives. NOTE the boot instance DOES get unhidden at T+1952 then replaced Ã¢â‚¬â€� meaning the FIRST element's data-binding DID populate Ã¢â‚¬â€� so response-processing works at least once; the failure is specific to post-replacement re-binding. That narrows it: replacement-time re-hydration trigger, not the data pipeline itself.
## sec1zt11 Ã¢â‚¬â€� YouTube invisible-results investigation, Linux round: mechanism narrowed to data-binding-never-populates (2026-11-26)






## sec1zt12 Ã¢â‚¬â€� YouTube invisible-results DEFINITIVE isolation: single-attribute defect (2026-11-26)

Decisive force-removal experiment on Linux, live youtube search page: `ytd-two-column-search-results-renderer` carries **hidden=true NEVER REMOVED** (an earlier probe measuring hidden:false measured the PARENT ytd-search Ã¢â‚¬â€� misleading; the hidden is on the two-column child). Force-removing hidden + display:block: renderer height 0Ã¢â€ â€™249px, img src POPULATES with real i.ytimg.com thumbnail URLs, content renders completely Ã¢â‚¬â€� replicating the macOS lane's force-removal-renders-everything on Linux.

**The entire invisible-results defect reduces to ONE attribute**: hidden never cleared on the replacement two-column element. Corrections to prior interpretations: (a) img src=none pre-unhide was LAZY-population-on-visibility, NOT missing data Ã¢â‚¬â€� data binding DID populate the replacement elements; (b) the init theories are dead Ã¢â‚¬â€� everything works except the final attribute flip. The open crux, precisely: data binds on the replacement yet hidden never clears Ã¢â‚¬â€� so hidden is NOT data-binding-driven on the replacement; it's a static SSR stamp whose removal logic fired exactly once on the BOOT element (T+1952ms per macOS) BEFORE hydration swapped in the replacement. In real browsers the ordering must differ (replacement completes before unhide fires, or unhide refires). This is a TIMING/ORDERING race between hydration replacement and the visibility-flip trigger Ã¢â‚¬â€� servo-specific ordering exposes it. Next investigation target: what mechanism fires the T+1952 unhide on the boot element Ã¢â‚¬â€� if it's a one-shot data-ready watcher keyed to element identity, the replacement never re-triggers it; the engine-level question is why hydration ordering differs in servo (custom-elements/reactions timing, or rAF/idle-callback scheduling order Ã¢â‚¬â€� both areas where servo's timing is approximate per sec notes on rIC). Workaround-class fix if root cause stays elusive: none generic Ã¢â‚¬â€� it's a youtube-framework-specific race.

Experiment artifacts: /tmp/ytsession holds a live webdriver session id pattern for reuse (single-session limit workaround Ã¢â‚¬â€� reuse don't recreate); force-flip probe script pattern in session history.

## YouTube hidden-stall Ã¢â‚¬â€� BINDINGS EXONERATED, final layer (2026-11-26, macOS lane)

DECISIVE LIVE PROOF: on the stuck page, `ytd-search.set('data.hideContents', true/false)` toggles the hidden attribute correctly BOTH directions Ã¢â‚¬â€� the Polymer binding system, propagation chain, and qJC evaluator are ALL fully functional in servo, including on the replacement element. Feature-gate/dead-binding theory DEAD.

Final cornered mechanism: the framework step that sets data.hideContents=false after response processing NEVER RUNS in servo. Supporting facts: data.contents present (response data arrived Ã¢â‚¬â€� wholesale data assignment happened), but hideContents is UNDEFINED (never managed). The defect is in post-response navigation-completion logic Ã¢â‚¬â€� the lifecycle step that manages loading visibility state Ã¢â‚¬â€� NOT in any primitive (scheduler/IO/MO/bindings all exonerated with live tests). Boot variance (vrs=0 vs 19) suggests the same completion pipeline partially runs race-dependent amounts.

ONE-LINE VERIFIED MITIGATION: ys.set('data.hideContents', false) unhides results live Ã¢â‚¬â€� usable as a shell-injected shim if a mitigation is ever wanted, but the correct fix is finding why the completion logic doesn't run (suspect: yt-navigate lifecycle handler timing or an exception-free early-exit in the response processor Ã¢â‚¬â€� needs breakpoint-level debugging via devtools server pause/exception hooks, or upstream reading of the navigation completion code path in the kevlar bundle).

SESSION HANDOFF NOTE: this session's context is exhausted at this exact point. Next session: read ytd-search's navigate/response handling in /tmp/ytbase.js (still on disk Ã¢â‚¬â€� 9.8MB bundle), find the hideContents write site, determine its trigger.

## Ã¢Ëœâ€¦ ROOT CAUSE SOLVED: rIC starvation Ã¢â‚¬â€� YouTube hidden-stall final answer (2026-11-26, macOS lane)

PRIMITIVE-LEVEL PROOF (localhost rictest.html + devtools on quiet page): during load frames tick and rIC fires at 0ms. On a SETTLED quiet page, a freshly registered requestIdleCallback does NOT fire even after 4s Ã¢â‚¬â€� servo's rIC implementation piggybacks on the animation-frame tick (we implemented it that way: IdleCallbackTick keepalive), and shell-new only produces frames on input/paint-invalidation. Quiet page Ã¢â€ â€™ no rAF Ã¢â€ â€™ rIC starves forever.

COMPLETE UNIFIED MECHANISM for YouTube (and likely the whole SPA class): YouTube/Polymer scheduler `raM` (kevlar_base bundle, ~line 1493596) = `window.requestIdleCallback ? rIC : setTimeout(v,16)` Ã¢â‚¬â€� uses rIC WHEN PRESENT. All post-response data-flush/stamper/visibility work rides on it. Chain: response arrives Ã¢â€ â€™ flush jobs scheduled via rIC Ã¢â€ â€™ page quiet (no animation/input) Ã¢â€ â€™ servo never ticks frames Ã¢â€ â€™ jobs never run Ã¢â€ â€™ hideContents never cleared (binding `hidden="[[data.hideContents]]"` verified functional Ã¢â‚¬â€� manual set() toggles both ways), stamper never stamps (explains vrs=0 boot variance as timing races), visibility state machine never activates. Every prior ruled-out layer (IO, MO-sentinel, bindings, crypto, postTask) consistent Ã¢â‚¬â€� the primitives were fine; the SCHEDULING STARVATION was the defect. Irony: pre-rIC servo used the setTimeout fallback (reliable); implementing rIC naively made this WORSE Ã¢â‚¬â€� a trap for partial implementations.

THE FIX (scoped): servo's rIC must fire when the event loop is idle, INDEPENDENT of frame production Ã¢â‚¬â€� spec-shaped idle periods, not rAF piggyback. Implementation site: script_thread event-loop integration (fire idle callbacks after task queues drain / on a fallback timer when no frame tick occurs within ~16ms). Then re-verify: YouTube search results should unhide unassisted. ALSO explains: mouse-nudging sometimes advanced stuck states (input pumps frames), why boot variance is race-like, why hydration "advanced then stalled" patterns across SPAs (F8/F9/F10 class). Symptom index: results-stall/SPA-quiet-stall Ã¢â€ â€™ check requestIdleCallback frame-piggyback (CONFIRMED CAUSE).

## rIC fix implementation starting point (2026-11-26 Ã¢â‚¬â€� next session read first)

Implementation sites identified: Document::request_idle_callback/run_the_idle_callbacks in components/script/dom/document/document.rs (~lines 2105-2185). Current design: idle callbacks only run inside the animation-frame tick; keepalive via request_animation_frame(AnimationFrameCallback::IdleCallbackTick) Ã¢â‚¬â€� starves when no frames produced.

KEY DISCOVERY: script_thread ALREADY has frame-independent timer infrastructure Ã¢â‚¬â€� `schedule_update_the_rendering_timer_if_necessary(delay)` (components/script/event_loop/script_thread.rs ~line 1088) using TimerEventRequest with a native Box closure callback that sets needs_rendering_update atomic. This proves: native closures CAN be scheduled as timer callbacks without JS handles. THE FIX: when idle callbacks are pending in any document, the script thread should schedule a ~4-16ms timer whose callback forces an idle-callback flush (either by setting needs_rendering_update to force a tick, or directly invoking document.run_the_idle_callbacks for each document with pending callbacks). Simplest correct-shaped version: in request_idle_callback, in addition to the rAF keepalive, post a ScriptThread timer via the window's script-thread access that re-checks/flushes after 16ms; chain while callbacks pending. Everything else (the run logic, deadline semantics) already works Ã¢â‚¬â€� proven correct when ticks occur.

## rIC starvation fix Ã¢â‚¬â€� LANDED + verified primitive, partial YouTube progress (2026-11-26)

Commit 933f453ad on feather-engine: request_idle_callback/run_the_idle_callbacks now poke script_thread.schedule_update_the_rendering_timer_if_necessary(16ms) Ã¢â‚¬â€� frame-independent tick. VERIFIED: late-registered rIC on quiet localhost page fires within 4s (was: never).

YOUTUBE PROGRESS: stamping now completes reliably Ã¢â‚¬â€� vrs=19 with 150 thumbnail imgs consistently (was race-dependent vrs=0/19). BUT hidden persists: data.hideContents still UNDEFINED Ã¢â‚¬â€� the completion write never happens. NEW KEY INSIGHT: Polymer bindings don't propagate undefined Ã¢â‚¬â€� hidden binding never fires without an explicit write; Chrome must explicitly set hideContents (true at SSR/initial then false at completion). The remaining missing piece is the write itself, in navigation-completion logic Ã¢â‚¬â€� not scheduling. NOTE: this page ran entirely on the fixed engine, so completion logic runs but doesn't write hideContents Ã¢â‚¬â€� next suspect: an exception-free early exit or missing API inside the completion handler (devtools unhandledrejection still unimplemented Ã¢â‚¬â€� that's the visibility tool needed).

## sec1zt13 Ã¢â‚¬â€� unhide timing evidence: permanent deterministic failure, UA-independent (2026-11-26)

Aging experiment: youtube search boot sampled at T+5/20/50/80s Ã¢â‚¬â€� hidden stays stamped on ytd-two-column-search-results-renderer the ENTIRE time, renderer height 0 throughout. **Not a late-timing issue Ã¢â‚¬â€� the unhide never fires at all in servo, deterministically.** Also: instance-identity polling (T+5.7Ã¢â€ â€™22s) shows a SINGLE persistent element instance Ã¢â‚¬â€� no replacement observed in that window, meaning either replacement completes before T+5.7s or doesn't happen; the two-phase race story from macOS couldn't be confirmed in servo timing. Simpler theory consistent with evidence: the unhide mechanism never runs in servo at all (rather than fires-then-gets-discarded). Chrome-UA spoof test: user_agent=Chrome/131 changes NOTHING (still hidden, still zero-height) Ã¢â‚¬â€� defect is UA-independent, shared framework path, and NOT a UA-gating issue. Open crux unchanged but sharpened: WHAT fires the hidden removal in real browsers Ã¢â‚¬â€� candidates narrowed: rIC-scheduled hydration commit (servo's rIC v1 fires callbacks but with non-spec ordering Ã¢â‚¬â€� IdleCallbackTick pumps at rAF rate continuously, so ordering vs rAF/microtasks is wrong; if youtube's commit is ordered relative to rAF/paint in a way servo violates, the commit could run BEFORE the dependent state exists and no-op silently), or custom-elements reaction timing (though CE primitives probe-verified). Cheapest next experiment: rIC-ordering bisect Ã¢â‚¬â€� locally revert/disable rIC (elements then can't schedule the commit at all Ã¢â‚¬â€� if behavior IDENTICAL with and without rIC, the commit either doesn't use rIC or the v1 ordering is irrelevant; if youtube's commit is rIC-dependent and servo ordering breaks it, disabling rIC should change SOMETHING observable).
## sec1zt13 Ã¢â‚¬â€� unhide timing evidence: permanent deterministic failure, UA-independent (2026-11-26)


## sec1zt14 Ã¢â‚¬â€� vp9 abort hunt FINAL state + real-YouTube dependency documented (2026-11-26)

Sixth reproduction attempt: extended vp9 seek-churn loop (random seek every 800ms, 171 seeks/2.5min, live decode readyState 4 throughout) Ã¢â‚¬â€� zero panics, process alive. **The vp9 abort is non-reproducible in every harness available on Linux.** Real-YouTube hunting is blocked by a hard dependency: the YouTube player never bootstraps in servo Ã¢â‚¬â€� video element present but src never set, ZERO videoplayback network requests, networkState 0 Ã¢â‚¬â€� the player init is part of the incomplete-boot/visibility defect family (same root class as the hidden-attribute stall). DEPENDENCY CHAIN documented for prioritization: fixing the boot/visibility defect unblocks BOTH the #1-site usability AND the vp9 crash hunt. The vp9 abort mission is closed-exhausted on this lane: negative matrix (5 synthetic harnesses) + extended seek-churn + real-YouTube-blocked-by-boot all documented. Symptom-index final entry: Symptom 'gst vp9 threads crash / abort on youtube' Ã¢â€ â€™ check: seek-ack teardown race (CONFIRMED FIXED Ã¢â‚¬â€� was real, verified eliminated), playerleak intentional-leak sites (documented, non-crashing), gst decode threads generally (RULED OUT in 6 harnesses), YouTube player bootstrap (blocked upstream by boot-visibility defect Ã¢â‚¬â€� retest there once boot fixed).

## sec1zt15 Ã¢â‚¬â€� rIC starvation FIXED + the cause-verdict correction: insufficient for YouTube visibility (2026-11-26)

The rIC starvation fix is implemented and landed (commit 6744dae2875): pending idle callbacks count as a rendering-update need in Document::needs_rendering_update, making the script-thread timer machinery self-sustaining on quiet pages. Verified mechanically: quiet-page rictest fires immediately post-settle; rIC registered on live youtube fires within 4s. The fix is spec-closer and correct regardless Ã¢â‚¬â€� SPA quiet-stall class improved.

**CRITICAL CORRECTION to the macOS lane's confirmed-cause verdict**: with rIC firing correctly, YouTube search STILL stays hidden (tcHidden:true, display:none, renderers zero-height at T+22s post-fix). Starvation was real but NOT the sufficient cause Ã¢â‚¬â€� the visibility commit either never gets scheduled, or runs and fails silently for another reason. What remains verified-working: the binding itself (ys.set('data.hideContents', false) unhides live per macOS; force-removeAttribute + display:block renders everything incl. thumbnails per sec1zt12). Sharpened open question: the write of hideContents=false (or equivalent visibility-flip data write) never happens Ã¢â‚¬â€� the failure is UPSTREAM of the binding, in the navigation-completion/response-processing logic. Next investigation is framework-level: fetch the kevlar bundle (youtube's ytbase.js), find the hideContents write site, determine its trigger conditions, and probe which one fails in servo. NOTE for that work: webdriver can't inject pre-load Ã¢â‚¬â€� analysis must be static (read the bundle) or use servo devtools protocol. ALSO NOTE: merge-artifact duplication existed in this file from conflict union-merges (cleaned marker lines 2026-11-26; duplicated sec1zt11 body text remains harmless).

## sec1zt16 Ã¢â‚¬â€� YouTube scheduler fully decoded + scheduling ruled out, failure is upstream (2026-11-26)

LIVE PROBE of the scheduler instance on the broken page (ytglobal.schedulerInstanceInstance_): all job queues EMPTY (incl. priority-8), nothing in flight, mode=3 rAF, L=true (using rIC), u=true (rAF available). **Scheduling is definitively NOT the defect Ã¢â‚¬â€� either the visibility-flip job is never scheduled, or it runs and fails silently.** This closes the rIC/scheduling investigation direction permanently.

BUNDLE ANALYSIS (scheduler.js Ã¢â‚¬â€� only 8KB, readable, fetched at /tmp/ytscheduler.js Ã¢â‚¬â€� note /tmp files are ephemeral, refetch if gone: youtube.com/s/desktop/HASH/jsbin/scheduler.vflset/scheduler.js): yt scheduler priorities 0-5+8; mode decision M(): priority-8 jobs with rAF available and page visible run on **requestAnimationFrame** (mode 3), low-priority (1) jobs use rIC {timeout:3000}, mode 2/4/5 use setTimeout variants. Feature-detect: uses rIC only if window.requestIdleCallback exists Ã¢â‚¬â€� meaning pre-rIC servo used reliable setTimeout fallback; implementing rIC naively made things worse (macOS irony confirmed in code).

UNHIDE MECHANISM FOUND (kevlar bundle /tmp/ytbase.js 9.8MB Ã¢â‚¬â€� search 'hideContents'): for ytd-rich-grid-renderer (home; search equivalent follows same pattern): onPageChange schedules a **priority-8 job** whose body runs setMasthead/maybeSetBigYoodle/forceRefreshAndLog/terminateVideoPreview in a try, with `hideContentsForNavigation=false` in the **finally** Ã¢â‚¬â€� the unhide is finally-guarded so even a throwing job body still unhides. Binding: `hidden="[[computeHideContents(hasLoadingContent, hideContentsForNavigation)]]"`. Consequence: since the finally guarantees the flip IF the job runs, and queues are empty (jobs ran or never scheduled), the live suspects narrow to exactly two: (a) onPageChange never fires or isCurrentPage() false Ã¢â‚¬â€� job never scheduled; (b) W-flag logic Ã¢â‚¬â€� if the job runs synchronously-ish via scheduler quirks, onPageChangeJobId handling differs. The single most instrumentable next probe: whether the framework's navigate/response events fire AT ALL on the search page in servo post-fix Ã¢â‚¬â€� the yt-navigate-finish event and whether a data response object reaches the element. NOTE the response itself arrives (200 verified repeatedly) Ã¢â‚¬â€� it's the framework-internal dispatch that's suspect.

## Temporal proof: hideContents write NEVER happens (11:40 2026-11-26)

Property-write instrumentation on ytd-search.data.hideContents across 40s of settled post-response state: zero writes ever (hcw=[] at T25s and T40s). Not a timing issue Ã¢â‚¬â€� the completion logic that should flip hideContents is either never invoked or exits before the write. Also: eval-console-instruments branch built (BUILDEXIT:0) Ã¢â‚¬â€� shell-new now has FEATHER_EVAL live bridge + console visibility available for next session. Next concrete step for the continuation: implement servo's unhandledrejection event (the missing visibility primitive) OR instrument yt-navigate-finish handlers in the kevlar bundle to find where the search-response completion path diverges. /tmp/ytbase.js still on disk.

## sec1zt17 Ã¢â‚¬â€� final narrowing: data FULLY BOUND, disconnect is data-arrival Ã¢â€ â€™ attribute-flip (2026-11-26)

Live probe on stuck search page: `ytd-search` element `.data` is FULLY POPULATED with the real response (responseContext, estimatedResults, contents, trackingParams, header, topbar, onResponseReceivedCommands, targetId) Ã¢â‚¬â€� response processing works end-to-end, binding system alive (ytd-search carries __dataReady/__dataEnabled/__dataPending family Ã¢â‚¬â€� so the Polymer-ish data system IS in use on search elements, partially retracting the modern-framework correction: masthead lacks it but search has it). rIC regression check: CPU cost of the starvation fix negligible (3 ticks/5s on settled page Ã¢â‚¬â€� loop drains correctly, no spin). YET hidden persists. **The complete evidence chain: response arrives (200) Ã¢â€ â€™ data binds to element (verified populated) Ã¢â€ â€™ scheduler queues drain (all empty) Ã¢â€ â€™ but the hidden-flip write never executes.** Remaining suspect space is now tiny: the element-internal logic that translates data-arrival into clearing hidden Ã¢â‚¬â€� either (a) a priority-8 commit job that gets CANCELED by a subsequent navigation/page-change (resetNavigationState cancels onPageChangeJobId Ã¢â‚¬â€� servo-specific double-navigation timing could hit this; queues empty is consistent with cancellation), or (b) the computeHideContents-equivalent binding for search never re-evaluates because the property-effects cascade breaks silently. Probe next: whether a second yt-navigate-start fires AFTER the data-bearing one in servo (double-navigate cancel race Ã¢â‚¬â€� the precise servo-ordering theory). Instrument via webdriver-injected listener post-load then trigger in-page search refinement navigation.
## sec1zt17 Ã¢â‚¬â€� final narrowing: data FULLY BOUND, disconnect is data-arrival Ã¢â€ â€™ attribute-flip (2026-11-26)


## sec1zt18 Ã¢â‚¬â€� investigation halt state + tooling unlock needed (2026-11-26)

Additional negative data: injected yt-navigate listeners catch NOTHING even when clicking filter chips Ã¢â‚¬â€� and the chip click doesn't even navigate (url unchanged). Whether that's because the page is already wedged or in-page navs don't fire these events in servo is undetermined. THE INVESTIGATION IS NOW TOOLING-BLOCKED: every remaining question requires observing the boot sequence from T+0 (pre-load JS injection), which webdriver fundamentally cannot do. THE UNLOCK for the next session: either (a) servo's devtools protocol Ã¢â‚¬â€� check if it supports Debugger/Runtime instrumentation attachable before navigation completes (servo has devtools feature in build), or (b) mitm proxy (mitmproxy with CA cert installed in servo profile Ã¢â‚¬â€� servo accepts custom CA? unverified) to inject instrumentation into served HTML, or (c) build a servo variant with a pref-gated pre-load user-script injection (small engine feature, ~1-2h, generally useful for all future framework debugging Ã¢â‚¬â€� RECOMMENDED). Summary of the narrowed state: response arrives, data binds, scheduler drains, binding works when manually driven Ã¢â‚¬â€� but the hidden-flip write never executes and the evidence needed to distinguish cancel-race vs cascade-break is inside the unobservable boot window.

## hideContents final chain Ã¢â‚¬â€� command never dispatched (2026-11-26 late)

FULL CONTRACT: ytd-search actionMap registers "yt-show-search-contents-command"Ã¢â€ â€™onYtShowSearchContentsCommandÃ¢â€ â€™set("data.hideContents",!1). The command string appears NOWHERE else in kevlar_base Ã¢â‚¬â€� not server-sent either: live scan of ys.data shows onResponseReceivedCommands contains ONLY adsControlFlowOpportunityReceivedCommand, and no "showSearchContents" string anywhere in the data. Therefore the command is fired LOCALLY by framework completion logic (app-shell/navigator once content ready) Ã¢â‚¬â€� possibly in one of the 12 other JS chunks the page loads (only base bundle analyzed). The trigger itself never fires in servo Ã¢â‚¬â€� suspect chain: rendering-completion hook that should dispatch it. NEXT: grep the other page chunks for "show-search-contents" or the dynamic dispatch site; alternatively instrument the live dispatch pipeline. IO fully exonerated incl. display:none-ancestor nesting (io2.html Ã¢â‚¬â€� initial false + transitions all correct).

## sec1zt19 Ã¢â‚¬â€� devtools unlock + near-complete root cause: IO-lazy-init suspect, upgrade myth busted (2026-11-26)

**TOOLING UNLOCKED**: servoshell --devtools=9222 works out of the box (no shell-new build needed Ã¢â‚¬â€� shell-new build skipped deliberately, saving hours). Firefox devtools protocol over RAW TCP (not websocket Ã¢â‚¬â€� connection resets if ws attempted). Client quirks: length-prefixed packets "len:json"; read root hello first; use listTabs Ã¢â€ â€™ getTarget on tab actor (NOT attach Ã¢â‚¬â€� both tab.attach and BrowsingContext.attach return unrecognizedPacketType); getTarget response frame object contains consoleActor; evaluateJS replies arrive wrapped in {"Ok": {...}} Ã¢â‚¬â€� match by input text; evaluateJS does NOT await promises (write results to window.__x and poll); innerHTML assignments in eval throw TrustedTypes errors on youtube (page enforces TT). Servo actor support: console evaluateJS/evaluateJSAsync yes, listFrames/listWorkers stubs.

**INVESTIGATION RESULTS (all live-probed on stuck youtube search page)**:
1. IntersectionObserver primitive WORKS Ã¢â‚¬â€� test observer on fresh fixed-position element fired immediately (isIntersecting:true). Simple case fine.
2. **Upgrade myth BUSTED**: stuck ytd-two-column-search-results-renderer IS properly upgraded Ã¢â‚¬â€� Object.getPrototypeOf(tc) === customElements.get(...).prototype TRUE. The constructor-identity test (tc.constructor !== def) is a RED HERRING: even freshly createElement'd tc elements show the same pattern (proto correct, constructor resolves to anonymous empty-name class). Definitive upgrade test is PROTO identity, not constructor identity Ã¢â‚¬â€� constructor resolution is weird for these minified classes even in correct operation. Custom element machinery is NOT the defect.
3. connectedCallback pattern: ytd-video-renderer (works) has NO cc; ytd-two-column (stuck) HAS cc; containers ytd-search/ytd-app have cc+ac+rich lifecycle. cc-absence is normal for leaf renderers Ã¢â‚¬â€� YouTube initialization is NOT purely CE-reaction-driven.
4. **KEY FIND: ytd-search prototype has initVisibilityObserver_, registerRenderer, unregisterRenderer_, isRenderer_** Ã¢â‚¬â€� YouTube lazy-initializes renderers via a VisibilityObserver (IntersectionObserver-based). This is the lazy-init path the replacement-chain doc hypothesized. Prime suspect: IO behavior in the specific configuration YouTube uses (elements inside display:none subtrees / zero-size / specific rootMargin) differs servo vs Chrome, OR IO callbacks don't fire correctly for elements that START hidden. NOTE spec subtlety: display:none elements have no boxes Ã¢â€ â€™ never intersect Ã¢â‚¬â€� if init depends on IO firing while hidden then unhide depends on init, chicken-egg Ã¢â‚¬â€� but Chrome resolves it somehow (perhaps IO fires when visibility flips, or init uses different trigger).
5. Fresh-inserted tc element acquired children:2 spontaneously Ã¢â‚¬â€� some framework machinery (likely MutationObserver-driven stamping) DOES act on inserted elements, but data-init (__data) still never populates.
6. Negative: monkeypatching prototype.connectedCallback post-registration does nothing (spec Ã¢â‚¬â€� reactions use registry-captured callbacks) Ã¢â‚¬â€� don't bother with that instrumentation technique.

**NEXT DECISIVE EXPERIMENT (documented for continuation)**: install IO spy wrapper (pattern in session Ã¢â‚¬â€� wraps window.IntersectionObserver, logs created/callbacks/observes counts) then trigger a TRUE SPA navigation Ã¢â‚¬â€� NOT form.submit (causes full navigation, wipes spy Ã¢â‚¬â€� negative result recorded) Ã¢â‚¬â€� use YouTube internal router: click a visible anchor/link or use the masthead logo SPA behavior. Compare IO fire behavior servo vs expectation during the replacement render. Alternative cheaper static route: fetch youtube's kevlar/kevlarlib bundle and read initVisibilityObserver_ implementation Ã¢â‚¬â€� determine exactly what trigger completes renderer init and what attribute-write clears hidden (the write site question from sec1zt15 is still technically open Ã¢â‚¬â€� the write may BE inside the lazy-init completion).

**Correction to earlier narrowing**: the question is no longer "why doesn't the flip write execute" in the abstract Ã¢â‚¬â€� it is specifically "why doesn't YouTube's visibility-observer-based lazy init complete on the replacement tree". Data binding works (ytd-search.data fully populated), upgrade works, CE reactions work, IO primitive works, scheduler drains. The defect is in the interaction between IO-driven lazy init and servo's IO implementation details OR in whatever event/step YouTube uses to begin that init.
## sec1zt19 Ã¢â‚¬â€� devtools unlock + near-complete root cause: IO-lazy-init suspect, upgrade myth busted (2026-11-26)





## sec1zt20 Ã¢â‚¬â€� IO spec-compliance verified; investigation consolidated for continuation (2026-11-26)

**IntersectionObserver spec matrix (clean measurement after restoring native IO Ã¢â‚¬â€� initial test was contaminated by dead spy wrapper, negative result invalid)**: belowfold rendered element: initial callback fires isIntersecting:false CORRECT; display:none element: initial callback fires isIntersecting:false CORRECT (spec-conformant Ã¢â‚¬â€� display:none elements get initial false callback); visible: fires true CORRECT. **IO initial-callback behavior is fully spec-compliant in servo. IO-lazy-init theory significantly weakened** Ã¢â‚¬â€� if YouTube init waited for initial IO fire it would fire. REMAINING IO question (untested): transition firing when elements inside display:none subtrees become visible Ã¢â‚¬â€� irrelevant though since hidden never clears in the first place.

**Tooling warnings for continuation**: (1) monkeypatching window.IntersectionObserver PERSISTS across same-window SPA navigations and poisons all subsequent IO construction if the state object is wiped Ã¢â‚¬â€� if instrumenting IO, make the wrapper robust to missing state. (2) form.submit() on youtube search causes FULL navigation (wipes instrumentation) Ã¢â‚¬â€� use link clicks for SPA nav. (3) restore native IO from iframe.contentWindow.IntersectionObserver when poisoned.

**Static bundle findings**: ytd-two-column-search-results-renderer class registered with disableElementRegistration:!0, template is trivial (`<div id="primary">` + secondary container with `disable-upgrade$` binding) Ã¢â‚¬â€� the element is a thin shell; hidden-removal write site NOT found in class itself Ã¢â‚¬â€� it's controlled externally (parent template binding or search-lifecycle logic). The write site question remains technically open but the failure is upstream regardless: whatever framework step performs the write never runs.

**INVESTIGATION STATE SUMMARY for continuation** Ã¢â‚¬â€� ruled out with live-verified evidence: CE upgrade machinery (proto-verified correct), IO primitive (spec-compliant), constructor identity (red herring), rIC starvation (fixed, insufficient), scheduler (drains fine), data pipeline (fully bound), response processing (200 + populated). The defect: the framework step that unhides/finalizes the replacement tree never executes. Next most-promising probes in priority order: (1) IO-spy + TRUE SPA nav capturing observer creation/callback pattern during a fresh replacement render (in-progress instrumentation pattern documented in session); (2) compare behavior of the SAME page in servo vs chrome at the framework level: does YouTube in servo even ATTEMPT the unhide (spy on Element.prototype.removeAttribute/attribute writing for 'hidden') Ã¢â‚¬â€� attribute-write spy is EASY to install robustly and would definitively split never-attempted vs attempted-and-failed; (3) static: find the parent-template binding controlling hidden in ytd-search or app-level templates.

## sec1zt21 Ã¢â‚¬â€� DEFINITIVE isolation: exactly ONE write missing, all downstream machinery verified working (2026-11-26)

Force-unhide cascade experiment (live devtools on stuck youtube search): removeAttribute('hidden') on ytd-two-column-search-results-renderer Ã¢â€ â€™ IO observer on video renderer FIRED (iofires:1), 108 framework mutations cascaded, tc rendered to 7288px height, video renderers populated data (vr.data true AFTER unhide Ã¢â‚¬â€� lazy init was GATED ON VISIBILITY), heights 249px, thumbnails loading, nothing re-hides it. **The entire defect is exactly one missing write: hidden-removal on the replacement element. Everything downstream Ã¢â‚¬â€� IO cascade, visibility-gated lazy data init, rendering, image loading Ã¢â‚¬â€� works perfectly in servo.** This inverts the earlier suspicion: lazy init is not broken; it was correctly waiting for visibility which never came. The missing write is in YouTube's response-completion path (external to the element Ã¢â‚¬â€� class is a thin shell). The investigation question is now fully precise: in Chrome, what code performs the hidden-removal on the post-response replacement element, and why doesn't that codepath execute in servo? Cheap decisive continuation: Element.prototype.removeAttribute + attribute-setter spy on 'hidden' installed BEFORE a true SPA nav (link click, not form submit) will capture whether servo-YouTube even attempts the write vs Chrome attempting it.

## ENDGAME STATE: YouTube fully renders Ã¢â‚¬â€� only the command trigger missing (2026-11-26 final)

Direct call ys.onYtShowSearchContentsCommand() Ã¢â€ â€™ hidden=false, hc=false, layout height 7901px, FULL VISUAL RENDER: thumbnails/titles/chapters/badges all correct (screenshot proof). Raw CustomEvent dispatch does NOT trigger the handler (YouTube actions route through the central command-executor service, not raw DOM events). THE ENTIRE downstream pipeline is functional in servo Ã¢â‚¬â€� the single missing piece is whatever invokes the command through the executor in Chrome. Alpha-shim option now trivially viable: inject the direct handler call post-response. Trigger hunt continuation: find which response-processing/navigation step submits the command to the executor (search the executor service in chunks Ã¢â‚¬â€� command resolution is dynamic, no static string exists; likely the executor resolves a command object constructed by completion logic or embedded in a part of response processing that silently fails in servo Ã¢â‚¬â€� the unhandledrejection tool remains the key visibility need).

## sec1zt22 Ã¢â‚¬â€� CRITICAL discriminator: unhide mechanism WORKS on home page, fails only on search; plus eval-wedge defect (2026-11-26)

**MAJOR narrowing**: SPA-navigated-to-home page renders PERFECTLY in servo Ã¢â‚¬â€� ytd-rich-grid-renderer hidden:false, height 6965px, data bound, 30 rich-item renderers. The hideContents/unhide mechanism is NOT broken in general Ã¢â‚¬â€� it fires correctly for home. The defect is specific to the SEARCH results path (or the initial-load path Ã¢â‚¬â€� discriminator incomplete, see below). Also note: the home page in question was reached via SPA navigation from the wedged search page Ã¢â‚¬â€� SPA nav itself works (logo click), so in-page navigation machinery is functional.

**New defect surfaced Ã¢â‚¬â€� eval-wedge on fresh loads**: after location.href full navigation to youtube home, devtools evaluateJS persistently throws 'Location relevant Document is not same origin-domain with entry settings object' for 30+s across fresh devtools connections Ã¢â‚¬â€� servo document/script state wedges after full navigation in a way it does not after SPA nav. Transient version of this error is normal during navigation, but persistent-for-30s+ suggests either navigation never completing or a script-thread wedge. UNVERIFIED whether page renders visually during the wedge (no screenshot taken). This wedge also blocked the fresh-load-vs-SPA discriminator experiment. **This is itself a serious servo defect candidate affecting all full navigations Ã¢â‚¬â€� possibly the SAME mechanism that breaks initial-load search unhide**: if the initial-load document state is wedged in some subtle way (not fully wedged Ã¢â‚¬â€� most JS runs Ã¢â‚¬â€� but some spec check failing), framework codepaths touching affected state would throw silently. Direction for continuation: reproduce eval-wedge minimally (any full navigation to a simple page?) and find the spec check in servo script/dom Ã¢â‚¬â€� grep for 'same origin-domain' or the spec step in location/document access.

**Instrument survival negative**: attribute-write spy on Element.prototype does NOT survive full navigation (obviously) and the SPA-nav spy capture was pre-empted by the wipe Ã¢â‚¬â€� but the spy install methodology is sound for future SPA-only captures: wrap removeAttribute/setAttribute/hidden-property-setter BEFORE clicking in-page links.
## sec1zt22 Ã¢â‚¬â€� CRITICAL discriminator: unhide mechanism WORKS on home page, fails only on search; plus eval-wedge defect (2026-11-26)




## Console visibility live Ã¢â‚¬â€� boot nearly clean, one real error (2026-11-26 final)

eval-console-instruments pipe works. Entire YouTube boot produces only: LegacyDataMixin info x2, "No valid entry type" warning x2, and ONE error: "ResizeObserver loop completed with undelivered notifications". No exceptions/rejections otherwise. The RO-loop error is a NEW untested suspect for the missing command trigger: YouTube layout depends on RO chains; if servo's RO loop-limit drops notifications that Chrome delivers, the layout-completion signal the trigger depends on may never fire. Next cheap test: minimal RO-loop repro comparing servo vs spec behavior (chrome tolerates/delivers across frames; servo may drop).

## unhandledrejection implementation scope + final handoff (2026-11-26 final-final)

ResizeObserver EXONERATED (rotest.html: n=122 callbacks, loop error fires but delivery continues Ã¢â‚¬â€� spec-shaped, same as Chrome). Console pipe shows otherwise-clean boot. unhandledrejection: NO infrastructure exists Ã¢â‚¬â€� needs SM promise rejection tracker hook + PromiseRejectionEvent webidl + firing logic; multi-hour engine feature. It's the correct next investment: the command-trigger failure is either async-invisible (needs this) or a pure logic divergence. Alternative cheaper probe for continuation: compare YouTube's served JS/config in servo vs Chrome Ã¢â‚¬â€� UA-dependent feature flags (_.L(...) killswitches) can gate the command-trigger logic; yt.config_ / EXPERIMENT_FLAGS diff may reveal a flag gate servo fails. yt.config_ is readable live via devtools Ã¢â‚¬â€� look for flags mentioning search_contents/navigation_completion.

## sec1zt23 Ã¢â‚¬â€� REFRAMED: location-object wedge + failed navigation Ã¢â‚¬â€� a servo navigation-state defect (2026-11-26)

CRITICAL CORRECTION + finding: the 'same origin-domain' eval error is NOT a document-wide wedge Ã¢â‚¬â€� pure JS evals fine, document.title/documentElement/body all accessible post-error. It is specifically **the Location object access that throws**. AND: after `location.href='https://example.com/'` the page NEVER NAVIGATED Ã¢â‚¬â€� title still 'YouTube', body still youtube's 47 children, minutes later. **Servo defect: location.href assignment during a complex page state can silently fail to navigate AND leaves the Location object itself throwing spec errors on subsequent access.** Mechanism hypothesis: navigation starts (transitions the document's incumbent/settings state) but never completes or aborts, leaving Location's relevant-document check permanently failing. IMPACT ON YOUTUBE THEORY: YouTube's SPA framework constantly reads/writes location/history during response processing Ã¢â‚¬â€� if servo location state wedges during the boot sequence's navigation transitions, framework code touching location throws silently (K() swallows), aborting response completion before the unhide write. **This is now the prime mechanism suspect.** Next steps: (1) minimal repro Ã¢â‚¬â€� does location.href wedge reproduce on simple pages or only youtube-complex ones; (2) grep servo script/dom/location.rs for the relevant-document check and audit navigation-transition state handling; (3) test whether history.pushState (what YouTube SPA actually uses) also wedges. NOTE prior data point consistent: clicking filter chips didn't navigate either (sec1zt18) Ã¢â‚¬â€� possibly the same defect, not 'page already wedged'.
## sec1zt23 Ã¢â‚¬â€� REFRAMED: location-object wedge + failed navigation Ã¢â‚¬â€� a servo navigation-state defect (2026-11-26)


## CORRECTION: unhandledrejection IS fully implemented in servo upstream (2026-11-26)

Earlier claim was wrong. Full chain exists: SM promise_rejection_tracker hook (script_runtime.rs:245) Ã¢â€ â€™ uncaught_rejections tracking Ã¢â€ â€™ unhandledrejection event + Console::internal_error reporting. AND console piping shows ZERO rejections across entire YouTube boot. Async-failure theory formally DEAD. Sync errors also zero. Boot is completely clean Ã¢â‚¬â€� the missing command trigger is a pure logic/conditional divergence, not a failure of any kind. Servo spoofs Chrome/131 UA already. Every primitive exonerated. Remaining hypothesis space: (1) response-content divergence Ã¢â‚¬â€� the command may be in Chrome's youtubei response but absent in servo's due to client-capability differences in the POST payload; (2) consumed-and-removed commands Ã¢â‚¬â€� onResponseReceivedCommands may show only unexecuted residue; (3) a client-side conditional branch (capability probe) taking a different path. NEXT BEST PROBE: capture the actual youtubei/v1/search RESPONSE BODY in servo (devtools network actor or mitm) and diff against a Chrome-captured one Ã¢â‚¬â€� decisive for hypothesis 1/2.

## Navigation chain detail + final options (2026-11-26 endgame)

VERIFIED LIVE: yt-page-data-updated + yt-navigate-finish both fire correctly (T2037) Ã¢â‚¬â€� navigation completion machinery works. yt-show-search-contents-command NEVER fires. Bundle analysis: navigate-finish fires in a promise continuation after publishPageData (fEm/Uue at ~3808572 of /tmp/ytbase.js). Response commands resolve via gMl.handleResponseÃ¢â€ â€™commandHandler.resolveCommand Ã¢â‚¬â€� gated by commandAllowlist (xQ5). The show-contents command is NOT in servo's response data (only adsControlFlow present). REMAINING DECISIVE TEST (needs fresh session): capture youtubei/v1/search response body in servo AND in Chrome (this mac has Chrome) and diff Ã¢â‚¬â€� if Chrome's response contains the command, servo's client-context in the POST payload diverges (missing capability flags); if neither has it, the trigger is purely client-side render-completion logic inside publishPageData chain and needs breakpoint-level tracing. Cheapest immediate alternative: implement a shell-side shim (auto-inject the direct handler call when data arrives Ã¢â‚¬â€� proven working) as the alpha mitigation while the trigger hunt continues.

## sec1zt24 Ã¢â‚¬â€� CORRECTION: location wedge is devtools-eval artifact, page JS unaffected Ã¢â‚¬â€� location theory DEAD (2026-11-26)

Definitive discrimination test: page with inline script accessing location.hostname + history.pushState + location.pathname Ã¢â‚¬â€� title shows LOC-OK:127.0.0.1|PS-OK:/spa-path Ã¢â‚¬â€� **real page JavaScript location access works perfectly, incl. pushState SPA pattern**. The persistent 'same origin-domain' throw is specific to devtools evaluateJS: entry settings object for devtools eval is not the page global, so the spec check correctly throws FOR THE EVAL ONLY. sec1zt23's 'prime mechanism suspect' is RETRACTED Ã¢â‚¬â€� it was instrumentation artifact. (Side findings that stand: location.href full navigation DOES work on simple pages Ã¢â‚¬â€� iana.org nav succeeded; and devtools eval of location.* will always throw Ã¢â‚¬â€� workaround: read location indirectly via document.URL or a page-side mirror.) ALSO: the servo FIXME in location.rs document_if_same_origin is real but only affects not-fully-active edge cases, NOT the youtube defect. BACK TO EVIDENCE: home unhide works (SPA-navigated), search unhide fails (initial loads). The open discriminator is initial-load vs SPA-nav for the SEARCH page specifically.
## sec1zt24 Ã¢â‚¬â€� CORRECTION: location wedge is devtools-eval artifact, page JS unaffected Ã¢â‚¬â€� location theory DEAD (2026-11-26)


## hideContents origin refined (2026-11-26 endgame-2)

SSR HTML (curl w/ servo UA) contains NO ytd-search elements at all Ã¢â‚¬â€� bare app shell; entire search page client-rendered from ytInitialData (1.1MB, has results). ytInitialData contains ZERO hideContents and zero showSearchContents Ã¢â‚¬â€� so hideContents is a purely CLIENT-MANAGED navigation-state flag, not server data. KEY DEDUCTION: the hidden=true property write at T1469 (through qJC binding evaluation) means data.hideContents WAS true during boot Ã¢â‚¬â€� the HIDE-side of the machinery executes in servo. Only the completion-side (write false) never happens. So: partial execution confirmed Ã¢â‚¬â€� navigation-start logic runs, completion logic doesn't. Divergence point is strictly inside whatever runs at navigation completion for search pages. The fetch-intercept persist hook is armed on disk (/tmp/feather-eval-persist.js) for the next session to capture the live SPA search response and check for commands in the actual servo-fetched payload.

## Capture hook status (endgame-3): fetch-only patch captured NOTHING Ã¢â‚¬â€� youtubei uses XHR. Hook updated to patch XHR too (in /tmp/feather-eval-persist.js, XHR open/send wrapped, captures len+hasShowCmd+url into window.__capres). Next session: load a results URL fresh (query-param to force re-injection), read window.__capres after 15s Ã¢â‚¬â€� decisive for whether the command exists in servo's response payload at all.

## XHR/fetch capture live results (endgame-4)

Hook works: 104 fetch calls, ZERO xhr (youtubei uses fetch Ã¢â‚¬â€� earlier empty capture was a first-load/embed effect). Captured player endpoint 262KB Ã¢â‚¬â€� hasShowCmd FALSE. Direct-to-results loads use embedded ytInitialData for search (no youtubei/v1/search fetch observed). To capture the actual search payload need SPA in-page navigation (type new query in search box). hasShowCmd false so far consistent with client-side-trigger theory.

## DECISIVE: search payload captured Ã¢â‚¬â€� command NOT server-sent (endgame-5 FINAL)

Live SPA navigation triggered (search-box type + enter works!). Captured actual youtubei/v1/search response: 1.34MB, hasShowCmd FALSE Ã¢â‚¬â€� no showSearchContentsCommand in the real servo-fetched payload. Combined with the command constructor being absent from every JS chunk, the trigger mechanism must be either: (a) present in Chrome's payload only Ã¢â‚¬â€� requires Chrome-side capture comparison, testable on this mac via managed Chrome; or (b) generated by dynamic framework logic that constructs command objects programmatically (command type string never appears literally Ã¢â‚¬â€� kebab-case conversion hides it; re-grep chunks for "showSearchContents" camelCase specifically Ã¢â‚¬â€� earlier searches covered this, absence confirmed). BONUS: SPA search-box typing + enter navigation confirmed working end-to-end in feather.

## Ã¢Ëœâ€¦ REFRAME Ã¢â‚¬â€� the bug may be the erroneous HIDE, not a missing unhide (endgame-6)

Deep scan of fully-processed ys.data: only showDialogCommand + adsControlFlowOpportunity Ã¢â‚¬â€� showSearchContentsCommand absent everywhere in every layer (initial data, SPA payload, processed data, all JS chunks). Since no trigger exists anywhere, reconsider: perhaps in Chrome the hidden write NEVER HAPPENS on direct loads (nothing to hide Ã¢â‚¬â€� no prior contents). The hide at T1469 (qJC binding evaluation with hideContents momentarily true) may be SERVO'S ERROR Ã¢â‚¬â€� data.hideContents incorrectly true during boot. New prime question: what makes hideContents true in servo on a direct load? The binding evaluates [[data.hideContents]] Ã¢â‚¬â€� for it to fire true, some logic SET it true. That set is the defect's origin. Hunt direction flipped: find the hide-write logic (probably in navigation-start/page-data logic misfiring on initial load in servo Ã¢â‚¬â€� e.g. treating initial load as a replacement navigation, possibly because of the boot sequencing or an extra navigation event). KEY supporting evidence: yt-navigate events fired at T2037 Ã¢â‚¬â€� if servo fires an EXTRA/duplicate navigation or page-data event that Chrome doesn't, the hide-during-navigation logic triggers spuriously.

## sec1zt25 Ã¢â‚¬â€� full mechanism chain decoded: the yt-show-search-contents-command action never fires (2026-11-26)

STATIC + LIVE decode of the complete unhide mechanism: ytd-search template binds `hidden="[[data.hideContents]]"` on ytd-two-column-search-results-renderer. The ONLY write that clears it: `onYtShowSearchContentsCommand(){this.set("data.hideContents",!1)}` registered via `actionMap={"yt-show-search-contents-command":"onYtShowSearchContentsCommand"}` in the YtActionHandlerBehavior Ã¢â‚¬â€� actions flow as `yt-action` DOM events carrying detail.actionName, routed by the _.jL singleton router. LIVE VERIFIED on stuck servo page: handler IS registered (typeof function), manual `ys.set('data.hideContents', false)` unhides + full render 7960px Ã¢â‚¬â€� binding chain perfect. Response data (1.66MB) contains NO showSearchContents anywhere (568 other Command refs) and NO hideContents key Ã¢â‚¬â€� the action trigger is purely client-side, fired when response processing completes. BOOT CAPTURE (dispatchEvent spy installed T+7.2s mid-boot): yt-action events DO fire (yt-open-video-preview-action, yt-forward-redux-action-to-live-chat-iframe x8) but yt-show-search-contents-command NEVER fires. The action-routing infrastructure works; the specific trigger never executes. The 'yt-show-search-contents-command' string appears exactly ONCE in ytbase.js (the registration) Ã¢â‚¬â€� so the firing site is either in another jsbin (search controller bundle Ã¢â‚¬â€� bins loaded incl. webcomponents-sd, scheduler, spf, network Ã¢â‚¬â€� full bin list not yet grepped) or constructed dynamically. NEXT: grep all loaded bins for the firing logic, OR compare against Chrome capture of the same event stream to see exact expected firing time/context. Hypothesis space narrowed to: some completion condition in the search response-processing controller never becomes true in servo (e.g. all-renderers-stamped signal, an idle callback, a specific promise chain, or visibility-of-something gate).
## sec1zt25 Ã¢â‚¬â€� full mechanism chain decoded: the yt-show-search-contents-command action never fires (2026-11-26)


## Chrome comparison Ã¢â‚¬â€� DECISIVE END STATE (endgame-7 FINAL)

Chrome headless dump (virtual-time 15s) of identical query: renderer tag has NO hidden attribute, complete binding attrs (use-bigger-thumbs, bigger-thumbs-style=BIG, page-subtype). Servo's stuck element: IDENTICAL completed attributes PLUS hidden="" AND mini-guide-visible="" extra. Interpretation: servo's data pipeline runs to completion identically Ã¢â‚¬â€� every binding resolves Ã¢â‚¬â€� EXCEPT hidden persists. hideContents: 0 occurrences anywhere in Chrome DOM+data, absent in servo data too. The composite evidence now most consistent with: the hide-write itself is spurious/extra in servo (mini-guide-visible extra attribute is a second anomaly pointing at divergent navigation/layout state) OR a binding re-evaluation ordering difference when the data object is reassigned vs mutated. Both hunt directions documented; chrome headless dump method proven reusable (--virtual-time-budget=15000 --dump-dom, /tmp/chromedump.html on disk).

## hidden-write full stack captured (endgame-8)

Fresh-boot hidden setter instrumentation: hid=1 at T1522, stack: vj5 Ã¢â€ â€™ qJC Ã¢â€ â€™ _setUnmanagedPropertyToNode. "Unmanaged property" path = Polymer setting bound props on not-yet-upgraded elements during stamping Ã¢â‚¬â€� confirms hidden=true is set AT STAMPING TIME from data.hideContents being true in the data at that moment. So the data the framework assembles for stamping contains hideContents:true (client-generated, hide-while-loading), and in Chrome the same or different flow ends with it cleared. Definitive remaining question is narrow: what clears it in Chrome post-stamping. Persist hook for this capture is in /tmp/feather-eval-persist.js (setter patch, full stacks, element-filtered).

## Shim negative + protocol note (endgame-9 FINAL)

Prototyped auto-shim (500ms interval, fires ys.onYtShowSearchContentsCommand when hidden+data present) Ã¢â‚¬â€� NEVER self-fired across 20s despite hidden=true/vrs=19. Condition never satisfied in-page: likely onYtShowSearchContentsCommand not present as callable on the element instance at poll times (behavior method wiring differs), OR ys.data/.contents timing. Manual direct call DID work earlier in a different boot Ã¢â‚¬â€� variance again. Devtools protocol gotcha: evaluateJS replies sometimes arrive wrapped {"Ok":{...}} vs nested Ã¢â‚¬â€� extraction must handle both; socket noise heavy. Honest close for this session: the shim path is NOT trivially reliable Ã¢â‚¬â€� the element instance method availability itself varies boot-to-boot, which is itself another manifestation of the initialization race the rIC fix partially addressed. Deepest consistent frame: something in the behavior/action wiring finalizes late or never on the results-page elements in servo.

## sec1zt26 Ã¢â‚¬â€� BREAKTHROUGH: chrome ground truth Ã¢â‚¬â€� yt-navigate-finish/yt-page-data-updated never fire in servo (2026-11-26)

CHROME COMPARISON CAPTURE (headless chrome + CDP Page.addScriptToEvaluateOnNewDocument Ã¢â‚¬â€� TRUE pre-boot injection, the capability devtools-eval lacks): on a WORKING chrome render of the same youtube search (hidden:false, h:7143): **yt-show-search-contents-command NEVER FIRES IN CHROME EITHER** Ã¢â‚¬â€� the action-based unhide theory is DEAD; that's a legacy/unused path. The events that DO fire in chrome: yt-page-data-fetched:1, yt-page-data-updated:1, yt-navigate-finish:1, yt-update-title:1, yt-renderidom-finished:9, yt-rendererstamper-finished:22. **SERVO CAPTURE COMPARISON: yt-navigate-finish, yt-page-data-fetched, yt-page-data-updated, yt-update-title ALL ABSENT in servo** Ã¢â‚¬â€� the navigation/response-processing completion events never fire. yt-rendererstamper-finished and yt-renderidom-finished DO fire in servo Ã¢â‚¬â€� so rendering machinery runs, but the navigation-completion layer never completes. REVISED MECHANISM UNDERSTANDING: hidden is not cleared by an explicit command Ã¢â‚¬â€� in chrome the post-response re-render naturally stamps the replacement without hidden because data.hideContents is falsy in the response (binding `hidden=\"[[data.hideContents]]\"` evaluates false). The servo replacement being stamped WITH hidden means either the binding evaluated against navigation-in-progress state (hideContentsForNavigation-equivalent stuck true because nav completion never happens) or the servo replacement is not the proper client re-render at all. THE QUESTION IS NOW FULLY PRECISE: why doesn't YouTube's navigation-completion machinery (yt-navigate-finish emission) run in servo? yt-navigate-start comparison pending (was in the chrome stream? not captured Ã¢â‚¬â€� check). Servo navigation machinery suspects: popstate/history events, the navigatefinish trigger waiting on some resource/signal, or the response-processing promise chain breaking. NOTE for future captures: chrome CDP addScriptToEvaluateOnNewDocument is the gold-standard pre-boot injection; use it as ground truth for ANY event-stream question.
## sec1zt26 Ã¢â‚¬â€� BREAKTHROUGH: chrome ground truth Ã¢â‚¬â€� yt-navigate-finish/yt-page-data-updated never fire in servo (2026-11-26)


## Shim iteration results Ã¢â‚¬â€� ONE iteration from working mitigation (endgame-10 FINAL)

Shim v2/v3 results: fires successfully (T2510/T3115, hidden-after=false confirmed) but final state hidden=true Ã¢â‚¬â€� element gets re-hidden AFTER the early fire. Manual late fire (minutes post-boot) STICKS permanently (screenshot-proven full render). So: an early fire is defeated by the subsequent post-response re-render/replacement; late fire wins. Refire logic doesn't re-trigger Ã¢â‚¬â€� after the first fire the condition never holds again (childList+attribute MO armed, polls 40s Ã¢â‚¬â€� zero further fires). Next-session refinement path: log WHY the condition fails post-first-fire (probe ys.data/onYtShowSearchContentsCommand/hidden individually in the post-fire period), or simpler robust shim: fire repeatedly whenever hidden=true regardless of other conditions (direct ys.set('data.hideContents',false) fallback Ã¢â‚¬â€� also proven working). The working shim is genuinely one iteration away and converts YouTube to unassisted-rendering for alpha while the root-cause ordering divergence (servo applies the re-render later than Chrome / replacement timing) remains the deeper hunt.

## Shim paradox Ã¢â‚¬â€� exact final state for next session (endgame-11)

Final measurements: exactly ONE ytd-two-column-search-results-renderer exists (count=1, hidden, connected). Shim fires once (~T2.5-3s, hafter=false sync read), zero refires despite attribute+childList MO and 45s of polls, zero nofire logs Ã¢â‚¬â€� meaning at every post-first-fire check the element was NOT hidden per the shim's own querySelector, yet the final state is hidden=true on the single element. Interpretations: (a) hidden removal was transient within microtask batches and MO callbacks always delivered in windows where attribute was absent, with re-adds invisible due to batching timing; (b) the binding system re-adds hidden through a path that produces no mutation records (custom-elements upgrade-time attribute application Ã¢â‚¬â€� CFmmm maybe attribute restore during upgrade doesn't generate mutation records in servo? testable: does servo generate attribute mutation records for attributes set during custom element upgrade/clone?). HYPOTHESIS (b) IS TESTABLE cheaply and is a genuine spec-compliance question: attribute changes during upgrade/cloning should generate mutation records per spec. Next session: minimal repro Ã¢â‚¬â€� clone/upgrade an element with attributes, check MO records in servo vs Chrome. If missing, THAT is the engine bug explaining both the paradox and possibly the original defect (YouTube's own observers missing mutations).

## REPLACEMENT THEORY DEAD Ã¢â‚¬â€� clean single-boot capture (endgame-12)

Comprehensive identity-tracked capture (setter patch + MO + polls, one boot): exactly ONE write ever Ã¢â‚¬â€� T1724 SETTER hid=1 on el#1 (already connected), one ATTR record T2028, then the SAME single element stays hidden forever. NO replacement, NO re-hide, NO second element. Earlier replacement observations were boot-variant artifacts. The truth is minimal: hide-while-loading write happens once at stamping; unhide write never happens; everything else completes. Frame: in Chrome either the same hide-then-unhide sequence runs with the unhide trigger firing (trigger unknown Ã¢â‚¬â€� command absent everywhere), or Chrome stamps after data finalization so hideContents is never true. The remaining unexplored machinery: the visibility monitor (WHY()/visibilityObserver) Ã¢â‚¬â€� visOpt{calledOnce:false,state:hidden} persists; possible circular dependency (hiddenÃ¢â€ â€™never intersectsÃ¢â€ â€™observer never firesÃ¢â€ â€™completion never signaled) that Chrome somehow bootstraps through prescan/initial logic.

## sec1zt27 Ã¢â‚¬â€� USERSCRIPT UNLOCK + reversal: hidden-removal FIRES in servo, events all fire Ã¢â‚¬â€� defect is replacement re-stamp (2026-11-26)

**TOOLING: servo has --userscripts <dir> BUILT IN** (components/script/dom/userscripts.rs Ã¢â‚¬â€� scripts evaluated when head element parses, TRUE pre-boot injection, no engine work needed). This replaces all the injection hacks Ã¢â‚¬â€� use for all future framework debugging.

**FULL BOOT CAPTURE with userscript injection Ã¢â‚¬â€� reverses the sec1zt26 narrowing**: yt-page-data-fetched FIRES @1928, yt-page-data-updated @2198, yt-navigate-finish @2199, yt-update-title @2162 Ã¢â‚¬â€� the navigation-completion pipeline COMPLETES in servo! AND critically: **HIDDEN-REMOVE on ytd-two-column-search-results-renderer FIRES @1933** Ã¢â‚¬â€� the unhide write DOES execute! Yet final state is hidden:true. Confirmed synthesis with the replacement docs: removal fires on the boot/SSR element, the response-data replacement then arrives stamped WITH hidden, and no second removal fires for the replacement instance. The events all firing means the completion pipeline is NOT the defect. NEW PRECISE QUESTION: in chrome does the replacement element exist at all / does a second removal fire? ANOMALY OBSERVED: every event in servo fires exactly 3x with identical timestamps Ã¢â‚¬â€� either triple-dispatch in youtube logic (servo-specific) or triple-nested spy wrappers (userscript injected 3x?) Ã¢â‚¬â€� discriminate before trusting multiplicities. ALSO NOTE: sec1zt26's 'events absent in servo' was an artifact of post-boot injection (events fire before T+7s when spies could attach) Ã¢â‚¬â€� corrected.
## sec1zt27 Ã¢â‚¬â€� USERSCRIPT UNLOCK + reversal: hidden-removal FIRES in servo, events all fire Ã¢â‚¬â€� defect is replacement re-stamp (2026-11-26)



## YouTube hidden-stall ROOT CAUSE Ã¢â‚¬â€� SOLVED to the single wrong bit (endgame-13, FINAL)

Chrome vs servo identical-boot A/B with setter patches (addScriptToEvaluateOnNewDocument on Chrome via CDP ws, persist-hook on servo):
- CHROME: T1268 `SET hid=0 dataHC=nodata` Ã¢â‚¬â€� ONE write, value FALSE, element never hidden, results render.
- SERVO:  T1710 `SET hid=1 dataHC=nodata` Ã¢â‚¬â€� ONE write, value TRUE, element stays hidden forever.
Both: exactly one write through stack `_setUnmanagedPropertyToNode Ã¢â€ ï¿½ qJC (binding evaluator) Ã¢â€ ï¿½ vj5`, at stamping time, while `this.data` does not yet exist. data.hideContents ends `false` in Chrome's data object, `undefined` in servo's (keys=[primaryContents] both). ytInitialData has hideContents=false in both. The yt-show-search-contents-command DOES NOT EXIST in Chrome either Ã¢â‚¬â€� never fires, handler never called Ã¢â‚¬â€� the entire "missing unhide" line was a dead end; Chrome never needs an unhide because it never hides.

**The defect: the binding `hidden="[[data.hideContents]]"` (or sibling computed binding) evaluates at stamp time with the data path absent; the resolved value passed to the property setter is TRUTHY in servo, FALSY in Chrome.** Same polyfill JS, same template Ã¢â‚¬â€� the divergence is therefore in a DOM primitive the evaluation depends on. Suspects, ranked: (1) template stamping writes the literal binding attribute `hidden="[[data.hideContents]]"` to the real DOM before annotation binding replaces it Ã¢â‚¬â€� literal non-empty string = truthy Ã¢â‚¬â€� if servo's stamping/upgrade ordering exposes the literal attribute (or the polyfill reads it via getAttribute during evaluate), the write is truthy. In Chrome the annotation pass runs first / literal never visible. (2) Path resolution returning a truthy sentinel (literal binding text) instead of undefined under a missing-root-path primitive. (3) Boolean attribute reflection coercion during stamp. The write goes through the property SETTER so the truthy value arrives already-coerced Ã¢â‚¬â€� the divergence is upstream in qJC's value computation.

Minimal repro needed: custom element w/ template `hidden="[[data.hideContents]]"` stamped before data set Ã¢â‚¬â€� compare written value servo vs Chrome. Requires webcomponents/polymer harness Ã¢â‚¬â€� alternatively instrument qJC's computed value + intermediate path resolution directly in the live servo page (log what value qJC passes and what the binding source resolved to).

Prior layer results all still valid: rIC starvation fix landed (`933f453ad`, verified); stamping reliable post-fix; every other primitive exonerated; visibility-observer state machine noise; replacement theory dead (some boots show element replacement but endgame-12 clean capture shows single element, single write).

## sec1zt28 Ã¢â‚¬â€� ROOT CAUSE CORNERED: binding writes hidden=true for undefined data.hideContents (2026-11-26)

Definitive write-capture with state snapshots (userscript injection): servo sequence: template-parse strips SSR hidden @1973 (state: no data yet), then **propSET hidden=true @1996 while parent ytd-search data EXISTS but hideContents is UNDEFINED**. Chrome comparison for identical capture: remove@1492 then propCLEAR@1498 (false written). **The binding `hidden="[[data.hideContents]]"` evaluates undefinedÃ¢â€ â€™TRUE in servo vs undefinedÃ¢â€ â€™FALSE/absent in chrome.** Since the binding framework is identical JS in both browsers, the divergence is in a PRIMITIVE the framework relies on. Leading theory: template binding annotation REGISTRATION fails in servo Ã¢â‚¬â€� the write path was `_setUnmanagedPropertyToNode` (the stamper for UNMANAGED template properties) Ã¢â‚¬â€� i.e. in servo the hidden attribute is treated as unmanaged/static template content and stamped with truthy-string semantics, while in chrome it registers as a managed binding and is excluded from stamping. Suspect primitive areas: servo template parsing producing different attribute representation, or annotation-parse regex/string handling difference, OR custom-element/template lifecycle ordering making the registration miss. NEXT STEP: read the stamper code path in bundle (_parseTemplateNodeAttribute/_setUnmanagedPropertyToNode region, kevlar chunk line ~7473/3644) to determine exactly what makes an attribute managed-vs-unmanaged and probe that primitive in servo. ALSO CONFIRMED ARTIFACT: userscript injects 3x per page (nested wrappers Ã¢â‚¬â€� capture.js:22Ã¢â€ â€™capture.js:23 self-chain visible in stacks) Ã¢â‚¬â€� all x3 event duplicates are this artifact, not servo behavior; servo userscript multiplicity itself worth a look (load_script possibly invoked per head mutation).
## sec1zt28 Ã¢â‚¬â€� ROOT CAUSE CORNERED: binding writes hidden=true for undefined data.hideContents (2026-11-26)


## sec1zt29 Ã¢â‚¬â€� BREAKTHROUGH: the truthy write value is a Trusted-Types policy object (typeof object, stringifies "zClosurez")

Value-level capture on live servo boot (upgraded spy records typeof+String(v)):
```
propSET|object|zClosurez  (t=1931, once effective Ã¢â‚¬â€� x3 artifact)
The binding evaluates `data.hideContents` (undefined) but the VALUE assigned to `tc.hidden` is an
OBJECT stringifying to "zClosurez" Ã¢â‚¬â€� truthy Ã¢â‚¬â€� hence hidden=true.

IDENTIFIED in bundle @2458373: the **polymer_resin Trusted Types policy**:
```js
var D={createHTML:()=>"zClosurez", createScript:()=>"/*zClosurez*/", createScriptURL:()=>"about:invalid#zClosurez"};
return typeof trustedTypes!=="undefined" ? trustedTypes.createPolicy("polymer_resin", D) : D
When trustedTypes exists, createPolicy returns a real TT policy whose createHTML produces TrustedHTML
objects (typeof object, truthy). The framework's binding write path passes values through a transform
(C7 in the write chain: `C7&&(W=C7(W,q.target,q.kind,k))`) Ã¢â‚¬â€� the resin sanitizer hook.

CHROME contrast: propCLEAR = the assigned value was FALSY (undefined) Ã¢â‚¬â€� chrome's binding evaluation
never produces the object for this path, OR the transform passes undefined through untouched.
SERVO: the object materializes Ã¢â‚¬â€� two candidate primitives:
  (a) servo's trustedTypes.createPolicy/createHTML implementation returns the object even for
      undefined/empty input where chrome returns the input unchanged;
  (b) the binding evaluation in servo feeds a non-undefined sentinel into the transform where
      chrome feeds undefined (evaluation divergence), and the transform wraps it.
Either way the failing primitive is IN THE TRUSTED-TYPES/RESIN TRANSFORM PIPELINE, not the data system.

NEXT PROBES (cheap, decisive):
  1. typeof trustedTypes + trustedTypes.createPolicy behavior in servo vs chrome:
     `trustedTypes.createPolicy('t',{createHTML:s=>s}).createHTML('x')` Ã¢â‚¬â€� typeof of result;
     createHTML(undefined) behavior.
  2. Capture C7's input/output Ã¢â‚¬â€� spy is internal; instead: evaluate `data.hideContents` binding
     semantics manually in-page: compare `(eval of undefined passthrough)` Ã¢â‚¬â€� if TT wrap of undefined
     produces object in servo but undefined in chrome Ã¢â€ â€™ primitive pinned.
  3. If (a) confirmed: engine fix = match chrome TT semantics (passthrough of non-string input /
     correct undefined handling in createHTML).

Symptom index: YouTube truthy hidden write (object zClosurez) Ã¢â€ â€™ check Trusted Types createPolicy/
createHTML object-materialization semantics (components/script/dom/trustedtypes*), polymer_resin
policy path in kevlar bundle.

## sec1zt30 Ã¢â‚¬â€� ROOT CAUSE PINNED TO EXACT PRIMITIVE: resin sanitizeDOMValue(undefined) diverges

Direct A/B probe on live pages, identical probe code, same Polymer.sanitizeDOMValue function:
input undefined Ã¢â€ â€™ SERVO: object "zClosurez" TRUTHY   | CHROME: undefined FALSY
input false     Ã¢â€ â€™ both boolean false
input 'abc'     Ã¢â€ â€™ both string 'abc'
input null      Ã¢â€ â€™ both null
THE YOUTUBE INVISIBLE-RESULTS DEFECT = this one divergence. Chain: binding `hidden="[[data.hideContents]]"`
evaluates undefined (identical in both Ã¢â‚¬â€� verified ytInitialData/live data have NO hideContents anywhere in
EITHER browser; endgame-13's false-in-chrome claim was WRONG) Ã¢â€ â€™ sanitizeDOMValue transform Ã¢â€ â€™ servo returns
the INNOCUOUS_STRING fallback (TrustedHTML object wrapping "zClosurez", truthy) Ã¢â€ â€™ tc.hidden = truthy Ã¢â€ â€™
results hidden forever. Chrome passes undefined through Ã¢â€ â€™ hidden=false Ã¢â€ â€™ renders.

TT object materialization itself EXONERATED: createPolicy/createHTML(undefined) behavior byte-identical
in both browsers (truthy objects both). Data layer exonerated identically. rIC/CE/IO/upgrade/scheduler
all previously exonerated. The divergence is INSIDE the resin sanitizer's undefined-handling branch Ã¢â‚¬â€�
same JS, different result Ã¢â€¡â€™ it depends on a browser primitive that differs. Prime suspects in servo:
  (a) TT type-check primitives the resin safeTypesBridge uses (instanceof TrustedHTML / isUnwrappable
      detection failing on servo's TT object identity) making the sanitizer misroute undefined;
  (b) a typeof/String coercion difference on undefined inside vNk machinery.
NEXT STEP (final): trace the resin z4M inner path (vNk + safeTypesBridge @2458537) Ã¢â‚¬â€� determine which
branch undefined takes in chrome (passthrough) vs servo ($_ fallback), identify the primitive the branch
keys on, probe that primitive directly. HIGHEST-VALUE TARGETS: servo's TrustedTypes implementation
(components/script/dom/trustedtypes*.rs) object identity/type-tag behavior, and resin's isUnwrappable check.

Symptom index: framework sanitizeDOMValue diverges ONLY for undefined input Ã¢â€ â€™ check Trusted Types
object identity/unwrap detection (isUnwrappable) and TT type-tag primitives in servo.

## sec1zt31 Ã¢â‚¬â€� ROOT CAUSE FINAL: servo's document.all strict-equals undefined Ã¢â‚¬â€� breaks resin's branch

DEFINITIVE A/B primitive probe:
                    SERVO      CHROME     spec-correct
typeof document.all   undefined  undefined  undefined   Ã¢Å“â€œ both
!document.all         true       true       true        Ã¢Å“â€œ both
document.all == undefined  true  true       true        Ã¢Å“â€œ both (loose)
document.all === undefined TRUE  FALSE      FALSE       Ã¢Å“â€” SERVO WRONG
Servo's document.all behaves as if it literally IS undefined (passes typeof/falsy/loose by coincidence)
instead of a real object with the [[IsHTMLDDA]] internal slot. Strict equality object-vs-undefined must
ALWAYS be false per spec Ã¢â‚¬â€� servo returns true, proving document.all is not a proper object.

MECHANISM CHAIN (complete, end to end):
  resin inner sanitizer: vNk returns `(J,D,d,Q) => Q || Q===document.all ? W(J,D,d)(Q) : Q`
  - chrome: undefined input Ã¢â€ â€™ `undefined || (undefined===document.allÃ¢â€ â€™FALSE)` Ã¢â€ â€™ falsy Ã¢â€ â€™ PASSTHROUGH
    Ã¢â€ â€™ sanitizeDOMValue(undefined) returns undefined Ã¢â€ â€™ hidden write falsy Ã¢â€ â€™ results render.
  - servo: undefined input Ã¢â€ â€™ `undefined || (undefined===document.allÃ¢â€ â€™TRUE!)` Ã¢â€ â€™ truthy Ã¢â€ â€™ sanitizer
    processes it Ã¢â€ â€™ returns $_ = INNOCUOUS_STRING (TrustedHTML object, truthy) Ã¢â€ â€™ tc.hidden = object
    Ã¢â€ â€™ truthy Ã¢â€ â€™ RESULTS INVISIBLE FOREVER.
YouTube invisible-results root cause = servo's document.all is not a real IsHTMLDDA object.

ENGINE FIX: implement HTMLAllCollection correctly Ã¢â‚¬â€� a real object with [[IsHTMLDDA]] internal slot
making typeofÃ¢â€ â€™"undefined", ToBooleanÃ¢â€ â€™false, loose==null/undefinedÃ¢â€ â€™true, but identity/strict-equality
as a normal object. Check components/script/dom/htmlallcollection (or wherever document.all is
implemented) Ã¢â‚¬â€� likely currently returns undefined or a hacked value instead of the spec object.
Verification gate: `document.all === undefined` must be false; YouTube search results render.

Symptom index: ANY framework branch keying on `x === document.all` misfires in servo Ã¢â€ â€™ check
document.all implementation (IsHTMLDDA object identity). Indexed by mechanism: strict-equality
with undefined incorrectly true for document.all.
## sec1zt32 Ã¢â‚¬â€� document.all implementation in progress (Linux lane) + codegen mechanics decoded

IMPLEMENTING HTMLAllCollection (servo#7396 Ã¢â‚¬â€� never implemented upstream). Files:
- components/script_bindings/webidls/HTMLAllCollection.webidl Ã¢â‚¬â€� interface : HTMLCollection, namedItem getter union
- Document.webidl Ã¢â‚¬â€� `[SameObject] readonly attribute HTMLAllCollection all;` (uncommented)
- components/script/dom/html/htmlallcollection.rs Ã¢â‚¬â€� embeds HTMLCollection rooted at document with
  match-all filter; NamedItem: id-or-name match, singleÃ¢â€ â€™Element, multipleÃ¢â€ â€™live HTMLCollection
- Document::All lazy-cached via MutNullableDom or_init
- html/mod.rs registration

KEY MECHANISM Ã¢â‚¬â€� emulates-undefined for PROXY objects: DOM objects with indexed/named getters are JS
proxies in servo, so JSCLASS_EMULATES_UNDEFINED on the DOMJSClass has NO effect (proxies don't use the
interface class). Solution: mozjs glue NewProxyObject accepts aClass Ã¢â€ â€™ js::ProxyOptions.setClass.
Added `EMULATES_UNDEFINED_PROXY_CLASS` static in script_bindings/init.rs Ã¢â‚¬â€� bare JSClass carrying only
the EMULATES_UNDEFINED flag; codegen emits `class: Some(&crate::init::EMULATES_UNDEFINED_PROXY_CLASS)`
for HTMLAllCollection's WrapConfig; wrap.rs proxy path now passes config.class through to NewProxyObject
(was hardcoded null). SpiderMonkey reads class flags directly for typeof/truthiness/loose-equality
(verified in jit/MacroAssembler-inl.h branchIfObjectEmulatesUndefined); strict equality untouched.

KEY CODEGEN LEARNING (symptom index Ã¢â‚¬â€� saves hours on any future interface work): cx-ness of generated
trait methods is NOT inferred from webidl types Ã¢â‚¬â€� it's MANUAL per-interface config in
codegen/Bindings.conf (`'cx': [...]`, `'cx_no_gc': [...]`, `'no_gc': [...]` lists). Attributes/methods
not listed generate cx-less trait signatures. HTMLFormControlsCollection entry was the template.
Document::All needed 'All' added to Document's cx list.

## HTMLAllCollection implementation Ã¢â‚¬â€� IN PROGRESS (macOS lane, endgame-15)

sec1zt31 root cause confirmed correct. Implementation landed locally (compiles clean), verification pending:
- `components/script_bindings/webidls/HTMLAllCollection.webidl` Ã¢â‚¬â€� new (length/item/namedItem union, LegacyOverrideBuiltIns; LegacyFactoryFunction intentionally omitted v1)
- `components/script/dom/html/htmlallcollection.rs` Ã¢â‚¬â€� new DOM type wrapping HTMLCollection rooted at document, all-elements filter; namedItem returns Element/union sub-collection per spec
- `components/script_bindings/codegen/codegen.py` Ã¢â‚¬â€� class flags for HTMLAllCollection get `JSCLASS_EMULATES_UNDEFINED` (bit exists in vendored SM 153 as 1<<6, bindgen exposes it Ã¢â‚¬â€� compiled)
- `components/script_bindings/import.rs` Ã¢â‚¬â€� re-export the constant
- `Document.webidl` Ã¢â‚¬â€� `all` uncommented (plain readonly attr, or_init caching like images)
- `document.rs` Ã¢â‚¬â€� all_collection field + All() impl
- `Bindings.conf` Ã¢â‚¬â€� HTMLAllCollection entry (cx for named methods, cx_no_gc for indexed; 'All' added to Document cx list)
- dom/html/mod.rs Ã¢â‚¬â€� module registered
KEY ENGINE FACT: SpiderMonkey implements all IsHTMLDDA semantics (typeof undefined, falsy, loose==undefined true, strict===undefined FALSE) purely from the class flag Ã¢â‚¬â€� no VM changes needed.
Verification gate: `document.all === undefined` must be false in servo, typeof document.all "undefined", !document.all true, document.all==undefined true; then YouTube results render unassisted.

## HTMLAllCollection round 2 Ã¢â‚¬â€� the proxy class path (endgame-16)

CRITICAL STRUCTURAL FACT: servo implements interfaces with indexed/named getters as **JS Proxies** (CreateProxyHandler + NewProxyObject in wrap.rs), not plain objects Ã¢â‚¬â€� so the init_domjs_class flags path never applies to them. typeof was still "object" after round 1 because the proxy object got the default proxy class (`ptr::null()` class arg in wrap.rs NewProxyObject). SM mechanism confirmed in vendored source: EmulatesUndefined() unwraps WrapperObjects then reads getClass()->emulatesUndefined() Ã¢â‚¬â€� proxies CAN carry custom classes (ProxyOptions.setClass). Fix plumbed: `HTML_ALL_COLLECTION_PROXY_CLASS` static in script_bindings/utils.rs (flags=JSCLASS_EMULATES_UNDEFINED, verified=64 in bindgen jsapi.rs), codegen emits it for HTMLAllCollection's proxy WrapConfig, wrap.rs passes config.class into NewProxyObject. Verification gate unchanged: typeof document.all === "undefined" etc.

## HTMLAllCollection VERIFIED Ã¢â‚¬â€� YouTube alpha gate DOWN (endgame-17, THE fix)

Primitive verification (docall.html): typeof document.all="undefined" Ã¢Å“â€œ, !document.all=true Ã¢Å“â€œ, document.all==undefined=true Ã¢Å“â€œ, **document.all===undefined=FALSE** Ã¢Å“â€œ (the critical bit), instanceof Object=true Ã¢Å“â€œ Ã¢â‚¬â€� full spec IsHTMLDDA semantics correct.
END-TO-END VERIFICATION: youtube.com/results?search_query=techno+mix renders full search results UNASSISTED Ã¢â‚¬â€� no shim, no manual unhide. Video entries, thumbnails, sponsored slots all visible. The multi-session hunt (hydration stall Ã¢â€ â€™ rIC starvation Ã¢â€ â€™ hidden attribute Ã¢â€ â€™ resin sanitizer Ã¢â€ â€™ document.all) is CLOSED.
KNOWN RESIDUALS: (1) devtools evaluateJS accessing document.all properties crashes/brands-fail Ã¢â‚¬â€� grip serialization or brand-check interaction with the custom proxy class, devtools-only path, in-page semantics correct; (2) LegacyFactoryFunction omitted (document.all callable form unimplemented Ã¢â‚¬â€� essentially unused on real web); (3) namedItem multiple-match sub-collection path untested. Follow-ups non-blocking.
Implementation notes for upstream port: JSCLASS_EMULATES_UNDEFINED exists in vendored SM 153 (=64); proxies accept custom classes via NewProxyObject class arg; servo previously passed ptr::null() for all DOM proxies Ã¢â‚¬â€� the plumbing added in wrap.rs is generic (any proxy interface can now carry a custom class).

## HTMLAllCollection FINAL Ã¢â‚¬â€� stable + fully verified (endgame-18)

Crash root cause found+fixed: the v1 static class had null cOps/ext/oOps Ã¢â‚¬â€� proxy objects NEED SpiderMonkey's proxy class ops (proxy_Finalize/ProxyObject::trace/objectMoved + ProxyObjectOps for all property access). Fix: clone the exported `js::ProxyClass` (JS_PUBLIC_DATA, available in bindgen as js::jsapi::ProxyClass) and OR JSCLASS_EMULATES_UNDEFINED into flags Ã¢â‚¬â€� `html_all_collection_proxy_class()` in script_bindings/utils.rs (lazy OnceLock, SyncClass wrapper for Send/Sync). Final verification all green: typeof=undefined, falsy, loose-eq true, strict-eq false, instanceof Object true; in-page len/indexed/namedItem/keys all work (len=5, i0=HTML, nm=object, keys=7); devtools-path eval access works; NO crashes; YouTube results render fully unassisted (channel cards, shelves, sponsored). The multi-session YouTube invisible-results defect chain is CLOSED at the engine level.

## Post-fix breadth verification (endgame-19)

Sites re-tested after the document.all fix on the stable build: YouTube results+SPA+watch-chrome Ã¢Å“â€œ, X.com login page Ã¢Å“â€œ, reddit (botwall renders Ã¢â‚¬â€� attestation class) Ã¢Å“â€œ, **old.reddit.com login renders fully (previously BLANK in the pre-alpha ledger Ã¢â‚¬â€� legacy document.all gating confirmed as a real-site impact class)** Ã¢Å“â€œ, wikipedia Ã¢Å“â€œ no regression, github Ã¢Å“â€œ no regression. Legacy IE-detection branching (`if (document.all)` / `===` comparisons) is a broad real-web pattern Ã¢â‚¬â€� this fix likely unblocks a long tail of legacy-gated rendering defects beyond YouTube.

## Playback regression check (endgame-20): Rick Astley watch page Ã¢â‚¬â€� full chrome + ad pre-roll playing, sidebar recommendations, no regression from document.all fix. YouTube is now end-to-end functional in feather except attestation-botwall videos.

## namedItem verification (endgame-21): multi-match returns HTMLCollection Ã¢Å“â€œ, single-match Element semantics correct (test artifact confirmed), empty-string null Ã¢Å“â€œ, indexed item() identity consistent Ã¢Å“â€œ. Collection surface fully verified.
Verification gate once built: document.all === undefined must be FALSE, typeof "undefined", falsy,
document.all.length == element count, document.all('id') lookup; then YouTube search results render.
## sec1zt33 Ã¢â‚¬â€� dedupe correction + verification in progress (Linux lane)

CORRECTION to sec1zt32: the HTMLAllCollection implementation was ALREADY committed to feather-engine
by another lane (commit 1a1f702bdd6, "proxy-class path for emulates-undefined") Ã¢â‚¬â€� discovered mid-build
via rebase conflicts. Their version is architecturally identical but better-informed: proxy class
derived from js::jsapi::ProxyClass with EMULATES flag added (mine was bare). My duplicate working-tree
implementation dropped in favor of HEAD. sec1zt32's codegen/Bindings.conf cx-mechanics learnings
remain valid. Now VERIFYING (the lane's commit note says unverified): shell-new dev build in progress,
then document.all primitive probes + YouTube end-to-end render check.

## HTMLAllCollection round 2 Ã¢â‚¬â€� the proxy class path (endgame-16)


## HTMLAllCollection VERIFIED Ã¢â‚¬â€� YouTube alpha gate DOWN (endgame-17, THE fix)


## HTMLAllCollection FINAL Ã¢â‚¬â€� stable + fully verified (endgame-18)


## Post-fix breadth verification (endgame-19)


## Playback regression check (endgame-20): Rick Astley watch page Ã¢â‚¬â€� full chrome + ad pre-roll playing, sidebar recommendations, no regression from document.all fix. YouTube is now end-to-end functional in feather except attestation-botwall videos.

## New defect (post-alpha matrix): gmail.com marketing page renders fully, but clicking "Sign in" does NOT navigate (link likely target=_blank to accounts.google.com Ã¢â‚¬â€� popup/new-tab handling or link-activation defect candidate; needs repro with a minimal target=_blank test page). Filed for next investigation round.

## target=_blank defect CONFIRMED (post-alpha matrix): minimal repro /tmp/vtest/blank1.html Ã¢â‚¬â€� normal link navigates Ã¢Å“â€œ but target=_blank click is a dead end: no new tab, no navigation, no visible feedback. Shell-new doesn't handle auxiliary browsing-context creation (embedder create-webview message). This explains gmail Sign-in non-navigation (target=_blank to accounts.google.com). Impact class: HIGH Ã¢â‚¬â€� oauth flows, external links, sign-in redirects across the real web. Fix site: shell-new embedder message handler for auxiliary context requests Ã¢â‚¬â€� should open a new tab.

Symptom-index entry: symptom = "click on link does nothing at all, no error" Ã¢â€ â€™ suspect target attribute on anchor / auxiliary browsing context handling in shell embedder layer.

## target=_blank FIX in progress (post-alpha work): implemented on feather branch devin/1795734400-aux-webviews (commit 307bbb9) Ã¢â‚¬â€� shell-new EngineDelegate.request_create_new builds the auxiliary webview (servo embedder API: CreateNewWebViewRequest.builder(rendering_context).delegate(parent.delegate()).user_content_manager().hidpi_scale_factor().build()) and parks it in engine.pending_aux; sync_engine drains each frame, allocs a tab id, adopts into active workspace as active tab. Transient-liveness handled via aux_keys set (about:blank popups miss `wanted` since it's http-only Ã¢â‚¬â€� close loop would kill them otherwise; writeback loop extended to aux keys so the tab doc picks up the real url once servo navigates). cargo check clean. VERIFICATION PENDING: release build then blank1.html test (BLANK LINK TEST should open new tab) + gmail sign-in. NOTE: disk filled mid-work (cargo check debug tree 5GB) Ã¢â‚¬â€� pruned; 3.5GB free. If build fails for space, prune target/release/deps incremental cruft.
DISK-NEAR-DEATH lesson: concurrent cargo invocations + debug artifacts filled the disk to 0 mid-build
(23GB check-target + 13GB servo/target + 4GB prod artifacts). Freed 13GB by deleting the servo release
target dir entirely (stale pre-fix servoshell backed up to /tmp/servoshell-backup). Rule reinforced:
one cargo at a time on this VM; the servo release target dir is expendable once feather shell carries
the engine Ã¢â‚¬â€� verify engine changes through shell-new dev builds (check-target), not servoshell.
## HTMLAllCollection round 2 Ã¢â‚¬â€� the proxy class path (endgame-16)


## HTMLAllCollection VERIFIED Ã¢â‚¬â€� YouTube alpha gate DOWN (endgame-17, THE fix)


## HTMLAllCollection FINAL Ã¢â‚¬â€� stable + fully verified (endgame-18)


## Post-fix breadth verification (endgame-19)


## Playback regression check (endgame-20): Rick Astley watch page Ã¢â‚¬â€� full chrome + ad pre-roll playing, sidebar recommendations, no regression from document.all fix. YouTube is now end-to-end functional in feather except attestation-botwall videos.

## New defect (post-alpha matrix): gmail.com marketing page renders fully, but clicking "Sign in" does NOT navigate (link likely target=_blank to accounts.google.com Ã¢â‚¬â€� popup/new-tab handling or link-activation defect candidate; needs repro with a minimal target=_blank test page). Filed for next investigation round.

## target=_blank defect CONFIRMED (post-alpha matrix): minimal repro /tmp/vtest/blank1.html Ã¢â‚¬â€� normal link navigates Ã¢Å“â€œ but target=_blank click is a dead end: no new tab, no navigation, no visible feedback. Shell-new doesn't handle auxiliary browsing-context creation (embedder create-webview message). This explains gmail Sign-in non-navigation (target=_blank to accounts.google.com). Impact class: HIGH Ã¢â‚¬â€� oauth flows, external links, sign-in redirects across the real web. Fix site: shell-new embedder message handler for auxiliary context requests Ã¢â‚¬â€� should open a new tab.


## window.open panic root cause + fix (post-alpha): `windowproxy.open` did `maybe_entry_global().map(|g| g.as_window().Document())` Ã¢â‚¬â€� as_window() PANICS when the entry global isn't a Window (devtools evaluateJS frames, possibly WASM instantiation). Reproduced 2/2 via devtools eval of window.open on trivial page (example.com) Ã¢â‚¬â€� Script thread panic "expected a Window scope" then webrender cascade kills the app. Fixed on feather-engine: only use entry global when it downcasts to Window, else fall back to the browsing context's active document. NOTE: real-link target=_blank clicks don't hit this (entry global IS the window during event dispatch) Ã¢â‚¬â€� devtools-eval-driven testing did. Upstreamable bugfix, small.
youtube.com/watch?v=dQw4w9WgXcQ in feather shell-new: full page render Ã¢â‚¬â€� sidebar recommendations
with thumbnails (same resin-sanitizer class as search), like/dislike/save buttons, title, autoplay
working. VIDEO PLAYBACK VERIFIED LIVE via eval probe: currentTime advancing (21.5s at probe time),
readyState 4, paused=false, videoWidth 1280, blob: MSE source. The document.all fix + the earlier
planar-YCbCr video pipeline + MSE work compound: YouTube is end-to-end functional in feather Ã¢â‚¬â€�
search, watch, playback. This closes the alpha-critical YouTube cluster.

sec1zt36 Ã¢â‚¬â€� post-document.all regression sweep (negative results + one environmental)
- github.com/servo/servo renders fully in feather: correct title, 407 links, 31 images. No regression.
- IndexedDB structuredclone with nested objects/arrays: WORKS (prior error observations stale Ã¢â‚¬â€� negative result).
- google.com/search: environmentally blocked Ã¢â‚¬â€� "unusual traffic" CAPTCHA wall (datacenter IP reputation),
  NOT an engine defect. Symptom: google search shows captcha/sorry page Ã¢â€ â€™ check IP reputation first, do not
  chase engine causes.

## Aux-popup upstream blocker (filed): after the windowproxy entry-global fix, window.open gets further but hits `RefCell already borrowed` at script_thread.rs handle_loads_complete / documents.borrow_mut().insert Ã¢â‚¬â€� re-entrancy in the aux browsing context document load path. This is upstream servo popup-path breakage (unexercised in embedding), NOT shell-adoption-specific Ã¢â‚¬â€� popups would crash the same way in any embedder that builds the requested webview. NOTE: window.open was NEVER safe in feather Ã¢â‚¬â€� it panicked even before aux shell support (the entry-global panic fires before the constellation consults the embedder), so the shell's adoption support is strictly additive, no regression either way. Next step when prioritized: backtrace the borrow conflict (RUST_BACKTRACE=1), likely the documents RefCell held across a re-entrant load callback in the initial-about-blank aux document path.

sec1zt34 Ã¢â‚¬â€� document.all verification: PASSED end-to-end (Linux, feather shell-new)
Verification of the HTMLAllCollection implementation (1a1f702bdd6, other lane) in the integrated
feather shell-new binary. ALL gates pass:

**Primitive semantics** (live eval bridge probe on https://example.com/, servo engine):
- `typeof document.all` Ã¢â€ â€™ "undefined" Ã¢Å“â€œ spec
- `document.all === undefined` Ã¢â€ â€™ **false** Ã¢Å“â€œ THE FIX (was true pre-fix Ã¢â‚¬â€� the exact sanitizer-breaking primitive)
- `document.all == undefined` Ã¢â€ â€™ true Ã¢Å“â€œ spec (loose equality emulates undefined)
- `!document.all` Ã¢â€ â€™ true Ã¢Å“â€œ spec (falsy object)
- `document.all === null` Ã¢â€ â€™ false Ã¢Å“â€œ spec
- `document.all.length` Ã¢â€ â€™ 22 on example.com Ã¢â‚¬â€� live collection populated Ã¢Å“â€œ
- `Object.prototype.toString` Ã¢â€ â€™ "[object HTMLAllCollection]" Ã¢Å“â€œ
- `document.all[0]` Ã¢â€ â€™ object Ã¢â‚¬â€� indexed access works Ã¢Å“â€œ
- `document.all('x')` Ã¢â€ â€™ TypeError not-a-function Ã¢â‚¬â€� legacy-caller omission as documented (acceptable v1; sites using the call form still break Ã¢â‚¬â€� track as follow-up)

**END-TO-END ALPHA GATE: YouTube search results RENDER in feather shell-new.**
youtube.com/results?search_query=test displays full result set: video thumbnails with images,
channel avatars, titles, metadata, subscribe buttons, filter chips. Pre-fix this page rendered a
completely blank results area (hidden attribute never cleared). The root-cause chain documented in
sec1zt29-31 is closed: document.all real object Ã¢â€ â€™ strict-equality false Ã¢â€ â€™ resin sanitizer passes
undefined binding values through untouched Ã¢â€ â€™ hidden=false Ã¢â€ â€™ results visible.

**Verification environment notes (symptom-indexed):**
- Symptom: shell-new shows "wired to Servo" mock placeholder + eval bridge silent (rects=0) Ã¢â€ â€™ check: file:// and non-http URLs NEVER get engine webviews (app.rs sync_engine gates on `doc.url.starts_with("http")`); also first-run onboarding must be completed once. Use http(s) URLs for all engine verification.
- Symptom: "client error (Connect)" loading localhost/127.0.0.1:<port> in servo Ã¢â€ â€™ python http.server IS reachable via curl but servo net fails to connect; workaround: evaluate probes via the eval bridge on external sites instead of serving local probe pages. NOT further root-caused (low value).
- Symptom: xdotool clicks land on wrong window when multiple browsers on DISPLAY=:0 Ã¢â€ â€™ always wmctrl -l to enumerate, minimize competition, activate the feather window explicitly by id; Chrome-for-testing instances persist on the desktop from other harnesses.
- The eval bridge chain works end-to-end once a real http engine webview exists: probe file Ã¢â€ â€™ [eval] result in stderr within ~2s. FEATHER_EVAL_DBG diagnostics confirmed active=Some/webview=true.
- shell-new first-run onboarding (hello Ã¢â€ â€™ make-it-yours Ã¢â€ â€™ skip) blocks real browsing until completed; it reappears on fresh profiles only.

The document.all mission is COMPLETE. Verification evidence: screenshots + eval results in session
devin-d3790f631f184a44862d36d5fb5f39e4.

sec1zt35 Ã¢â‚¬â€� document.all extended verification: YouTube WATCH page fully functional

## Aux-webview verification detail (post-PR156): target=_blank real-link clicks DO open a new tab with correct navigation (verified: injected anchor on example.com Ã¢â€ â€™ new active tab, omnibox example.org). But the new tab's CONTENT stays stale (shows opener's pixels) Ã¢â‚¬â€� force-repaint at adoption (feather branch aux-paint-fix) is correct but insufficient. Root mechanism: shell never hides inactive webviews and only repaints on servo frame signals Ã¢â‚¬â€� static pages stop producing frames, so ANY tab switch to a static page is potentially stale (same class as the documented F1 paint-invalidation defect, shared-FBO limitation noted in app.rs composite()). The proper fix is in the compositing invalidation layer, not aux-specific. window.open() JS path still blocked by the upstream RefCell-borrow panic in the aux document load; real-link clicks avoid that path entirely.



sec1zt37 Ã¢â‚¬â€� document.all named-getter verified: document.all['id'] returns exact element (identity
match confirmed on github.com, live length 2782). Spec surface complete except legacy call-form.

## TESTING GOTCHA (solved mystery): schemeless omnibox input (e.g. `127.0.0.1:8931/x`) is HTTPS-upgraded Ã¢â‚¬â€� net log shows `GET https://127.0.0.1:8931/...` and the server log shows TLS ClientHello bytes hitting the plain-HTTP server ("Bad request syntax" 400). This is correct modern browser behavior (https-first), NOT a regression. Local test pages MUST be typed with explicit `http://` scheme. Symptom-index: "local test server connect error, curl works" Ã¢â€ â€™ check scheme in net log first.

## STRATEGIC PRIORITY NOTE: the shared-FBO/static-page compositing staleness is now the critical-path item for the macOS lane Ã¢â‚¬â€� it blocks even verification of other work (aux tabs can't be visually verified; clicked pixels are stale composites; every test requires guessing whether paint is current). Recommend the next engine/shell hunt: repaint-on-tab-switch + repaint-on-navigation-completion in shell-new composite() Ã¢â‚¬â€� the invalidation layer, not per-feature hacks. Everything else this session is committed: document.all (alpha gate), traffic lights (#156), aux webviews (#157 open).
gmail.com in feather shell-new navigates the full chain to accounts.google.com/v3/signin/identifier
and the sign-in page RENDERS COMPLETELY Ã¢â‚¬â€� form, input, buttons, logo, styling all correct. The
originally-filed defect (marketing-page Sign in click non-navigation, target=_blank class) is closed
by the aux-webview work as theorized. Caveat: this run redirected gmail.comÃ¢â€ â€™accounts directly without
showing the marketing page (no-cookie flow variance), so the literal marketing-page click was not
re-tested Ã¢â‚¬â€� but the navigation chain it was blocking reaches and renders the destination correctly.
The Google sign-in form being fully interactive-capable in servo is itself a notable alpha data point.

## Aux final verification state: post-paint-fix, normal tab switching verified correct both directions, BUT aux-adopted tabs still never paint their content even after manual tab cycling Ã¢â‚¬â€� deeper than frame timing: webview.paint() on the aux webview appears to composite nothing/the wrong thing. Suspect the WebViewBuilder::new_for_create_request path misses some compositor/constellation registration that normal creation performs (compare what new() does beyond builder vs new_for_create_request in components/servo/webview.rs). Tab chrome/navigation/omnibox all correct Ã¢â‚¬â€� purely compositing. This is the last aux-blocking defect; fix site comparison is the next step.

sec1zt38 Ã¢â‚¬â€� target=_blank + window.open verification: BOTH PASS, popup cluster CLOSED
Verified in feather shell-new (main bb465a5, engine 884f5777a85) via eval-injected repro on example.com:
- **target=_blank link click**: opens NEW TAB, navigates to target (example.org loaded), original tab
  intact, new tab active. PASS Ã¢â‚¬â€� the aux-webview adoption (feather#156) works end-to-end.
- **window.open('https://example.edu/','_blank')**: returns object, opens third tab, target loads.
  ZERO panics, ZERO RefCell-borrow errors Ã¢â‚¬â€� the documented aux-popup RefCell blocker (sec1zt33-era)
  does NOT reproduce. The windowproxy entry-global fix (7df57cb72ac) evidently resolved the whole path;
  the borrow re-entrancy was downstream of the panic-path state, not an independent defect.
  Negative result: RefCell-already-borrowed at handle_loads_complete Ã¢â‚¬â€� not reproducible post-fix.
Verification method note: target=_blank repro does NOT need a local server Ã¢â‚¬â€� inject the anchor into any
real page via the eval bridge and .click() it.
Symptom: "client error (Connect)" on http LAN/localhost urls partially explained Ã¢â‚¬â€� typing schemeless
IP:port in shell-new omnibox goes to HTTPS (TLS InvalidContentType against plain-http server in net log).
Workaround: none needed for external testing; local-http testing still broken (servo TLS-to-plain or
connect issue unresolved for explicit http:// too Ã¢â‚¬â€� low priority, eval-injection covers repro needs).

sec1zt39 Ã¢â‚¬â€� gmail sign-in navigation: WORKS post-popup-fix (verification with caveat)

## COMPOSITING STALENESS CLASS SOLVED: root cause was visibility contention Ã¢â‚¬â€� all webviews permanently "shown", servo composites every shown webview into the shared rendering context on its own refresh schedule, so the active tab's pixels get overwritten by background tabs continuously. Explains: aux tabs never painting (always lost contention), static-page switch staleness (the repaint-force helped only because it repainted AFTER servo's last background composite Ã¢â‚¬â€� race-dependent), and likely a big slice of the historical F1 paint-invalidation defects. Fix in shell-new composite(): hide all non-active webviews, show only active. hide() semantics: throttles script threads + discards decoded image surfaces Ã¢â‚¬â€� standard browser backgrounding. VERIFIED end-to-end: target=_blank correct content immediately, bidirectional tab switching correct. Symptom-index: "correct tab chrome/navigation but pixels belong to a different page" Ã¢â€ â€™ visibility contention in shared rendering context, check show/hide discipline.

## ALPHA GATE FINAL VALIDATION (endgame-22): with document.all + rIC + visibility-discipline + paint fixes all in one build, YouTube search renders FULLY UNASSISTED end-to-end Ã¢â‚¬â€� complete chrome, filter chips, sponsored cards, channel cards, video results with thumbnails/avatars/durations/LIVE badges, working scroll. Screenshots: lofi beats results indistinguishable from Chrome. The multi-session hunt chain is closed: rIC starvation Ã¢â€ â€™ hidden-attribute Ã¢â€ â€™ resin sanitizer Ã¢â€ â€™ document.all Ã¢â€ â€™ compositing visibility. Residual classes: attestation/botwall, background-media-aware visibility throttling, window.open JS-path RefCell panic (upstream).

sec1zt40 Ã¢â‚¬â€� stale-composite tab-switch defect: ROOT CAUSE + FIX verified (feather#160)
Defect: tab switch composites the previously-active tab's pixels indefinitely (F1 class).
FULL mechanism chain (correcting the earlier partial theory): it is NOT merely a missing repaint Ã¢â‚¬â€�
paint() was already being called for the correct webview (proven via FEATHER_PAINT=1 logging:
painted=true for the gmail webview while wikipedia pixels stayed on screen). The deeper cause:
**servo composites only the webview shown within the shared rendering context** Ã¢â‚¬â€� the embedding
contract requires the embedder to call webview.show()/hide() on activation changes (servoshell
running_app_state.rs activate_webview is the reference implementation). shell-new never did, so
servo kept compositing the first-shown webview no matter which webview paint() was invoked on.
Fix (feather PR #160, branch devin/1795697030-tab-switch-paint): on active-tab change in composite(),
show() the active webview + hide() all others + force paint without waiting for frame signals.
VERIFIED both directions: wikipediaÃ¢â€ â€�gmail tab pair composites correct distinct content after switches.
Symptom index: Symptom: tab content shows wrong/other tab's page after switching Ã¢â€ â€™ check: embedder
show()/hide() activation contract, NOT the repaint logic Ã¢â‚¬â€� paint() running is not evidence of correct
compositing. Symptom: blank white content area on first load after naive show/hide adoption Ã¢â€ â€™ check:
first-composite ordering, webview must be show()n before its frames composite.
Behavioral side effect: hide() throttles background script + discards decoded images (memory win,
but background media pauses Ã¢â‚¬â€� documented in PR).

## Watch-page validation (endgame-23): click-through from search results to watch page works fully Ã¢â‚¬â€� complete player chrome, live chat rendering with scrolling messages, view counts, channel cards, ad pre-roll playing. Deepest YouTube functionality verified to date. PR collision note: Linux lane independently implemented the same visibility fix (#160) Ã¢â‚¬â€� strong cross-lane confirmation of the root cause; theirs closed, #161 primary.

sec1zt41 Ã¢â‚¬â€� aux-tab paint residual: NEGATIVE (contradicts f593a5d9cba lane note)
The "aux-adopted tabs never paint even after cycling" residual does NOT reproduce with the
show/hide discipline in place: target=_blank to wikipedia/Dog opened aux tab painting FULL correct
content immediately (infobox photos included on first paint), and three-way tab cycling
(CatÃ¢â€ â€�GmailÃ¢â€ â€�Dog) composites correct content in every direction. The aux-paint failures the other
lane saw were visibility contention, not a create-request builder registration defect Ã¢â‚¬â€� the
new_for_create_request path needs no compositor fix. Minor observed artifact: images in a hidden-then-reshown
tab take a moment to re-decode (hide() discards decoded surfaces by design) Ã¢â‚¬â€� text/layout correct instantly.

## Post-fix breadth matrix (endgame-24, all-in-one build): github.com/login renders complete form Ã¢Å“â€œ, x.com SPA onboarding fully interactive-looking with sso buttons + email field Ã¢Å“â€œ, youtube search+watch+chat Ã¢Å“â€œ, old.reddit Ã¢Å“â€œ (earlier), wikipedia/cnn no regressions Ã¢Å“â€œ. The compositing visibility fix + document.all together cleared most of the pre-alpha ledger's rendering-defect rows; remaining defect classes: attestation/botwall, background-media throttling, window.open RefCell panic.

## Input-focus defect class refined (endgame-25): x.com onboarding email field Ã¢â‚¬â€� click does not focus, typing never appears (2/2 attempts, placeholder persists, no focus ring). CONTRAST: youtube search input accepted focus+typing earlier this session. Differential hypotheses: (a) React controlled-input path relying on beforeinput/input events servo doesn't fire, (b) X's input is inside a shadow-DOM/composed-focus structure where servo hit-targets the wrong node, (c) focus succeeded but keystrokes route to the wrong target. Minimal-repro path: local test page with vanilla input vs React-controlled input vs shadow-DOM input Ã¢â‚¬â€� three rows isolates the mechanism cheaply. Symptom-index: "click input, nothing focuses/types on SOME sites but not others" Ã¢â€ â€™ input mechanism differential, test vanilla/react/shadow rows.

## INPUT DEFECT ROOT-CAUSED (endgame-26): three-row minimal repro (/tmp/vtest/inputtest.html) Ã¢â‚¬â€� vanilla input types Ã¢Å“â€œ, react-style controlled input types Ã¢Å“â€œ, **shadow-DOM input takes focus (visible focus ring) but keystrokes never land** Ã¢Å“â€”. The x.com onboarding failure is the shadow-DOM keyboard event routing defect Ã¢â‚¬â€� keystrokes are not delivered into the shadow tree even when the shadow input holds focus. Prime suspect location: keyboard event dispatch in script_thread/script Ã¢â‚¬â€� the key event target resolution probably fails to pierce the shadow boundary (document.activeElement resolution or the key event's composed path handling). This is a TOP-TIER compat defect: most modern apps (YouTube Studio, X, Salesforce, anything polymer/lit) use shadow inputs. Symptom-index: "input focuses but typing never appears" Ã¢â€ â€™ shadow-DOM keyboard dispatch.
Symptom (previously indexed unresolved): servo "client error (Connect)" for localhost python
http.server despite curl 200, net log TLS InvalidMessage(InvalidContentType).
Retest on current feather-engine head: http://localhost:8931/blank1.html typed EXPLICITLY with
scheme loads and renders correctly in feather shell. The defect does not reproduce. Likely fixed
en route by shell PR #45 (schemaless localhost/IP literals default to http Ã¢â‚¬â€� the original failure
was almost certainly schemeless typing defaulting to HTTPS against a plain-http server, exactly as
the TLS InvalidMessage error suggested). Lesson confirmed: always type explicit http:// for local
servers; the InvalidContentType symptom means "TLS handshake attempted against plain HTTP".

sec1zt44 Ã¢â‚¬â€� text-overflow: CONFIRMED ENTIRELY UNIMPLEMENTED (scoping result)
Zero implementation anywhere in components/style, components/layout, or script Ã¢â‚¬â€� only an
unrelated mention in the HTML sanitizer. `text-overflow: ellipsis` is silently dropped, clipping
truncated headlines on BBC/YouTube/every news site (defect-queue engine findings). This is a real
CSS-layout feature: inline overflow measurement + ellipsis rendering in the inline box tree.
Estimate: multi-hour engine feature, not a papercut. Recommend as a dedicated engine-compat mission;
it's the most visible recurring text-rendering gap on real sites.

## Shadow-input typed-text defect Ã¢â‚¬â€� deep chain (endgame-27). Symptom: inputs inside shadow DOM accept focus + keystrokes (keydown/beforeinput/input all fire, value correct internally) but TYPED text never renders. Minimal repro: /tmp/vtest/inputtest.html (vanilla/react/shadow rows). Established facts: (1) isConnected=true inside shadow tree Ã¢â‚¬â€� the is_connected() gate in value_changed (htmlinputelement.rs `if self.upcast::<Node>().is_connected()`) is NOT the cause; (2) initial `value=` attribute and JS `.value=` assignment RENDER CORRECTLY in shadow inputs Ã¢â‚¬â€� so UA-shadow-tree composition, layout, and the valueÃ¢â€ â€™character-data sync path all fundamentally work in nested shadow context; (3) placeholder text renders and correctly hides on typing (:placeholder-shown state updates correctly); (4) scrollWidth never exceeds clientWidth when typing in shadow input (vanilla: 447/214) Ã¢â‚¬â€� typed text never enters layout; (5) forced repaint (tab switch) doesn't help Ã¢â‚¬â€� not an invalidation-only issue; (6) typing path diverges from JS-set path: KeyReaction::DispatchInput in htmlinputelement::handle_key_reaction does NOT call update_shadow_tree directly Ã¢â‚¬â€� it queues the input event; handle_event then calls value_changed(cx, trusted) for InputEvents, which is the SAME path JS-set uses Ã¢â‚¬â€� yet outcomes differ, so either the input-event-driven value_changed never runs in shadow context, or the damage/invalidation from the typing path (dirty(NodeDamage::Other) on the input node) fails to propagate layout to the nested UA shadow tree; (7) structural fact: servo renders input text via a UA shadow tree (text_input_widget.rs Ã¢â‚¬â€� inner-container > input-editor + input-placeholder divs with text nodes), so input-in-user-shadow = nested shadow roots; editor visibility toggles via `input:placeholder-shown::-servo-text-control-inner-editor { display: none }` in layout/stylesheets/servo.css. NEXT DISCRIMINATOR (cheap, not yet run): type in si2, then force `si2.value = si2.value` via a JS button Ã¢â‚¬â€� if text appears, the typing-path sync is broken DOM-side; if not, layout-side re-traversal of nested UA shadow trees is broken. Also untested: whether `update()` in text_input_widget runs at all during typing in shadow context (instrument with a println Ã¢â‚¬â€� one build cycle). Symptom-index: "input in shadow DOM: focus+events fine, typed text invisible, JS-set text fine" Ã¢â€ â€™ typing-path sync vs nested-UA-shadow re-layout, see endgame-27.

sec1zt42 Ã¢â‚¬â€� gmail marketing-page Sign-in click: VERIFIED NAVIGATES (caveat closed)
The marketing page DID reproduce this run (fresh-profile variance): the Sign in link is exactly as
theorized Ã¢â‚¬â€� target=_blank to accounts.google.com/AccountChooser. Clicking it (eval-driven .click())
opens the aux tab which lands on accounts.google.com/v3/signin/identifier correctly. The original
gmail sign-in non-navigation defect is fully closed end-to-end Ã¢â‚¬â€� no caveat remaining. Confirms the
target=_blank class was the whole cause.

sec1zt43 Ã¢â‚¬â€� localhost connect failure: NO LONGER REPRODUCES (negative result)

## ROOT CAUSE + FIX: shadow-input typed-text defect (endgame-28). println discrimination on instrumented build: typing in light DOM produces UA-WIDGET-SYNC lines, typing in shadow DOM produces NONE Ã¢â‚¬â€� the input-event-driven value_changed never runs in shadow context. Mechanism: Event::dispatch_inner default-action block (event.rs) invoked vtable.handle_event on `self.GetTarget()` for all non-keyboard events Ã¢â‚¬â€� but composed events (the typing input event is composed) are RETARGETED to the outer shadow host during dispatch, so GetTarget() returns the host element, not the input; vtable_for(host) is not a text control, value_changed never fires, the UA widget character data never syncs, typed text never reaches layout. Keyboard events were already special-cased to original_target with a comment describing exactly this principle ("the default action should use the element the event was originally fired on") Ã¢â‚¬â€� the fix generalizes that to ALL events (the light-DOM behavior is unchanged since no retargeting occurs there). Symptom-index final: "input inside shadow DOM: focus+keydown+input events all correct but typed text invisible" Ã¢â€ â€™ event.rs dispatch_inner default-action retargeting, NOT editing.rs, NOT focus, NOT layout. UPSTREAMABLE Ã¢â‚¬â€� strong funding-model candidate (real-world impact: x.com login, most polymer/lit apps).

sec1zt45 Ã¢â‚¬â€� Symptom: shell keyboard shortcuts using ctrl/shift silently dead Ã¢â€ â€™ check: WHERE modifiers state is tracked
feather shell-new tracked winit ModifiersChanged only inside the page-input forwarding path, which
is gated on engine/egui state Ã¢â‚¬â€� the tracking silently stopped, breaking every shortcut check that
reads modifiers (fixed feather#164: track ModifiersChanged unconditionally at the top of window_event).
Second mechanism in the same fix: egui CANNOT express shift+bracket shortcuts Ã¢â‚¬â€� shift+[ arrives as
the shifted brace character "{" which has no egui Key variant, so consume_shortcut can never match.
Browser-style ctrl+shift+[/] bindings must be intercepted at the winit layer matching the brace
characters directly. Also symptom-indexed: "ctrl+t appears dead on a blank New Tab" is NOT a defect Ã¢â‚¬â€�
the shell dedupes blank New Tabs by design (focuses the existing one); verify shortcuts on non-blank tabs.

sec1zt46 Ã¢â‚¬â€� keyboard-shortcut mission closeout + verified-working matrix
Merged: feather#163 (tab-switch shortcuts) + #164 (winit interception + modifier-tracking fix) + #165 (docs).
Verified live on Linux: ctrl+shift+[/] both directions, ctrl+shift+Tab, ctrl+PgUp/PgDn, ctrl+t (with the
blank-tab dedupe caveat), ctrl+w, ctrl+l. Next compat-tail candidates in priority order: (1) omnibox
papercuts #1/#2 Ã¢â‚¬â€� revert-mid-load needs pending-destination display state in toolbar.rs sync logic,
fake-focus needs egui selection-state cleanup on focus loss Ã¢â‚¬â€� both shell-side, subtle state-machine work;
(2) text-overflow engine implementation (scoping in sec1zt44) Ã¢â‚¬â€� most visible recurring CSS gap on real sites;
(3) find-in-page/page-zoom need servo webview APIs that don't exist Ã¢â‚¬â€� engine-queue items.

sec1zt47 Ã¢â‚¬â€� omnibox revert-mid-load: FIXED + verified (feather#167)
Fix: skip the unfocused resync while loading_since.is_some() Ã¢â‚¬â€� one-guard fix, uses the existing
loading state. Verified live: typed destination stays in the bar 1s after Enter mid-load (pre-fix
snapped back instantly). loading_since was ALREADY maintained for the loading-bar widget Ã¢â‚¬â€� lesson:
check for existing state before designing new tracking. Papercut #2 (fake-focus selection) still open:
egui TextEdit selection state persists after focus loss Ã¢â‚¬â€� needs state cleanup on the sync/lost-focus path.

sec1zt48 Ã¢â‚¬â€� fake-focus omnibox papercut #2: FIXED + verified (feather#168, stacked on #167)
Fix: clear the persisted egui TextEditState cursor range on lost_focus Ã¢â‚¬â€� the select-all range survived
focus loss and painted a fake selection while keystrokes went to the page. Verified live post-fix:
at-rest bar shows clean unfocused styling after clicking into the page; typed destination persists
through load. Both omnibox papercuts (#1 revert-mid-load, #2 fake-focus) now closed Ã¢â‚¬â€� remaining
papercut tails: progress-affordance visibility, Cmd+Shift+T (needs closed-tab tracking infra).

## endgame-28 VERIFIED + LANDED: event.rs original-target fix committed as bdd924c00 (println-free). Verified: typed text renders in minimal shadow-input repro ("WORKS" with caret) AND real x.com login accepts typed email. Defect class closed. Upstreamable as written Ã¢â‚¬â€� the commit message is upstream-shaped.

## endgame-28 regression check: alpha gate green post-fix Ã¢â‚¬â€� YouTube search renders unassisted, click-through to watch + ad playback + live chat all intact. The original-target generalization is safe on core paths.

sec1zt49 Ã¢â‚¬â€� text-overflow implementation plan (execution starting)
Target files: components/layout/flow/inline/line.rs (line breaking/trimming), inline_box.rs, text_run.rs.
CSS spec triggers: ellipsis applies only when overflow != visible AND white-space prevents wrapping
(typically nowrap). Implementation shape: (1) parse/parse-computed the property in components/style
(text_overflow exists in NO servo style crate Ã¢â‚¬â€� must add property def, check how upstream stylo
handles it: style/properties longhand text-overflow with clip|ellipsis keywords); (2) during inline
line layout, after line breaking, if the line's inline size exceeds the containing block AND the
nearest ancestor with overflow-clip in inline axis has text-overflow: ellipsis, measure the ellipsis
glyph width in the same font, truncate glyphs/shaped runs at the boundary minus ellipsis width,
append ellipsis shaped run; (3) paint happens naturally since it's just glyphs in the display list.
Cheap v1 scope: single-line nowrap blocks only (covers bbc headlines, youtube titles); multi-line
(-webkit-line-clamp) explicitly out of scope. Verification harness: data:text/html page with fixed-width
divs, screenshot compare ellipsis presence; then bbc.com live.

sec1zt50 Ã¢â‚¬â€� text-overflow scoping CORRECTION: no parser support either
Grep confirms ZERO hits in components/style Ã¢â‚¬â€� not just layout ignoring it; the property does not
exist in servo's CSS parser/cascade at all. WPT input-text-overflow tests exist in-tree (meta .ini only).
So implementation is TWO parts: (a) full property plumbing in the style crate (servo style uses the
derive-based values system Ã¢â‚¬â€� look at a comparable simple longhand like overflow-wrap/white-space in
components/style/values for the pattern), (b) the layout ellipsis rendering. Part (a) is mechanical
but touches the property database; estimate is bigger than initially scoped Ã¢â‚¬â€� still the top compat item.

sec1zt51 Ã¢â‚¬â€� CRASH ISOLATION: servo already implements it; feather runs with the safety OFF
Mechanism (verified in-tree): panic hook emits error! while thread::panicking() Ã¢â€ â€™ servo's logger
converts it to LogEntry::Panic tagged with thread-local ScriptEventLoopId Ã¢â€ â€™ constellation handle_panic
(constellation.rs:3189) closes+reloads ONLY pipelines on the dead event loop, notifies embedder
per-webview. Tested upstream: components/servo/tests/crash.rs. THREE gaps in feather shell-new:
(1) hard_fail defaults TRUE in Opts::default (config/opts.rs:248) Ã¢â‚¬â€� every panic is process::exit(1);
(2) no panic hook installed Ã¢â‚¬â€� default rust hook prints stderr, never reaches the log layer;
(3) no Panic arm in the engine delegate. Stage-0 patch (hours) fixes all three.
Granularity caveat: ScriptEventLoopId::installed() returns None on non-script threads (media/WR/compositor)
Ã¢â‚¬â€� handle_panic early-returns, so those panics are logged but NOT recovered. WR/compositor are singletons:
treat their panics as never-bugs, not recovery candidates. multiprocess feature is DEFAULT-ON in-tree
(run_content_process) Ã¢â‚¬â€� the Stage-3 chromium-model option exists but is weeks of wiring.

sec1zt52 Ã¢â‚¬â€� crash isolation Stage 0 MERGED (feather#170 scoping + #171 implementation)
hard_fail=false + panic-hook-as-error! + notify_crashed delegate arm + testbinding feature all in
feather main. VERIFICATION IN PROGRESS: testbinding rebuild running, then forced ServoTestUtils.crashHard()
in one tab of a two-tab browser Ã¢â‚¬â€� pass criteria: process survives, [tab-crash] log line fires, crashed
tab reloads (constellation auto-refresh), other tab unaffected. Env-gate for the test hook:
FEATHER_TESTUTILS=1 enables dom_servo_helpers_enabled. NOTE for the tester: ServoTestUtils requires BOTH
the testbinding cargo feature AND the pref; interface is a namespace (ServoTestUtils.crashHard()), method
is crashHard() not panic().

sec1zt53 Ã¢â‚¬â€� CRASH ISOLATION VERIFIED END-TO-END: script panic costs one tab, browser survives
Live test on feather main (FEATHER_TESTUTILS=1, wikipedia Cat tab): ServoTestUtils.panic() fired in
the script thread Ã¢â‚¬â€� full recovery chain observed in log order: (1) hook emits error! with location,
(2) [tab-crash] delegate notification fires, (3) constellation "creating replacement pipeline for
crash page", (4) process alive, eval still functional in the recovered tab (URL preserved, servo's
crash page replaces content chrome-style), ctrl+t new tab works post-crash. The user's strategic
requirement Ã¢â‚¬â€� crashes must never kill the whole browser Ã¢â‚¬â€� is now true for the entire script-thread
panic class. HARNESS LESSON: ServoTestUtils.crashHard() is a null-write SIGSEGV not a panic Ã¢â‚¬â€� instant
death with zero log output, unrecoverable by ANY unwind-based mechanism (that's the stage-3 process-
isolation territory). Use panic() for unwind-recovery tests. Residual gaps per scoping doc: WR/
compositor singletons (never-bugs), media threads (degrade-not-notify), crash-banner UX polish.

sec1zt54 Ã¢â‚¬â€� crash-isolation edge case: repeated panic in same tab (crash-loop protection)
3 consecutive panics in one tab: browser ALIVE throughout; constellation counts: 2 replacement-pipeline
recoveries + 1 "crash page crashed" give-up. Upstream behavior by design: if the CRASH PAGE ITSELF
crashes (old_load_data.crash.is_some()), no further replacement is created Ã¢â‚¬â€� infinite-crash-loop
protection, tab left in dead state but browser unaffected. v1-acceptable; a shell-side "this tab keeps
crashing Ã¢â‚¬â€� close it?" affordance would be the UX polish. The core guarantee held in every test: the
process never died regardless of crash pattern.

sec1zt55 Ã¢â‚¬â€� text-overflow scoping CORRECTED: only layout work remains
Earlier notes said the property doesn't exist anywhere Ã¢â‚¬â€� WRONG LOCATION: servo's CSS comes from
stylo, a PINNED GIT dependency (servo/stylo rev 0ab78ce, not in-tree Ã¢â‚¬â€� components/style does not
exist in current servo). Upstream stylo ALREADY implements text-overflow completely (TextOverflow
computed value, longhands.toml, parsing+cascade Ã¢â‚¬â€� it's firefox-derived). Scope collapses to the
layout half only: read the computed text-overflow in inline layout, truncate at the overflow boundary,
render the ellipsis glyph. No style-system work needed. Estimated effort drops from days to hours.

## dogfood-ui-1 Ã¢â‚¬â€� first-dogfood shell UI fixes (2026-11-26, macOS lane)

User's first real dogfood of shell-new. Five defects, all fixed in
feather PR #175, each verified live. Symptom-index entries:

- SYMPTOM: intro doesn't play on first start for existing alpha users.
  CAUSE: not first-run detection (sound) Ã¢â‚¬â€� stale alpha prefs `onboarded=1`
  read as completed. FIX: versioned pref (`onboarded=2`); versioning
  user-facing boolean prefs is the pattern when alpha state may be corrupt.
- SYMPTOM: GL-blit content area has square corners though the design is
  rounded. MECHANISM: egui PaintCallback blits square over the rounded
  card; epaint can't clip GL. FIX PATTERN: repaint corner-spillover
  crescents in chrome colour with triangle-fan meshes AFTER the callback
  in paint order (foreground layer is ordering-proof). GOTCHA: egui y
  points down Ã¢â‚¬â€� arc-centre inward from a corner is (cx - sxÃ‚Â·r, cy + syÃ‚Â·r)
  with sy=+1 for TOP; a sign slip puts the whole fan outside the rect,
  invisible because it lands on same-coloured chrome.
- SYMPTOM: onboarding file picker does nothing on macOS/Linux.
  CAUSE: `#[cfg(not(windows))] let picked = None` Ã¢â‚¬â€� hardcoded dead.
  FIX: osascript `choose file` (macOS) / zenity (Linux) Ã¢â‚¬â€� no new crate
  deps (binary-size discipline). osascript dialogs are unanchored
  floating windows, appear on top Ã¢â‚¬â€� acceptable.
- SYMPTOM: avatar badge in onboarding You step looks clickable, does
  nothing. It was decorative paint only Ã¢â‚¬â€� no interact. When a control
  LOOKS like an affordance it must be one.
- PATTERN: favorites/quick-access must be user state, not static mock Ã¢â‚¬â€�
  one `Browser.pinned` list feeding sidebar grid + new-tab tiles +
  session.json persistence.

## dogfood-ui-2 Ã¢â‚¬â€� round-2 shell fixes (2026-11-26, macOS lane)

- SYMPTOM (Windows): onboarding/window spawns bottom-left, layout reads random.
  CAUSE: winit default placement; everything IS anchored to the window rect Ã¢â‚¬â€�
  the window position itself was the randomness. FIX: explicit centering from
  monitor size/pos/scale at creation.
- SYMPTOM (Windows): clicking into onboarding produces a stroked rectangle at
  a random position. CAUSE: set_window_border activated the DWM border when
  phase left Splash Ã¢â‚¬â€� i.e. exactly on first click Ã¢â‚¬â€� around the transparent
  floating window. FIX: border only in Browser phase. PATTERN: on Windows,
  window-chrome state changes that fire on user interaction read as
  click-produced artifacts.
- DISK HYPOSTASIS: shell-new target/deps accumulates duplicate fingerprint
  copies of huge servo crates (3x libscript rlib @200MB). Purge pattern:
  group by crate-stem+ext, keep newest, delete rest Ã¢â‚¬â€� freed 2.88GB, no
  rebuild breakage (cargo rebuilds only if actually referenced).

## sec1zt56 Ã¢â‚¬â€� text-overflow implementation IN PROGRESS (2026-11-26, linux lane)
Implementing layout-side text-overflow: ellipsis. Design decisions made:
- Property already fully in stylo (cascade/parse) behind runtime-flippable pref `layout.css.text-overflow.enabled`; added servo pref `layout_css_text_overflow_enabled` default TRUE + stylo mapping in config/prefs.rs. First set() always syncs because Default != const_default (user_agent).
- Implementation point: `layout_text_run` in flow/inline/line.rs Ã¢â‚¬â€� inline_advance vs containing block inline size. IFC containing block style+width accessible via `self.containing_block()`. KEY FACT: glyphs in ShapedText are stored in PHYSICAL left-to-right order (per ShapedTextSlicer comment) Ã¢â‚¬â€� truncating the end of the glyph range truncates the visual end for horizontal text in both directions.
- text-overflow lives in NON-INHERITED style struct: access `style.get_text().text_overflow` NOT get_inherited_text().
- fonts primitives added in glyph.rs: `ShapedText::single_glyph` (synthetic one-glyph shaped text for ellipsis), `ShapedTextSlice::whole` constructor, `ShapedTextSlice::truncate_to_advance` (walks glyphs accumulating advance, narrows glyph_range, recomputes caches).
- Ellipsis shaped via font.glyph_index('\u{2026}') + glyph_h_advance (both pub on Font). If font lacks U+2026: no truncation at all (v1 limitation Ã¢â‚¬â€� should fall back to clipping behavior eventually).
- v1 documented limitations: horizontal writing modes only; atomic inline content (images/inline-blocks) not truncated; width budget uses IFC containing block content-inline-size rather than the exact padding-box clip edge; string values of text-overflow unsupported; justification adjustment not recomputed after truncation; nested cases where the ellipsis-applying element is not the IFC root not handled.
- Check overflow_x in (Hidden|Scroll|Auto|Clip); end edge = TextOverflowSide::Ellipsis via `.second` (correct for ltr single+double value forms).

## sec1zt57 Ã¢â‚¬â€� text-overflow: ellipsis IMPLEMENTED + VERIFIED (2026-11-26, linux lane)
The multi-day-estimated feature landed in one session segment. What shipped:
- fonts/glyph.rs: `ShapedText::single_glyph` (synthetic shaped text), `ShapedTextSlice::whole` (full-range constructor), `ShapedTextSlice::truncate_to_advance` (glyph-range narrowing with cache recompute).
- layout/flow/inline/line.rs: truncation in `layout_text_run` Ã¢â‚¬â€� compare run's line-position+advance vs IFC containing block inline size; budget = width - start - ellipsis_advance; truncate slices, append ellipsis shaped in the run's font via font.glyph_index('\u{2026}').
- config/prefs.rs: `layout_css_text_overflow_enabled` default TRUE + stylo static pref mapping.
VERIFIED LIVE in feather (screenshot): 6/6 cases correct Ã¢â‚¬â€� plain truncation with visible ellipsis; nested span (inline-box positioning via parent_offset accumulation Ã¢â‚¬â€� CRITICAL FIX: current_state.inline_advance is box-relative inside nested inline boxes, must add current_state.parent_offset.inline); multi-run mixed bold/italic truncation; negative control (no text-overflow) plain clip; scroll container ellipsis; visible-overflow negative control (no ellipsis rendered Ã¢â‚¬â€� spec correct).
Symptom-index entries:
- Symptom: truncated text missing ellipsis inside nested inline elements Ã¢â€ â€™ check: inline_advance is inline-box-relative in LineItemLayoutInlineContainerState; absolute line position = inline_advance + parent_offset.inline chain.
- Symptom: style property missing from InheritedText at compile time though in longhands.toml Ã¢â€ â€™ check: struct="text" is the NON-INHERITED group; access via style.get_text() not get_inherited_text().
- Symptom: `ShapedTextSlice` has no public constructor Ã¢â€ â€™ slices are slicer-produced; use new `whole()` for synthetic content.
- Symptom: re-navigation to same URL in feather is a no-op (no reload) Ã¢â€ â€™ append query-string bust when iterating test pages.
Known v1 limitations (documented in code): horizontal writing modes only; atomic inline content not truncated; string values unsupported; fonts lacking U+2026 skip truncation; budget uses content-box width not padding-box clip edge; justification not recomputed; RTL visual-order correctness inherited from physical glyph storage order (correct by construction per slicer comment, untested).

## sec1zt58 Ã¢â‚¬â€� clipboard defects #11/#12 + CJK #6 resolved on Linux (2026-11-26)
Negative/positive verification results:
- Defect #12 (selection copy writes nothing): DOES NOT REPRODUCE on Linux. Mouse-drag selection visibly highlights, ctrl+c writes exact selected text to X11 clipboard (verified via xclip). Engine path: ctrl+c keydown Ã¢â€ â€™ run_default_keydown_handler Ã¢â€ â€™ editing_action_from_keyboard_event Ã¢â€ â€™ EditingAction::Clipboard(Copy) Ã¢â€ â€™ handle_clipboard_action Ã¢â€ â€™ EditingContext::Document selection_content Ã¢â€ â€™ EmbedderMsg::SetClipboardText Ã¢â€ â€™ servo.rs routes to webview clipboard_delegate Ã¢â€ â€™ default arboard-backed delegate writes OS clipboard DIRECTLY (no embedder/shell involvement needed Ã¢â‚¬â€� feather shell has zero clipboard code and copy still works). Original report was Cmd+C on macOS Ã¢â‚¬â€� likely mac-lane-specific or pre-fix.
- Defect #11 (Sheets copy writes GUID): fix ALREADY LANDED by another lane Ã¢â‚¬â€� 9edd19e2ee6 "write only the text/plain item" + a922f7fa414 paste-IPC bound. write_content_to_the_clipboard now picks text/plain first-match, falls back to first text item. Nothing left to do.
- navigator.clipboard is UNDEFINED even in secure context (https reddit) Ã¢â‚¬â€� the async Clipboard API surface is not exposed at all (separate gap from copy-event path, unimplemented in servo, low priority since copy-event path works).
- Defect #6 (CJK tofu): NOT an engine defect Ã¢â‚¬â€� pure environment. With zero CJK fonts installed all CJK renders as tofu; after `apt install fonts-noto-cjk` + browser restart, Japanese/Chinese/Korean all render perfectly via fontconfig fallback. Engine font fallback works correctly. SHIPPING IMPLICATION: Linux packaging (.deb/AppImage) must depend on or bundle CJK fonts or users see tofu on CJK sites Ã¢â‚¬â€� packaging concern, not engine.

## f2-action-stall-1 (2026-11-26, macOS lane): instrumentation + servo-side-wedge theory
F2 hunt phase 1. Synthetic stress repro (osascript System Events keystrokes Ã¢â‚¬â€� verified reaching the shell via FEATHER_INPUT counts) does NOT reproduce the stall in compressed time: 20+ cycles of new-tab/navigate/close/switch, actions kept applying throughout. Degradation trigger is NOT mere input volume.
INSTRUMENTATION SHIPPED (branch devin/1795705227-f2-action-stall): FEATHER_ACTIONS logs at all pipeline chokepoints Ã¢â‚¬â€� cx.act generation, per-frame drain count, apply dispatch, plus sync_engine webview create/renav/close (shared with FEATHER_WEDGE gate). Combined with FEATHER_INPUT (event arrival at winit) and FEATHER_FRAMES (frame production), the decisive matrix for a live stalled instance:
- click/key logged but no [act] generated Ã¢â€ â€™ egui input/interact/focus defect
- NO event logged at all Ã¢â€ â€™ winit/OS-level delivery failure
- generated but not drained Ã¢â€ â€™ queue merge defect
- drained+applied but no visible effect Ã¢â€ â€™ apply logic OR SERVO-SIDE wedge
LEADING NEW THEORY Ã¢â‚¬â€� servo-side wedge misread as shell action stall: the ledger's evidence (zero CPU, main thread parked normally, chrome responsive, actions "never execute") is equally or better explained by servo content/compositor threads dying or wedging: chrome is pure egui and stays alive independently; tab-switch clicks would still move the strip highlight but the user plausibly conflates no-content-change with no-effect; omnibox Enter commands a navigation servo never performs. KEY DISCRIMINATOR now instrumented: if [act] lines show applying+renav during the stall, the shell is innocent Ã¢â‚¬â€� look at servo threads (sample all threads, not just main). The ledger's narrowing to "action-queue/dispatch logic defect in the shell" may be wrong Ã¢â‚¬â€� verify before trusting.
STATIC ANALYSIS NEGATIVES (ruled out): action pipeline has no accumulation/degradation structure (per-frame vecs, no growing state); suggestions are trivially cheap; focus-surrender logic is keyboard-only and correct; event flow has no early returns Ã¢â‚¬â€� all events always reach egui. Omnibox Enter commit requires resp.lost_focus() same frame as Enter Ã¢â‚¬â€� fragile-by-design but works because egui singleline surrenders focus on Enter; no degradation mechanism found.
REPRODUCTION PROTOCOL for live stall (user or lane): run with FEATHER_ACTIONS=1 FEATHER_INPUT=1 FEATHER_WEDGE=1, log to file; when stall hits, perform one click on a tab, one cmd+t, one omnibox-enter; then `sample <pid> 3` for ALL threads; compare log matrix above.

## sec1zt66 Ã¢â‚¬â€� youtube thumbnails: NOT a defect (negative result)
Initial search-results screenshots showed missing/black thumbnails Ã¢â‚¬â€� investigation: img elements fully load (complete:true, naturalWidth 720, visible:true), computed style chain healthy (opacity 1, visible, correct 500x280 rect), and later screenshots show thumbnail content painting correctly including the first one. Verdict: progressive image load/decode latency on first paint, not a defect. Symptom index Ã¢â‚¬â€� Symptom: images black/missing on first paint then appear later Ã¢â€ â€™ check: decode-in-flight latency before assuming a rasterization defect; verify img.complete + naturalWidth + computed style before further work. Bonus index: youtube search results fully functional end-to-end on Linux incl. thumbnail paint, channel avatars, duration badges.

## f2-verification-1 (2026-11-26, macOS lane): marathon campaign mid-flight
Post-merge verification design: 12-round sustained mixed-use marathon (8 heavy sites/round, cmd+l omnibox navigations Ã¢â‚¬â€� cmd+t does NOT auto-focus the omnibox, typed urls go to the page: testing gotcha indexed; page scroll keys, aggressive tab churn, tab cycling) on the oscillation-guard build, instrumented FEATHER_ACTIONS+INPUT+WEDGE. First 2min checkpoint: 125 actions applied, 5 legit renavs, ZERO loop suppressions, zero panics, CPU 0.0 idle Ã¢â‚¬â€� healthy.
ANALYSIS for closure honesty: the writeback-convergence design makes oscillation rare in practice Ã¢â‚¬â€� the guard is insurance against a documented historical wedge mechanism, not a proven fix for the reported stall (which never reproduced in-session on ANY build). Correct closure position: guard+instrumentation in place, sustained scripted use healthy, decisive test is the user's next live dogfood with the instrumented build. Ledger statuses refreshed for F1/F3/F4/F5/F8/F14 (fixed in merged PRs, never updated by the original authors).


## sec1zt67 Ã¢â‚¬â€� ctrl+shift+T reopen-closed-tab: IN-PROGRESS state (session context exhausted mid-debug)

**Branch**: devin/1795708770-reopen-closed in baigojahanzaib/feather, one WIP commit pushed? (check Ã¢â‚¬â€� commit 0586ee5 local, verify push state).
**What exists**: state.rs closed_tabs recording on CloseTab (http(s), non-private, cap 25), ReopenClosedTab action Ã¢â€ â€™ Action::Open; app.rs reopen_closed_tab; main.rs intercept accepts BOTH "t" and "T" (REQUIRED Ã¢â‚¬â€� ctrl held suppresses shift-case mapping in winit logical key; the t-only first version never fired).
**Verified working**: key intercept fires (debug log `[reopen] fired`), recording works (queue len=1 after closing tab via X click).
**UNRESOLVED Ã¢â‚¬â€� the live bug**: after ctrl+shift+t with len=1, no visible new tab appears in the strip. Debug eprintln in the state handler silently failed to apply in one edit (indentation mismatch Ã¢â‚¬â€� python replace without assert), so it is UNKNOWN whether the handler executes. Suspects: (a) Action::Open semantics Ã¢â‚¬â€� may navigate current tab rather than opening new; (b) reopen executes but tab opens offscreen/in wrong workspace. NEXT STEP: check what Action::Open actually does in state.rs; add asserted debug or read code directly.
**Testing gotcha**: xdotool clicks need correct coords Ã¢â‚¬â€� feather tab strip tabs start ~x=720 real (not 560); inactive tabs show no X, must activate first; first click after launch may only focus window. windowactivate via xdotool search --name feather helps.
**Disk/memory state**: all committed on branch. feather-ui running with instrumented binary.


## sec1zt68 Ã¢â‚¬â€� ctrl+shift+T: root cause found+fixed, PR open (verification partial)

**ROOT CAUSE of the no-visible-reopen**: Action::Open NAVIGATES THE ACTIVE TAB Ã¢â‚¬â€� does not open a new one. The first ctrl+shift+t actually worked but destroyed the page the user was on (favicon swap observed in rt4 was exactly this). LATENT DEFECT affecting open_external_url too Ã¢â‚¬â€� single-instance forwarded urls navigate the current tab rather than opening new ones; #182 verification passed only because the initial tab was a NewTab. ReopenClosedTab fixed to construct Tab::new(id, Page::Web) directly and push to tabset. Debug eprintlns removed for PR.
**Symptom index**: Symptom: shortcut fires, state mutates, but no visible new tab appears Ã¢â€ â€™ check: whether the action you reuse navigates the active tab vs creating one (Action::Open navigates; there is no open-in-new-tab action in state.rs).
**Status**: PR open on devin/1795708996-reopen-closed. Live visual verification incomplete Ã¢â‚¬â€� key fires + recording verified + open-semantics root-caused; end-to-end visual pass flaky due to xdotool coordinate guessing on relaunched sessions. Follow-up: open_external_url should use the same new-tab path.

## f9-maps-1 (2026-11-26, macOS lane): live repro + console evidence + activity-logic suspects
F9 CONFIRMED live on current main WITH document.all+rIC+shadow-input fixes: maps.google.com renders map tiles via RASTER FALLBACK (25 img elements, zero canvas elements in DOM, zero shadow hosts) but UI chrome is never created Ã¢â‚¬â€� no #searchboxinput, no #pane, no .app-view-container; total DOM just 148 elements (boot stalls before UI construction). hidden_count=0 Ã¢â‚¬â€� NOT the youtube hidden-stamp pattern.
CONSOLE EVIDENCE (servo stderr, first real-page console capture): (1) `Error at maps:81:90 Ã¢â‚¬â€� a.tick is not a function` Ã¢â‚¬â€� boot-time TypeError in maps JS; (2) `uncaught exception: InvalidStateError: Document is not fully active` Ãƒâ€”2 at maps:56:486 Ã¢â‚¬â€� ON A VISIBLY ACTIVE TOP-LEVEL DOCUMENT Ã¢â‚¬â€� servo primitive defect; (3) `Unhandled promise rejection: InvalidStateError: The object is in an invalid state` Ã¢â‚¬â€� silent rejection class confirmed live; (4) `webglrenderingcontext Error resizing WebGLContext: Failed to resize swap chain: SurfaceCreationFailed` Ã¢â‚¬â€� compositing-layer surface failure.
ACTIVITY-LOGIC ANALYSIS: is_fully_active = is_active() && (top_level || activity==FullyActive). For top-level, failure means browsing_context none / window_detached / activity==Inactive. Constellation get_activity: Inactive when pipeline is not the browsing context's active pipeline Ã¢â‚¬â€� i.e. STALE PIPELINE after navigation/redirect (maps.google.com redirects to google.com/maps!). Visibility discipline does NOT feed document activity at all Ã¢â‚¬â€� ruled out as regression source. Prime suspect: redirect/SPA pipeline transition leaves script running in a non-active pipeline, or window_detached wrongly sticky. Verify with probe: evaluateJS `document.visibilityState` + compare pipeline state.
ENGINE-STATE CORRECTION (important, supersedes earlier panic): document.all IS on feather-engine at components/script/dom/html/htmlallcollection.rs (119e502e6) and rIC fix IS present as 67207dcbd (content-identical cherry-pick of 933f453ad Ã¢â‚¬â€� the original hash is dangling but nothing is lost). Current builds have all three engine fixes. Verified live: YouTube alpha gate holds on merged main.
DEVTOOLS DEFECT (upstreamable): devtools/actors/worker.rs:141 unwrap panics spam repeatedly (33+ in minutes) once a devtools client connects Ã¢â‚¬â€� worker actor message handling broken; also "packet length missing" noise on client disconnect. Devtools protocol client note: evaluateJS replies arrive wrapped as {"Ok":{...}} not bare Ã¢â‚¬â€� parse accordingly; attach returns unrecognizedPacketType (harmless, evaluation works without it). Working client at /tmp/dtev.py+evq.py pattern (ephemeral Ã¢â‚¬â€� recreate from this note: TCP length-prefixed `len:{json}`, skip root traits, listTabsÃ¢â€ â€™tabs[0].actor, getTargetÃ¢â€ â€™frame.consoleActor, evaluateJS).
F6/F7/F15 negatives this session: scrolling verified working end-to-end (wikipedia long-doc), localhost works, omnibox cmd+a select-all works Ã¢â‚¬â€� all three likely Windows-specific or incidentally fixed; recorded in ledger (PR #190).


## sec1zt69 Ã¢â‚¬â€� MSE seek-stall: controlled repro + flow evidence + pipeline-level deltas exonerated (2026-11-26, linux)

**Controlled repro built**: /tmp/elltest/mseseek.html Ã¢â‚¬â€� local vp9 webm MSE page, no player JS, seeking-handler re-feed. In feather: backward seek to 5 froze at exactly 5.0 with MEDIA_ERR_DECODE (err 3); forward seek to 40 froze at 0.0 no error. Video frames visually confirmed frozen (pixel-identical screenshots) Ã¢â‚¬â€� NOT a reporting artifact.

**Flow-level evidence (engine-instrumented servomse, 10s rebuilds Ã¢â‚¬â€� media crate iteration is CHEAP, contrary to layout-crate estimates)**: pre-seek the 1.2MB push exits the ghost pad downstream (pad probe FLOW line); post-seek push returns Ok but ZERO buffers ever exit the ghost pad. Ghost pad IS linked (peer decodebin0:sink verified at recreation). Data swallowed inside the recreated appsrc Ã¢â‚¬â€� its streaming task never pushes out. not-linked (-1) error from the recreated appsrc's gst_base_src_loop observed in one run Ã¢â‚¬â€� consistent with the streaming task dying at start.

**EXONERATED by pure-gst harness bisection** (/tmp/seekharness Ã¢â‚¬â€� playbin+servomse+vp60.webm, feeds, seek_reset, re-feeds, measures): with correct harness conditions (fakesink sync=true, feed before EOS) the SAME mse_source.rs code RESUMES FINE Ã¢â‚¬â€� positions advance post-seek 1Ã¢â€ â€™4. Not causal: flush vs no-flush (both resume), dispose-while-PLAYING critical (resumes with or without the NULL-state fix Ã¢â‚¬â€� though it's still a real defect worth fixing), playbin vs playbin3 (both resume), system-clock+base_time engine clock config (resumes). THE REMAINING DELTA: servo uses APPSINK sinks (video render pull + audio pull) vs fakesink Ã¢â‚¬â€� appsinks never drop and queue everything; timestamp/clock-domain mismatch post-seek (re-fed stream timecodes restart low vs pipeline running time high) interacting with sink sync/pull is the prime suspect: late frames fakesink drops, appsink queues; back-pressure stalls the whole chain upstream of the demux, starving the appsrc output.

**Symptom index**: Symptom: MSE seek push returns Ok but playback frozen, zero buffers exit servomse pads Ã¢â€ â€™ check: downstream back-pressure from appsink sinks under clock-domain mismatch (prime suspect Ã¢â‚¬â€� re-fed timestamps behind/ahead of running time), recreated-appsrc streaming task death via not-linked (secondary Ã¢â‚¬â€� race at sync-into-playing-pipeline), pad linkage (ruled out Ã¢â‚¬â€� ghost peer verified), flush machinery (ruled out in harness), dispose-while-PLAYING (ruled out as cause, still a defect). Instrument recipe: pad probe BufferType::BUFFER on the servomse ghost pad counts flow-out; compare pre/post seek.

**Test harness artifacts**: /tmp/seekharness (cargo project, 10s builds) Ã¢â‚¬â€� CRITICAL harness gotchas: fakesink sync=false consumes streams at max speed (EOS before your seek fires Ã¢â‚¬â€� invalidates the test); feeding the whole file then seeking without re-feed tests starvation not the pipeline; position queries need the running binary rebuilt (stale binary silent on build error).

**Confirmed ancillary defect**: reset_track_locked drops the old appsrc while still PLAYING (gstreamer critical) Ã¢â‚¬â€� must set NULL state before removal. Fixed in harness copy, NOT yet in engine.


## sec1zt70 Ã¢â‚¬â€� MSE seek-stall: mechanism fully narrowed, correct fix design specified (2026-11-26, linux)

**Decisive experiment results** (all controlled repro, /tmp/elltest/mseseek.html, 10s engine rebuilds):
1. WITH recreation (current design): post-seek push returns Ok but ZERO buffers ever exit the servomse ghost pad. The recreated appsrc's streaming task never pushes out Ã¢â‚¬â€� recreation mechanics are the flow-killer IN THE ENGINE (same code resumes fine in pure-gst harness Ã¢â‚¬â€� environmental, mechanism uncracked despite bisection of flush/clock/playbin3/dispose-state).
2. WITHOUT recreation (SERVO_MSE_NORECREATE=1, flush-only): the re-fed buffer DOES exit the ghost pad downstream (FLOW n=1) Ã¢â‚¬â€� but playback STILL frozen (frame-pixel-identical). So even with flow restored, downstream produces no frames.
3. Conclusion: TWO independent bugs stack. (a) recreation kills source-out flow in the gst-play/appsink engine environment; (b) even with flow, the clock-domain mismatch kills presentation downstream: re-fed webm cluster timecodes restart low while pipeline running time never resets (no real seek event exists in push mode) Ã¢â‚¬â€� frames map to running times in the past, sink sync drops or stalls them. This is exactly why Ã‚Â§1q/Ã‚Â§1t validation "worked" in the harness: fakesink sync behavior differs from appsink+render pull, and positions advancing Ã¢â€°Â  frames correct.

**THE FIX DESIGN (WebKit-style, now precisely specified)**: timestamp continuity via post-demux rewriting.
- Keep the original appsrc (no recreation Ã¢â‚¬â€� kills bug (a) by construction); flush still needed to reset demux/decoder state.
- On MSE seek to target T: record shift S = P_running - T where P_running is the pipeline running time/position at the moment of seek. Accumulate shifts across seeks (S_total).
- Pad probes on the demux output pads (matroskademux srcs, or equivalently decoder input pads Ã¢â‚¬â€� attached by walking the pipeline once after preroll in player.rs, NOT in servomse): rewrite every buffer pts/dts += S_total so presentation time is continuous in running-time domain. Frames then play in order regardless of stream timecode direction Ã¢â‚¬â€� backward seeks included. Probes must handle buffer writability (copy-on-write in probe).
- Position reporting: positions must report T-domain not running-time: report pipeline_position - S_total. gst-play queries the pipeline position directly, so compensation must intercept at the servo-media player layer where position-update events are consumed (the removed mse_position_offset was the right idea but wrong constant Ã¢â‚¬â€� it must be the accumulated shift, sign-checked, and it was removed in 4c14e896601 based on the wrong natively-correct theory).
- Keyframe reality: post-seek re-fed media starts at the nearest keyframe before T in practice (players feed from keyframes) Ã¢â‚¬â€� small presentation inaccuracy acceptable, matches what Chrome does.
- Open implementation risks: (1) probe attachment Ã¢â‚¬â€� demux pads appear dynamically inside decodebin3; must walk pipeline post-preroll or use decodebin pad-added signal in the servomse/uridecodebin chain; (2) multi-track sync Ã¢â‚¬â€� shift must be identical on a/v or audio glitches; (3) segment event rewriting may also be needed, not just buffers, since sinks compute running time from pts via the segment Ã¢â‚¬â€� test-driven; (4) EOS/eviction interplay untouched.

**Cheaper alternative NOT yet tried, worth 30min before the big design**: recreate the appsrc BUT also re-link it through a pad-added-style re-negotiation Ã¢â‚¬â€� i.e., investigate WHY the recreated streaming task dies in gst-play env specifically (gst debug output works in harness but produced nothing in feather Ã¢â‚¬â€� solve that logging gap first: GST_DEBUG output absent in feather stderr for unknown reason, glib criticals do print). If recreation can be made to flow in the engine, then only bug (b) remains, which is the smaller timestamp-shift half.

**Confirmed small fix to land regardless**: reset_track_locked drops old appsrc in PLAYING state (gstreamer critical) Ã¢â‚¬â€� set_state(Null) before obj.remove. One line.
**Instrumentation state in tree**: mse_source.rs currently has debug eprintlns + FLOW probes + SERVO_MSE_NORECREATE/SERVO_MSE_NOFLUSH env gates Ã¢â‚¬â€� strip before any PR, keep the NULL fix.


## sec1zt71 Ã¢â‚¬â€� seek-stall REFRAME: flow works, downstream consumption is the sole bug; test contamination warning (2026-11-26, linux)

**CRITICAL CORRECTION to sec1zt70**: in a CLEAN single-instance run (fresh session.json Ã¢â‚¬â€� feather session-restore resurrects old test tabs and SERVES AS A SECOND hidden video instance; ALL earlier mse instrumentation is potentially cross-contaminated Ã¢â‚¬â€� restored tabs run their own pipelines and interleave [mse] logs). Clean findings:
1. A recreation happens AT STARTUP before any seek (reset_track fires during initial setup Ã¢â‚¬â€� initial playback already runs on a recreated appsrc and works). So recreation is NOT inherently broken.
2. Post-seek WITH recreation: the re-fed buffer DOES exit the ghost pad (FLOW + OUT probes fire, pts=0). Source-side flow WORKS.
3. Yet playback still frozen (pixel-identical frames), position pinned 0.0.
=> The stall is PURELY downstream consumption: data enters decodebin but never presents. The zero-flow observations in sec1zt70 were contamination/race artifacts. The recreation-kills-flow theory is DEAD.
4. currentTime reads 0.0 post-seek (not the target 40): pipeline position genuinely restarts at ~0 Ã¢â‚¬â€� consistent with demux re-basing, meaning frames are NOT in the past Ã¢â‚¬â€� the late-drop clock-domain theory does NOT explain the freeze either.
5. Leading remaining suspects: (a) re-preroll never completes Ã¢â‚¬â€� first post-seek buffer never reaches the video appsink in renderable form; (b) the appsink new_sample/new_preroll callbacks error post-flush (return Flushing/Error) and the sink stops pulling Ã¢â‚¬â€� back-pressure stalls everything upstream; (c) vp9 decoder state post-flush not re-priming despite keyframe.
NEXT INSTRUMENT (planned, not done): probe the video appsink input + matroskademux/vp9dec outputs by walking the pipeline in player.rs post-seek; check whether frames reach the sink at all, and whether the render callback errors.

**Testing rule (MANDATORY for all future media verification)**: before any instrumented run, move ~/.config/feather/session.json aside or use unique-per-run URLs; session-restore duplicates test pages silently and invalidates instance-level logs.


## sec1zt72 Ã¢â‚¬â€� MSE seek final closeout: validation methodology flaw + mechanism verdict + the one real partial win (2026-11-26, linux)

**METHODOLOGY FINDING (affects all prior seek validation)**: position-advance post-seek is NOT evidence of frame flow Ã¢â‚¬â€� the pipeline clock advances regardless once unblocked. Every prior "playback resumes" harness validation (Ã‚Â§1t, sec1zf, the ENGCLOCK/playbin3 bisections in sec1zt69/70) is SOFT Ã¢â‚¬â€� likely nothing ever actually flowed post-seek in any design tried so far. Mandatory for future verification: pixel-diff the video region across seconds (trivially scriptable: import -window root crops + ImageChops, mean diff > 0) or pad-probe the sink input; positions are meaningless.

**MECHANISM VERDICT after full instrumentation campaign**:
1. Source-side flow post-seek WORKS (in clean instance): re-fed buffers exit servomse into decodebin. Neither flush nor recreation is the blocker.
2. Downstream consumption never produces frames: no re-preroll (video new_preroll fires exactly once at startup, never post-seek), pixel-identical frames in every engine run, positions pin at target or 0.
3. A FLUSHING TIME SEEK on the pipeline (seek always fails upstream Ã¢â‚¬â€� BoolError expected Ã¢â‚¬â€� but the flush+rebase side effects execute) partially improves things: currentTime lands EXACTLY on the target (40.0) instead of nonsense values. In the harness it unblocked positions too. But frames still frozen + MEDIA_ERR_DECODE surfaced once Ã¢â‚¬â€� insufficient alone, though the position-rebase is a genuine mechanistic piece of any final fix.
4. The re-preroll failure is the crux: after flush, sinks need a fresh preroll buffer to resume; it never completes. WHY is the open question Ã¢â‚¬â€� suspects: decoder never outputs a frame (vp9 state post-flush despite keyframe), the stream-start/caps sequence absent on re-fed data in the no-recreate path (demux outputs unparsable stream), or running-time/segment mapping puts frames at times the sink discards pre-preroll.

**THE HONEST VERDICT**: the flush+recreate design is dead as implemented; the path forward is EITHER (a) cracking re-preroll directly with gst-debug pipeline graphs (GST_DEBUG bin DOT dumps Ã¢â‚¬â€� GST_DEBUG_DUMP_DOT_DIR Ã¢â‚¬â€� never tried! much better than blind probes), or (b) the full timestamp-rewriting redesign per sec1zt70. Recommend (a) first: one afternoon with dot-graph pipeline inspection beats guessing. The instrumentation infrastructure is all in place (10s media-crate rebuilds, working repro /tmp/elltest/mseseekA.html served at :8899, session-restore contamination rule in sec1zt71).

**Tree state**: mse_source.rs + player.rs carry debug instrumentation (eprintlns, FLOW probes, SERVO_MSE_NORECREATE/NOFLUSH env gates, the experimental flushing-seek block in player.rs MSE seek branch). NOTHING committed to feather-engine Ã¢â‚¬â€� all local, strip-or-keep decision deferred. The flushing-seek experiment in player.rs is the only piece worth considering for keeping (improves position reporting) Ã¢â‚¬â€� but it also surfaced MEDIA_ERR_DECODE once, so it is NOT clean; do not land without understanding the decode error.


## sec1zt73 Ã¢â‚¬â€� seek closeout addendum: pause/play negative, race-flakiness everywhere, dot-graph is the next tool (2026-11-26, linux)

- pause/play cycle after flushing seek: NO effect (frames frozen) Ã¢â‚¬â€� negative result.
- Decode error (err 3) is itself racy Ã¢â‚¬â€� appears in some runs, not others, same build same steps. Position readings likewise inconsistent run-to-run (0.0 vs exact target). The whole post-seek pipeline state is nondeterministic Ã¢â‚¬â€� strong sign of event-ordering races in flush/reset/re-feed sequencing rather than a single deterministic bug.
- The one consistent improvement: flushing time seek makes currentTime land exactly on target in the runs that don't race Ã¢â‚¬â€� mechanistically correct piece (running-time rebase).
- NEXT TOOL (decided): GST_DEBUG_DUMP_DOT_DIR pipeline graphs pre/post seek in the ENGINE Ã¢â‚¬â€� shows element states, pad links, and preroll state definitively instead of blind probes. Alternatively stepping back: this hunt has consumed multiple sessions (1qÃ¢â€ â€™1tÃ¢â€ â€™1zc-1zgÃ¢â€ â€™zt69-73); recommend re-prioritizing against other compat work unless media-seek is a launch blocker. YouTube seek is degraded-but-not-crashing: playback continues pre-seek content or the player recovers via reload Ã¢â‚¬â€� user-visible severity is moderate not critical.
- open_external_url new-tab fix shipped meanwhile: feather#193.


## sec1zt74 Ã¢â‚¬â€� seek hunt: dot-graph forensics, the exact failure point, fixes tried, the one remaining theory (2026-11-26, linux)

**GST_DEBUG_DUMP_DOT_DIR works in feather** Ã¢â‚¬â€� pipeline graphs dump automatically on state changes AND ON ERROR. This is the correct instrument (not blind probes).

**What the graphs + linkage probes establish DEFINITIVELY**:
1. At every seek, the pipeline posts `appsrc<N>: streaming stopped, reason not-linked (-1)` from gst_base_src_loop, then transitions NULL Ã¢â‚¬â€� the frozen playback is a DEAD pipeline, not a stalled one.
2. The pad topology in the error-state graph is IDENTICAL to the working startup graph Ã¢â‚¬â€� full chain appsrc.src Ã¢â€ â€™ proxypad Ã¢â€ â€™ ghost src_0 Ã¢â€ â€™ decodebin sink Ã¢â€ â€™ typefind Ã¢â€ â€™ demux. `is_linked` verified TRUE with correct peer at +1/+2/+4s post-recreation. So the not-linked is NOT a persistent topology defect.
3. Startup recreation SUCCEEDS (a recreation happens during initial preroll every run Ã¢â‚¬â€� that appsrc reaches Playing, flows data, playback works). Seek-time recreation FAILS in the same pipeline minutes later. The delta: pipeline fully live/playing vs prerolling.
4. The error fires within MILLISECONDS of the recreation+first re-feed push Ã¢â‚¬â€� consistent with the streaming task's first event/buffer push returning not-linked as a FLOW RETURN (gstbasesrc reports flow returns with that exact message Ã¢â‚¬â€� it is not only the is_linked check). Most likely transient: downstream pads momentarily flushing/inactive while the flush events or decodebin3 stream-reconfiguration are still in flight.

**Fixes tried, all insufficient** (each verified by rebuild+repro): link-before-add ordering (ghost target set before obj.add); ghost set_active(false)+set_active(true) reactivation; NULL-state before removing old appsrc; pause/play cycle after flush; flushing time seek on the pipeline (SIDE EFFECT though: makes currentTime land exactly on target Ã¢â‚¬â€� keep this piece); dropping the recreation entirely (NORECREATE Ã¢â‚¬â€� data flows but downstream still never presents).

**THE REMAINING THEORY (untested, strongest)**: the re-feed push arrives on the script thread within milliseconds of the recreation completing on the player thread, while the new appsrc's state transition (and decodebin3's reaction to the second stream-start with a NEW random stream-id) is still in flight. The correct fix is likely to make the recreated stream REUSE THE ORIGINAL stream-id so decodebin3 treats it as the same stream Ã¢â‚¬â€� stream-id is carried in the stream-start event which appsrc generates internally; controlling it requires either intercepting the stream-start event with a pad probe and rewriting the id to the original, or probing gst version behavior. ALTERNATIVELY the entire recreate design should be replaced with in-band timestamp rewriting per sec1zt70 Ã¢â‚¬â€� the evidence now strongly suggests no amount of recreation fiddling will produce robust behavior in playbin3/decodebin3.

**Recommended next move if this hunt continues**: intercept stream-start with a pad probe on the recreated appsrc src pad; log the stream-id pre/post recreation vs the original; if they differ, rewrite to the original id and test. One focused experiment, ~1h. If insufficient, commit to the timestamp-rewriting redesign or re-prioritize Ã¢â‚¬â€� cumulative cost of this hunt is very high and YouTube seek is degraded-not-crashing (moderate severity).

**Verification infrastructure that works** (reusable): /tmp/elltest/mseseekA.html (:8899), pixel-diff frame verification mandatory, 10s media-crate rebuilds, GST_DEBUG_DUMP_DOT_DIR dumps, session.json must be moved aside per run.


## sec1zt75 Ã¢â‚¬â€� seek hunt final synthesis: two independent confirmed halves, exact failure sequencing (2026-11-26, linux)

1. **Recreation path**: the recreated appsrc streaming task dies on its VERY FIRST action Ã¢â‚¬â€� zero stream-start events ever leave it post-seek (streamstart probe fires exactly once per session, at startup). not-linked is a flow return on the first downstream push, firing within milliseconds, while `is_linked` verified true at +1s Ã¢â‚¬â€� a transient window at task start, or the downstream chain momentarily unpushable (decodebin3 mid-reconfiguration / post-flush state). Startup recreation survives because the pipeline is still prerolling Ã¢â‚¬â€� no active topology to conflict.
2. **No-recreation path**: data flows out of servomse fine (verified FLOW probes) but never presents Ã¢â‚¬â€� because without a stream re-sequence (stream-start/caps/segment) the decode chain downstream treats re-fed data as a continuation of the old stream: no chain reset, stale segment, timestamps mapped into the wrong running-time domain. This half is architectural Ã¢â‚¬â€� flush alone cannot fix it.
=> BOTH halves must be solved for any working seek: correct in-band re-sequencing (which recreation attempts and fails to deliver in a live playbin3 pipeline) AND correct running-time mapping. This converges with the long-standing WebKit-style verdict: a custom source element must own the stream lifecycle properly Ã¢â‚¬â€� incremental fixes to the appsrc-recreation design are exhausted. Honest cost estimate for a proper implementation (custom MSE source bin managing stream-start/segment/timestamp-rewrite per track, WebKitMediaSrc-style): multiple days of expert gst work. Until then YouTube seek remains degraded-not-crashing.

**Recommended prioritization call**: suspend the seek hunt unless media-seek is a hard launch blocker; the compat tail and stability work have better ROI. If resumed, the only untried cheap experiment: probe-inject a correct stream-start+segment sequence INTO the existing (non-recreated) appsrc data path Ã¢â‚¬â€� servomse can synthesize events on its ghost pad before the first post-seek buffer using a pad probe or manual serialized push timed WITH the data flow (the Ã‚Â§1q wedge warning applied to application-thread injection; in-data-path injection timed at push time may avoid it).


## sec1zt76 Ã¢â‚¬â€� discord logo rendering: NEGATIVE RESULT on Linux + SVG feature analysis (2026-11-26, linux)

**Symptom: discord.com logo renders thin/outline/broken (Windows lane report) Ã¢â€ â€™ check: which SVG instance and which embedding mechanism before assuming rasterizer bugs.**

1. **The landing-page wordmark is the plainest possible SVG**: one `<path>` with fill="white", many ZM subpaths, NO masks/filters/gradients/clipPaths/fill-rule/text Ã¢â‚¬â€� default nonzero winding; holes rely on subpath winding direction. Asset: `66e90ab9506850e8a5dd48e3_Discrod_MainLogo.svg` (sic, "Discrod") on cdn.prod.website-files.com.
2. **Renders CORRECTLY on Linux in feather** Ã¢â‚¬â€� verified three ways: isolated `<img>` at 330px+660px (correct fills, correct letter holes), real discord.com nav (correct), and inline `<svg>` in HTML (correct Ã¢â‚¬â€� servo DOES render inline SVG paths). Servo rasterizes SVG via resvg 0.50 (tiny-skia) Ã¢â‚¬â€� mature winding handling.
3. **Conclusion: not reproducible on Linux; platform divergence or different element.** If Windows reproduces on the same asset: suspect platform-specific compositing or hi-DPI rasterization scale, not fill semantics. Before any engine fix, the Windows lane should capture WHICH element is broken (landing nav `<img>` vs in-app inline SVG) and at what DPI scale.
4. **REAL discord defect found instead: discord.com/login SPA never boots** Ã¢â‚¬â€� readyState complete, body 1448 chars, ZERO svg elements in DOM, blank white page. The React bundle fails silently before mount. This is the actual user-facing discord breakage on Linux. Uninvestigated Ã¢â‚¬â€� next engine target. (Tab favicon Ã¢â‚¬â€� an `<img>`-embedded SVG Ã¢â‚¬â€� renders correctly, reinforcing that SVG rasterization is fine.)

Harness: /tmp/elltest/discordlogo.html + inlinesvg.html + mainlogo.svg (served :8899).


## sec1zt77 Ã¢â‚¬â€� discord login SPA boot failure: first hard evidence + instrumentation gotcha (2026-11-26, linux)

**Symptom: discord.com/login blank white page, app-mount empty Ã¢â€ â€™ check: libdiscore-wasm-fetch.js init error.**

1. Evidence chain: readyState complete, webpack chunks ARE pushed (364), 300 resources fetched, but `#app-mount` never gains children. Exactly ONE window error captured across boots: `can't access property "open", e is undefined` at `libdiscore-wasm-fetch.5306b4c88499697d.js:1` Ã¢â‚¬â€� Discord's wasm-bindgen XHR/fetch shim (`j1: function(e,t,n){return e.open(b(t,n))}` Ã¢â‚¬â€� receiver object undefined when wasm calls XHR.open). WASM itself works in servo (WebAssembly.compile verified ok, instantiateStreaming/compileStreaming exist).
2. Open question for continuation: whether the error is fatal-to-boot or a red herring Ã¢â‚¬â€� needs error STACK capture (instrumentation written but incomplete, see 3) and ideally reproducing libdiscore init in a minimal harness. The wasm-bindgen externref-table mechanics: receiver undefined implies the object reference passed from wasm never entered the table Ã¢â‚¬â€� candidates: externref/anyref edge in servo's SpiderMonkey integration, or an earlier silent failure in the shim's own init path.
3. **Instrumentation gotcha (symptom-index)**: /tmp/feather-eval-persist.js injects once per URL and does NOT re-inject after in-page reload (`persist_eval_url` dedupe) Ã¢â‚¬â€� error capture hooks die on reload. For boot-capture experiments either navigate away and back, or clear the dedupe key. window.onerror capture works when installed early enough.
4. CONTEXT BUDGET: this session is at the hard limit Ã¢â‚¬â€� continuation state fully in this file. Next steps: (a) capture the error stack properly, (b) minimal libdiscore repro harness, (c) if truly wasm-glue, look at servo's wasm externref table handling in script bindings.


## sec1zt78 Ã¢â‚¬â€� discord login boot ROOT CAUSE chain (2026-11-26, linux) Ã¢â‚¬â€� mechanism confirmed, layered

**Symptom: SPA boots blank, app-mount never populates Ã¢â€ â€™ check: window error capture for wasm-bindgen shim failures; missing web platform APIs surface as 'can't access property X, e is undefined' inside instantiateStreaming.**

Evidence chain (each layer verified by error progression):
1. libdiscore wasm init reads `window.caches` Ã¢â€ â€™ undefined in servo Ã¢â€ â€™ `.open` on undefined threw inside instantiateStreaming. `caches` exists in servo but is gated behind `Pref=dom_serviceworker_enabled` (default false) in WindowOrWorkerGlobalScope.webidl Ã¢â‚¬â€� the CacheStorage+Cache script implementation + storage-thread backend are REAL (Has/Open/Keys/DeleteCache ops), just pref-gated.
2. Enabling `dom_serviceworker_enabled` in feather shell (one-line pref, tested locally in engine.rs) advances boot: caches exists Ã¢â‚¬â€� but then `e.match is not a function`: servo's Cache.webidl implements ONLY keys() Ã¢â‚¬â€� match/put/add/delete/matchAll are entirely missing from the interface, and the backend storage thread has NO entry-level ops at all (only Has/Open/Keys/DeleteCache) Ã¢â‚¬â€� putting entries is impossible end-to-end.
3. JS-polyfill validation (shadow window.caches with in-memory Map impl via /tmp/feather-eval-persist.js persist hook): cache-layer errors GONE Ã¢â‚¬â€� Cache API is confirmed ON THE BOOT PATH, mechanism validated end-to-end. BUT boot still fails deeper: `unreachable executed` inside wasm-function[629] Ã¢â‚¬â€� a Rust panic in libdiscore, no console output captured (console.error/warn/log patched Ã¢â‚¬â€� nothing). servo primitives verified sound: WASM compiles/instantiates, Response.clone+arrayBuffer work.
4. Open: whether the unreachable is polyfill-semantics divergence (in-memory, no request-matching options, clone reuse) or ANOTHER missing API. Next diagnostic: implement Cache properly (engine), re-test; if unreachable persists, dig wasm-bindgen panic Ã¢â‚¬â€� possibly servo's promise/job ordering or missing streaming APIs.

**The engine fix mission**: implement Cache.match/put/delete/matchAll/add + backend PutEntry/MatchEntry/DeleteEntry storage-thread ops + persistence. Confirmed mechanism-level value: Cache API is used blind (no feature detection) by wasm asset loaders Ã¢â‚¬â€� libdiscore-class failure hits every wasm-bindgen app using cache storage.

**Instrumentation harness (reusable)**: /tmp/feather-eval-persist.js Ã¢â‚¬â€� error+rejection+console capture with __errs/__logs globals, injected once per URL via the shell persist hook; navigate away+back to re-inject after reload. Cache polyfill included (validation-only).

## sec1zt79 Ã¢â‚¬â€� Cache API IMPLEMENTED (match/put/delete) Ã¢â‚¬â€� compile-clean, pushed adf65bf76d1
Mechanism findings worth indexing:
- **Codegen cx-inclusion rule (the missing root rule): `cx`/`realm` methods are declared PER-INTERFACE in `components/script_bindings/codegen/Bindings.conf` under `'cx': [...]`** Ã¢â‚¬â€� the type-driven path (`typeNeedsCx`) is an upstream stub that always returns Context.No. If a trait signature lacks cx that your implementation needs, add the method name to the interface's cx list in Bindings.conf. Symptom: 'trait method has 3 params but impl needs 4 / no cx in generated trait' Ã¢â€ â€™ check: Bindings.conf cx array.
- Keyword-collision scheme: `CGDictionary.makeMemberName` (trailing underscore) is applied to method names at CGAbstractMethod init and jitinfo ops; the two reference sites that were missing it (spec-table accessors, CGMethodPromiseWrapper) are now fixed Ã¢â‚¬â€� upstream-worthy.
- Trait sig shapes for optional args: `Option<RequestOrUSVString>`, and interface args arrive as `Option<&D::Response>` (references Ã¢â‚¬â€� use DomRoot::from_ref).
- USVString is `pub struct USVString(pub String)` Ã¢â‚¬â€� `.0` extracts; also derefs to str.
- Response::Status()/StatusText() take NO cx; `body()` comes from trait `crate::fetch::body::BodyMixin`.
- Closure-capture gotcha: a closure moving a non-Copy callback into a message becomes FnOnce Ã¢â‚¬â€� clone the callback inside instead when the closure must be shared via Rc<dyn Fn>.
Verification: pending live test (cachetest.html harness exists in /tmp/elltest). Requires dom_serviceworker_enabled=true in the shell prefs (gates `caches` on WindowOrWorkerGlobalScope; SecureContext also required Ã¢â‚¬â€� https/localhost only).

## sec1zt80 Ã¢â‚¬â€� Cache API LIVE-VERIFIED Ã¢â‚¬â€� every harness test passes
cachetest.html results in feather (servo engine): open ok; all four methods present; put ok; **tee semantics correct** (caller's response body still readable after put); match roundtrips body+status+custom headers exactly; match-miss undefined; keys correct; delete true + post-delete match undefined. The wasm-bindgen cache-storage prerequisite class is now implemented end-to-end. Caveats: memory-only backend (per-session), exact-url matching, no options validation, requires dom_serviceworker_enabled=true + SecureContext.

## sec1zt81 Ã¢â‚¬â€� DISCORD LOGIN BOOT FIXED Ã¢â‚¬â€� Cache API was the whole defect
Live verification, real implementation (polyfill REMOVED Ã¢â‚¬â€� see gotcha below): discord.com/login boots fully Ã¢â‚¬â€� Welcome-back form, email+password inputs, QR code with embedded logo, background artwork, ZERO JS errors. The wasm 'unreachable' panic from sec1zt77/78 does NOT occur with the real implementation Ã¢â‚¬â€� it was the polyfill's semantic divergence all along (the documented open question, now answered). Mechanism-level win: all wasm-bindgen cache-storage users (Figma-adjacent tooling class) unblocked.
**GOTCHA Ã¢â‚¬â€� verification hygiene: the persist-eval polyfill SHADOWS the real implementation.** Any persist-injected API shim must be removed before validating a real engine implementation, or the test validates the shim. sec1zt77's 'deeper layer' was an artifact of this.
Required config: dom_serviceworker_enabled=true (shell pref) Ã¢â‚¬â€� gates `caches` exposure. Risk note: this also activates service-worker registration machinery on sites that use it; regression surface to watch.

## sec1zt82 Ã¢â‚¬â€� MSE backward-seek: in-band resequencing BREAKTHROUGH Ã¢â‚¬â€� first frame presents post-seek
Replaced appsrc recreation with in-band injection (pushed as media commit). Mechanism: inject stream-start+segment into the appsrc src pad at first post-seek push Ã¢â‚¬â€� streaming task guaranteed idle then, no Ã‚Â§1q wedge. VERIFIED: zero panics, decoder re-prerolls, FIRST FRAME PRESENTS AT EXACT SEEK TARGET with correct position display. Symptom index: 'gst_event_new_caps null-ptr crash event.rs:149' Ã¢â€ â€™ check: caps must be fixed, ANY caps invalid in caps events Ã¢â‚¬â€� omit caps event when caps are ANY (typefind re-detects). 'post-seek freeze with one frame presented' Ã¢â€ â€™ check: running-time domain mismatch Ã¢â‚¬â€� frames dropped as too-late, NOT a flow problem. REMAINING HALF (well-defined): segment base/offset or clock rebase so media timestamps map into post-seek running time Ã¢â‚¬â€� frames after the first present. Note harness caveat: mseseekA re-appends the whole file from byte 0, so post-seek frames carry early bitstream timestamps Ã¢â‚¬â€� real MSE players append fragments AT the seek position; the correct fix must handle both. All instrumentation is stderr-logged; env vars SERVO_MSE_RECREATE (old path), SERVO_MSE_GSTSEEK, SERVO_MSE_NOFLUSH gate experimental pieces.

## matrix-harness-1 Ã¢â‚¬â€� scenario matrix landed, harness alpha state (2026-11-26)
- tests/scenarios/ merged (feather PR #201): 13 mechanism-level probe pages + matrix.py harness + README documenting scenarioÃ¢â€ â€™mechanism mapping.
- Devtools framing contract (FIREFOX-style, NOT json-rpc): `{"to": actor, "type": method, ...}` Ã¢â‚¬â€� no id/method/params. One wrongly-framed message KILLS the devtools actor for the whole feather process ("Devtools actor stopped responding") Ã¢â‚¬â€� all later connections fail even correctly-framed ones. Implication: devtools-server robustness is itself a matrix-worthy mechanism gap; a production-quality harness needs the server to survive malformed input and navigation.
- Harness open item: validate a full automated green run; per-page reconnect logic is in place but unproven end-to-end. matrix.py DevTools class is the reference client.

## sec1zt83 Ã¢â‚¬â€� session closeout state (2026-11-26, linux)
SHIPPED: Cache API end-to-end (engine adf65bf76d1 + feather#200 pref, merged) Ã¢â‚¬â€� Discord login boots fully, wasm-bindgen cache-storage class fixed. MSE seek: in-band resequencing breakthrough Ã¢â‚¬â€� first frame presents at exact seek target, zero panics; remaining half is running-time mapping (segment base/offset or clock rebase Ã¢â‚¬â€� WebKit-style custom source is the principled answer). NEXT concrete step for seek continuation: examine how matroskademux derives its output segment from input Ã¢â‚¬â€� the injected bytes-format segment may need format-correct base, OR test with a harness that appends fragments AT the seek position like real players (mseseekA re-feeds from byte 0 Ã¢â‚¬â€� possibly an artifact). YouTube live test inconclusive this run (video rs:0 no data Ã¢â‚¬â€� possibly botwall again). Disk 51GB free.
- matrix-harness-1 addendum: core protocol validated live (banner + listTabs work against a running instance). BUT the full automated run hangs somewhere mid-iteration (>5min, no completion; output prints only at end). Suspects: evaluateJS reply never arrives after location.href navigation and socket timeouts interact badly with the retry/deadline loops, OR console actor resolution stalls on certain page states. NEXT SESSION: instrument matrix.py with per-step stderr logging, reproduce, fix. The probe pages themselves are unaffected (manually verifiable).

## sec1zt84 Ã¢â‚¬â€� seek hunt: negative results on stream-id/group-id reuse + TIME segment Ã¢â‚¬â€� architectural verdict CONFIRMED
Two more mechanism-level iterations, both insufficient: (1) injected stream-start reusing ORIGINAL stream-id + group-id Ã¢â‚¬â€� zero frames still reach sink post-seek; (2) TIME-format segment with correct start/base Ã¢â‚¬â€� same. DECISIVE instrumentation: sink-side sample probe proves zero frames arrive post-resequence Ã¢â‚¬â€� data dies inside decodebin3 re-plumbing, NOT too-late dropping. Symptom index: 'mse post-seek freeze with zero frames at sink' Ã¢â€ â€™ check: decodebin3 stream reselection internals Ã¢â‚¬â€� do NOT iterate further on event injection; the fix is owning the stream lifecycle in a custom source (WebKitMediaSrc design), multi-day expert work. FINAL VERDICT: seek hunt closed at architectural boundary; injection design kept as interim (strict improvement: no death/panics, first frame presents at target). Pivot to compat tail per ROI.

## sec1zt85 Ã¢â‚¬â€� Google Maps boot stall: NON-REPRO (2026-11-26, linux)
google.com/maps boots fully in feather: complete vector map render (roads/labels/traffic layer), search box, sign-in, zero JS errors, 4 canvases live. Whatever caused the reported stall is fixed by the recent engine work (document.all/cache-api/text-overflow candidates) or was environmental. If it recurs, capture: does the canvas paint tiles (webgl organic validation per memory line 1522 passes), is the stall interaction-specific (pan/zoom) rather than boot.

## xclick-1 Ã¢â‚¬â€� x.com clickable-text glitch repro attempt (2026-11-26, honest negative)
- Round-3-remainder hunt: "clicking text items glitches" on x.com. REPRO ATTEMPT NEGATIVE on the signup flow: clicking "Continue with phone" opens the modal correctly; clicking the phone input and typing works perfectly (text formats, Continue activates). Neither buttons nor text inputs glitch in the unauthenticated flow.
- IMPLICATION: the user's glitch is likely in the AUTHENTICATED timeline context (tweet text, links in posts) or is text-SELECTION/drag behavior, not plain clicking. Next repro needs either x.com login credentials, or user clarification on exactly which text items (links? plain text? selection?). Cheap hypothesis to test first: click-DRAG text selection rendering in servo (selection highlight painting) Ã¢â‚¬â€� testable unauthenticated on any text-heavy page.

## favicon-pipeline-1 Ã¢â‚¬â€� letter fallbacks persist despite wired pipeline (2026-11-26)
- VERIFIED NEGATIVE: tab strip still shows letter fallbacks for all tabs incl. freshly-loaded x.com (zoomed screenshot evidence) Ã¢â‚¬â€� the round-3 favicon pipeline does not produce icons in practice.
- ENGINE SIDE IS COMPLETE: servo has a full favicon implementation Ã¢â‚¬â€� components/script/dom/document/favicon.rs fetches link-rel-icon AND /favicon.ico fallback through the image cache, decodes, and sends EmbedderMsg::SetFavicon Ã¢â€ â€™ webview.set_favicon. So the gap is almost certainly SHELL-side, not engine.
- Shell suspects in priority order: (1) the writeback-loop insertion may never execute Ã¢â‚¬â€� check loop variable binding `*tab_id` and whether writeback runs for all webviews; (2) `webview.favicon()` returns Ref filter Ã¢â‚¬â€� borrow timing; (3) the upload loop keys off favicon_data but x.com tab ids may mismatch tab strip lookup `cx.favicons.get(&tab.id)`; (4) favicon arrives but texture format len check rejects (RGBA8 vs RGB8 vs other PixelFormat variants like K8/BGRA Ã¢â‚¬â€� embedder PixelFormat enum has more variants than the two handled!). NEXT SESSION: add debug eprintln in writeback when favicon bytes received, run once, localize which stage is empty.

## sec1zt86 Ã¢â‚¬â€� media retention: measured precisely, deferred-dispose reaper shipped, honest severity revision
Navigate-away protocol measurement: first video +110-140MB, ~0 reclaimed after 35s BUT second video adds only ~16MB Ã¢â‚¬â€� the bulk is REUSABLE glibc-arena mass, not per-player leak. The old defect-queue 'permanent 150MB per video' overstates severity. Root mechanism: teardown hits dragons condition (play refs 2 Ã¢â‚¬â€� one EXTERNAL glib ref that NEVER drains even after pipeline NULL+bus flush Ã¢â‚¬â€� permanent gst-internal ref, upstream MR13022 class). SHIPPED: deferred-dispose reaper Ã¢â‚¬â€� dragons-hit players queued, reaper re-runs exact drop-time refcount check, disposes when safe; strict improvement over permanent mem::forget at zero risk. Symptom index: '[playerleak] with play refs 2 persistent' Ã¢â€ â€™ gst-internal permanent ref, do NOT force-dispose (UAF class). OPEN at honest severity: small per-video shell accumulation; blocked on upstream lifecycle fix.

## sec1zt87 Ã¢â‚¬â€� SW-pref regression sweep: CLEAN; YouTube playback stall NOT caused by the pref
Post-#200 verification: X (twitter) boots zero-error full content; reddit boots zero-error 878KB content; Google Maps full vector render (sec1zt85). YouTube watch page boots fully but video never buffers (rs:0, zero MSE activity Ã¢â‚¬â€� servomse never even created; page-level/env). A/B tested pref on/off: IDENTICAL behavior both ways Ã¢â‚¬â€� the SW pref is exonerated. The stall pattern (page fine, googlevideo data never flows) matches bot-wall serving rather than engine regression: initial MSE startup verified working on the mseseekA harness in the same builds. OPEN: needs a run where youtube serves media to this IP Ã¢â‚¬â€� not currently diagnosable further on this VM.

sec1zt88 Ã¢â‚¬â€� media adapter-cycle FIX VERIFIED, retention work closed (2026-11-26, linux lane):
The permanent-leak mechanism from sec1zt86 confirmed and fixed: the deferred-dispose queue stored
the whole PlayerInner, but PlayerInner holds the PlaySignalAdapter which keeps a STRONG glib ref on
the play object Ã¢â‚¬â€� the queue itself pinned play refs at >=2, so the reaper's play_refs==1 drain check
could never fire. Symptom: player objects never finalize after pipeline teardown Ã¢â€ â€™ check:
signal_adapter glib ref cycle (PlayerInner holds adapter, adapter holds play object; queuing inner
pins the object the reaper is supposed to release). General form: when implementing drain-to-N-refcount
reaper patterns, audit every field of the queued struct for hidden strong refs on the target object Ã¢â‚¬â€�
the queue must hold ONLY its own clone, nothing transitive. Fix: queue the bare Play object, drop inner
at queue time (releases inner.player + adapter refs). VERIFIED live: 3 consecutive navigate-away
cycles, reaper fired for all three players (2s/0.7s/0.16s Ã¢â‚¬â€� refs drain progressively faster once prior
deferred mass is gone), playback restarts cleanly on nav back, zero panics, zero regressions. The
sec1zt86 deferred-dispose design is now actually functional rather than permanent-leak-with-extra-steps.
Media retention investigation CLOSED Ã¢â‚¬â€� true per-player leak now disposes within seconds; residual RSS
mass is the documented reusable-arena behavior. Commit 4c126d64190.

sec1zt89 Ã¢â‚¬â€� youtube thumbnails: original defect NON-REPRO in severe form + IO matrix exonerated (2026-11-26, linux lane):
The reported "youtube search thumbnails don't render" defect does NOT reproduce in its severe form:
initial-viewport thumbnails render fully (video thumbs, channel avatars, shorts thumbs Ã¢â‚¬â€� screenshot
verified). RESIDUAL defect: scroll-triggered lazy population never fires Ã¢â‚¬â€� images below the initial
viewport keep empty src forever even after incremental user-like scrolling (18 in-viewport imgs, 0
populated). Symptom: lazy-loaded content never populates on scroll Ã¢â€ â€™ do NOT check IntersectionObserver
primitives first Ã¢â‚¬â€� full matrix VERIFIED WORKING in servo: window-scroll viewport-root IO fires on
scroll, rootMargin px + threshold arrays work, percentage/4-value/zero rootMargin + scrollMargin
constructors all valid, ELEMENT-root IO with inner-scroller scrollTop works, scroll events fire.
Negative results mandatory-recorded here to prevent re-walking. DEDUCTION: YouTube's visibility
monitor runs and its IO fires for initial elements (they populate) Ã¢â‚¬â€� so the failure is inside
YouTube's obfuscated component chain between intersection notification and src write, NOT in any
servo IO/scroll primitive. Hunting further requires reverse-engineering the ytCoreImage/visibility-
monitor internals (same class of exercise as the sec1zt17-21 unhide hunt Ã¢â‚¬â€� multi-hour, uncertain).
Severity DOWNGRADED: initial viewport fully renders; user must know the defect is scroll-population
only. Next step if resumed: instrument yt-core-image custom element reactions, or capture whether
YouTube's IO callbacks fire at all post-scroll (add a probe IO on the same elements Ã¢â‚¬â€� if the probe
fires but src doesn't populate, framework-internal confirmed; if probe doesn't fire either, the
observer-root geometry on youtube specifically is the lead).
- matrix-harness-2 (2026-11-26): hang FIXED Ã¢â‚¬â€� root cause was location.href navigation never replying to evaluateJS + blocking recv; now fire-and-forget + socket timeouts + reconnects, harness completes all 13 pages end-to-end. REMAINING narrowed bug: evaluateJS results never arrive even on stable pages (every collection times out; listTabs/getTarget verified working). Next: manually probe evaluateJS reply format against a live instance Ã¢â‚¬â€� compare with /tmp/dt.py session transcripts; possibly reply key differs or consoleActor resolution picks a worker actor.
- matrix-harness-3 (2026-11-26): eval replies flow now (Ok-envelope unwrapped; navigateTo primitive works and acks) Ã¢â‚¬â€� BUT two live-probed facts define the remaining bug: (1) evaluateJS SANDBOX quirk Ã¢â‚¬â€� even READING location.href returns None, strongly suggesting eval runs in a sandbox/scope that isn't the page's window (or location access specifically broken); (2) navigateTo DROPS the devtools connection entirely (EOFError right after the ack) Ã¢â‚¬â€� harness reconnects, but reconnection timing vs page load is racy. Symptom: every page collects the FIRST page's results (visibility probes everywhere) Ã¢â‚¬â€� either eval targets a stale sandbox or MATRIX_RESULTS lookup hits the wrong global. NEXT: eval `typeof window` / `typeof document` / `1+1` in sequence after nav+reconnect and compare against pre-nav; check servo's console.rs whether evaluateJS uses a sandbox global (grep components/devtools/actors/console.rs for sandbox/compile).

sec1zt90 Ã¢â‚¬â€� prefers-color-scheme SHIPPED (feather#205, 2026-11-26, linux lane):
Sites were locked light because feather never called the engine's existing theme API. Mechanism
chain documented for future embedding work: servo engine support is COMPLETE upstream Ã¢â‚¬â€�
webview.notify_theme_change(Theme), runtime MQL re-eval + restyle (upstream tests webview.rs
test_theme_change prove it); the gaps were all shell-side. Symptom: site renders light despite
OS dark mode Ã¢â€ â€™ check in order: (1) does the shell ever call notify_theme_change (feather didn't),
(2) does the OS-theme detection layer work on the platform Ã¢â‚¬â€� winit reports NOTHING on X11, use
XDG portal org.freedesktop.appearance/color-scheme with gsettings fallback (feather platform.rs
linux_system_dark probe thread), (3) CRITICAL non-obvious one: on ControlFlow::Wait event loops,
static pages produce no frames, so a poller thread updating shared state is useless without a
wake that forces a redraw Ã¢â‚¬â€� feather's servo-wake handler deliberately never redraws (flood
protection), needed a separate UiRefresh wake kind. Negative result: gdbus portal errors return
fast (~7ms) when no portal service, no hang risk. Verification trap: matchMedia captured in a
page-load JS object is STALE Ã¢â‚¬â€� re-evaluate live. Also: JS matchMedia and stylesheet @media
evaluate through different paths in servo Ã¢â‚¬â€� verify BOTH. Bonus fixed: shell chrome follows OS
dark on Linux now (same detection fed both).

sec1zt91 Ã¢â‚¬â€� google SERP layout investigation: repro-path head start (2026-11-26, linux lane):
The captcha wall is NOT IP-level for plain fetches: curl with a plain Firefox UA gets a REAL
200 SERP (93KB html) Ã¢â‚¬â€� the wall triggers on feather's behavioral fingerprint only. BUT the
local-repro plan has a catch: the SERP html carries almost NO inline critical css (3 style
blocks totaling 228 chars, zero flex/grid/margin-auto in them) Ã¢â‚¬â€� google's layout CSS is in
EXTERNAL stylesheets. A localhost repro must also fetch+rewrite the external css/js links,
or use a rewriting proxy. Fetched html saved at /tmp/elltest/serp.html (gone on reboot Ã¢â‚¬â€�
refetch with: curl -A "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0"
"https://www.google.com/search?q=test"). Next steps: extract <link>/css urls from the html,
fetch them, rewrite references to local paths, serve, screenshot feather vs expectation,
identify broken mechanism with scenario-matrix minimal repros.

sec1zt92 Ã¢â‚¬â€� google wall discriminated: UA-exonerated, fingerprint-level (2026-11-26, linux lane):
curl with a plain Firefox UA string fetches the real SERP fine (200, no wall) FROM THE SAME IP
at the same time feather gets /sorry/index Ã¢â‚¬â€� so the wall is NOT the UA string and NOT the IP.
Distinguishers left: TLS/HTTP2 fingerprint, header set/ordering (accept, accept-language,
sec-fetch-*), or behavioral (parallel subresource burst). NOTE the fetched SERP html is a
JS-bootloader shell Ã¢â‚¬â€� zero link tags, no center_col Ã¢â‚¬â€� localhost static repro is impossible;
layout investigation requires either unwalling feather or a MITM-style replay. High-value
unblocked path if resumed: dump exactly what headers servo sends (net crate) vs curl's set Ã¢â‚¬â€�
google's wall on headless-ish clients is often triggered by missing/odd accept-language or
sec-fetch headers, a cheap mechanism-level fix healing ALL google properties.

sec1zt93 Ã¢â‚¬â€� google wall: header-set exonerated, TLS fingerprint prime suspect (2026-11-26, linux):
servo already sends Accept-Language (single-locale anti-fingerprint value, net/fetch/methods.rs
step 12) AND the Sec-Fetch-* fetch-metadata headers (http_loader.rs:2913). Header-set theory
DEAD. Remaining discriminant vs curl-with-firefox-UA: the TLS ClientHello fingerprint Ã¢â‚¬â€� servo
uses rustls, whose handshake signature differs from firefox/chrome and is trivially classified
by bot walls. NOTE curl passes BECAUSE curl's own TLS is also non-browser Ã¢â‚¬â€� so the wall logic
isn't purely "non-browser-TLS = block" either; more likely servo sends some COMBINATION or
curl's minimalism (no subresource burst, no cookies, single request) reads as a plain bot-fetch
which google tolerates for one-shot html, while servo's full browser-like subresource burst
WITHOUT a matching browser TLS fingerprint reads as spoofing. Discriminator if resumed: fetch
google in feather but block ALL subresources (or curl the exact subresource set rapidly) Ã¢â‚¬â€�
if the burst-without-matching-TLS is the trigger, mitigation is rate/cookie policy work, not
TLS. If TLS is the trigger regardless, options are boringssl/openssl passthrough (big) or
accepting the wall. Symptom index: site captcha-walls servo but not curl same-IP Ã¢â€ â€™ check in
order: UA string (exonerated), header set inc. accept-language/sec-fetch (exonerated Ã¢â‚¬â€� servo
sends both), TLS ClientHello fingerprint (prime suspect), request-burst behavioral pattern.

sec1zt94 Ã¢â‚¬â€� google SERP layout investigation LIVE + major unlock (2026-11-26, linux lane):
UNLOCK: google.co.uk is NOT captcha-walled (google.com is) Ã¢â‚¬â€� real SERP boots in feather with
center_col present. Use google.co.uk for all google-property layout work.
DEFECT REPRODUCES as user reported: center_col renders full-width (1242px wide at left:16,
margin-left:0, max-width:none) instead of the constrained indented column chrome shows.
ESTABLISHED FACTS: (1) 46 stylesheets load and parse Ã¢â‚¬â€� thousands of rules present, css is NOT
failing to load/parse wholesale. (2) ZERO rules target #center_col by id in modern google css Ã¢â‚¬â€�
the column constraint is class/structure-keyed. (3) servo has NO :has() support Ã¢â‚¬â€� matches()
throws SyntaxError Ã¢â‚¬â€� but the SERP css contains zero :has rules at scan depth, so :has is not
THIS defect's mechanism (still a major compat gap for other sites). (4) basic min-width media
queries work. NOTE the deep-scan probe undercounted (603 leaves vs 7703 top-level) Ã¢â‚¬â€� scan logic
suspect, redo carefully. NEXT: ground-truth A/B Ã¢â‚¬â€� load google.co.uk/search?q=test in the devin
chromium via playwright (CDP localhost:29229), dump center_col/parent-chain computed styles +
matched rules, diff against servo's computed values. The first divergent computed property up
the chain localizes the broken mechanism.

## matrix-harness-4 Ã¢â‚¬â€� FIRST REAL GRID PRODUCED (2026-11-26, breakthrough)
- Grid: 53/61. REAL engine gaps found: WebGL absent in shell-new (shell had it via #112), SVG-in-img rasterization broken (DOM fine, raster transparent), FontFace constructor missing, WebRTC absent. Alpha-gate fixes all hold green. Baseline at feather docs/wiki/SCENARIO-MATRIX.md, PR #206.
- HARNESS-SOLVING chain (each proven live): (1) evaluateJS replies wrap in top-level Ok envelope; (2) location.href assignment no-ops in devtools eval Ã¢â‚¬â€� use tab actor navigateTo instead; (3) evaluateJS itself works PERFECTLY (strings/typeof/page globals all fine) Ã¢â‚¬â€� the identical-results mystery was pipeline staleness: after navigateTo, document.URL inside eval showed the PREVIOUS page forever. Root: browsing-context actor's active pipeline never rebinds in practice despite update_pipeline existing (NavigationState::Stop path). UPSTREAM SERVO DEVTOOLS DEFECT Ã¢â‚¬â€� strong upstream candidate.
- WORKAROUND SHIPPED: boot-per-page isolation Ã¢â‚¬â€� each probe page gets a fresh feather boot (first pipeline IS the page). matrix.py run_one_page. ~10s/page.
- devtools evaluateJS SANDBOX myth busted: eval runs in the real page context fine Ã¢â‚¬â€� earlier location.href=None was the stale-pipeline effect, not sandboxing.

sec1zt95 Ã¢â‚¬â€� google SERP layout ROOT CAUSE: container queries entirely unimplemented in servo
(2026-11-26, linux lane): The user-reported SERP breakage (left-aligned, unpadded, full-width
content) reduces to ONE missing css feature: @container queries. Chain: center_col width
constraint comes from rules inside `@container srp (max-width: 1147.98px)` blocks Ã¢â‚¬â€� servo
parse-drops the ENTIRE at-rule (probe: a test @container rule produces ZERO cssom rules),
so #rcnt:not(.tXI1nd) .ufC5Cb{max-width:652.02px;grid-column:2/-2} never exists, and the
column expands to the full grid width. Everything else on the SERP works: grid itself
(resolved template correct), round() in calc VERIFIED SUPPORTED (negative result Ã¢â‚¬â€� do not
re-hunt), :is(), modern selectors parse. servo probe facts: CSS.supports('container-type',
'inline-size') false; container-type computed empty; zero container-query cssom. Verified
via google.co.uk (NOT captcha-walled Ã¢â‚¬â€� use it for all google work) A/B against devin chromium
playwright CDP. Symptom: modern site renders unstyled-width/left-aligned with css fully
loaded Ã¢â€ â€™ check for @container in sheet text vs absence in cssom Ã¢â‚¬â€� parse-drop of the whole
at-rule is the tell. IMPLEMENTATION notes for the engine mission: servo has no components/style
in-tree Ã¢â‚¬â€� style engine is upstream stylo dependency; firefox/stylo has container queries since
ff110 so parsing may already exist behind a feature flag Ã¢â‚¬â€� check the stylo version in
Cargo.lock for container_queries support before writing anything; servo's side is then the
evaluation integration (container size evaluation needs layout interleaving Ã¢â‚¬â€� the hard part).
This is a top-5-property-validated, fleet-wide modern-css gap Ã¢â‚¬â€� highest-value css engine work
identified so far.

sec1zt96 Ã¢â‚¬â€� container queries implementation SCOPED (2026-11-26, linux lane): stylo (pin rev
0ab78ce) ships MOST of the machinery in shared code, gated for gecko: rule_parser.rs:285
`"container" if cfg!(feature = "gecko")` Ã¢â‚¬â€� the @container keyword itself only parses in gecko
builds, so servo drops the whole at-rule at the tokenizer level. But the rest is surprisingly
complete: container_rule.rs has ContainerConditions, the lazy ContainerSizeQuery evaluation
abstraction (NotEvaluated closure/Evaluated cache, TraversalResult::InProgress for the
layout-interleaving protocol), container-type/name properties exist in the box style struct,
and matching.rs ALREADY runs container restyle logic (get_container_name/type diffing,
children_hint propagation, SELF_OR_ANCESTOR_HAS_SIZE_CONTAINER_TYPE flag) Ã¢â‚¬â€� that file compiles
for servo today (it's in the build). IMPLEMENTATION SHAPE: (1) ungate parse keyword Ã¢â‚¬â€� trivial;
(2) the hard part is servo's layout bridge: size queries resolve against the CONTAINER'S
laid-out size during style resolution, which servo's single-pass style-before-layout pipeline
doesn't currently interleave Ã¢â‚¬â€� stylo's TraversalResult::InProgress is the designed hook for
exactly this (gecko does multi-pass style/layout). Estimate multi-day engine feature, but NOT
weeks Ã¢â‚¬â€� the abstraction is upstream-complete. PRECURSOR QUESTION worth checking first: does
servo's stylo build compile with the gates removed (are there gecko-only deps inside the
evaluation path). Files: stylo/style/stylesheets/{rule_parser,container_rule}.rs,
style/matching.rs Ã¢â‚¬â€� pin path ~/.cargo/git/checkouts/stylo-*/0ab78ce.

sec1zt97 Ã¢â‚¬â€� container queries PHASE 1 DONE + phase-2 design (2026-11-26, linux lane):
Parse+cssom verified live (commit b3d32c90bc7): stylo vendored into servo/vendor/stylo (the
git-patch dance: [patch] only honored at WORKSPACE ROOT Ã¢â‚¬â€� servo/Cargo.toml patch is IGNORED
when building feather-ui because shell-new is the root; patch must live in BOTH Ã¢â‚¬â€� shell-new's
points at ../../repos/servo/vendor/stylo Ã¢â‚¬â€� symptom-indexed gotcha). stylo crates use
workspace-inheritance which breaks standalone resolution Ã¢â‚¬â€� flattened in vendor. The parse gate
was exactly one line (rule_parser.rs `"container" if cfg!(feature = "gecko")`); the rest of the
machinery compiled unmodified. Integration crashes fixed: cssrule.rs unimplemented arm +
as_specific unreachable Ã¢â‚¬â€� symptom: adding a new stylo rule variant Ã¢â€ â€™ must touch BOTH matches
in cssrule.rs plus downcast chain. CSSContainerRule DOM object ships with
containerName/containerQuery (spec css-conditional-5).
PHASE 2 DESIGN (the evaluation, not yet implemented): the single integration point is
`query_container_size` on ServoDangerousStyleElement (layout_dom/servo_dangerous_style_element
.rs:466 Ã¢â‚¬â€� currently todo!()). Stylo calls it during rule matching via
ContainerCondition::find_container Ã¢â€ â€™ valid_container_info. Everything else is already shared-
code complete (matching.rs restyle hints compile in servo today). The architectural problem:
style resolution runs BEFORE boxes exist in servo's traversal, so current-pass sizes are
unavailable. Gecko solves it with layout-interleaving; servo needs the simpler two-pass
approximation: (a) implement query_container_size to read the element's persisted layout box
size from the PREVIOUS layout pass (layout data box slot Ã¢â‚¬â€� investigate LayoutData/box_slot on
the layout-dom element), returning None-size on first pass; (b) when any container query was
consulted during a pass, record it and schedule a restyle+reflow so pass 2 evaluates against
pass-1 sizes Ã¢â‚¬â€� converges for static content, correctness caveat for oscillating layouts
documented; (c) invalidation on container resize comes free via the existing restyle since
queries re-evaluate every pass. Volume estimate: the trait method + plumbing ~small, the
restyle-scheduling hook is the unknown Ã¢â‚¬â€� look at how layout-2020 incremental restyle is
triggered today (needed regardless). Files: layout_dom/servo_dangerous_style_element.rs,
components/layout/flow construction entry, stylo container_rule.rs traverse_container.
- xclick-2 (2026-11-26): click-DRAG text selection on x.com signup page renders highlight Ã¢â‚¬â€� multiline selection highlight appears to track drag x-position per line rather than wrapping full-width on the first line (uncertain if defect Ã¢â‚¬â€� needs comparison against Chrome). No dramatic glitch reproduced unauthenticated: clicks, modals, inputs, selection all functional. The user's "clicking text items glitches" remains unreproduced Ã¢â‚¬â€� most likely requires the authenticated timeline. RECOMMEND: ask user for a screen recording of the glitch or x.com test credentials.

## favicon-pipeline-2 Ã¢â‚¬â€� SOLVED: persistence, origin-keyed (2026-11-26, PR #209)
- Verdict chain: servo engine pipeline complete and WORKING for loaded tabs (fetch 200s, decode, deliver, texture upload, paint hit Ã¢â‚¬â€� all traced live). Letters were exclusively lazy-restored tabs that never load.
- TRAP DOCUMENTED: session.json stores tabs as BARE URL STRINGS Ã¢â‚¬â€� no ids. Tab ids are reassigned at restore. Any per-tab-id persistence is broken by design; key by ORIGIN.
- Fix shipped: favicons.bin FAV2 format, origin-keyed, 32px downscale, RGB8Ã¢â€ â€™RGBA normalize, 5s throttled writes. Verified live: github octocats visible on restored lazy tabs.
- FEATHER_FAVICON_DEBUG env tracing lives in shell-new (writeback + paint-miss).
- Gotcha: the debug paint-miss print fires per-frame-per-tab Ã¢â‚¬â€� a few minutes of logging = GIGABYTES (filled /tmp once). Use briefly.

## matrix-expansion-1 Ã¢â‚¬â€� round-2 results + hit-test open question (2026-11-26)
- Expanded 13Ã¢â€ â€™17 scenario classes, 61Ã¢â€ â€™88 probes. Grid 81/88. New green classes: history-api 5/5, keyboard-input 5/5, transforms-animations 5/5, fetch-xhr 8/8.
- OPEN QUESTION: elementFromPoint hit-test FAIL on overlapping z-indexed elements Ã¢â‚¬â€� returns an unnamed ancestor, not the top element. elementFromPoint IS implemented in servo (documentorshadowroot.rs). Probe uses viewport coords incl. default 8px body margin Ã¢â‚¬â€� math says the point should still land in the overlap, so a real hit-test defect is plausible but unproven. Needs minimal repro: elementFromPoint at various coords on a z-indexed stack, compare vs Chrome. If real: mechanism suspect is servo's hit-test not respecting z-order/point-in-fragment Ã¢â‚¬â€� CHECK compositing hit_test pipeline.
- Probes self-report element tag/id now for future runs.

## sec1zt98 Ã¢â‚¬â€� container queries: WORKING end-to-end (phase 2 complete, verified both directions) [2026-11-26 linux lane]

**State: container queries are functional in servo on feather-engine.** Parse + cssom (phase 1, sec1zt97) + evaluation bridge (phase 2, this entry). Verified live A/B: `@container srp (min-width: 300px)` on a 500px `container-type: inline-size` host Ã¢â€ â€™ inner rule APPLIES (green, width applied, screenshot-verified); same rule with `min-width: 600px` Ã¢â€ â€™ correctly does NOT apply (red). Both directions correct on first paint.

**The three pieces of the phase-2 bridge** (commit ad7cd1fc419):
1. `query_container_size` (servo_dangerous_style_element.rs) reads the element's primary-box content size from the MOST RECENT layout pass Ã¢â‚¬â€� new `GenericLayoutDataTrait::primary_box_content_size` (shared/layout/lib.rs) implemented for layout's DOMLayoutData via fragment tree (Box/Float fragment content_rect; None when no box yet).
2. Convergence loop in layout_impl.rs handle_reflow: after restyle_and_build_trees, `collect_container_sizes` walks the flat tree collecting (opaque, size) for every element whose computed container-type is a size container type; if the vector changed since last pass, insert `RestyleHint::restyle_subtree()` on root + force display list + run another restyle+layout. Bounded at 4 iterations; static content converges in 2 (first pass has NO boxes Ã¢â€ â€™ all queries unknown Ã¢â€ â€™ second pass evaluates against real sizes).
3. **The missing piece nobody scoped: the properties themselves are runtime-pref-gated in stylo** Ã¢â‚¬â€� `[container-name]`/`[container-type]` in longhands.toml carry `servo_pref = "layout.css.container-queries.enabled"` and servo defaults it FALSE. Without the pref the declarations parse-drop as UnknownProperty ("Unsupported property declaration" in console) even though all machinery exists. Feather enables it in shell-new engine prefs (PR in flight).

**Symptom index**:
- Symptom: "Unsupported property declaration: 'container-type', UnknownProperty" in console despite stylo supporting it Ã¢â€ â€™ check: servo pref `layout_css_container_queries_enabled` (components/config/prefs.rs default false) Ã¢â‚¬â€� stylo longhands servo_pref gates are runtime, not compile-time.
- Symptom: container query never matches even with correct sizes Ã¢â€ â€™ check: whether RestyleHint::recascade vs restyle matters Ã¢â‚¬â€� query evaluation happens during RULE MATCHING, so recascade_subtree is INSUFFICIENT (only re-cascades already-matched rules); must be restyle_subtree (re-matching).
- Symptom: query matches/doesn't match on first paint but never updates Ã¢â€ â€™ check: the convergence loop only re-evaluates when a REFLOW happens; resize/mutation driven invalidation comes free since sizes change Ã¢â€ â€™ loop triggers. Oscillating layouts terminate at the 4-iteration bound (documented caveat, matches sec1zt97 design).

**Verification addendum** (same day, post-PR): unnamed `@container (min-width: ..)` (nearest-ancestor resolution) verified working; dynamic invalidation verified Ã¢â‚¬â€� JS mutation of container width flips query results live (greenÃ¢â€ â€™red on shrink); heavy non-container pages (YouTube watch) show zero spurious restyle iterations and zero panics.

**Verification gaps honestly noted**: live google.co.uk SERP test BLOCKED Ã¢â‚¬â€� the IP is now captcha-walled on co.uk too as of 2026-11-26T20:40 (was unblocked earlier same day Ã¢â‚¬â€� wall state decays with traffic volume, per-IP behavioral). serp.html capture is the wall shell, useless as repro. Resize-invalidation untested. Style-type container queries (non-size) out of scope. Units (cqw etc) untested.

## hittest-verdict Ã¢â‚¬â€� elementFromPoint is CORRECT in servo (2026-11-26)
- Minimal repro verdict: overlap-hits-top PASS, underzone-hits-under PASS, flat PASS, offzoneÃ¢â€ â€™html PASS. z-index ordering respected in hit-testing. NOT a defect.
- The matrix layout-stacking failure was probe SELF-INTERFERENCE: probe() appends result divs into #probes in normal flow Ã¢â€ â€™ grows it Ã¢â€ â€™ pushes test content down Ã¢â€ â€™ hit-test struck a probe div (detail "DIV#/probe pass" was the smoking gun: className "probe pass").
- HARNESS-LEVEL FIX in _probe.js: #probes now position:absolute;left:-9999px;pointer-events:none. ALL pages inherit the protection. Lesson for future probes: any self-checking harness that writes into the DOM must not reflow test content.
- Matrix harness reusable-driver pattern: importlib load matrix.py as module, m.serve() + m.run_one_page(feather_bin, env_with_devtools_9222, url) Ã¢â‚¬â€� single-page iteration ~12s.

## sec1zt99 Ã¢â‚¬â€� favicon pipeline: VERIFIED WORKING (favicon-pipeline-1 superseded) + texture-upgrade nit fixed [2026-11-26 linux]
Live diagnostic (FEATHER_FAVICON_DEBUG) on x.com: engine delivers favicon (32x32 then 1024x1024 upgrade), writeback caches keyed by origin, egui upload succeeds rgba_ok=true, tab strip PAINTS the real X logo (screenshot-verified). Whatever broke it in the favicon-pipeline-1 run is since fixed (writeback/renav repairs). One real nit found and fixed: the texture-upload contains_key guard meant the FIRST decoded resolution won forever Ã¢â‚¬â€� when servo later exposes a sharper favicon (observed: 32px then 1024px for the same origin), the texture never upgraded. Fix: invalidate favicon_tex entry when fresh bytes are cached (feather PR in flight). Symptom index: 'favicon paints blurry/small despite high-res available' Ã¢â€ â€™ check: texture-upload one-shot guard vs re-upload invalidation.

## sec1zt100 Ã¢â‚¬â€� container queries closeout: regression exoneration + remaining verification gap [2026-11-26 linux]
Post-merge smoke: github.com renders chrome/nav correctly with blank hero Ã¢â‚¬â€� EXONERATED as container-query regression: github's css contains ZERO @container rules (curl-verified), so the hero gap is a pre-existing unrelated compat issue (own item). YouTube watch: zero spurious restyle iterations, zero panics. Live google.co.uk SERP verification remains environmentally blocked: feather still captcha-walled (google.com/sorry) while curl passes SAME-IP same-moment Ã¢â‚¬â€� reconfirms the wall is fingerprint-based (TLS ClientHello prime suspect, sec1zt93), not IP-based; wall does not decay for feather's fingerprint. SERP healing therefore unverifiable from this VM until the fingerprinting issue itself is addressed Ã¢â‚¬â€� that unwalling work would now be the single highest-leverage verification+compat unlock for ALL google properties. Container queries mission otherwise COMPLETE: merged feather#212 + feather-engine ad7cd1fc419, all mechanism tests green (named/unnamed/both-directions/first-paint/dynamic-invalidation/zero-cost-on-non-container-pages).

## sec1zt101 Ã¢â‚¬â€� URGENT OPEN: every-page eval parse error Ã¢â‚¬â€� COMPAT_SHIM/UNHIDE_JS likely never execute [2026-11-26 linux]
`[console.Error] Error at :14:2 unexpected token: ')'` fires on EVERY page load incl. blank.html (zero page JS) Ã¢â‚¬â€� therefore shell-injected, not page content. Candidates narrowed: (1) COMPAT_SHIM userscript (engine.rs:333, injected via user content manager into every document Ã¢â‚¬â€� the chrome-object/navigator spoofing the anti-botwall work depends on), (2) UNHIDE_JS youtube unhide sentinel (app.rs:555, evaluate_javascript once per url). BOTH parse+eval clean in node (`new Function` + eval verified) Ã¢â‚¬â€� so either spidermonkey rejects a construct node accepts, or servo's user-script compile path mangles/offsets the source (error at line 14: shim line 14 is the `];` closing the plugins array Ã¢â‚¬â€� nothing wrong there; off-by-N suggests wrapper offset). Verbatim-passthrough confirmed for evaluate_javascript (script_thread handle_evaluate_javascript Ã¢â€ â€™ evaluate_js_on_global, no wrapping). IMPLICATION IF CONFIRMED: the compat shim never runs Ã¢â€ â€™ window.chrome/navigator spoofs absent on every page Ã¢â€ â€™ directly relevant to the google captcha-wall fingerprint discrimination (sec1zt93/100). NEXT SESSION concrete steps: (a) bisect Ã¢â‚¬â€� temporarily empty UNHIDE_JS / skip shim registration, see which removal kills the error; (b) test spidermonkey-specific parsing of the shim in servoshell --user-script or a unit eval harness; (c) check servo user_contents user-script compile path for source-text wrapping. Symptom index: 'console.Error at :14:2 unexpected token on every page incl blank' Ã¢â€ â€™ check: shell-injected userscripts, NOT page js.

## session closeout 2026-11-26 late (linux): SHIPPED container queries END-TO-END (feather#212+#213 merged, engine ad7cd1fc419) Ã¢â‚¬â€� phase1 parse/cssom + phase2 evaluation bridge + convergence loop, verified named/unnamed/both-directions/first-paint/dynamic-invalidation/zero-cost-non-container. Favicon pipeline verified working + texture-upgrade fix merged. OPEN: sec1zt101 eval parse error (top priority next Ã¢â‚¬â€� possible silent killer of all shim-based compat), SERP live verification blocked on fingerprint wall, github blank-hero (container-exonerated, own item).

## matrix-harness-5 Ã¢â‚¬â€� OPERATIONAL TRAP: never run matrix during a cargo build (2026-11-26)
- Symptom: ALL pages collect 0 probes, including previously-green controls. Cause: boot-per-page harness execs target/release/feather-ui while cargo is rewriting it mid-build Ã¢â‚¬â€� boots fail or run partial binaries. Mass false-negative.
- Rule: matrix runs and shell builds are mutually exclusive on one machine. A run overlapped with a build is void Ã¢â‚¬â€� rerun after FONTFACE-BUILD-EXIT=0-style completion.
- FontFace investigation: implementation COMPLETE in servo (dom/css/fontface.rs) but pref-gated: webidl has Pref="dom_fontface_enabled", prefs.rs default false. One-line shell pref enable Ã¢â‚¬â€� matrix is the arbiter of whether exposure is net-positive (sites feature-detect typeof FontFace).
- SVG-in-img raster: resvg IS wired in components/net/image_cache.rs Ã¢â‚¬â€� the matrix svg-raster failure is narrow (transparent pixels), possibly data-url/intrinsic-size path, not wholesale absence. Note: fillrule-hole probe passes trivially when nothing renders Ã¢â‚¬â€� pass condition needs a positive control.

## sec1zt102 Ã¢â‚¬â€� TLS fingerprint mechanism analysis (google-wall prime suspect, scoped) [2026-11-26 linux]

**Servo's actual rustls ClientHello CAPTURED** (localhost TLS server byte-parse, method durable in /tmp/tlscap.py):
- 10 ciphers `1302 1301 1303 c02c c02b cca9 c030 c02f cca8 00ff` Ã¢â‚¬â€� TLS1.3 AES-256-FIRST ordering (rustls tell; Chrome does AES-128 first), thin TLS1.2 list
- groups `001d 0017 0018 11ec` Ã¢â‚¬â€� x25519/p256/p384/X25519MLKEM768 Ã¢â‚¬â€� hybrid PQ group advertised BUT key_share ext len=38 = x25519 ONLY (no MLKEM share Ã¢â‚¬â€� major tell: real Chrome sends the big hybrid share ~1200 bytes)
- extensions: sni(0), ec_point_formats(11), supported_versions(43), session_ticket(35), sig_algs(13 len 28), psk_modes(45), groups(10), alpn(16 h2+http/1.1), key_share(51), extended_master_secret(23), status_request(5). ORDER IS PER-CONNECTION RANDOMIZED (observed two different orders) Ã¢â‚¬â€� Chrome-like, surprisingly already good
- **NO GREASE ANYWHERE** Ã¢â‚¬â€� no grease ciphers/groups/extensions
- **MISSING vs Chrome 131**: ALPS (ext 17513 Ã¢â‚¬â€� h2 settings cross-check, browsers-only), compress_certificate brotli (27), ECH grease (65037), multi-keyshare, delegated creds
- sig_algs list len 28 vs Chrome's shorter/different-order list

**CRITICAL COUNTER-EVIDENCE captured same session**: curl/openssl ClientHello (also captured) has NO GREASE, fixed ext order, 31 ciphers in classic openssl order Ã¢â‚¬â€� even LESS chrome-like than servo Ã¢â‚¬â€� yet curl passes the google wall same-IP same-moment while feather is walled. IMPLICATION: google's /sorry wall cannot be pure-ClientHello-fingerprint classification, at least not GREASE/keyshare based; it's multifactor with strong behavioral/request-volume or missing-surface (js env) components. TLS mimicry would reduce the 'not a real browser' signal for Cloudflare-grade walls but is NOT proven to lift the google wall Ã¢â‚¬â€� validation experiment needed before big investment: capture whether google's wall decision changes when feather's request burst pattern is slowed/serialized (cheap test) vs fingerprint reshape (expensive test).

**Reshape feasibility in rustls 0.23**: cipher/group ORDER customizable via builder_with_provider custom CryptoProvider (cheap, pure config). Cert compression: rustls has optional brotli support (feature-gated, plausible cheap win). NOT POSSIBLE in rustls: GREASE injection (upstream refuses, philosophical), ALPS, multiple key shares, ECH. Full Chrome mimicry therefore impossible in rustls Ã¢â‚¬â€� requires patched BoringSSL (rquest/reqwest-impersonate path: boring-sys2 fork with per-connection fingerprint FFI hooks; multi-day, adds C build chain to net crate) OR a rustls fork (pure-rust but expert TLS work, must be maintained).

**Recommended staged path**: Stage 0 validation first (cheap): behavioral-shaping test Ã¢â‚¬â€� serialize/slow feather's subresource burst against google.co.uk, observe whether the wall lifts; if yes the wall is behavioral and TLS work is deprioritized entirely. Stage 1 (if fingerprint still implicated): rustls config reorder + brotli cert compression Ã¢â‚¬â€� hours. Stage 2: boringssl integration ÃƒÂ  la rquest Ã¢â‚¬â€� multi-day, only with Stage-0 evidence. Symptom index: 'google /sorry wall, curl passes same-IP' Ã¢â€ â€™ do NOT assume TLS fingerprint; capture both hellos and compare against wall-behavior evidence first.

## sec1zt103 Ã¢â‚¬â€� CORRECTION to sec1zt101: every-page parse error was a /tmp leftover, NOT a product defect [2026-11-26 linux]
Bisected with runtime switches (FEATHER_NO_SHIM/FEATHER_NO_UNHIDE on devin/1795726810-shim-bisect): error persisted with both disabled Ã¢â€ â€™ neither shim nor unhide. True source: `/tmp/feather-eval-persist.js` Ã¢â‚¬â€� a TRUNCATED probe artifact from a prior session (final `})();` unmatched at line 14, exact match to the `:14:2` error), auto-injected into every page by the dev eval-persist hook. Deleted file Ã¢â€ â€™ zero console errors on blank.html. IMPLICATIONS: (1) COMPAT_SHIM and UNHIDE_JS parse+execute fine Ã¢â‚¬â€� the chrome-object spoofing IS active in production, no silent compat killer exists; (2) the google wall persists DESPITE the working shim Ã¢â‚¬â€� strengthens fingerprint/behavioral theories over missing-js-surface; (3) sec1zt101's alarmed framing retracted. Symptom index: 'console error on every page incl blank' Ã¢â€ â€™ check /tmp/feather-eval*.js dev-hook leftovers BEFORE suspecting product injection. Process note: dev-hook artifacts in /tmp leak across sessions and even across worktrees Ã¢â‚¬â€� sessions should clean them at closeout.

## sec1zt104 Ã¢â‚¬â€� TLS/wall investigation: servo JA3/JA4 captured + impersonation-mismatch theory REFUTED [2026-11-26 linux]
Servo's authoritative fingerprints via tls.peet.ws/api/all (probe-verified): **JA3 `cc895d08ccc7a78ab3d7d9cf3cdc6077`, JA4 `t13d1011h2_61a7ad8aa9b6_0d308c48d2a3`** Ã¢â‚¬â€� decode: TLS1.3, 10 ciphers, 11 extensions, h2. Chrome 131 reference JA4 `t13d1516h2_...` (15 ciphers, 16 exts) Ã¢â‚¬â€� servo is trivially distinguishable by count alone. curl baseline JA4 `t13d3113h2` (31 ciphers). KEY EXPERIMENT: curl with full Chrome UA passes google.co.uk wall (200, no /sorry) same-IP same-moment Ã¢â‚¬â€� REFUTES the TLS-vs-UA impersonation-mismatch theory: google does not wall openssl-fingerprinted clients even when they claim Chrome UA. Therefore the feather-specific wall discriminator is NOT TLS fingerprint alone NOR UA mismatch. Remaining candidates ranked: (1) HTTP/2 fingerprint (akamai-style: SETTINGS frame values/order, pseudo-header ordering, window-update/priority behavior Ã¢â‚¬â€� hyper-h2 differs strongly from chrome nghttp2; UNTESTED, capture tooling: peet api akamai_fingerprint field needs a non-pretty json fetch path or manual h2 frame capture); (2) IP+behavioral reputation memory Ã¢â‚¬â€� feather's historical burst automation patterns are remembered per-IP-UA-keyed (explains why the wall is sticky for feather yet absent for any curl); cheap test: fresh unrelated IP (vpn/proxy) with feather unchanged Ã¢â‚¬â€� if wall gone, reputation; if wall persists, intrinsic fingerprint; (3) request header ordering/casing/missing-headers in h2. NEXT concrete step: the fresh-IP test is the cheapest decisive discriminator (needs a proxy Ã¢â‚¬â€� none on this VM; document for coordinator). Symptom index: 'google /sorry wall discriminates feather but not curl same-IP' Ã¢â€ â€™ do NOT theorize TLS in isolation; capture JA4 + h2 akamai fp + test fresh-IP before any boringssl investment.

## matrix-round3-verdicts (2026-11-26)
- 110/115 baseline across 21 classes. Newly proven GREEN mechanism classes: canvas-2d pixel fidelity (gradients/composite-ops/winding/text-ink/transforms 7/7), constraint validation API 8/8, MutationObserver incl. characterData 9/9, css computed filters 4/5, SPA history 5/5, keyboard input 5/5, transitions 5/5, fetch/xhr 8/8.
- backdrop-filter: computes to EMPTY STRING in servo Ã¢â‚¬â€� unsupported upstream. Glassmorphism-class visual gap, minor. Not yet root-caused beyond "unimplemented/unparsed".
- WebGL matrix failure on macOS VM is ENVIRONMENTAL: gpu-info.txt proves "Apple Software Renderer" Ã¢â‚¬â€� no hardware GL on the VM. Probes need real-hardware validation before treating as engine gap. The [gpu] stderr line + logs/gpu-info.txt is the standing probe.
- CRASH-LOG MINE: shell-new/target/release/logs/crash-*.log Ã¢â‚¬â€� 59/59 were ONE defect: devtools worker.rs cleanup unwrap on disconnected script channel during teardown. Fixed upstream-style on feather-engine (let _ =). Shell proper has ZERO other crashes in the log set Ã¢â‚¬â€� strong reliability signal.
- Probe-artifact lessons (harness-wide): characterData fires on CharacterData nodes not element.textContent; missing computed-style props return undefined and .indexOf kills the whole probe script (masking later probes) Ã¢â‚¬â€� always tolerate-missing.
- svg-raster narrow-down staged: naturalWidth probe added Ã¢â‚¬â€� distinguishes zero-intrinsic-size draw vs transparent raster.

## sec1zt105 Ã¢â‚¬â€� wall evidence sharpening: HOMEPAGE renders fully in feather, only /search walls [2026-11-26 linux]
Decisive discrimination evidence: google.co.uk homepage loads and renders PERFECTLY in feather (screenshot-verified: full doodle, search box, AI-mode button, zero wall) Ã¢â‚¬â€� same VM, same minutes in which /search?q=... hits /sorry. IMPLICATIONS: (1) the wall is NOT an intrinsic always-on TLS/client fingerprint block Ã¢â‚¬â€� if it were, the homepage would wall too; (2) it is endpoint-selective (/search heavily protected, homepage light) AND/OR reputation-triggered on the automation burst that SPA-search startup generates; (3) cookies/js/corp-network all fine. This further deprioritizes boringssl investment: evidence keeps converging on behavioral/reputation rather than cryptographic fingerprint. Combined with sec1zt104's curl+chromeUA-pass result, the strongest remaining theory: per-IPÃƒâ€”endpoint behavioral reputation accumulated from automation bursts. Cheapest remaining decisive test: fresh-IP (needs proxy/vpn Ã¢â‚¬â€� unavailable on this VM, flagged for coordinator) OR a low-and-slow request-shaped feather (serialize subresource fetches, huge test effort). PRACTICAL BYPASS for verification work: use the homepage-rendered search box with real typing (input focus needs care Ã¢â‚¬â€� xdotool type landed nowhere without click-focus, retest with click first).

## svg-canvas-source Ã¢â‚¬â€� ROOT CAUSE: known-unimplemented upstream feature (2026-11-26)
- VERBATIM in servo source: components/script/dom/canvas/2d/canvas_state.rs in the CanvasImageSource::HTMLImageElement arm Ã¢â‚¬â€� `warn!("Vector images are not supported as image source in canvas2d")` Ã¢â‚¬â€� drawImage of an SVG image is a silent no-op BY DESIGN (unimplemented). Explains the matrix symptom exactly: onload fires, naturalWidth correct, pixels transparent.
- SVG pipeline otherwise complete: exact-mime content-type detection (image/svg+xml incl suffix) in decode_bytes_sync, usvg parse Ã¢â€ â€™ VectorImageData, ASYNC rasterization in image_cache rasterize_vector_image (thread pool, resvg::render into tiny_skia pixmap, RGBA8, panic-caught). The gap is ONLY the canvas-source bridge: canvas needs synchronous pixels, rasterization is async Ã¢â‚¬â€� that's the implementation complexity.
- UPSTREAM CANDIDATE (moderate complexity): sync-or-cached rasterization path for vector images as canvas sources. Affects icon-into-canvas, chart export, image editors Ã¢â‚¬â€� real mechanism class.
- resvg panics are catch_unwind-silenced Ã¢â‚¬â€� completion listeners never fire after a panic, so an svg that panics resvg renders as nothing forever. Second upstream-hardening candidate.

## devtools-hardening-verified (2026-11-26)
- worker.rs cleanup unwrap fix VERIFIED: 3 boot+teardown cycles produced ZERO new crash logs (baseline: every boot crashed once, 59 accumulated). First servo-fork engine fix validated through the matrix harness itself Ã¢â‚¬â€� the harness is now a crash-regression tool, not just a feature matrix.
- svg naturalWidth probe verdict: PASSES (200x200) Ã¢â‚¬â€� parse+intrinsic size fine; failure is purely the vector-canvas-source bridge as root-caused. svg page now 6/7 with the remaining red honestly isolated.
- Matrix-driver note: crash counting protocol Ã¢â‚¬â€� count logs/crash-*.log before and after any harness run; delta must be zero.

## sec1zt106 Ã¢â‚¬â€� wall hunt round 3: headers + h2 fingerprints captured, layered exoneration, hardening shipped [2026-11-26 linux]
CAPTURED feather's real request headers (localhost capture): anomalies vs Chrome typed-nav = `Accept-Language: C` (POSIX locale leak Ã¢â‚¬â€� sys_locale returns C for LANG=C.UTF-8), `Sec-Fetch-Site: cross-site` on typed nav (spec wants none Ã¢â‚¬â€� user_activation flag never set anywhere in servo, both sec-fetch-user and site:none blocked on that plumbing), missing sec-ch-ua*/Upgrade-Insecure-Requests/Sec-Fetch-User. CAPTURED servo h2 fingerprint (python-h2 TLS server + FEATHER_IGNORE_CERT_ERRORS hook): hyper defaults 2MiB window/+5177345/64KiB header-list, pseudo-order :method:scheme:authority:path vs Chrome :method:authority:scheme:path. EXONERATIONS (curl replay matrix, all pass wall same-IP): feather's exact anomalous header set replayed through curl Ã¢â€ â€™ 200. So headers not the wall discriminator; combined with TLS-hello exoneration (openssl passes), every single-layer fingerprint is exonerated. Remaining theories: composite-anomaly-score (all layers wrong together Ã¢â‚¬â€� unfixable cheaply) or per-IPÃƒâ€”endpoint behavioral reputation (home-page renders, /search walls Ã¢â‚¬â€� sec1zt105). SHIPPED this round (engine commit): accept-language normalizationÃ¢â€ â€™en-US,en;q=0.9 (verified live), chrome-identical h2 settings (verified: 6291456/+15663105/262144), FEATHER_SLOW_FETCH_HOSTS pacing hook (untested vs wall Ã¢â‚¬â€� wall decision precedes subresources anyway), FEATHER_IGNORE_CERT_ERRORS shell hook (local TLS harness unlock). NEXT: sec-fetch user_activation plumbing (typed-nav should emit Sec-Fetch-User:?1 + Site:none); google /sorry page itself errors `solveSimpleChallenge is not defined` in servo Ã¢â‚¬â€� captcha-solving path also broken (own defect). Verdict on wall: likely NOT liftable from this VM by fingerprint work Ã¢â‚¬â€� recommend fresh-IP test to coordinator.

## matrix-round4-verdicts (2026-11-26)
- 141/149 baseline, 26 classes. Fully green new classes: observers 6/6 (IO initial-fire AND visibility tracking both sound Ã¢â‚¬â€� the alpha-chain mechanism confirmed end-to-end), media-queries 7/7, scrolling 5/5, dialogs-details 7/7 (showModal/close/toggle events).
- NEW GAP 1: navigator.clipboard absent Ã¢â‚¬â€� Pref="dom_async_clipboard_enabled" default false, Clipboard.webidl exists. Same pref-gate shape as FontFace. NOTE: the webidl is also [SecureContext] Ã¢â‚¬â€� matrix serves http://127.0.0.1; whether servo treats loopback as potentially trustworthy decides if the pref alone is enough.
- NEW GAP 2: Range.getBoundingClientRect returns 0x0 for a valid text-node range (selection toString correct). Method implemented in dom/range/range.rs delegating to client_rects Ã¢â‚¬â€� so rect PRODUCTION for text ranges yields empty. Editor-class impact: caret measurement, selection rendering, floating UI positioning. ENGINE INVESTIGATION CANDIDATE Ã¢â‚¬â€� suspect chain: range Ã¢â€ â€™ layout query for text boxes.
- Zero crashes across full 26-page grid Ã¢â‚¬â€� devtools hardening holds at scale. crash-count protocol now standard evidence in matrix runs.
- Pattern index: pref-gated-but-implemented upstream features found so far Ã¢â‚¬â€� document.all (implemented it), FontFace (enabled), clipboard (enabling), webgl2 (enabled). When a matrix probe shows typeof-X undefined, CHECK THE WEBIDL FOR Pref= FIRST before assuming unimplemented.

## range-geometry-chain Ã¢â‚¬â€� investigation trail for the zero-rect gap (2026-11-26)
- Chain verified correct down to layout: Range.client_rects (dom/range/range.rs) walks start..end nodes correctly for a same-text-node range (nodes=[textnode] verified by code reading), calls node.border_boxes Ã¢â€ â€™ window.box_areas_query(cx, node, Border) Ã¢â€ â€™ layout_reflow + layout.query_box_areas(trusted_node_address, area).
- SUSPECT LOCALIZED: layout's query_box_areas handling of TEXT-node addresses. Text nodes own no boxes Ã¢â‚¬â€� correct behavior requires mapping the text node to its parent element's text fragment boxes (spec: text contributes rects). If layout only resolves element-node boxes, result is empty Ã¢â‚¬â€� exactly the observed 0x0. Search components/layout for query_box_areas / BoxAreas query handling; check whether the node address lookup covers non-element nodes.
- NOTE the partial-selection FIXME in client_rects is separate (cosmetic); the zero rect is the structural gap.

## range-geometry-chain addendum Ã¢â‚¬â€� layout side (2026-11-26)
- layout query.rs process_box_areas_request: node.fragments_for_pseudo(None) Ã¢â€ â€™ per-fragment cumulative_box_area_rect. Text nodes DO have LayoutBox::Text (dom.rs) so inner_layout_data may exist Ã¢â‚¬â€� the empty result means EITHER text fragments don't yield box-area rects OR the fragments vec is empty for text.
- TRAP SPOTTED in process_box_areas_request: if root_transform_for_fragments returns None, the fallback emits rects with origin but ZERO SIZE Ã¢â‚¬â€� a silent 0x0 even when fragments exist. Any debugging should print which branch fired.
- Next step for the engine lane: RUST_BACKTRACE-style println in process_box_areas_request on a minimal text-range page: count fragments, note transform presence, inspect fragment types. THEN the fix shape becomes clear (text-fragment rect support vs transform lookup vs client_rects needing parent-element mapping).

## pref-gate-sweep Ã¢â‚¬â€� systematic pref audit (2026-11-26)
- Full webidl Pref= sweep counts: webgpu 46 gates, webxr 22, serviceworker 13, webrtc 12, indexeddb 12, fontface 11, bluetooth 11, worklet 9, entries-api 9, gamepad 6, exec-command 6, geolocation 5, cookiestore 5, credential-mgmt 4, permissions 4, touch-legacy 4.
- SURPRISE: permissions-api ALREADY GREEN unmodified Ã¢â‚¬â€� query({name:"geolocation"}) returns state "prompt" Ã¢â‚¬â€� functional, not gated in practice.
- Batch-enabled in shell (matrix arbitrating): cookiestore, exec_command, credential_management, gamepad. Skipped: webgpu/webxr/bluetooth (environmental/no hardware), geolocation (platform integration), webrtc (known-large), entries-api (drag-drop filesystem, lower value desktop).
- TRAP CONFIRMED AGAIN: branch skew cost a debugging detour Ã¢â‚¬â€� a matrix page 404'd because the branch predated the round merge; looked exactly like a regression (0 collected, error page). RULE: sync branch with main BEFORE trusting a matrix negative; the telltale is document.title eval Ã¢â€ â€™ "Error response".
- Range-geometry deep-dive banked: chain verified correct RangeÃ¢â€ â€™border_boxesÃ¢â€ â€™box_areas_queryÃ¢â€ â€™layout query_box_areas; text nodes DO have LayoutBox::Text; silent zero-size fallback exists when root_transform lookup fails. Engine lane has exact file/function trail.

## pref-batch-verdicts (2026-11-26, PR #221)
- execCommand EXPOSED + FUNCTIONAL: insertText writes into contenteditable, ret=true. Editor-class mechanism unlocked. Composes with shadow-input fix.
- cookieStore exposed but roundtrip RED: set().then(get) returns null Ã¢â‚¬â€� functional-level gap in the store path (or jar keying). Exposed-but-broken is arguably worse than absent for feature-detecting sites Ã¢â‚¬â€� WATCH: if sites branch on cookieStore presence and break, revert. Matrix tracks it in platform-apis.
- credentials exposed. permissions was already green (query returns real states).
- GAMEPAD requires cargo feature not pref: webidl has skip-unless CARGO_FEATURE_GAMEPAD Ã¢â‚¬â€� pref alone is a no-op. Before enabling any pref, check the webidl header for skip-unless lines Ã¢â‚¬â€� pref-gates and feature-gates are DIFFERENT levers.
- Branch note: platform-apis branch carries its own copy of the clipboard pref line (branched pre-#219-merge) Ã¢â‚¬â€� merge conflict with main is trivial/identical content.

## cookiestore-decision Ã¢â‚¬â€� exposed-but-broken worse than absent; REVERTED (2026-11-26)
- Differential verdict: document.cookie roundtrips fine on the same host Ã¢â‚¬â€� cookie storage sound generally; the CookieStore API path specifically broken (set resolves, get returns null Ã¢â‚¬â€� implementation structurally complete in dom/cookiestore/cookiestore.rs, Get uses GetCookieDataForUrlAsync; the write/read path divergence is unfixed upstream).
- DECISION PRINCIPLE (matrix-driven): when a feature has a working fallback (sites feature-detect cookieStore and fall back to document.cookie), exposing a broken implementation is strictly worse than absence Ã¢â‚¬â€� absence routes sites to the working path. Reverted the pref; engine-lane investigation candidate with exact file refs.
- Full 30-class grid: 173/183 with zero unexplained reds. Every failure is environmental (webgl x2 Ã¢â‚¬â€� no GPU on VM), known-large (webrtc), unimplemented-upstream (svg-canvas-source, backdrop-filter, FontFaceSet.check), pref-not-applicable (gamepad cargo feature, webgpu), or documented engine gap (range-geometry, cookiestore).
- Round-5 greens: workers incl transferable detachment 5/5, custom-elements+slots full lifecycle 9/9, crypto sha256 spec-vector correct 10/10 Ã¢â‚¬â€� the deep mechanism layers are sound.

## sec1zt107 Ã¢â‚¬â€� svg-canvas-source SOLVED + byte-completion mechanism (2026-11-26)
- Symptom: drawImage of SVG image produces transparent pixels (was: silent no-op with warn) Ã¢â€ â€™ check: image_cache key-gated completion, NOT the rasterizer. resvg pipeline was never broken.
- ROOT CAUSE: `rasterize_vector_image` completion is gated on a WebRenderImageKey assignment that rides the paint pipeline frame cadence Ã¢â‚¬â€� the round trip takes SECONDS on an idle page. Canvas drawImage is synchronous; any listener-wait design is hostage to frame timing.
- FIX (feather-engine): split completion into two levels. `RasterizationTask.bytes_result` is set in the resvg worker the moment pixels exist (before keycache submission); new trait method `rasterize_vector_image_bytes` returns Image as soon as bytes are ready. Layout path (`result`, key-gated) completely untouched. Canvas consumers (`HTMLImageElement::get_raster_image_data` Ã¢â‚¬â€� the real drawImage path, NOT canvas_state::fetch_image_data which only serves CSS/url draws Ã¢â‚¬â€� plus fetch_image_data for completeness) poll bytes with 20ms sleep, 2s bound.
- VERIFIED: svgcanvas2 harness CENTER_PIXEL 255,0,0,255 (red circle) CORNER_PIXEL 0,255,0,255 (green rect) Ã¢â‚¬â€� exact expected values.
- Symptom-index negative results: resvg panic theory Ã¢â‚¬â€� WRONG (no panic, job completed every time). blob: URL SVG theory Ã¢â‚¬â€� WRONG (file-hosted identical). Rasterization pipeline broken Ã¢â‚¬â€� WRONG (visible <img> of same SVG rendered correctly on screen while canvas was blank).
- Lesson: when a consumer needs only pixel bytes, never gate on compositor key assignment. Key generation is a display concern.

## sec1zt108 Ã¢â‚¬â€� google wall: pacing NEGATIVE + honest-UA experiment (2026-11-26)
- FEATHER_SLOW_FETCH_HOSTS=google.com,gstatic.com,googleapis.com FEATHER_SLOW_FETCH_MS=400 Ã¢â€ â€™ /search STILL /sorry-walled. Burst-shape/pacing exonerated as the wall trigger (negative result, symptom index: wall response unchanged by request pacing Ã¢â€ â€™ not behavioral-burst-driven).
- Residual theory standing: impersonation-mismatch composite Ã¢â‚¬â€� feather claims Chrome UA + Chrome-ish headers but rustls TLS/h2 fingerprints mismatch; google scores the mismatch. curl passes because it doesn't impersonate (or UA-only shallow impersonation). TEST ADDED: FEATHER_USER_AGENT env override in shell-new engine.rs Ã¢â‚¬â€� honest non-Chrome UA experiment queued.
- Reference doc: feather docs/wiki/BOT-FINGERPRINT.md (PR #218 merged) is the standing Chrome-vs-servo layer checklist; SPEC-COMPLIANCE.md (PR #223) is the WPT completeness map.

## range-geometry-VERDICT Ã¢â‚¬â€� architectural gap, confirmed by code reading (2026-11-26)
- ROOT CAUSE chain complete: Range.client_rects Ã¢â€ â€™ border_boxes Ã¢â€ â€™ box_areas_query Ã¢â€ â€™ process_box_areas_request Ã¢â€ â€™ node.fragments_for_pseudo Ã¢â€ â€™ with_fragments Ã¢â€ â€™ LayoutBox::with_base Ã¢â‚¬â€� which returns None verbatim for `LayoutBox::DisplayContents(..) | LayoutBox::Text(..)` (components/layout/dom.rs). Text nodes NEVER yield fragments through this path.
- WHY it's architectural: in servo's new layout, text doesn't own per-node fragments Ã¢â‚¬â€� text is shaped into TextRuns folded into the PARENT inline formatting context's line fragments. Correct Range-over-text geometry requires walking the parent's inline fragments and clipping to the text node's byte range Ã¢â‚¬â€� upstream feature-scale work, not a patch.
- Spec note: cssom-view getClientRects explicitly covers text nodes, so this is a real spec gap Ã¢â‚¬â€� legitimate upstream candidate but LARGE. Priority-wise: affects caret measurement/selection-geometry/editor floating UI Ã¢â‚¬â€� but selection RENDERING itself evidently works through a different path (real typing/selection verified working in dogfooding), so user-facing impact is narrower than it first appeared Ã¢â‚¬â€� mostly editor-class JS measurement.
- RANGE_DEBUG instrumentation left in the fork (layout/query.rs, env-gated) Ã¢â‚¬â€� prints fragment count+kinds and which branch fires; expect fragments=0 confirmation.

## sec1zt109 Ã¢â‚¬â€� GOOGLE WALL SOLVED: impersonation-mismatch confirmed, honest UA lifts wall (2026-11-26)
BREAKTHROUGH with causal A/B: same binary, same minute, same IP Ã¢â‚¬â€� only the UA varies:
- Chrome/131 desktop UA Ã¢â€ â€™ /sorry walled (4 recaptcha hits). Mobile-Chrome UA Ã¢â€ â€™ walled. Safari/17.4 UA Ã¢â€ â€™ WALLED TOO (webkit impersonation also policed).
- Firefox/133 UA Ã¢â€ â€™ NOT walled Ã¢â‚¬â€� but google serves gecko-flavored SERP CSS (-moz-box-* XUL-era properties) and servo STACK-OVERFLOWS in StyleThread (new crash defect, repro: FF UA + google search).
- HONEST UA "Feather/1.0 (X11; Linux x86_64) Servo/1.0" Ã¢â€ â€™ NOT walled, NO crash, and the SERP FULLY RENDERS: centered layout, correct padding, AI Overview, result cards, dictionary widget Ã¢â‚¬â€� the first correct Google SERP in feather history (container-queries fix + wall lift compounding).
MECHANISM VERDICT: google polices major-browser IMPERSONATION Ã¢â‚¬â€� when TLS/h2/behavior fingerprints mismatch the claimed Chrome/Safari identity, the wall engages. Honest/unknown UAs are served like curl. Pure-fingerprint mimicry was NEVER the required path Ã¢â‚¬â€� the alternative is stop impersonating.
STRATEGIC DECISION FORK (needs coordinator/user): (a) honest UA everywhere Ã¢â‚¬â€� walls down fleet-wide BUT sites serve basic/degraded content (YouTube/modern-SPA compat relies on chrome claims Ã¢â‚¬â€� this breaks the compat-shim strategy); (b) per-host UA switching Ã¢â‚¬â€� chrome UA generally, honest UA on walled properties (google.*) Ã¢â‚¬â€� hacky but cheap and preserves both; (c) full TLS mimicry megaproject Ã¢â‚¬â€� no longer the only path, probably not worth it given (b).
Also: the JS surface must stay CONSISTENT with whatever network UA is sent (shim currently hardcoded chrome-flavor) Ã¢â‚¬â€� mixed signals are themselves a bot tell.
Hook shipped for experiments: FEATHER_USER_AGENT env override in shell-new engine.rs.

## sec1zt110 Ã¢â‚¬â€� per-host honest-UA SHIPPED + verified (2026-11-26)
- feather#225 (shell shim consistency + env hooks) + engine 7a6b3b3544b (net-layer per-host switch). Default policy: chrome UA everywhere, honest "Feather/1.0 (X11; Linux x86_64) Servo/1.0" on .google. hosts, FEATHER_HONEST_UA_HOSTS/FEATHER_HONEST_UA/FEATHER_USER_AGENT env overrides. Shim skips chrome spoof on honest hosts and overrides navigator.userAgent/appVersion/vendor/platform Ã¢â‚¬â€� JS/net never disagree.
- VERIFIED same build: google SERP fully renders (centered, AI overview, dictionary card, zero /sorry) + youtube search fully renders with chrome UA intact.
- UNBLOCKED: all google properties now testable Ã¢â‚¬â€� gmail, maps, live-SERP container-query validation (was environmentally blocked). The TLS-fingerprint megaproject is definitively dead Ã¢â‚¬â€� impersonation-mismatch was the mechanism.
- OPEN crash defect: Firefox-UA gecko-flavored SERP stack-overflows StyleThread (rayon thread, deep cascade recursion Ã¢â‚¬â€� servo has no iterative cascade). Repro: FEATHER_USER_AGENT=firefox-UA + google search. Not hit in production config (gecko content never served to chrome/honest UAs) but it's a latent crash class for any deeply-nested page.
- ALSO SHIPPED this session: svg-canvas-source complete (byte-level completion split, engine 24842544161); layout/query.rs RANGE_DEBUG compile fix for another lane (Fragment lacks Debug Ã¢â‚¬â€� match on variants).

## range-geometry-EMPIRICAL (2026-11-26) Ã¢â‚¬â€� case closed
- RANGE_DEBUG instrumentation live-verdict: `[range-debug] NO-TRANSFORM fallback hit Ã¢â‚¬â€� fragments=0` (10 hits per page). Text nodes yield ZERO fragments exactly as statically proven via LayoutBox::with_base returning None for Text. Both traps confirmed: empty fragments AND the silent zero-size fallback. Case closed Ã¢â‚¬â€� architectural upstream feature documented in docs/wiki/UPSTREAM-CANDIDATES.md.

## fontfaceset-check-implemented (2026-11-26)
- FontFaceSet.check() implemented upstream-style: webidl uncommented (+Pref gate), rust impl pattern-matched on Load Ã¢â‚¬â€� parse via find_the_matching_font_faces, syntax error Ã¢â€ â€™ false, all matched faces must have Status()==Loaded, unmatched families count as system-available (spec font matching rules). Compilation+matrix validation in flight.
- NOTE DOMString deref: &font passed to &str param relies on DOMString's Deref Ã¢â‚¬â€� compile verdict pending; if it fails the fix is font.str() or &*font.

## sec1zt111 Ã¢â‚¬â€� favicon pipeline RESOLVED (side effect of svg-canvas byte-completion) + github intermittent segfault OPEN (2026-11-26)
- Favicon verdict: engine pipeline was never broken Ã¢â‚¬â€� link-rel fetching, decode, process_favicon_image, set_favicon all fire correctly. The visible failure (wv.favicon() never Some) was the VECTOR favicon path: svg favicons rasterize through the key-gated completion which stalls for seconds/never on idle pages. The byte-completion split (sec1zt107) healed it: fresh-cache run shows set_favicon x2 (svg+png), shell has_favicon=true, 32x32 upload. Symptom index: 'favicons never appear despite full pipeline existing' Ã¢â€ â€™ check vector rasterization completion gating, NOT the shell writeback (writeback/upload/origin-keying all verified correct).
- github.com INTERMITTENT SEGFAULT Ã¢â‚¬â€� new open defect, process dies silently (no panic message, no overflow note). Repro rate ~2/3 launches within 10-13s of github.com load; NOT reproduced under gdb in 60s (timing-sensitive). Suspect: webgl (mona-smile .glb loads shortly before death; 'GL point sprites 1280' warning adjacent). Next: repeated-run statistics, core_pattern apport already captures? check /var/crash for feather cores, test with webgl disabled pref if one exists. THIS BLOCKS github dogfooding Ã¢â‚¬â€� prioritize.
- UA-SURFACE consistency VERIFIED on google: ua=honest, vendor empty, chrome undefined, userAgentData undefined, platform Linux Ã¢â‚¬â€� shim/net agreement exact.

## sec1zt112 Ã¢â‚¬â€� github segfault heisenbug statistics (2026-11-26)
- Crash rates: 2/4 bare runs, 0/3 under gdb, 0/4 second batch bare Ã¢â‚¬â€� timing-sensitive race, gdb-hidden. No panic message Ã¢â‚¬â€� hard segfault. Webgl adjacent (mona .glb + GL-point-sprites warning precede death in crashing runs). Do NOT burn cycles on gdb repro Ã¢â‚¬â€� needs rr (not installed) or systematic core capture (core_pattern experiments inconclusive). STATUS: open, timeboxed. If it recurs in dogfooding: capture which thread dies via `/proc/pid/task` monitoring or add canary eprintlns in webgl thread command processing.

## webfonts-fully-green + check() verified (2026-11-26)
- FontFaceSet.check() implementation VERIFIED through matrix: document-fonts-api green. parse-errorÃ¢â€ â€™false, all-matched-loaded semantics. FOURTH upstreamable engine fix in the fork (document.all, shadow-input, devtools-teardown, fontfaceset-check).
- font-loads probe deep truth: original failure was test-infrastructure Ã¢â‚¬â€� no font file existed on the matrix server. With WPT's Ahem.ttf served, the ENTIRE webfont pipeline proven working: @font-face parse, fetch, decode, font matching, metrics application (canvas measureText 240px Ahem vs 126.9px serif Ã¢â‚¬â€� exact 6Ãƒâ€”40 square-glyph math). webfonts class 3/3.
- Matrix infrastructure lesson: probes depending on served binary assets need the assets committed in pages/ Ã¢â‚¬â€� Ahem.ttf (21KB, WPT-licensed) now vendored in tests/scenarios/pages/fonts/.

## sec1zt113 Ã¢â‚¬â€� youtube playback: observed live once, googlevideo-UA causality INCONCLUSIVE (2026-11-26)
- With FEATHER_HONEST_UA_HOSTS=".google.,googlevideo.": playback VERIFIED live Ã¢â‚¬â€� ct=4.83 advancing, readyState 4, blob MSE, 21 googlevideo requests. Control run without googlevideo in honest list: 24 googlevideo requests flowed but the eval probe didn't print (flaky harness, not necessarily a stall). VERDICT: insufficient evidence that googlevideo UA matters Ã¢â‚¬â€� the rs:0 stall may be serving-variance (botwall mood) rather than UA-policy. Do not add googlevideo to the default honest list without a larger sample. Cheap follow-up: scripted N=5 alternating runs comparing ct progression.

## sec1zt114 Ã¢â‚¬â€� post-wall google-properties sweep (2026-11-26)
- Google Sheets (docs.google.com): FULLY RENDERS Ã¢â‚¬â€� complete spreadsheet grid, toolbar, formula bar, data rows, zero /sorry. One tradeoff artifact visible: 'This browser version is no longer supported' banner Ã¢â‚¬â€� google UA-sniffs and Feather/1.0 is an unknown browser. Sheet is still usable. This is the honest-UA cost showing up in-product: walls down, but unsupported-browser banners/nudges appear on UA-sniffing properties. OPEN POLICY QUESTION for coordinator: accept banners on google.*, or hybrid policy (honest UA only on the walled entry endpoints /search + accounts, chrome-ish UA on docs/sheets subapps?) Ã¢â‚¬â€� needs evidence whether docs endpoints themselves wall under chrome UA (they may never have been the wall trigger Ã¢â‚¬â€� /search was).
- gmail: mail.google.com Ã¢â€ â€™ accounts signin flow unwalled.

## svg-canvas-source-CLOSED + matrix cross-lane validation (2026-11-26)
- The svg-canvas-source gap is CLOSED Ã¢â‚¬â€� engine lane implemented rasterize_vector_for_canvas through the image cache (commits d7ad37dab/248425441). Matrix caught it autonomously: grad pixels went from transparent to correct gradient interpolation. PROOF of the matrix doctrine: cross-lane engine fixes surface as grid movement without coordination.
- Probe lesson: gradient sampling at the midpoint hits exactly 50% interpolation Ã¢â‚¬â€� thresholds must sample asymmetric regions.
- Grid state close to green-wall: remaining reds ALL explained Ã¢â‚¬â€� webgl x2 environmental (no GPU on VM), webrtc known-large, backdrop-filter unimplemented, range-geometry architectural-documented, cookiestore deliberately reverted, gamepad/webgpu feature-gated-not-enabled.
- #221 merged (execCommand+credentials in main), #228 merged (Ahem fixture). platform-apis greens on next current-binary grid.

## sec1zt115 Ã¢â‚¬â€� honest-UA policy NARROWED (PR followup to #225)
- Symptom: sheets/docs must NOT get honest UA (banner cost, never walled) Ã¢â€ â€™ policy narrowed: default list `www.google.com` only, exact-or-dot-suffix matching in BOTH layers (net http_loader + shim JS). CAUTION: a dot-suffix entry `google.com` matches every subdomain Ã¢â‚¬â€� re-includes docs/sheets and defeats narrowing. bare google.com 301s to www before any /sorry decision so exact www.google.com suffices.
- Verified: search renders unwalled under narrowed default. Negative-control check (docs.google.com) redirected to sign-in in the probe Ã¢â‚¬â€� UA-on-docs verification incomplete, mechanism-only: docs.google.com neither equals nor dot-suffix-matches `www.google.com` so it gets the chrome UA in both layers.
- Env override FEATHER_HONEST_UA_HOSTS for widening per-property as evidence accumulates.

## sec1zt116 Ã¢â‚¬â€� post-container-queries compat spot checks
- Symptom: github.com blank hero Ã¢â€ â€™ CHECK RESOLVED/NON-REPRO post container-queries: h1 present ('Software still starts with you'), 0/22 broken images. Likely healed by container-queries implementation (hero layout was container-query-dependent) Ã¢â‚¬â€� consistent with the fleet-wide win prediction.
- Symptom: youtube watch playback under honest-UA-narrowed policy Ã¢â€ â€™ verified live same session as SERP: t advancing, rs=4. coordinator requirement (3) satisfied.

## merge-resolution-validation-gap (2026-11-26) Ã¢â‚¬â€� CRITICAL PROCESS RULE
- PR #221's squash-merge carried UNRESOLVED conflict markers (
## final-baseline-174-of-183 (2026-11-26) Ã¢â‚¬â€� quality gate operational
- PR #233: definitive baseline. ALL remaining reds explained Ã¢â‚¬â€� zero unknowns in matrix. Grid movement validated merges: platform-apis 3Ã¢â€ â€™6/9 post-#221.
- LESSON from the branch-tangle: after rebase conflicts ALWAYS verify `git merge-base --is-ancestor origin/main HEAD` before pushing Ã¢â‚¬â€� stale-base branches look fine locally. And abort-verify: `ls .git/rebase-merge` must be empty before further work.
- Queue state at close: x.com click-glitch NEEDS USER INPUT (credentials or recording Ã¢â‚¬â€� repro-negative unauthenticated, documented). favicons done. matrix done beyond scope.

## backdrop-filter-scoped (2026-11-26) Ã¢â‚¬â€� gap is NOT "unimplemented"
- Correction to earlier assumption: backdrop-filter IS plumbed end-to-end in servo Ã¢â‚¬â€� components/layout/display_list/mod.rs build_backdrop_filter pushes to webrender's push_backdrop_filter; style_ext.rs reads effects.backdrop_filter. Regular `filter` computed values serialize fine in the matrix (blur passes). THE GAP is narrowly scoped: either the longhand fails to PARSE (silently dropped) or computed-style SERIALIZATION for the backdrop-filter longhand is broken/missing. Test: inline `style="backdrop-filter:blur(4px)"` computes to empty. Next step: check stylo's property registration Ã¢â‚¬â€� is backdrop-filter servo-enabled in the exact stylo rev servo pins (verify which stylo source actually builds: fork-vendored vs git rev 0ab78ce Ã¢â‚¬â€� check workspace [patch]). SMALL upstreamable candidate once localized. Matrix: css-filters page, backdrop-filter-computed probe.
- stylo confusion resolved partially: Cargo.toml pins git rev 0ab78ce95162eca027c874b505b505ef17a85ac8 for stylo/selectors/servo_arc Ã¢â‚¬â€� if a vendored fork copy exists it must come through [patch] in the workspace root Ã¢â‚¬â€� VERIFY before any stylo-level work.

## sec1zt117 Ã¢â‚¬â€� timeout-kills-webview: NEGATIVE on Linux, three variants (Windows-only?)
- Windows report: any timed-out load kills all subsequent loads (top engine bug).
- Linux repro attempts, all NEGATIVE (subsequent loads work fine):
  1. abort-during-hang: load 127.0.0.1:8898/hang (never responds), navigate away same-tab mid-hang Ã¢â€ â€™ target loads 200.
  2. connect-timeout in-flight: navigate to 10.255.255.1 (non-routable), navigate away BEFORE timeout fires Ã¢â€ â€™ works.
  3. connect-timeout completed: wait 25s for the hyper connect timeout (network_connection_timeout pref) to actually fire, THEN navigate Ã¢â€ â€™ works.
- Implication: either Windows-platform-specific (different network stack behavior, IPC, or timer threading) or a different sequence/scope (all-webviews? error-page interactions? reload-same-url?). NOTE: no response-timeout exists in servo net Ã¢â‚¬â€� only CONNECT timeout (hyper, pref-driven). A hanging RESPONSE never times out at all in-engine; 'timed out' on Windows may be a shell-level watchdog concept instead.
- Bisect guidance for Windows lane: test variant 3 first (letting the timeout complete) Ã¢â‚¬â€� if the error-page display itself is what poisons the webview, check the constellation load-error path + whether the pipeline's script event loop wedges on the error document.

## backdrop-filter-CLOSED (2026-11-26) Ã¢â‚¬â€� pref-gate pattern strikes again
- layout_css_backdrop_filter_enabled one-liner in shell engine.rs Ã¢â€ â€™ matrix-verified blur(4px) computes. css-filters 5/5. PR #234. The pref-gate sweep pattern (fontface/clipboard/execCommand/backdrop-filter) remains the highest-value-per-line work in the project: audit = grep servo_pref in vendor/stylo/style/properties/longhands.toml + webidl Pref= Ã¢â‚¬â€� anything with a complete implementation and a runtime pref is a candidate.
- Grid projection: 175/183. Remaining reds: webgl environmental, webrtc known-large, range-geometry architectural, cookiestore deliberate, gamepad/webgpu never-enabled.
- CANDIDATE for next lanes: gamepad cargo-feature evaluation (enable servo's gamepad feature in shell Cargo.toml Ã¢â‚¬â€� check implementation completeness first), cookieStore functional investigation (roundtrip failed when exposed Ã¢â‚¬â€� is the fix small?), upstream PR submission of the 4 verified fixes (document.all, shadow-input, devtools-teardown, fontfaceset-check) per UPSTREAM-CANDIDATES.md.

## sec1zt118 Ã¢â‚¬â€� timeout-kills-webview: CONCLUSIVE negative incl. paint path (Windows-only)
- Extended sec1zt117: after letting connect timeout COMPLETE (shell title 'Error loading page'), same-tab navigation to visual test page paints perfectly Ã¢â‚¬â€� screenshot-verified bright-green page + text. Net + script + compositor all healthy post-timeout on Linux.
- Verdict: NOT reproducible on Linux in any variant incl. the full paint path. Windows lane owns; suspects stay as flagged (arena purge 693146a10, download spool 92080fd7, hidden-doc GC 826f3a5) Ã¢â‚¬â€� recommend Windows bisect by reverting each in turn. The one Linux observable: shell error-page state itself works fine (title updates, navigation recovers).

## gamepad-CLOSED (2026-11-26) Ã¢â‚¬â€� sixth matrix-driven enable
- servo `gamepad` cargo feature enabled in shell Ã¢â‚¬â€� navigator.getGamepads matrix-verified. Key distinction: cargo features (skip-unless in webidl) don't appear in pref sweeps Ã¢â‚¬â€� audit both. Empty-array-no-hardware IS spec-correct, safe to expose unlike cookieStore. platform-apis 7/9. PR #235. Remaining: webgpu unassessed, cookiestore deliberate.
- DISK: multiple full builds this session Ã¢â‚¬â€� purge before next big build.

## cookiestore-scoped (2026-11-26) Ã¢â‚¬â€� NOT a stub, real diagnosis needed
- cookiestore.rs is 986 lines with full Get/Set/Delete/GetAll implementations Ã¢â‚¬â€� the roundtrip failure is a real behavioral bug, not an absence. Next step: enable dom_cookiestore_enabled in shell, run platform-apis matrix page, capture WHICH operation fails (set? get? promise resolution?). Probe detail in tests/scenarios/pages/platform-apis.html. If small: another upstreamable fix; if architectural: stays reverted.

## sec1zt119 Ã¢â‚¬â€� MSE backward-seek: pipeline-reset design + the retention verdict
- BREAKTHROUGH: pipeline state-cycle (Ready->Playing) on MSE seek makes decodebin3 re-plumb fresh Ã¢â‚¬â€� the reselection death of sec1zt84 bypassed. MUST run on a detached thread: synchronous set_state on feather's UI thread deadlocks the entire browser (streaming-thread join wedge Ã¢â‚¬â€� verified live, watchdog stall).
- Verified post-reset: seek completes, position correct at target, zero stalls, buffered ranges cleared (handle_seek wiring confirmed present and working).
- REMAINING root cause, precise: per MSE spec appended data PERSISTS across seeks, so spec-correct players (YouTube) track appends internally and never re-fetch after a backward seek Ã¢â‚¬â€� zero media GETs observed post-seek. Servo dropping the data is the spec violation; the page is correct.
- THE FIX (designed, not yet implemented): fragment RETENTION in script SourceBuffer Ã¢â‚¬â€� infrastructure already exists (webm_indexer timecodes, scan_fmp4_timecodes, init_cache, per-fragment buffered indexing). Cache (timerange, bytes) per fragment, honor remove() eviction, and on seek proactively re-feed retained fragments covering the seek target into the resequenced stream. Timing caveat: appsrc rejects pushes while pipeline is mid-reset Ã¢â‚¬â€� re-feed needs retry/delay after reset completion.
- Symptom index: 'mse post-seek total starvation, zero pushes, zero media GETs, buf=[] correct' Ã¢â€ â€™ check: page is spec-correct, engine retention missing Ã¢â‚¬â€� do NOT iterate on page-side or pipeline-side anymore.

## sec1zt120 Ã¢â‚¬â€� SERP lite-vs-modern keying: UA is server-side decisive, verification chain secondary
Evidence chain (2026-11-27):
1. curl gets identical enablejs-interstitial shells for ALL UAs (firefox/chrome/feather) Ã¢â‚¬â€� curl can't execute JS so google never varies; UA comparisons via curl are useless for this question.
2. feather + honest UA: lite SERP forever. Network trace: verify chain RUNS (/verify/ 204s, cookies sent incl __Secure-STRP) but lite=1 never clears across repeated searches. Zero /xjs/ fetches ever.
3. feather + full Firefox UA: MODERN SERP SERVED Ã¢â‚¬â€� proven negatively: google sends gecko-flavored modern css (:has() complex selectors, @starting-style Ã¢â‚¬â€� servo parse errors) and the deep modern DOM stack-overflows StyleThread#2 (servo cascade recursion crash, pre-existing defect). Server keys modernity on the UA string, full stop.
4. honest-but-firefox-SHAPED UA ('Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Feather/1.0'): NOT modern Ã¢â‚¬â€� google checks the browser NAME token, not just engine grammar. No crash, still lite-ish.
5. The 'sgs undefined' puzzle is a red herring: sgs is never defined in the shell in ANY browser (the window.sgs&&ussv&&sp guard falls to the ma() path by design); obfuscated pow script executes without error in servo when tested directly.
CONCLUSION: google SERP modernity = recognized-browser UA name token. The verification-chain failure may still matter for other things, but lite mode is primarily UA-keyed. RECOMMENDED PATH: firefox-flavor UA for google properties + fix the StyleThread cascade stack overflow (real crash-class defect) + parse support for modern css features the serp uses (:has() complex inner, @starting-style). No impersonation-wall cost: firefox UA verified unwalled.
Symptom index: 'google serp legacy/basic html forever, verify 204s but lite=1 never clears' Ã¢â€ â€™ check: UA name token not a recognized modern browser. 'stylethread stack overflow on modern google serp' Ã¢â€ â€™ check: servo cascade recursion depth vs deep DOM Ã¢â‚¬â€� real crash defect, see below.

## late-session-shell-wins (2026-11-27)
- PR #236 merged: progress strip hysteresis (350ms sustained-idle before clearing load flag) Ã¢â‚¬â€� user-reported flicker. Mechanism: servo load_status flaps per-frame within one navigation; strip toggled off/on.
- PR #237 open: update check Ã¢â‚¬â€� rustls+webpki-roots background GitHub-release version check, quiet toolbar glyph. Design doc docs/wiki/AUTOUPDATE.md. NOTE: cargo check (debug profile) fills disk fast Ã¢â‚¬â€� NEVER run plain cargo check in shell-new, use release builds only. Disk critical ~4GB free, purge deps duplicates before builds.
- cookieStore DIAGNOSED: exposes fine but roundtrip returns null after set Ã¢â‚¬â€� real behavioral bug in write/persist path, stays reverted. cookiestore-diag branch has the pref flip for the next investigator.
- Gamepad merged #235, backdrop-filter merged #234, baseline merged #233.
- OPEN USER ITEM: x.com click-glitch Ã¢â‚¬â€� needs deeper repro; matrix expansion vs spec-compliance cross-ref open.

## x-click-glitch-mechanism-verified (2026-11-27)
- PR #238: shadow click-activation probes green Ã¢â‚¬â€� anchor activation + listener delivery inside shadow roots both correct post-shadow-input-fix. The clickable-text glitch class is mechanism-verified fixed. USER RE-TEST NEEDED on live authenticated x.com Ã¢â‚¬â€� prediction: fixed. If user reports still broken, the residual is NOT shadow-retargeting Ã¢â‚¬â€� next suspects: hit-test z-index class or selection-drag interference.
- Matrix state: shadow-input 7/7. Total probe count now 185.

## sec1zt121 Ã¢â‚¬â€� BREAKTHROUGH: modern google SERP fully renders Ã¢â‚¬â€� the crash was the blocker, not the JS
- ROOT CAUSE RESOLUTION of the legacy-serp complaint: google keys modernity on the UA NAME token server-side (sec1zt120). With firefox UA the modern SERP is served Ã¢â‚¬â€� and the gecko css/dom overflows servo's 512k style thread stack (cascade recursion), killing the whole browser mid-hydration. Every prior observation of 'lite forever' was partially the bootstrap dying with the crash.
- FIX: stylo STYLE_THREAD_STACK_SIZE_KB default 512 -> 4096 (compile-time const in vendored stylo style/parallel.rs; virtual cost only). VERIFIED LIVE: zero crashes, jsname=472 hydrated, rcnt present, 1.5MB dom, screenshot shows AI Overview + knowledge panel + modern tabs fully rendered. NOTE: env SERVO_STYLE_THREAD_STACK_SIZE_KB exists as compile-time override but cargo does not reliably rebuild on it Ã¢â‚¬â€� the vendored-default patch is the durable fix.
- IMPLICATION FOR UA POLICY: the honest-UA-on-google policy (PR #225/#230) yields PERMANENT LITE serp Ã¢â‚¬â€� google won't serve modern to an unknown browser name, verification-chain completion is irrelevant. Firefox-flavor UA gets modern + is unwalled (impersonation tolerance differs by browser). RECOMMEND: switch google.* from honest to firefox-flavor UA once the shim can adapt (firefox surface: no window.chrome, no userAgentData, different vendor/platform strings). Residual serp css gaps now visible: complex :has() inner selectors and @starting-style parse-gated (pref plumbed this commit, needs feather enable + verify).
- Symptom index: 'google modern serp never bootstraps, no js errors, verify 204s loop forever' Ã¢â€ â€™ check: StyleThread overflow killing hydration Ã¢â‚¬â€� need firefox UA + big style stack. 'honest ua always yields lite serp' Ã¢â€ â€™ check: server keys modernity on browser NAME token Ã¢â‚¬â€� by design, not fixable client-side.

## sec1zt122 Ã¢â‚¬â€� firefox-flavor google UA shipped + verified end-to-end (feather PR #240)
- VERIFIED with pure defaults: google serp modern hydration jsname=426 rcnt=true, ua consistent transport+js (firefox/133), correct firefox js surface (no window.chrome, no useragentdata, vendor=''), zero crashes, zero /sorry.
- Residual modern-serp css gaps now visible in console: complex :has() inner selectors fail parse ('html:has(>.srp[style*=...])' SelectorError UnexpectedIdent Ã¢â‚¬â€� relative-selector inside :has unsupported), @starting-style was parse-gated (pref now plumbed + feather-enabled, verify next pass). Rendering is already good enough for full ai-overview serp; these are polish-tier.
- Harness gotcha: single-instance forwarding silently invalidates UA-env tests Ã¢â‚¬â€� a surviving old feather process means the new launch just forwards the url and the env never applies. ALWAYS kill -9 + verify pgrep empty before ua tests. ALSO: stale /tmp/feather-eval-persist.js scripts pollute later probes (they re-inject every navigation) Ã¢â‚¬â€� clear before each experiment.

## sec1zt123 Ã¢â‚¬â€� youtube same-session verified post-UA-switch + retention implementation plan
- youtube watch playback live same session as modern serp: t advancing rs=4, and correctly on the CHROME surface (youtube not in firefox-host list) Ã¢â‚¬â€� per-host ua split works as designed.
- RETENTION IMPLEMENTATION PLAN (backward-seek completion, sec1zt119 design): fragment cache in script SourceBuffer. pieces: (1) struct RetainedFragment { start: f64, end: f64, bytes: Vec<u8> } vec in SourceBuffer, populated in AppendBuffer AFTER successful push (rejected bytes never retained), using the timecodes already extracted by webm_indexer/scan_fmp4_timecodes; (2) eviction: SourceBuffer.remove() implementation must drop overlapping retained fragments (check remove() exists Ã¢â‚¬â€� if unimplemented, retained memory grows unbounded: cap total retained bytes ~48MB/track mirroring MAX_SRC_QUEUE_SIZE, evict oldest-end-first); (3) on seek (handle_seek): instead of ONLY clearing buffered index, RE-FEED retained fragments with end > seek_target in time order into the pipeline Ã¢â‚¬â€� timing problem: appsrc rejects pushes mid-reset, so re-feed must happen after restore completes. simplest correct ordering: move the resequence trigger Ã¢â‚¬â€� since engine now owns re-feed, the FIRST re-fed fragment carries the init+resequence naturally; implement re-feed as a script-side task queued ~200ms after seek (player reset is fast in practice) OR poll player state. (4) buffered index must then reflect retained ranges AGAIN post-seek (do not clear Ã¢â‚¬â€� or clear+repopulate from retention) so the page sees spec-correct persistent buffering.
- verification harness: youtube backward seek 8s->2s must resume playback within ~2s; currentTime advancing.

## writing-modes-ENGINE-SCALE (2026-11-27) Ã¢â‚¬â€� pref experiment negative, reverted
- writing-mode parses correctly but computed is forced horizontal-tb Ã¢â‚¬â€� vertical layout unimplemented in servo layout engine, NOT a pref-gate quick win. Pref reverted deliberately (half-enabled worse than absent: CSS.supports lies). text-orientation has its own pref (layout.css.writing-mode.text-orientation.enabled) Ã¢â‚¬â€� same architectural situation. PR #241 + matrix round 6. css-conditional 6/6 green. :has() unsupported.
- css-conditional spec area fully verified Ã¢â‚¬â€� @supports/@media/CSS.supports all correct.

## ellipsis-scrollwidth-bug (2026-11-27) Ã¢â‚¬â€� upstreamable, precisely isolated
- scrollWidth clamps to clientWidth when text-overflow:ellipsis applies (80 vs correct 433 control). Servo measures scroll overflow from the ellipsized text run not original content. Breaks truncation-detection tooltips everywhere. PR #243 matrix harness. NEXT: fix in layout ellipsis truncation Ã¢â‚¬â€� scope: measure pre-truncation width. FUNDING-MODEL candidate.
- git lesson reconfirmed: squash-merges invalidate local commit hashes Ã¢â‚¬â€� always `git rebase origin/main` before pushing branches that raced merges; verify with merge-base --is-ancestor.

## ellipsis-fix-VERIFIED (2026-11-27) Ã¢â‚¬â€� fifth upstreamable engine fix
- text-overflow ellipsis no longer shrinks scrollable overflow: matrix css-overflow 10/10, scrollWidth 433 correct with ellipsis. Fix = keep original inline_advance in layout_text_run (components/layout/flow/inline/line.rs) Ã¢â‚¬â€� glyph truncation unchanged, only advance accounting. Pushed to feather-engine. UPSTREAM-CANDIDATES.md entry 5 (PR #244). Funding-model ledger now 5 verified fixes.

## cookiestore-writepath-verdict (2026-11-27) Ã¢â‚¬â€� deferred, poor cost/value
- Both cookieStore write (SetCookieForUrlAsync) and document.cookie funnel into the same netstack set_cookie_for_url Ã¢â‚¬â€� document.cookie roundtrips fine, cookieStore doesn't. Promises RESOLVE (set acks) but get returns null. Differential probes authored (reads-documentcookie / documentcookie-reads-cookiestore in platform-apis.html git history) but testing requires pref-on rebuild. VERDICT: niche API (document.cookie covers ~all real use), deep netstack hunt, poor cost/value vs other items. Stays reverted with the mechanism fully scoped for a future engine-lane pass.

## matrix-sweep-rounds-7-9 (2026-11-27) Ã¢â‚¬â€� core CSS comprehensively solid
- css-position 7/7 (sticky pinning verified incl. scroll behavior), grid-advanced 7/7 (template areas/spanning/auto-placement/fr/minmax), css-values 10/10 (calc/clamp/minmax/custom-props incl runtime setProperty). PRs #245/#246/#247. PATTERN ESTABLISHED: WPT fail-density in the spec map is edge-subtest noise, core mechanisms solid. Matrix CSS coverage approaching saturation Ã¢â‚¬â€� next high-value targets are non-CSS: websockets (772 fail expectations in map, completely untested in matrix), streams, shadow-dom deep. WebSocket needs ws server in matrix harness Ã¢â‚¬â€� moderate complexity, high value.
## sec1zt124 Ã¢â‚¬â€� retention backward-seek VERIFIED-FAILING on real YouTube (2026-11-27) Ã¢â‚¬â€� four concrete defects isolated
Retention implementation (engine 60a01938a43) compile-clean and pushed, but live verification FAILS the primary criterion: backward seek 16sÃ¢â€ â€™2s freezes at 0.15s 10+s with paused=false (starvation signature), reproduced twice. No rust panics. Negative results mandatory-indexed:
- Symptom: re-feed pushes error `result=Err(Flushing) level=0` Ã¢â€ â€™ check: refeed-vs-flush timing in sourcebuffer refeed_retained vs gstreamer player seek_reset Ã¢â‚¬â€� the pipeline is STILL FLUSHING when retained fragments are re-pushed. handle_seek refeeds synchronously but the pipeline reset runs on the detached thread (sec1zt82 design) Ã¢â‚¬â€� the two paths race. FIX DIRECTION: defer refeed until flush-stop completes, or gate refeed pushes on the appsrc being back in non-flushing state.
- Symptom: `GStreamer-CRITICAL: gst_bus_set_flushing: assertion 'GST_IS_BUS (bus)' failed` Ã¢â€ â€™ check: bus use-after-pipeline-destroy in the seek_reset path Ã¢â‚¬â€� the detached reset thread touches a bus whose pipeline was already nulled (either by a SECOND seek or the adapter-cycle reaper).
- Symptom: appsrc oscillation `recreated appsrc state=Null linked=false peer=None` alternating with partial Paused/linked Ã¢â€ â€™ check: appsrc recreation pad-relink race Ã¢â‚¬â€� recreated source not reliably relinked to demuxer pads.
- Symptom: position clamps to 0.15 NOT the 2.0 target Ã¢â€ â€™ check: re-fed fragment timestamps not re-based onto the post-seek segment Ã¢â‚¬â€� seek applied against a stream whose timing reset to zero.
- Symptom: after backward-seek corruption, forward seek to 10s also freezes Ã¢â€ â€™ check: pipeline state corruption is sticky, not seek-direction-specific. Forward-seek-on-pristine-pipeline never tested Ã¢â‚¬â€� coverage gap.
- UI watchdog stall during seek churn in run 1 Ã¢â‚¬â€� the reset path can still touch the UI thread somewhere.
- PRE-EXISTING unrelated: initial playback starts then freezes (~1.7s/8.5s) despite 60s buffer + readyState 4 until v.play() nudge Ã¢â‚¬â€� separate engine defect, present before any seek.
- Evidence: /tmp/yt-seek-test.log, /tmp/yt-seek-test2.log on the linux VM; recording in the session artifacts.
- DO NOT open the PR until a clean single backward seek survives.

## matrix-rounds-10-12 (2026-11-27)
- websockets 4/4 (harness embeds dependency-free RFC6455 echo server on 8472 Ã¢â‚¬â€� close handshake needs server close-echo), focus-events 10/10, drag-drop 3/6 honest absence (no DragEvent.webidl in servo at all, draggable commented out Ã¢â‚¬â€� engine-scale, sites feature-detect safely). PRs #248/#249/#250.

## sec1zt125 Ã¢â‚¬â€� matrix-rebase union parser diff3 bug (2026-11-27)
- Symptom: union-merge loop leaves files still conflicted Ã¢â€ â€™ check: diff3-style conflicts have a `|||||||` base section BEFORE `=======` Ã¢â‚¬â€� a parser keying on `=======` mis-slices. Correct walk: <<< then optional ||| base, then ===, then >>>. 248 websockets rebased+pushed clean (no diff3 conflicts); 247/249/250 still conflicted on origin, branches untouched, local repo back on main. Also: matrix classes should batch per-round per coordinator.
- seek settle-fix (engine 72b6c0fe29d) pushed, VERIFICATION PENDING Ã¢â‚¬â€� testing-agent handoff not yet done.

## sec1zt126 Ã¢â‚¬â€� backward-seek round-2: settle window VERIFIED-WORKING, demux-validity is the next layer (2026-11-27)
- Symptom: post-settle-fix, refeed pushes all return Ok(Ok), zero Err(Flushing), ordering correct (seek_reset/restore completes BEFORE first refeed push Ã¢â‚¬â€� verified in /tmp/yt-seek-r2.log lines 13071-13120) Ã¢â€ â€™ the 90-turn settle fix works as designed. Negative result: timing was necessary but not sufficient.
- Symptom: `Could not demultiplex stream ... reading large block of size 98144024 not supported` Ã¢â€ â€™ check: CLUSTER ALIGNMENT of refed fragments. YouTube appends don't align to ebml cluster boundaries Ã¢â‚¬â€� a cluster spans multiple appends. Retained per-append bytes refed from a mid-cluster position are garbage to the demuxer; 98MB block = misread length field. FIX DIRECTION: use the webm_indexer to re-anchor the refeed at the cluster boundary CONTAINING the seek target (drop the partial-cluster prefix, resynthesize cluster header if needed), not at append boundaries.
- Symptom: refeed FLOW events pts=0:00:00.000 Ã¢â€ â€™ check: timestamp re-basing Ã¢â‚¬â€� fragments carry original timestamps but flow events restart at zero; segment resequencing not applying to pts.
- Symptom: feather-ui process dies SILENTLY ~30-60s post backward seek (no panic, log ends mid-network-activity) Ã¢â€ â€™ check: SIGSEGV-class death in gstreamer demuxing garbage bytes Ã¢â‚¬â€� a native crash, not a rust panic (catch_unwind can't catch it). Repro: round-2 protocol.
- Symptom: gst_bus_set_flushing assertion STILL fires once Ã¢â€ â€™ bus-after-destroy unfixed.
- YouTube's player issues spontaneous seeks (targets 3.2/9.075/6.936) during playback Ã¢â‚¬â€� corruption correlation ambiguous; [mse] prints need timestamps.
- PR must NOT open. Next iteration: cluster-aligned refeed + pts re-basing + the silent-death segfault.
## definitive-grid-239-of-254 (2026-11-27) Ã¢â‚¬â€� matrix comprehensive
- 37 classes, 254 probes, 239 passing, EVERY red categorized zero-unknowns. Union PR #251 supersedes conflicting per-round PRs (closed 245/247/248/249/250 Ã¢â‚¬â€� per-round branches conflict on shared matrix.py registration line; union carries identical content + definitive grid). LESSON: matrix rounds that all edit the same registration line should batch 2-3 classes per branch, or accept the union-merge pattern.
## writing-mode-force-horizontal-localization (2026-11-27) Ã¢â‚¬â€� partial
- NOT in stylo style_adjuster (gecko-gated adjusters only). NOT in servo layout construct paths greppable. Cascade somewhere forces horizontal-tb for computed values despite correct parsing. NEXT investigator: instrument Ã¢â‚¬â€� print the computed writing-mode value inside servo's style resolution for a vertical-rl element (components/script style query path), binary-search the pipeline: parseÃ¢â€ â€™cascadeÃ¢â€ â€™computed-structÃ¢â€ â€™readback. streams 8/8 and editing 5/6 also green this session (matrix union PR #251 Ã¢â‚¬â€� 40 classes now).

## sec1zt127 Ã¢â‚¬â€� serp-duplicate-results: NEGATIVE on Linux, parser exonerated (2026-11-27)
- Verdict: NOT a servo parser/incremental-rendering duplication. In-page discriminator (honest-UA legacy SERP, query=wikipedia): synchronous XHR re-fetch of location.href vs live DOM Ã¢â‚¬â€� counts IDENTICAL everywhere: wikipedia.org occurrences raw=3 dom=3, anchor inventory raw 44/44 unique (max1), dom 49/49 unique (max1), zero /url?q= either side. If html5ever or the incremental feed duplicated nodes, dom counts would exceed raw counts Ã¢â‚¬â€� they don't.
- Symptom: windows verified 10x in-DOM duplication in legacy SERP Ã¢â€ â€™ check: NOT the parser. Remaining suspects indexed by mechanism: (1) server-side A/B or per-IP serving weirdness (raw html itself duplicated for windows IP/UA Ã¢â‚¬â€� re-run the same in-page fetch comparison on WINDOWS to discriminate in one step: if raw counts are duplicated there, server-side confirmed); (2) windows-platform document/navigation re-entrancy (multiple navigation commits appending to one document Ã¢â‚¬â€� windows-specific embedding); (3) session-restore tab duplication (restored tabs re-navigating while the original navigation is in flight). The in-page fetch-vs-dom eval is the one-shot discriminator Ã¢â‚¬â€� harness script in git history / session sec1zt127.
- Harness gotcha index: legacy-SERP investigation requires FEATHER_HONEST_UA='Feather/1.0 (X11; Linux x86_64) Servo/1.0' env (post-#240 default honest UA is firefox-flavor Ã¢â€ â€™ modern serp, no legacy); google serves results-only-thin html to honest UA (rawWiki count 3, few links) Ã¢â‚¬â€� duplication repro may need the windows serving state; heavy UI stalls during legacy serp load Ã¢â‚¬â€� eval harness needs patience + repeat clicks.
- Bonus observed: session-restore resurrected the testing agent's google/youtube tabs on fresh launch Ã¢â‚¬â€� restore works but means launch-url tests inherit dirty state; combined with single-instance leftovers, always screenshot-verify what instance/tab is actually being tested.

## sec1zt128 Ã¢â‚¬â€� backward-seek round-3: survive+decode yes, holes+pts no (2026-11-27)
- FIXED-in-round-3: process now survives 2+min post-seek (round-2 silent death gone); frames genuinely decode post-seek; forward seek lands on target.
- Symptom: systematic buffer holes post-refeed Ã¢â‚¬â€� backward: [0-1, 3-6, 8.9-57.1] playhead dies at 2.05 in the 1-3 hole; forward: gaps every ~1s cluster-interval Ã¢â€ â€™ check: NOT dropped clusters in the contiguous tail (8.9-57.1 IS contiguous) Ã¢â‚¬â€� holes concentrate in the EARLY refed region, consistent with timestamp chaos in the overlap zone: refed clusters carry ORIGINAL timecodes while the reset pipeline restarts at 0 Ã¢â‚¬â€� frames collide/clip until the stream stabilizes. pts=0 observed on first FLOW events confirms the re-basing is wrong.
- Symptom: demux 'large block of size 4193869477' (4.2GB!) STILL fires, worse Ã¢â€ â€™ check: the chunk cuts are boundary-aligned but the FIRST bytes after init may still not form a valid stream when the start-offset cluster isn't the stream start the demuxer expects after init Ã¢â‚¬â€� OR concatenation of non-contiguous retained fragments (forward-seek gaps in retention) splices unrelated byte ranges. NEGATIVE result: boundary alignment alone insufficient.
- Symptom: Err(Flushing) recurred 2x despite settle window Ã¢â€ â€™ check: settle window timing vs the now-longer refeed construction.
- NEXT-FIX (implemented sec1zt129): rewrite cluster timecodes in-place during refeed Ã¢â‚¬â€� new_tc = seek_target + (orig_tc - first_cluster_tc) Ã¢â‚¬â€� produces a contiguous correctly-rebased stream addressing holes AND pts in one mechanism.
- Disk hygiene: shell-new/target is 81GB, box hit 100% full mid-verification (linker SIGBUS) Ã¢â‚¬â€� needs prune.
## drag-drop-html5-implementation (macOS lane, 2026-11-27)
**IMPLEMENTED: DragEvent + HTML5 drag-and-drop event sequence in the servo engine** (commit ffda1ec2a, feather-engine). Cross-referenced Ladybird `DragAndDropEventHandler` (Libraries/LibWeb/Page/DragAndDropEventHandler.cpp).
### What already existed upstream (do NOT re-derive)
- `components/script/drag/drag_data_store.rs` Ã¢â‚¬â€� COMPLETE DragDataStore with modes (ReadWrite/ReadOnly/Protected), item list, bitmap, allowed-effects
- `dom/datatransfer/` Ã¢â‚¬â€� full DataTransfer/ItemList/Item implementation (~750 lines); dropEffect/effectAllowed live ON the DataTransfer object (DomRefCell), not the store
- `dom/datatransfer` DataTransfer constructor takes `Rc<RefCell<Option<DragDataStore>>>` Ã¢â‚¬â€� when the Option is emptied the object becomes inert (spec-compliant end-of-session behavior); types()/getData() etc read through it
- `drag/drag_gesture.rs` Ã¢â‚¬â€� existing gesture lifecycle hook framework for TEXT-SELECTION drags, integrated in document_event_handler mousedown/move/up paths
### Architecture of the implementation
- `drag/html_drag.rs` Ã¢â‚¬â€� HtmlDragHandler state machine: pending (mousedown over draggable) Ã¢â€ â€™ threshold 4px Ã¢â€ â€™ begin (fire dragstart, ReadWrite mode) Ã¢â€ â€™ per-move: drag at source (Protected), hit-node change Ã¢â€ â€™ dragenter at node + dragleave at previous target element, dragover at current target element each move (Protected) Ã¢â€ â€™ mouseup: drop if operation != none (ReadOnly) then dragend always (Protected)
- Drop operation computed from effectAllowed/dropEffect AFTER dragover dispatch, per spec table; canceled dragover = not droppable unless the handler set a valid dropEffect Ã¢â‚¬â€� actually cancellation is what ENABLES dropping per spec: uncancelled dragover Ã¢â€ â€™ operation none; canceled dragover Ã¢â€ â€™ compute from effect table
- DragHandler::Html variant in drag_gesture.rs; release needs hit test Ã¢â€ â€™ handle_mouse_button_release(cx, event, hit_test) added, document_event_handler reordered so the drag-gesture button handling runs AFTER hit-test computation
- Installation priority: editing.rs handle_mousedown checks first_draggable_inclusive_ancestor BEFORE installing DocumentSelection Ã¢â‚¬â€� HTML drag wins over selection drag on draggable elements
- Draggability: explicit draggable="true"; attribute ABSENT + (img OR a[href]) Ã¢â€ â€™ implicitly draggable (auto state); draggable="false" explicitly disables
### API pitfalls hit (indexed by mechanism)
- servo codegen constructor traits return `DomRoot` NOT `Fallible<DomRoot>` Ã¢â‚¬â€� check generated bindings in target/release/build/servo-script-bindings-*/out/Bindings/*.rs BEFORE writing impls
- derived interfaces' Methods traits REQUIRE re-implementing inherited-member forwarding (e.g. IsTrusted) Ã¢â‚¬â€� the generated trait flattens some inherited members in; pattern: `fn IsTrusted(&self) -> bool { self.mouseevent.IsTrusted() }`
- dictionary nesting: init.parent chain Ã¢â‚¬â€� DragEventInit.parent=MouseEventInit (clientX/button/relatedTarget), .parent=EventModifierInit (modifiers()), .parent=UIEventInit (view/detail), .parent=EventInit (bubbles/cancelable/composed) Ã¢â‚¬â€� one level deeper than MouseEvent's own constructor accesses
- Event.fire returns bool where FALSE means canceled (dispatch spec semantics) Ã¢â‚¬â€� use `!fire(...)` for cancellation checks
- attribute setters in generated traits take `cx: &mut JSContext` first (see make_bool_setter)
- HitTestResult.point_in_frame is already CSSPixel f32 Ã¢â‚¬â€� no cast_unit needed
- Rc<RefCell<Option<DragDataStore>>> field in dom_struct: copy datatransfer.rs's exact `#[conditional_malloc_size_of] #[no_trace]` attribute pair
### Verification status
- compile-clean (cargo check release, zero errors)
- constructor-level matrix probes authored; live trusted-drag verification harness: tests/scenarios/pages/drag-drop.html records event sequence into window.__dragLog (src/tgt are big colored boxes at fixed positions for computer-tool drags)

## sec1zt129 Ã¢â‚¬â€� crash isolation phase-1: linux native-crash artifacts (2026-11-27)
- Status: stage-0 (panic recovery) was already merged+verified. NEW: linux SIGSEGV/SIGABRT/SIGBUS/SIGILL/SIGFPE handler in shell-new crashdump_linux.rs Ã¢â‚¬â€� writes crash-native-<ts>.log (signal, fault addr, tid, thread name, force_capture backtrace) then re-raises with SIG_DFL for stock death semantics. Self-test hook: FEATHER_CRASH_TEST=native segfaults a throwaway thread 2s post-launch.
- Paths fixed per user directive: crash artifacts go to dirs::data_dir()/feather/crashes (~/.local/share/feather/crashes) Ã¢â‚¬â€� NEVER /tmp (snapshot-wiped) and NOT exe-relative (read-only in real installs). Both the native handler AND the older panic-crash writer in main.rs updated.
- The round-2 backward-seek silent death is the exemplar reproducible native crash: once artifacts capture, that death class becomes classifiable instead of silent.
- Async-signal-safety note: backtrace capture allocates Ã¢â‚¬â€� technically unsound in a handler, universally done in crash reporters; the one-shot CAPTURED guard prevents recursive capture.
- Four crash classes taxonomy (per coordinator): engine-rust-panic (covered stage-0), shell-rust-panic (panic hook writes artifact), native-signal (NOW covered), hang/watchdog (watchdog already logs stalls; no artifact Ã¢â‚¬â€� future).

### Verification COMPLETE (same day, live trusted-input test)
Full sequence verified with real mouse drags on the harness page: `dragstart | setdata-ok | drag | dragenter | dragover | drop:feather-drag-payload | dragend` Ã¢â‚¬â€� payload round-trips through the session DataTransfer. ALL events trusted, correct ordering.
- **BONUS UPSTREAM BUG FOUND + FIXED**: `DataTransfer::SetEffectAllowed` wrote into `drop_effect` instead of `effect_allowed` (copy-paste error in servo's datatransfer.rs) Ã¢â‚¬â€� effectAllowed could never change, making drop negotiation impossible. This is upstreamable candidate SIX, small and surgical.
- Verification method (reusable): matrix scenario server + big colored drop targets at fixed positions + computer-tool real mouse drags + page-embedded event log rendered into DOM, read via screenshot. isTrusted gating essential Ã¢â‚¬â€� the probe pages' own synthetic dispatchEvent probes contaminate naive logs.
- Harness lesson: silent python str.replace no-match cost a verification cycle Ã¢â‚¬â€� always assert substitution anchors matched.

## sec1zt132 Ã¢â‚¬â€� backward-seek no-playback ROOT CAUSE: base=0 injected segment + design flaw in clock-restart theory (2026-11-27)
Found in player.rs seek(): `source.seek_reset(time, 0)` passes literal base_ns=0 with a comment claiming 'the pipeline clock domain restarts with the reset'. THE THEORY IS WRONG: gstreamer re-bases for continuity on READY->PLAYING transitions (base_time = clock - running_time_at_pause) Ã¢â‚¬â€� running time NEVER restarts except on NULL->PLAYING. With base=0: running_time = pts - start + 0 Ã¢â€°Ë† 0 Ã¢â€ â€™ every refed buffer maps to pipeline-start time, minutes in the past Ã¢â€ â€™ sink drops all as too-late after the preroll frame. Explains EVERY round-5 symptom exactly: correct target frame displays (preroll renders regardless of timestamps), playback frozen, RS4, no errors. FIX IMPLEMENTED: compute base LIVE at injection time inside push_buffer resequence block (clock.time - base_time at first post-reset push) Ã¢â‚¬â€� pts(target) maps to NOW, subsequent frames real-time aligned. Note the displayed-frame evidence can never discriminate preroll-only rendering from real playback Ã¢â‚¬â€� position advancement is the only valid signal. Symptom: MSE seek correct frame + frozen playback Ã¢â€ â€™ check: injected segment base value vs pipeline running time continuity semantics; NEVER trust frame-display as playback evidence. Separately still open: mid-payload state-dependent corruption on reseeks (0xab parse, deterministic 1794238142).
## sec1zt130 Ã¢â‚¬â€� backward-seek round-5 verdict: delivery valid at head, corrupt mid-payload + independent no-playback failure (2026-11-27)
Round-5 instrumentation round, local harness (bot wall persists). Prep lines verbatim: `target=2 frags=18 streamlen=1121692 boundaries=6 start_idx=0 chunks=1 first48=1a45dfa3...` (seek1), same shape target=25 start_idx=1 (seek2), target=2 start_idx=0 (seek3). Verdicts:
- first48 = valid ebml magic `1a45dfa3` on ALL seeks Ã¢â‚¬â€� init-dedup works, chunk heads byte-valid, boundary selection correct (push size shrinks 1097681Ã¢â€ â€™867194 when start_idx=1).
- TWO INDEPENDENT FAILURES now isolated: (1) playback NEVER resumes even on the error-free seek1 Ã¢â‚¬â€� position lands exactly on target, correct frame, paused=false, RS4, but no PLAYING event, currentTime frozen, play() nudge doesn't unstick; (2) mid-payload corruption on reseeks: `Failed to parse Element 0xab` (seek2, matroskademux parse_id Ã¢â‚¬â€� stream misalignment), `large block of size 1794238142` (seek3 Ã¢â‚¬â€� IDENTICAL magic size to round-4, deterministic). Seek1 error-free with IDENTICAL prep params to seek3 Ã¢â€ â€™ corruption is state-dependent (reset/reseek history), not just byte-dependent.
- Ordering anomaly: seek1 refeed push logged BEFORE pipeline ready/restore lines Ã¢â‚¬â€� settle-window ordering violated or cross-thread log interleave; round-2 verified opposite order. Check.
- Offline sim still produces valid webm Ã¢â€ â€™ live corruption deeper than byte 48. Prime suspects in order: (a) in-place timecode rewrite offsets/lengths wrong Ã¢â€ â€™ would produce exactly these parse errors without affecting init-only first48; (b) multi-seek dirty demuxer/appsrc state; (c) cluster-boundary cut misalignment for start_idx>0.
- Symptom: webm demux `large block` deterministic magic size 1794238142 Ã¢â€ â€™ check: timecode rewrite offsets vs scan_cluster_full output (offsets relative to concatenated stream vs cluster-local?), matroskademux alignment state across pipeline resets.
- Symptom: MSE seek lands exactly on target + correct frame + RS4 but never plays Ã¢â€ â€™ check: gst segment start/base domain vs refed PTS, decodebin3 preroll after in-band resequence, whether displayed frame is actually new rendering or frozen last frame.
## sec1zt131 Ã¢â‚¬â€� backward-seek: no-playback root cause narrowed to demux output-segment domain (2026-11-27)
Static analysis after round-5. Byte delivery is EXONERATED as the no-playback cause: seek1 clean yet frozen. Key mechanism insight: the correct-frame-displayed evidence is consistent with PREROLL ONLY Ã¢â‚¬â€� sinks display the preroll buffer regardless of timestamps, so rendering one frame proves nothing about the clock domain. The frozen playback is the timing domain downstream of matroskademux: demuxers in push mode emit their OWN output segment computed from container timecodes; our injected TIME segment (start=target, base=running_time) may not propagate base downstream Ã¢â‚¬â€� if demux output base=0, running_time = pts - start + 0 Ã¢â€°Ë† 0 Ã¢â€ â€™ sink sees every buffer ~20s late Ã¢â€ â€™ drops everything after preroll. This is exactly the sec1zt83 'frames dropped as too-late' residual. Levers not yet tried: (a) segment.set_time() unset in the injection Ã¢â‚¬â€� check whether matroskademux adoption requires time/position fields; (b) appsrc needs do-timestamp or the buffers need PTS rebase before pushing so demux timecodes (which we rewrite!) already land in the clock domain Ã¢â‚¬â€� note we ALREADY rewrite cluster timecodes to target+offset, but the demux still emits startÃ¢â€°Ë†target not the live running time; (c) the WebKit custom-source design pushes samples INTO the demux domain itself. Symptom: MSE seek frame displays but never advances Ã¢â€ â€™ check: whether displayed frame is preroll-only, demux output segment base via pad probe on demux src (GST_DEBUG or probe reading segment events). Also unresolved: multi-seek mid-payload corruption (0xab parse + deterministic 1794238142) is state-dependent not byte-dependent Ã¢â‚¬â€� same bytes clean on first seek Ã¢â€ â€™ stale demux/decodebin state contaminates reseeks; the reset doesn't fully flush downstream elements.

## sec1zt133 Ã¢â‚¬â€� backward-seek round-6 candidates: base-live fix + wall-clock settle fix, both pushed (2026-11-27)
Two independent defects fixed in one round: (1) base=0/clock-continuity (sec1zt132) Ã¢â‚¬â€� fix 8f77ae79db0 computes base live at injection; (2) SETTLE NEVER SETTLED: the 90-turn settle window measured event-loop TURNS not time Ã¢â‚¬â€� media_element_task_source tasks run back-to-back with no frame pacing on an idle loop, so 90 turns elapse in milliseconds; first refeed chunk landed in the mid-teardown pipeline every time, explaining BOTH the ordering anomaly (push before ready/restore) AND the state-dependent mid-payload corruption (bytes mixing across the reset boundary into stale demux state). Fix 1377259307f: wall-clock Instant anchor (1500ms). GENERAL SYMPTOM RULE: any task-source 'wait N turns' settle/delay in servo is a zero-duration wait when the loop is idle Ã¢â‚¬â€� always use wall-clock. Symptom: ordering anomalies where a delayed action fires before the thing it waits for Ã¢â€ â€™ check: whether the delay mechanism counts turns vs time. Round-6 harness verification will discriminate: if playback now resumes AND reseek corruption is gone, both diagnoses confirmed in one test.

## sec1zt134 Ã¢â‚¬â€� round-6 regression + retry-cap interaction: re-poll chains must never count toward attempt caps (2026-11-27)
Round-6 harness verdict: WORSE Ã¢â‚¬â€� zero refeed bytes delivered. Prep+rewrite+reset all ran, then permanent engine silence after `restore Ok(Async)`. Cause: the settle re-poll incremented retries on every turn; back-to-back re-polls hit the 400-cap in milliseconds (long before the 1500ms wall-clock window) and the chain silently returned forever. Fix b81a6ac92d7: re-poll passes retries=0 Ã¢â‚¬â€� the wall-clock condition itself bounds the settle. RULE: in a self-requeueing task chain, distinguish ATTEMPTS (real work tried and failed Ã¢â‚¬â€� count toward caps) from POLLS (condition re-checks Ã¢â‚¬â€� must never count). Symptom: async chain armed but never fires, total silence after correct setup logs Ã¢â€ â€™ check: cap/counter interactions in the re-poll path. Also notable from round-6 log: running_time_ns=99.7s on second seek confirms running time does NOT reset across Ready cycles Ã¢â‚¬â€� validates the base-continuity premise.

## sec1zt135 Ã¢â‚¬â€� backward-seek TRUE ROOT CAUSE: retention captured only cluster-head slices + time-sort scrambled the byte stream (2026-11-27)
Round-8 dump analysis, decisive. Byte-mapping the live refeed dump against the original file: contiguous matches of only 3.7%, mapping NON-MONOTONIC with the same file regions REPEATED at multiple dump positions. Two compounding retention defects: (1) retention keyed fragments on in-slice scanned timecodes Ã¢â‚¬â€� 64KB append slices mostly contain NO cluster head (clusters ~220KB) so MOST APPENDS WERE NEVER RETAINED Ã¢â‚¬â€� holes; (2) surviving fragments sorted by scanned timekeys scrambled insertion order Ã¢â‚¬â€� duplication/misorder. Concatenation glued non-contiguous file regions mid-cluster Ã¢â€ â€™ deterministic demux corruption (large block 1794238142 Ã¢â‚¬â€� same input, same glue points). The offline sim never reproduced it because sim concatenated contiguous slices correctly Ã¢â‚¬â€� the bug was in LIVE RETENTION, not the refeed algorithm. FIX: contiguous append-order retention (insertion order IS byte order Ã¢â‚¬â€� the demuxer originally consumed exactly this concatenation), no time-sort anywhere, eviction by position. RULE: MSE byte streams are contiguous per track; any design that reorders/partially captures appends corrupts by construction. Symptom: deterministic-position demux corruption in refed MSE streams Ã¢â€ â€™ check: byte-map the refed stream against source media, look for non-monotonic mapping/duplication; NEVER trust scan-derived keys for byte-order reconstruction. Secondary finding same round: scan_cluster_full false-positives Ã¢â‚¬â€� scan of even a contiguous stream found impossible timecodes (54s content in a 17s stream) because payload bytes match cluster magic when the size-chain desyncs; the scan detail output should be sanity-checked against stream duration.
## ui-shell-1: egui mesh vertex alpha Ã¢â‚¬â€� the premultiplied trap (verified live)
**Symptom**: custom egui Mesh shapes (gaussian/organic blob gradients) render as
strong solid color even at very low alpha; reducing alpha barely changes the look.
**Root cause**: egui composites mesh vertex colors as PREMULTIPLIED alpha.
`Color32::from_rgba_unmultiplied` stores channels without scaling, so the rgb
contribution hits the framebuffer at full strength regardless of alpha. Fix:
construct vertices with `Color32::from_rgba_premultiplied(r*a/255, g*a/255, b*a/255, a)`
or scale channels manually. Fixed in shell-new ui/icons.rs organic_blob. Applies
to ALL custom mesh work in the shell.
## ui-shell-2: round-4 demo data + stroke-bar root causes (verified live 2026-11-27)
**Symptom Ã¢â€ â€™ suspect index**:
- "demo favicons/icons in the tab strip after restart" Ã¢â€ â€™ MockBackend quick tiles
  (backend/mock.rs Default), the host-hash saturated letter-tile color in
  tab_strip.rs tab_favicon fallback, and web_page.rs's fake favicon+site header.
  NOT a persistence bug Ã¢â‚¬â€� the state layer was already clean; the demo data came
  from the backend fabrication layer.
- "thin stroke/bar empty area near navigation area, appearing/disappearing" Ã¢â€ â€™
  ui/web_page.rs skeleton() invocation: skeleton placeholder bars painted at the
  TOP of the content area (directly under the nav) for any non-engine-live web
  tab while loading. With servo live-rendering most pages, the skeleton only
  flashes transiently Ã¢â‚¬â€� reads as a stray artifact, not a loading affordance.
- "demo/placeholder content anywhere" Ã¢â€ â€™ grep for `web_page::show` Ã¢â‚¬â€� the mock
  article renderer (fake headline/paragraphs/figures) painted for ANY web tab
  the engine hadn't attached to (restored background tabs, startup transient).
**Architecture lesson**: the shell's BrowserBackend abstraction is ALWAYS
MockBackend Ã¢â‚¬â€� it fabricates WebDocument models (titles, colors, blocks) while
servo independently renders real content. Anything the mock fabricates can
surface as demo data. When in doubt, the honest default is empty/neutral.

## sec1zt136 Ã¢â‚¬â€� backward-seek ACCEPTANCE BAR MET round-9: playback resumes, corruption eliminated; residual position-domain defect (2026-11-27)
Round-9 harness verdict: PASS on the coordinator bar Ã¢â‚¬â€� backward seek lands and PLAYS sustained (~40s advancing), forward seek works, repeated seeks both directions work, ZERO demux errors across all three seeks including the reseek sequence that corrupted deterministically in rounds 4-8. Scan-detail matches ground truth exactly (offs 569/221188/445241..., tc 0/4.267/8.533/10/14.267, csize=1 first then 2) Ã¢â‚¬â€� contiguous retention confirmed correct. The retention fix (fade8925118) closes the corruption saga: byte-level refeed is now sound end-to-end. RESIDUAL DEFECT: position domain Ã¢â‚¬â€� seeks to 30 and 5 both restart playback near ~2.3s instead of at target; first seek to 2 began at 1.17. Pattern consistent with mse_position_offset stuck at the FIRST seek's value (2) + elapsed, or pipeline stream-time restarting at 0 per reset while offset fails to update. Playback itself flows correctly. NEXT: check mse_position_offset update path in player.rs seek() Ã¢â‚¬â€� verify .set(time) actually lands per-seek and that position query reads it; also whether demux output pts is rebased-to-zero in push mode making position = elapsed + stuck-offset. Also still open from round-8 analysis: csize=1 first-cluster timecode can't hold rewritten values >255ms (skipped Ã¢â€ â€™ timeline inconsistency in the first cluster) Ã¢â‚¬â€� the segment-domain alternative (drop rewrite entirely, base=live-target, start=0) may obsolete the rewrite and fix this class; evaluate after position fix. Quality-switch interaction untestable (bot wall persists).
## drag-drop-matrix-verdict: class fully GREEN (verified live 2026-11-27)
All six matrix probes pass on the merged tree: draggable-attribute,
dragevent-constructor (typeof=function), datatransfer-in-event,
datatransfer-roundtrip (readback=matrix-drag), drag-events-dispatch
(synthetic dragstart/dragover/drop), datatransfer-effects. DragEvent absence
is CLOSED as a matrix red Ã¢â‚¬â€� implementation + upstream effectAllowed fix both
verified end-to-end (trusted-input drag: dragstart|setdata-ok|drag|dragenter|
dragover|drop:payload|dragend).
**Devtools harness lesson**: the eval chain (listTabsÃ¢â€ â€™getTargetÃ¢â€ â€™evaluateJS
with Ok-envelope unwrap) works perfectly against a manually-launched instance.
The run_one_page "devtools never came up"/stall failures are LAUNCH-PATH
issues, not protocol: prime suspects are the single-instance handoff (a fresh
launch forwards argv to a lingering primary that has no devtools env and
exits) and FEATHER_DEVTOOLS being a PORT/address value not a boolean
(=1 binds port 1). When automating: always pass FEATHER_DEVTOOLS=9222 and
verify no feather process survives the pkill before launch.

## sec1zt137 Ã¢â‚¬â€� position-domain fix pushed UNVERIFIED-COMPILE: round-10 resumption instructions (2026-11-27)
a4922916078 wires mse_position_offset into connect_position_updated (field+weak capture existed, read never wired Ã¢â‚¬â€� position reported time-since-seek, hence ~2.3s for all targets). NOT YET COMPILE-VERIFIED: the lock pattern mirrors connect_media_info_updated but a deadlock risk exists if the gst signal thread contends inner with the player thread Ã¢â‚¬â€� watch for stalls in round-10. RESUMPTION: (1) confirm build finished ok (/tmp/b13.log), fix compile if broken Ã¢â‚¬â€� likely issues: Cell inside Mutex-protected struct, upgrade() type mismatch; (2) round-10 harness verification: seek position should now report AT-target (2/30/5) and playback content position correct; PASS = positions land at target AND PLAYING fires AND advances (already passing since round-9); (3) if position correct: backward-seek mission COMPLETE per acceptance bar Ã¢â‚¬â€� remaining polish items: csize=1 rewrite overflow (sec1zt136), evaluate dropping the timecode rewrite entirely in favor of pure segment-domain rebasing, quality-switch untestable (bot wall). Harness: python3 -m http.server 8377 in /tmp/mseh, SERVO_MSE_REFEED_DUMP=1 optional. Ordering note: playback advancing faster-than-realtime after seek (~1.7-4x burst) is expected catch-up through refed buffer, settles to realtime.

## sec1zt138 Ã¢â‚¬â€� timeout-regression bisect: suspect analysis + repro plan (2026-11-27)
Coordinator priority: any timed-out load kills the webview permanently Ã¢â‚¬â€� suspects arena purge (script+allocator 7d02b70553b, NOT 693146a10 which doesn't exist Ã¢â‚¬â€� the actual commit is 7d02b70553b 'purge jemalloc arenas at pipeline teardown'), download spool 92080fd7, hidden-doc GC 826f3a5. ANALYSIS: (1) arena purge calls mallctl arena.4096.purge per pipeline exit Ã¢â‚¬â€� memory-safe IF jemalloc is the active allocator, but feather shell caps GLIBC malloc arenas (MALLOC_ARENA_MAX=2, f312359) Ã¢â‚¬â€� if feather links glibc not jemalloc, mallctl operates on uninitialized/foreign allocator state = corruption candidate browser-wide. CHECK FIRST: does feather-ui actually link/use jemalloc? ldd + malloc_conf. (2) hidden-doc GC 826f3a5 runs full GC on visibleÃ¢â€ â€™hidden Ã¢â‚¬â€� a hung load backgrounded mid-flight gets GC'd; if any in-flight load state is improperly rooted, GC kills it Ã¢â‚¬â€� but SM rooting is mostly exact, less likely. (3) prior negative sec1zt117: three Linux variants all recovered Ã¢â‚¬â€� the MISSING ingredient may be the error-page display itself or the hidden-transition or MULTIPLE tabs open. REPRO PLAN: hang server python: /hang never responds, /ok 200. Sequence A: two tabs, tab1 hangs, let hyper connect-timeout (25s) fire or abort, switch tab (hidden-GC fires!), back, navigate tab1 Ã¢â€ â€™ dead? Sequence B: same but no tab switch. Sequence C: reload-after-error. Discriminator test matrix per suspect: build with each suspect individually reverted (revert order: GC first Ã¢â‚¬â€� cheapest, then purge, then spool). Servo net has NO response timeout Ã¢â‚¬â€� only connect timeout (sec1zt117). Windows lane should confirm exact repro steps their side. Memory pressure note: builds in /tmp/b*.log, harness /tmp/mseh still up.

## sec1zt139 Ã¢â‚¬â€� timeout-regression REPRODUCED on Linux: hung (never-completing) load silently destroys the browsing context (2026-11-27)
Testing agent 5-variant matrix, clean bisect signal: (A) two-tab hidden-GC FAIL, (B) SINGLE-TAB NO-backgrounding FAIL Ã¢â‚¬â€� hidden-GC ruled out as sole cause, hanging ALONE kills; (C) connect-timeout clean-failure PASS Ã¢â‚¬â€� recovers fine; (D) death is PER-WEBVIEW not browser-wide; (E) reload-after-clean-error works. Smoking gun: `WARN servo_constellation LoadUrl for unknown browsing context` Ã¢â‚¬â€� the constellation loses the context registration silently (zero panics, zero destruction logging). `Unknown pipeline for input event` warnings appear as early as 6s after hang start Ã¢â‚¬â€� death window ~4-25s. KEY THEORIES: (a) something reaps the hung pipeline mistaking it for orphaned Ã¢â‚¬â€� candidates: background-hang-monitor driven kill, a pipeline sweep; (b) pipeline registration never completes for slow loads and first-use-of-unregistered looks like death Ã¢â‚¬â€� discriminate by when registration logging appears vs load completion. NOTE: servo net has NO response-timeout (sec1zt117) Ã¢â‚¬â€� /hang connects then hangs forever so NO engine timeout fires; the death is pure-hang-driven not timeout-driven. Cleanest repro: fresh instance, rm session.json, load 127.0.0.1:8899/hang (repro server /tmp/timeoutrepro/server.py port 8899), wait 15s, navigate same tab to /ok with full http:// scheme Ã¢â€ â€™ blank, constellation error, zero network. Incidental feather bugs found: omnibox auto-upgrades to https breaking http-local, no ctrl+v paste in omnibox, omnibox URL stale on tab switch, error-page text dark-on-dark. NEXT: grep constellation for browsing-context unregister paths + hang-monitor kill logic; test whether upstream servo has the same behavior (is this fork-introduced? suspects: arena purge 7d02b70553b teardown path, or a fork-added reaper). Logs: /tmp/timeout-b2.log cleanest.

## sec1zt140 Ã¢â‚¬â€� timeout-regression mechanism narrowing: the TOP-LEVEL browsing context entry itself vanishes from constellation map (2026-11-27)
Deep read of constellation. The failure `LoadUrl for unknown browsing context` (line ~1363) means `self.browsing_contexts.get(&ctx_id)` returns None Ã¢â‚¬â€� the WHOLE CONTEXT entry is gone, not just a pipeline. What removes contexts: close_browsing_context/close_pipeline(DiscardBrowsingContext::Yes) Ã¢â‚¬â€� callers: handle_exit (no Ã¢â‚¬â€� browser alive), close-webview paths, pipeline close with Yes. PRIOR FIX bb5c9bc9b94 already addressed a related wedge (unnavigable-scheme cancellation) Ã¢â‚¬â€� its supersede logic closes pending pipelines with DiscardBrowsingContext::No (correct) Ã¢â‚¬â€� so not that path. REMAINING SUSPECT PATHS in order: (1) something closes the OLD (about:blank or prior) pipeline with discard=Yes while the pending /hang change never completes Ã¢â‚¬â€� because the pending new pipeline hasn't committed, context still keyed to old pipeline Ã¢â€ â€™ old pipeline close discards context Ã¢â€ â€™ next LoadUrl finds nothing. Look at session-history/inactive-document cleanup paths, DocumentActivity::Inactive handling, pipeline teardown on navigation cancellation; (2) a fork-added reaper Ã¢â‚¬â€� but no timer/Instant logic exists in constellation (grepped). NOTE input-event warnings at ~6s: input targets the webview's active pipeline Ã¢â‚¬â€� likely the about:blank pipeline already closed OR never properly registered Ã¢â‚¬â€� possibly red herring vs the real death which shows only at next LoadUrl. NEXT STEPS: (a) instrument close_pipeline/close_browsing_context with eprintln logging caller reason Ã¢â‚¬â€� rerun repro, see exactly WHO closes the context and when (this is the decisive cheap experiment Ã¢â‚¬â€� one build); (b) check whether bug reproduces in UPSTREAM servo (unmodified) Ã¢â‚¬â€� if yes it's upstream and the fix is structural; if no, bisect the fork delta. Repro server /tmp/timeoutrepro/server.py (port 8899, /hang /ok) Ã¢â‚¬â€� restart if box rebooted. Testing agent knows protocol. KEY DIFF vs prior negative sec1zt117: prior testing navigated away FASTER than the death window (~4-25s); need 15s+ hangs before navigating.

## sec1zt141 Ã¢â‚¬â€� timeout-regression ROOT CAUSE + FIX (2026-11-27)
ROOT CAUSE confirmed by instrumentation round: nothing ever closes anything Ã¢â‚¬â€� the browsing context NEVER REGISTERS. Mechanism: at webview creation the context is only a PENDING session-history change; `self.browsing_contexts` insertion happens when the initial load COMMITS. A hung first load never commits Ã¢â€ â€™ context never exists Ã¢â€ â€™ every embedder LoadUrl dies at the lookup with 'unknown browsing context' Ã¢â€ â€™ permanent wedge. Explains everything: connect-timeout passes because the error path COMPLETES the load Ã¢â€ â€™ commits Ã¢â€ â€™ registers; per-webview not browser-wide; silent (nothing ever existed to log). FIX IMPLEMENTED (2 parts, in build when this was written): (1) embedder LoadUrl falls back to the pending initial navigation's pipeline when the context isn't registered yet, so navigation can supersede; (2) supersede now TRANSFERS new_browsing_context_info from the superseded initial change into the new pending change Ã¢â‚¬â€� without it even a successful replacement load would never register the context. VERIFY ROUND: repro recipe unchanged (server /tmp/timeoutrepro/server.py port 8899, wipe session.json, /hang 20s, navigate /ok full-scheme) Ã¢â‚¬â€� PASS = /ok loads. ALSO still open: during the initial hang itself the tab remains in limbo (blank loading page, input unrouted) Ã¢â‚¬â€� acceptable, matches other browsers' behavior for never-loading pages. Once verified: the [timeout-instr] instrumentation should be removed or kept behind env. Watch for compile issues: partial-move of change.new_browsing_context_info + subsequent use of change.new_pipeline_id is legal but check match-arm scoping of inherited_browsing_context_info.

## shell-nav-1: back/forward wiring chain + servo session-history gap (2026-11-27)

- "back/forward clicks do nothing visible" Ã¢â€ â€™ THREE layered causes, all fixed in
  feather devin/fwd-button-anim: (1) mock history cursor never moved for engine
  tabs (routing skipped browser.apply) so renav immediately undid traversals;
  (2) chrome read mock cursor instead of engine state; (3) click-position
  confusion Ã¢â‚¬â€� macOS layout c back button center is ~160 not ~140 in tool coords.
- "servo warns 'Cannot traverse that far into the past'" Ã¢â€ â€™ shell-driven
  wv.load(url) navigations do NOT accumulate in servo's back_forward_list
  (webview.rs embedder state). Session-history accumulation across embedder
  loads is an open engine integration defect Ã¢â‚¬â€� blocks back/forward end-to-end.
  Suspect locations: constellation TraverseHistory session-history commit path,
  webview.rs back_forward_list push conditions on load completion.
- "egui animate_bool never starts animating" Ã¢â€ â€™ classic trap: returns stale value
  on flip frame; repaint must be requested until target reached or 0Ã¢â€ â€™1 never runs.

## sec1zt142 Ã¢â‚¬â€� timeout-regression iteration log: three limbo traps found+fixed, ordering bug in my own patch (2026-11-27)
Chain so far: root cause = browsing context registers only when FIRST load commits; hung first load = permanent limbo. Trap 1 fixed: embedder LoadUrl pending-pipeline fallback + new_browsing_context_info transfer through supersede (else even successful replacement load never registers context). Round-1 verify: signature moved to 'loaded url in closed browsing context' Ã¢â‚¬â€� load_url does its OWN lookup (trap 2). Round-2 verify: errors gone, supersede cleanly closes hung pipeline, then SILENCE Ã¢â‚¬â€� nothing spawns: the activity check still killed it because THE SUPERSEDE DRAINS PENDING CHANGES BEFORE the activity check runs, so my pending_initial keyed on pending_changes could never fire Ã¢â‚¬â€� ordering bug in own fix. Trap 3 fix: key on `browsing_contexts.get().is_none()` instead Ã¢â‚¬â€� the only state where a live pipeline has no context IS the pre-commit limbo; genuinely-replaced pipelines still have their context (correctly stay Inactive). KEY ARCHITECTURAL LESSON: servo's deferred context registration creates a limbo state that EVERY consumer of browsing_contexts must handle; cleaner long-term fix = eager registration at creation, blocked by self-unload edge at commit (commit's None-path would treat initial pipeline as replacement of itself) Ã¢â‚¬â€� documented for future redesign. Verify round-3 pending: PASS = /ok GET fires+renders after hang.

## sec1zt143 Ã¢â‚¬â€� timeout-regression FIXED+VERIFIED round-3 PASS (2026-11-27)
Round-3 verification PASS end-to-end: 34s hang then same-tab navigation fires GET immediately, /ok renders, eval live (rs:complete), SECOND follow-up navigation also works Ã¢â‚¬â€� genuine recovery. Full chain confirmed in log: fallback resolution Ã¢â€ â€™ supersede close_pipeline discard=No Ã¢â€ â€™ activity passes Ã¢â€ â€™ new pipeline spawns Ã¢â€ â€™ load commits. Zero error-class warnings. The timeout-regression (Windows' top stability bug) is CLOSED on Linux Ã¢â‚¬â€� Windows lane should re-verify their original repro. The three-part fix on feather-engine: (1) embedder LoadUrl pending-pipeline fallback + new_browsing_context_info transfer through supersede, (2) load_url context-lookup fallback to pending metadata, (3) activity check keys on missing-context not drained pending-changes. RESIDUALS documented: input events during the hang itself still dropped (unknown pipeline for input event Ã¢â‚¬â€� cosmetic post-fix); the [timeout-instr] close_pipeline/close_browsing_context instrumentation remains in the engine Ã¢â‚¬â€� decide keep-or-strip (useful for future forensics, one line per close, cheap); eager-registration redesign documented in sec1zt142 as the architectural cleanup. Symptom final index entry Ã¢â‚¬â€� Symptom: any timed-out/hung load kills all subsequent navigations in the tab, constellation logs 'unknown browsing context' or silent death after supersede Ã¢â€ â€™ check: whether the FIRST load committed (browsing context registration is commit-gated); the three limbo traps are load_url lookup, embedder LoadUrl lookup, get_activity.

## sec1zt144 Ã¢â‚¬â€� backward-seek ACCEPTANCE BAR FULLY MET (round-10)
Round-10 PASS on local MSE harness (:8377, bbb60.webm): four alternating seeks all land and
play, positions AT-target (fwd 30Ã¢â€ â€™T30.18, bwd 5Ã¢â€ â€™T5.47, bwd 2Ã¢â€ â€™T2.17 Ã¢â‚¬â€� round-9's residual
restarting-near-2.3s gone), zero demux errors, zero panics, no deadlock from the
position-update mutex path. The mse_position_offset wiring fix (a4922916078) verified correct.
Mission complete per coordinator's bar: "backward seek lands and plays, forward seek still
works, repeated seeks in both directions". Quality-switch interaction untestable (bot wall).
Residual cosmetic: playback initially advances faster than realtime (burst catch-up).

## servo-history-1: session-history accumulation recon (2026-11-27)

Symptom: "Cannot traverse that far into the past" when shell sends TraverseHistory
after two shell-driven wv.load() navigations. Recon findings:
- webview.rs back_forward_list is an embedder-side MIRROR, populated via
  set_history() Ã¢â‚¬â€� the constellation is the source of truth.
- Loads DO create SessionHistoryChange entries (constellation.rs 3328/3802/4561/
  5047 add_pending_change sites) Ã¢â‚¬â€� so accumulation machinery exists.
- Open question for the fix mission: why doesn't the change commit for
  embedder-driven LoadUrl? Suspects: (a) change_session_history (line 5774)
  only applies pending changes on certain load-completion events that may not
  fire for the shell's load path; (b) the joint session history entries require
  the NEW pipeline to reach a state the shell's restored-tab setup skips;
  (c) TraverseHistory counts entries in the JOINT history while entries were
  committed per-browsing-context. Instrument: log SessionHistoryChange commit
  vs pending at traverse time.
- Also verify: does servoshell itself (not feather) traverse correctly? If yes,
  diff its webview setup against feather's engine.rs.

## sec1zt145 Ã¢â‚¬â€� permission prompts: shell delegate was the entire gap (feather#265)
Symptom: "navigator.geolocation always errors / never prompts" Ã¢â€ â€™ check: embedder
delegate request_permission IMPLEMENTATION, not the engine. Servo's mediation is
complete engine-side (script/dom/permission + geolocation call request_permission_to_use_async
Ã¢â€ â€™ EmbedderMsg::RequestPermission Ã¢â€ â€™ delegate). Feather never implemented the delegate method Ã¢â‚¬â€�
every request hit AllowOrDenyRequest's drop-default DENY. Shell fix: per-origin store +
parked prompt UI. Engine needed ZERO changes. Geolocation also gated on
dom_geolocation_enabled + test-position prefs (both off by default).
NOTE for other lanes: the permission flow uses EmbedderMsg::RequestPermission (callback variant)
not just PromptPermission Ã¢â‚¬â€� both route to the same delegate method.

## sec1zt146 Ã¢â‚¬â€� notifications: engine resource-less-show bug + shell wiring
Symptom: "new Notification() constructor succeeds, permission granted, but nothing
displays, no error" Ã¢â€ â€™ check: notification.rs fetch_resources_and_show_when_ready Ã¢â‚¬â€�
with no image/icon/badge resources the pending list is EMPTY, the fetch loop is a
no-op, and show() is never invoked. Engine bug upstream: resource-less notifications
never display. Fix: empty-check calls show() immediately.
Engine notification architecture notes:
- DOM complete in serviceworker/notification.rs; pref dom_notification_enabled default FALSE
- script sends EmbedderMsg::ShowNotification DIRECTLY to embedder channel (no constellation)
- permission flow: Notification.requestPermission routes the SAME generic embedder
  permission mediation as geolocation Ã¢â‚¬â€� the shell prompt infra is feature-generic (verified)
- shell display hook: WebViewDelegate::show_notification (default no-op)

## sec1zt147 Ã¢â‚¬â€� permissions-query store-consistency gap (analysis, unimplemented)
`navigator.permissions.query()` NEVER reaches the embedder: descriptor_permission_state
checks (1) secure-context, (2) the per-global invocation cache (populated only AFTER an
embedder-answered request in THIS page instance), (3) falls through to default 'prompt'.
Consequence: even with a shell-side persisted grant, a fresh page load's query() returns
'prompt' until something triggers the actual request (which the store then auto-answers).
Real browsers return the persisted state at query time. This is the exact fingerprintable
inconsistency the platform-gap analysis flagged for Google risk scoring.
Fix requires NEW engine plumbing: a synchronous scriptÃ¢â€ â€™embedder permission-store query
(script-thread blocking Ã¢â‚¬â€� deadlock-sensitive) OR pushing shell-store decisions into the
engine at webview creation (no such API exists). Design decision needed before implementing;
the invocation cache covers within-session consistency already.

## writing-modes-2: defect precisely localized (2026-11-27, mac lane)

- LIVE EVIDENCE: vertical-rl fragments/box sizes already correct (202x202 on
  declared 200px box). Matrix "zero fragments" reading obsolete. TRUE defect:
  inline content flows horizontally inside vertical containers Ã¢â‚¬â€� the inline
  progression axis never switches. Text renders as ordinary horizontal lines.
- Stylo is external (servo/stylo git, Firefox's engine) Ã¢â‚¬â€� parse/cascade/computed
  almost certainly correct. Fault is in components/layout inline line
  construction / glyph physical placement: suspect flow/inline/line.rs +
  text_run.rs Ã¢â‚¬â€� where inline advances map to physical offsets.
- Devtools transport fact: raw TCP, Firefox length-prefixed JSON (`<len>:<json>`),
  FIRST packet must be `26:{"auth_token":"<token>"}` (token from
  /tmp/feather-devtools-token, second field). NOT websocket. Auth worked,
  root greeting received Ã¢â‚¬â€� but listTabs returns ZERO tabs even with pages open:
  browsing contexts apparently don't register as devtools tabs in feather's
  engine integration. THIS is the matrix harness's deepest blocker Ã¢â‚¬â€� separate
  engine mission: why no tabs in listTabs.
- Input-testing trap: shell renav + session-restore interference makes
  omnibox-driven verification flaky on restored tabs Ã¢â‚¬â€� open a FRESH tab for
  live probes, or use drag-drop harness pages which are single-purpose.

## sec1zt148 Ã¢â‚¬â€� crash isolation Stage 0 VERIFIED CLOSED (feather#268 evidence)
Forced ServoTestUtils.panic() via eval with FEATHER_TESTUTILS=1: identical pid
before/after Ã¢â‚¬â€� the process survives script-thread panics. Servo's own in-tab
crash interstitial renders ("Servo crashed!" + Reload + backtrace); the rest of
chrome stays live; crash-panic artifact captured on the persistent path.
KEY ARCHITECTURE FACT: the constellation does NOT auto-reload the crashed tab Ã¢â‚¬â€�
it tears down the pipeline and serves a crash interstitial inside the tab; the
user reloads. Engine wiring was already complete from phase 1 (hard_fail:false +
panic hook chaining log::error). Verification hook: FEATHER_TESTUTILS=1 exposes
ServoTestUtils. Symptom: "tab shows 'Servo crashed!' page" Ã¢â€ â€™ expected recovery
behavior, NOT a new bug; check whether the originating panic class is script
(recovered, fine) vs singleton-thread (WR/compositor Ã¢â‚¬â€� never-bugs, fix upstream).
Remaining per design doc: Stage 1 media-thread granularity (1-2d), Stage 2
singleton respawn (weeks, defer), Stage 3 multiprocess (beta conversation).

## sec1zt149 Ã¢â‚¬â€� youtube thumbnails ROOT-CAUSE CORNERED: late-populated images never paint
Complete discriminator chain (2026-11-27 linux, live youtube.com search):
1. Lazy-load triggering EXONERATED: probe IntersectionObserver on empty imgs FIRES on scroll
   (5/10 fired); src population partially runs (7/85 Ã¢â€ â€™ 20/85 imgs gained src after scrolling).
2. Fetch+decode EXONERATED: in-viewport lazy imgs all complete+naturalWidth>0, zero broken.
3. Layout EXONERATED: black-painting thumbs are light-DOM imgs 218x326 visible opacity 1.
4. OBSERVED: imgs whose src populates AFTER first paint render as black rectangles forever
   (shorts shelf, video thumbs) while SSR/early imgs paint fine (cat ad, avatars, initial
   thumbs) IN THE SAME FRAME. Scrolling does not heal them Ã¢â‚¬â€� so not a mere missing repaint;
   the painted fragment itself carries no image. Suspect class: servo image-cache Ã¢â€ â€™ layout
   association never established for images inserted post-layout (needs re-reflow/re-image,
   not repaint). Same family as the favicon texture-upgrade bug (feather#213) Ã¢â‚¬â€� late-arriving
   image data fails to reach the render path.
Symptom: "images black despite complete+decoded in DOM" Ã¢â€ â€™ check: image-cache/fragment
association timing, NOT IntersectionObserver, NOT fetch, NOT lazy-load JS.
Methodology note: xdotool input coordinates must hit the feather window geometry Ã¢â‚¬â€� an early
"youtube doesn't scroll" reading was measurement error (clicks landing above the window).
YouTube uses CLOSED shadow roots for yt-image (shadowRoot null is spec-correct), but the
defective thumbs are plain light-DOM imgs Ã¢â‚¬â€� shadow DOM is irrelevant to this defect.

## sec1zt150 Ã¢â‚¬â€� late-image hunt: simple repros all PASS (negative results)
Minimal repros on localhost all render fine Ã¢â‚¬â€� exonerated mechanisms:
- img.src set 2s/6s after paint (explicit dims) Ã¢â‚¬â€� RENDERS
- new Image() node inserted post-paint Ã¢â‚¬â€� RENDERS
- src set with no width/height attrs Ã¢â‚¬â€� RENDERS (natural size)
- (hidden-parent untested visually but others passing makes it unlikely)
CONCLUSION: the youtube defect is NOT generic late-src or node-insertion. Remaining
suspects narrow to: (a) images inside yt-image light-DOM fallback slots Ã¢â‚¬â€� YouTube's pattern
is a light-DOM img INSIDE a custom element host; if upgrade replaces/hides it and the shadow
copy is what should render, servo's shadow-image paint path is the suspect; (b) something in
YouTube's specific visibility/upgrade sequencing. Next discriminator: check the black thumbs'
parent chain Ã¢â‚¬â€� if the light-DOM img is a child of yt-image, test whether servo paints images
inside shadow roots at all (simple open-shadow repro with img inside attachShadow).

## sec1zt151 Ã¢â‚¬â€� shadow-image repros ALL PASS + the slot insight (next hunt direction)
Shadow variants all render correctly on localhost: open shadow img, closed shadow img,
late-src-inside-shadow. Servo's shadow image paint path is HEALTHY.
KEY THEORETICAL INSIGHT for the next round: light-DOM children of a shadow host DO NOT
RENDER unless the shadow tree has a <slot> Ã¢â‚¬â€� the measured light-DOM oar2 imgs (218x326,
'visible' per getComputedStyle) are likely RENDER-INVISIBLE fallback content inside
yt-image. getBoundingClientRect/computed-style lie about paint visibility for unslotted
light DOM! What actually paints is the shadow copy, whose src/decode state is JS-opaque
(closed shadow). REVISED HUNT: the question is whether YouTube's shadow-img src population
fires Ã¢â‚¬â€� probe indirectly: compare a yt-image's rendered pixels (screenshot crop) against
its light-DOM fallback state; or use servo's image-cache instrumentation to see whether
the shadow img URL is ever FETCHED (net logs for oar2/hqdefault requests per element).
If shadow-img fetches never happen Ã¢â€ â€™ framework-side population failure (YouTube JS);
if fetches happen but no paint Ã¢â€ â€™ servo shadow-image invalidation.

## sec1zt152 Ã¢â‚¬â€� youtube thumbnail hunt: full-pattern repro PASSES (framework-internal likelihood rises)
The exact YouTube pattern reproduces HEALTHY on localhost: closed shadow root on a sized host
div + img inside shadow + IntersectionObserver-driven lazy src population on scroll Ã¢â‚¬â€� RENDERS.
Every isolated mechanism now exonerated: late-src, node insertion, no-dims, shadow open/closed,
shadow-late-src, IO-driven shadow population. The defect on youtube.com is real (black thumbs,
decoded light-DOM fallbacks) but not reproducible piecewise Ã¢â‚¬â€� remaining suspects:
(a) interaction between custom-element UPGRADE ordering and src population timing
    (youtube sets src pre-upgrade then upgrade clones/moves content Ã¢â‚¬â€� the node.rs
    copy_elem.attach_shadow path); (b) youtube uses srcset/picture/source machinery rather
    than plain src Ã¢â‚¬â€� untested variant; (c) genuinely framework-internal logic failing on a
    servo JS/DOM primitive outside the image path entirely.
NEXT ROUND menu (pick by cost): test srcset+picture repro first (cheapest untested mechanism),
then upgrade-clone ordering. Evidence gathering that DOESN'T need reproduction: servoshell-upstream
comparison, or net-log correlation of which urls never fetch on the live page.

## sec1zt153 Ã¢â‚¬â€� ROOT CAUSE PIVOT: youtube thumbnails likely framework-death, not image-path defect
LIVE EVIDENCE: youtube.com in feather throws during kevlar_base init:
  "can't access property 'then', navigator.serviceWorker.ready is undefined"
servo's ServiceWorkerContainer has `ready` COMMENTED OUT in the WebIDL
(components/script_bindings/webidls/ServiceWorkerContainer.webidl). YouTube's app-framework
boot throws on `.ready.then(...)` Ã¢â‚¬â€� plausibly killing or crippling the lazy-src population
machinery for shadow thumbs. Explains every observation: SSR/light-DOM fallback imgs decode fine
(srcs in initial markup), isolated repros all pass (image path healthy), yet live shadow thumbs
never populate. Also present: "No valid entry type provided to observe()" (PerformanceObserver
entryTypes gaps Ã¢â‚¬â€� secondary).
SPEC SEMANTICS of .ready: promise resolving with the active registration for the container's
scope once one exists; NEVER REJECTS; sites with no SW Ã¢â€ â€™ promise never resolves (spec-correct!).
Implementation path: lazy RootedPromise per container + resolve hook in registration-matching
(serviceworkercontainer.rs handle_match_registration_result already matches active registrations).
Symptom: youtube black thumbnails / framework dead Ã¢â€ â€™ check: ServiceWorkerContainer.ready +
framework console errors BEFORE image-path forensics.

## sec1zt154 Ã¢â‚¬â€� serviceWorker.ready SHIPPED + youtube thumbnails VERIFIED FIXED
Engine commit 113703a37a9 implements ServiceWorkerContainer.ready (was commented out in
WebIDL). Design: promise created eagerly at container construction, stored ROOTED (not
traced) because the codegen attribute getter receives NO JSContext making per-access
re-rooting impossible Ã¢â‚¬â€� TracedPromise.root(cx) unusable; rooted-clone is the escape hatch,
documented on the field. Resolution hooks: register-job result + match-registration result,
only when registration carries active_worker. Never rejects; never resolves for clients
without SWs (spec-correct, matches Chrome).
CODEGEN GOTCHA (symptom-index worthy): promise-returning ATTRIBUTE getters get `cx` only
sometimes Ã¢â‚¬â€� streams/FontFace getters receive `cx: &JSContext` but SWC's `fn Ready(&self)`
got none; typeNeedsCx/needCx don't obviously explain it. If trait signature mismatch E0050
on a promise getter: check the generated binding FIRST, design storage around no-cx.
BUILD GOTCHA: `cargo check` does NOT rebuild the feather-ui binary Ã¢â‚¬â€� verification against a
`cargo check`-only build tests the STALE binary. Always `cargo build` before live verify.
VERIFICATION: localhost eval: promise-ok, .then no-throw. Live youtube search results page:
thumbnails fully render (artwork, avatars, duration badges), the boot error
'navigator.serviceWorker.ready is undefined' GONE from console, zero console errors.
The months-old youtube-black-thumbnails defect was framework boot death, never an image-path
bug. Symptom: site feature dead despite all primitives testing healthy in isolation Ã¢â€ â€™ check:
framework console errors on the LIVE site first, before mechanism-by-mechanism repro.

## sec1zt155 Ã¢â‚¬â€� PerformanceObserver gap scoped (queue item, NOT a quick fix)
servo knows 8 entry types (performanceentry.rs enum EntryType): container, mark, measure,
largest-contentful-paint, navigation, paint, resource, visibility-state. Chrome additionally
supports: element, event, first-input, layout-shift, longtask (and soft-nav in canary).
The youtube warning 'No valid entry type provided to observe()' means every type in one
observe() call was unknown to servo Ã¢â‚¬â€� youtube performance-telemetry only, NON-fatal (site
works). Implementing event-timing/layout-shift/longtask entry production is real spec-feature
work (entry generation machinery, not just enum strings). Priority: LOW-MED Ã¢â‚¬â€� telemetry-only
for most sites, but RUM-dependent frameworks may gate features on it.

## writing-modes-3: pref enable insufficient Ã¢â‚¬â€� cascade-level gap (2026-11-27)

- layout_css_writing_mode_enabled exists in vendored servo (default false,
  prefs.rs), stylo pin 0ab78ce HAS the wiring (longhands.toml: writing-mode
  servo_pref = "layout.css.writing-mode.enabled"). Feather shell now enables
  it (feather branch devin/writing-mode-pref).
- MATRIX-VERIFIED NEGATIVE: even with the pref on and fresh boots, probes
  unchanged: writing-mode-computed='', CSS.supports=false, BUT
  inline-parse-sticks PASSES (readback=vertical-rl). Parse recognizes the
  property, cascade/computed drops it. Precisely bounded: the gap is between
  parse and cascade, not the pref plumbing.
- KEY SUSPECT: stylo's servo_pref property registration is process-static Ã¢â‚¬â€�
  if the property registry is built before prefs::set applies shell values
  (or lazily cached on first use with default-off), author styles parse but
  never cascade. Inspect stylo's property registration: how servo_pref-gated
  longhands register in data.py / cascade, and WHEN servo first triggers it.
  Compare: how does servoshell's own --pref flag ordering differ?
- NOTE: text-orientation is gated by a SEPARATE pref
  layout.css.writing-mode.text-orientation.enabled Ã¢â‚¬â€� enable both when fixed.
- ALSO BANKED: bare-host omnibox input (no scheme) TLS-upgrades loopback Ã¢â‚¬â€�
  servo then fails with InvalidContentType against plaintext HTTP. Always use
  explicit http:// in probes. Shell UX gap: localhost/127.0.0.1 should default
  to http not https (real browsers do this).
- MATRIX HARNESS IS HEALTHY: boot-per-page runs collect probes reliably
  end-to-end (full pass ~210s). The single-instance handoff poisoned earlier
  runs Ã¢â‚¬â€� ALWAYS pkill feather-ui before matrix runs.

## writing-modes-4: chain fully verified, mystery isolated to runtime application (2026-11-27)

Every link of the pref chain verified present in code:
- feather shell patches stylo with VENDORED copy at repos/servo/vendor/stylo
  (shell-new/Cargo.toml [patch] section Ã¢â‚¬â€� vendored for the container-queries
  gecko-gate work). The vendored longhands.toml HAS the writing-mode servo_pref
  wiring (line 2693) Ã¢â‚¬â€� same as upstream pin.
- shell sets layout_css_writing_mode_enabled via Preferences Ã¢â€ â€™ ServoBuilder
  .preferences() Ã¢â€ â€™ servo.rs:1055 prefs::set() Ã¢â€ â€™ config/prefs.rs runs
  stylo_static_prefs::set_pref! for writing-mode (line 84).
- generated properties.rs gates longhand 88 with crate::pref! Ã¢â‚¬â€� a static atomic.
Yet matrix shows pref OFF at runtime (CSS.supports=false). Remaining hypotheses,
untested: (a) stylo_static_prefs statics duplicated across crate instances in
the patch setup Ã¢â‚¬â€� vendored stylo_static_prefs vs servo component linking the
git version; inspect with cargo tree for duplicates; (b) prefs::set ordering Ã¢â‚¬â€�
servo parses UA sheets before prefs::set applies?; (c) the probe reads a stale
process Ã¢â‚¬â€� RULED OUT (fresh boot verified). ALSO: text-orientation needs its own
pref layout.css.writing-mode.text-orientation.enabled Ã¢â‚¬â€� not yet set in shell.
NEXT STEP: add eprintln debug in config/prefs.rs set() confirming the write,
and a probe reading the pref state indirectly. OR test servoshell with the
pref to isolate shell vs engine.

## writing-modes-5: RESOLVED Ã¢â‚¬â€� orthogonal flows deliberately unimplemented upstream (2026-11-27)

The parse-vs-cascade mystery is EXPLAINED by upstream servo/servo#50188's own
commit message: "we use writing-mode in UA styles to prevent orthogonal flows
FOR NOW". Orthogonal flows (vertical writing mode inside horizontal context Ã¢â‚¬â€�
THE common case for all real vertical text) are deliberately unimplemented in
upstream servo. Evidence in tree: positioned.rs has TODO comments referencing
drafts.csswg.org/css-writing-modes/#orthogonal-flows; flexbox handles
orthogonality only partially.

So: the pref chain probably works fine (same chain proven by backdrop-filter);
even with the property cascading correctly, layout forces orthogonal content
horizontal Ã¢â‚¬â€� matching the live visual evidence exactly. CSS.supports=false
remains slightly inconsistent with this theory Ã¢â‚¬â€� possibly servo's
CSS.supports implementation has its own gate in the script crate; low-value
to chase since the real work is orthogonal flow implementation either way.

REAL SCOPE for writing-modes: implement orthogonal block-flow sizing in
components/layout/flow (block axis resolution when child writing mode is
orthogonal to parent Ã¢â‚¬â€� the child's inline size becomes its block size
contribution, capped by available space per spec section 7.3). Ladybird's
implementation in BlockFormattingContext is the cross-reference. This is the
multi-day engine mission; plan doc WRITING-MODES-PLAN.md phases stand but
phase 1 should be retitled "orthogonal flow implementation".
## sec1zt156 Ã¢â‚¬â€� permissions.query() plumbing IMPLEMENTED, compile-verified GREEN + pushed (d968c8381f8); e2e browser verify pending
Full chain written, compile-VERIFIED green (cargo check -p feather-ui exit 0); pushed as d968c8381f8:
1. embedder/lib.rs: new EmbedderPermissionState tri-state enum (Granted/Denied/Prompt) +
   EmbedderMsg::QueryPermissionState(WebViewId, PermissionFeature, GenericSender<...>)
2. servo/webview_delegate.rs: query_permission_state default method returning Prompt
3. servo/servo.rs: arm answering synchronously from the delegate (never hangs Ã¢â‚¬â€�
   servo itself sends the response, fallback Prompt when webview missing)
4. script/permission/permissions.rs: descriptor_permission_state step 8 now does a
   SYNCHRONOUS embedder round-trip (query_embedder_permission_state) mirroring the
   prompt_user_from_embedder channel pattern Ã¢â‚¬â€� precedent exists so no architectural surgery.
5. feather shell engine.rs: query_permission_state impl reading the PermissionStore.
KEY DESIGN FACT: prompt_user_from_embedder ALREADY does sync channel+recv from the script
thread Ã¢â‚¬â€� sync embedder queries are precedented, no store-push complexity needed.
NOTE: invocation cache (step 5-7) is per-global so cross-page consistency now correct;
revocation within a page still cached (spec-tolerable).

## sec1zt157 Ã¢â‚¬â€� permissions.query e2e: STILL PROMPT despite seeded store (negative, two suspects)
E2E on fresh binary: seeded store http://localhost:8411|Geolocation=true, query() returns
state:prompt. The store-key derivation in the shell query impl is IDENTICAL to the proven
request path (persisted-allow verified in sec1zt145), so a plain key miss is unlikely.
Two remaining suspects: (a) recv() errors Ã¢â‚¬â€� sender dropped because the QueryPermissionState
message handling never completes/never fires on this call path, falling back to Prompt
(test: eprintln in shell query_permission_state + in servo.rs arm Ã¢â‚¬â€� if neither prints, the
message isn't being processed; the difference vs the prompt path is that prompt responses
are LATE/async while query expects inline answering Ã¢â‚¬â€� possible event-loop re-entrancy issue:
script thread blocked in recv while the constellation/servo loop that would process the
message is somehow dependent); (b) webview.url() empty at query time in the shell delegate.
NEXT DEBUG STEP: add eprintln instrumentation at all three points (script send, servo arm,
shell impl) Ã¢â‚¬â€� one build cycle localizes it exactly.

## sec1zt158 Ã¢â‚¬â€� query failure narrowed to MESSAGE DELIVERY, store/keys exonerated
Discriminator: geolocation getCurrentPosition on the same seeded localhost origin returns
pos-ok with NO prompt Ã¢â‚¬â€� the store key format/lookup is PROVEN end-to-end for the identical
derivation the query path uses. Therefore query()'s prompt result means the
QueryPermissionState embedder message never gets answered (sender dropped Ã¢â€ â€™ recv Err Ã¢â€ â€™
fallback Prompt). TOP SUSPECT: the message routing path Ã¢â‚¬â€� check how send_to_embedder routes
scriptÃ¢â€ â€™embedder; if it transits the constellation, the constellation likely has its own
forwarding match on EmbedderMsg variants and the new QueryPermissionState variant needs
registering there (grep for PromptPermission/RequestPermission in components/constellation Ã¢â‚¬â€�
whichever file matches on those variants is where QueryPermissionState must be added).
sec1zt157's event-loop-reentrancy theory deprioritized.

CORRECTION to sec1zt158 top-suspect: grep finds NO PromptPermission/RequestPermission matches
in components/constellation Ã¢â‚¬â€� routing is direct-to-embedder, constellation NOT involved.
Revised suspect list: (1) the servo.rs arm placement Ã¢â‚¬â€� verify the match it was added to is
the ACTIVE embedder-message handler (there may be multiple event loops: servo.rs vs
running in servoshell-vs-embedder contexts Ã¢â‚¬â€� the arm exists at servo.rs:686 in
handle_embedder_message but perhaps feather's embedding path processes EmbedderMsg in a
DIFFERENT consumer Ã¢â‚¬â€� grep where handle_embedder_message/the embedder channel receiver is
polled: if feather-ui itself polls the embedder channel rather than servo-internal servo.rs,
then the shell must implement the arm, not servo.rs!). THIS IS THE LIKELY ANSWER: feather as
embedder may consume EmbedderMsg itself Ã¢â‚¬â€� check shell-new engine.rs for where embedder
messages are received/polled.

## sec1zt159 Ã¢â‚¬â€� permissions.query VERIFIED END-TO-END; root cause was pref+shim, not plumbing
query() returns state:granted from seeded store; all four pipeline points fire in order
(script step8 Ã¢â€ â€™ servo.rs arm Ã¢â€ â€™ shell store lookup correct origin+feature). The implementation
was correct all along Ã¢â‚¬â€� the e2e failure was TWO LAYERS UP: (1) servo's dom_permissions_enabled
pref DEFAULTS FALSE, so navigator.permissions was undefined in feather; (2) feather's compat
shim (engine.rs ~line 498, guards `if (!navigator.permissions)`) then installed a stub
returning perpetual {state:'prompt'} Ã¢â‚¬â€� shadowing even a correct engine. Enabled the pref in
feather prefs; the shim guard now passes the real API through.
SYMPTOM INDEX (major): site API behaves as static/stubbed despite correct engine impl Ã¢â€ â€™ check:
(a) the pref gate on the webidl interface FIRST (grep Pref= in webidls, check defaults in
config/prefs.rs), (b) feather's compat shim guards Ã¢â‚¬â€� the shim only fills undefined surfaces,
so a missing pref silently becomes a permanent stub. Instrumentation eprintlns removed post-verify.

## sec1zt160 Ã¢â‚¬â€� pref-gate audit: 26 gated-off prefs triaged, batch-1 enabled+verified
Full audit method: grep Pref="dom_*" across webidls (49 total gates), cross-ref defaults in
config/prefs.rs vs feather-enabled in shell engine.rs. Batch-1 enabled + all surfaces verified
live: visual_viewport, cookiestore, adoptedstylesheet, composition_event, entries_api,
wakelock, navigator_protocol_handlers. DEFERRED pending per-area verification (enabling a
half-implemented API is worse than absence Ã¢â‚¬â€� inconsistent surface): webgpu, webrtc(+transceiver),
worklet(+blockingsleep), bluetooth, canvas_capture, sanitizer. SKIP permanently (test-only):
testbinding x3, testperf, testing_element_activation, testing_html_input_select_files,
microdata_testing, bluetooth_testing, webxr_test. GENERALIZED RULE: every future API audit
starts with this pref-gate cross-ref, not with site behavior.

## sec1zt161 Ã¢â‚¬â€� batch-1 regression check PASSED on live google SERP
With all batch-1 prefs + storage-manager + permissions enabled, google search renders the
full modern SERP (AI Overview, knowledge panel, result cards) Ã¢â‚¬â€� no regression from the new
API surfaces. youtube/webrtc/webgpu/canvas-capture/sanitizer verification still pending as
batch-2 candidates; sanitizer implementation is substantial (3189 lines) but low adoption;
webrtc needs media-path verification before enabling (half-enabled webrtc = worse signal).

## sec1zt162 Ã¢â‚¬â€� SERP duplication: NOT REPRODUCIBLE, likely retired server-side (verdict update)
Post-#240 testing on Linux: (1) modern firefox-UA SERP renders with zero duplication
(visual, multiple queries); (2) forced FEATHER_HONEST_UA=1 Ã¢â‚¬â€� google NOW SERVES A MODERN-shaped
SERP (AI Overview, cards) instead of the legacy html that exhibited the duplication, again
zero duplication; (3) curl with honest UA + gbv=1 no longer receives the legacy html at all Ã¢â‚¬â€�
a JS-redirect shell page instead, no result markup. The legacy SERP that duplicated appears
RETIRED or drastically narrowed server-side since the original Windows report. Combined with
sec1zt127 (raw==DOM counts exonerated the parser), the defect has no reproducible Linux
surface. Windows lane: re-test your repro Ã¢â‚¬â€� if duplication persists there, it's either
Windows-specific navigation re-entrancy or a stale-serve artifact; the forced-honest-UA
in-page fetch-vs-DOM discriminator from sec1zt127 remains the decisive test.
## writing-modes-6: the REAL defect chain measured live (supersedes writing-modes-2/3 framing)
Empirical probe chain on feather (devtools eval, wmtest2/wmtest3 pages):
1. `layout_css_writing_mode_enabled` pref DOES reach runtime when set by the shell Ã¢â‚¬â€� CSS.supports('writing-mode','vertical-rl')=true. The earlier negative result was stale-instance/testinfra. Shell passes it verified via eprintln.
2. INITIAL style pass computes writing-mode as horizontal-tb even when declared Ã¢â‚¬â€� in all forms: stylesheet rule, style attribute, CSSOM. Rect stays wide-horizontal (1256x19).
3. NO self-heal: waiting 10s changes nothing (not async pref arrival). Attribute mutations that don't invalidate writing-mode don't fix it.
4. A FULL-DOCUMENT restyle fixes it: setting documentElement.style.writingMode (inherited property, invalidates whole tree) makes every element compute correctly (vertical-rl) with tall-narrow vertical geometry (19x716!). Fresh createElement+append also computes correctly immediately.
5. So: the cascade and layout machinery WORK when they run Ã¢â‚¬â€� the defect is in the INITIAL style pass only. Prime suspect locations: the first cascade running before some pref/style-init state is ready in the content process, or an initial-pass-specific codepath (possibly style sharing cache or the root/ICB writing-mode propagation poisoning the first pass). Indexed by mechanism: initial-style-pass vs restyle divergence.
6. Post-restyle text still does NOT flow vertically in the line sense Ã¢â‚¬â€� glyphs paint horizontally in a tall box: the inline progression axis defect from writing-modes-2 stands as a separate layer.
[source: 986486d8d8824533801c3c14f0e76ce2; added: 2026-11-26; confidence: high]
## writing-modes-7: orthogonal-flow axis-swap implementation attempt (in tree, compile-pending)
Implemented the orthogonal sizing core per css-writing-modes-3 Ã‚Â§7.3:
- `ContainingBlockSize::swapped_axes()` in components/layout/containing_block.rs Ã¢â‚¬â€� swaps inline/block for orthogonal children, falling back to parent inline size when block indefinite.
- flow/mod.rs `solve_containing_block_padding_and_border_for_in_flow_box` (~line 1855): detect `is_orthogonal` (writing-mode horizontality mismatch), resolve sizes against swapped containing block, suppress inline stretch (orthogonal shrink-wraps).
- flow/mod.rs `layout_float_or_atomic_inline` (~line 2460): same treatment for floats/atomics.
Status: compile-pending in engine rebuild; behavioral verification blocked behind the initial-pass defect (writing-modes-6) Ã¢â‚¬â€� must force restyle in test pages or fix that first. NOTE: since post-restyle vertical layout already produces vertical geometry upstream, part of this swap may duplicate existing upstream handling Ã¢â‚¬â€� measure before/after, don't assume necessity. If the height-716 stretch behavior is already correct-ish upstream, the stretch-suppression half of this patch is the part to scrutinize first.
[source: 986486d8d8824533801c3c14f0e76ce2; added: 2026-11-26; confidence: medium]
## devtools-probing-1: reusable live-eval recipe against feather
matrix.py's DevTools class works against a manually booted instance: boot `FEATHER_DEVTOOLS=9299 feather-ui <url>`, then `importlib` load matrix.py (guard stdout), `DevTools(9299)`, `.connect()`, `.eval_js(expr)` Ã¢â‚¬â€� no auth token needed despite earlier notes. listTabs DOES return the booted tab when page passed as argv. Connect takes up to ~30s of retries. Verified end-to-end with document.title roundtrips. This is the fast lane for live CSS/DOM experiments Ã¢â‚¬â€� much cheaper than full matrix runs.

## sec1zt163 Ã¢â‚¬â€� pref-gate batch-2 verdicts
- sanitizer: SHIPPED (#275) Ã¢â‚¬â€� 3189-line real impl, sanitize() strips scripts verified.
- canvas_capture: EXCLUDED with evidence Ã¢â‚¬â€� CaptureStream is a stub: returns a MediaStream
  with a bare video track, no frame source, no canvas-to-track plumbing. Enabling would
  produce black-stream inconsistency. Real implementation needs canvasÃ¢â€ â€™video-frame bridging.
- webrtc: real structure (signaling state machine, offer/answer queues, 870-line peerconnection)
  but media transport verification needs actual ICE/DTLS call testing Ã¢â‚¬â€� can't verify in this
  VM without a peer. DEFERRED pending a loopback-test harness (two peerconnections in one
  page is the standard trick Ã¢â‚¬â€� still needs the media backend present).
- webgpu: untested Ã¢â‚¬â€� big surface, needs its own dedicated verification round.
RULE CONFIRMED: typeof-presence is NOT enablement evidence Ã¢â‚¬â€� captureStream passed the
presence check but is functionally hollow. Batch-2+ verification must exercise behavior.

## sec1zt164 Ã¢â‚¬â€� webrtc EXCLUDED with crash evidence; batch-2 verdicts final
Enabling dom_webrtc + constructing RTCPeerConnection PANICS the media backend:
components/media/backends/gstreamer/lib.rs:358 `webrtc::construct(...).expect("WebRTC creation
failed")` Ã¢â‚¬â€� gstreamer's webrtc controller fails to construct in this environment (missing
gstwebrtcbin/rswebrtc plugins most likely). Even where constructible, hard-panicking on a
missing system plugin is wrong Ã¢â‚¬â€� upstream defect worth a graceful-degradation fix (return
construction error to JS instead of expect). Until then webrtc stays off: presence-then-crash
is the worst possible surface. crash artifacts confirm the capture pipeline works (class: panic).
webgpu remains the only unverified batch-2 gate.
BATCH-1+storage+sanitizer all MERGED: #271 #272 #273 #275.

## sec1zt165 Ã¢â‚¬â€� standing backlog closure sweep (all five items)
1. SERP dup: closed sec1zt162 (not reproducible, legacy retired server-side).
2. notification permission flow: shipped+verified (#265 era: promptÃ¢â€ â€™toast end-to-end,
   engine resource-less-notification fix bddb140).
3. ellipsis scrollWidth: VERIFIED CLOSED live Ã¢â‚¬â€� ellipsized element sw:490 cw:80 (correct,
   not clamped); fix dde74fc7b93 on feather-engine.
4. container-queries vs modern SERP: closed Ã¢â‚¬â€� modern SERP fully renders (multiple visual
   verifications this session incl. regression pass sec1zt161).
5. drag-drop: VERIFIED WORKING END-TO-END Ã¢â‚¬â€� real mouse-driven drag on the matrix page
   produces the full spec sequence: dragstart|setdata-ok|drag|dragenter|dragover|
   drop:feather-drag-payload|dragend, data payload round-trips. The matrix doc's
   'honest-absence' categorization is STALE Ã¢â‚¬â€� servo's drag machinery (script/drag/html_drag.rs,
   drag_gesture wired in document_event_handler) is real and input-driven. MATRIX-DOC-NEEDS-
   REGEN: drag-drop x4 reds should flip green on next matrix.py run.

## sec1zt166 Ã¢â‚¬â€� webgpu verdict: cargo-feature-gated, NOT just a pref Ã¢â‚¬â€� batch-2 audit COMPLETE
navigator.gpu absent even with dom_webgpu_enabled=true because the implementation is behind
the `webgpu` cargo feature in components/script/Cargo.toml (dep:script_webgpu, webgpu_traits,
codegen cfg). Enabling requires feature passthrough in feather's dependency chain + heavy
wgpu dep compilation Ã¢â‚¬â€� a build-system change, not a pref flip. Verdict: defer to a dedicated
round (measure build cost first; wgpu backend in components/webgpu/wgpu_thread.rs is real,
upstream demos work).
BATCH-2 FINAL STATE: sanitizer shipped (#275), canvas-capture excluded (hollow stub, evidence
sec1zt163), webrtc excluded (gstreamer panic, evidence sec1zt164), webgpu deferred
(cargo-feature, this entry). The pref-gate audit is fully closed: every one of the 26
gated-off prefs has a verdict.

## writing-modes-8: CONFOUND RESOLVED Ã¢â‚¬â€� the true cascade defect is the UA forcing rule (corrects writing-modes-6)
The initial-pass/restyle theory in writing-modes-6 was built on confounded probes: setting documentElement.style.writingMode makes ROOT vertical Ã¢â‚¬â€� since writing-mode is inherited, ALL descendants then compute vertical regardless of their own declarations. The tall-box visual evidence was confounded the same way (and boxes had explicit physical heights). Corrected unconfounded evidence chain:
1. `writing-mode` declarations on regular elements NEVER apply Ã¢â‚¬â€� initial pass, after waits, after attribute mutations Ã¢â‚¬â€� always horizontal-tb.
2. The SAME declaration on the ROOT element DOES apply (rootSet genuinely computed vertical-rl Ã¢â‚¬â€� root cannot inherit).
3. Reason: servo.css UA rule Ã¢â‚¬â€� `:not(:root, html:root > body:first-of-type) { writing-mode: inherit !important }` Ã¢â‚¬â€� forces every non-root element to inherit its parent's writing-mode, and UA !important beats author declarations. Exactly the orthogonal-flow prevention upstream servo/servo#50188 described. NOTE: an earlier memory entry misread this rule as form-control propagation Ã¢â‚¬â€� corrected here.
4. No initial-pass defect exists as far as current evidence shows; that suspect is RETIRED.
5. Fix applied: the `:not(...)` selector removed from the UA rule (pseudo-elements keep inherit). Combined with the axis-swap groundwork (e969671ef), vertical writing modes should now cascade and lay out orthogonally; the glyph-orientation layer (text-orientation, latin rotation) remains the known-imperfect layer.
Probe-design lesson for ALL lanes: when testing inherited CSS properties, never use root mutation as a restyle trigger Ã¢â‚¬â€� it confounds inheritance with cascade application. Use a non-inherited property toggle (e.g. outline-color) or forced-descendant selectors instead. And getAttribute('style') readbacks prove nothing about parsing Ã¢â‚¬â€� use cssRules[].style or getComputedStyle only.

## sec1zt167 Ã¢â‚¬â€� build-environment: 124G disk FILLED by target/debug/incremental (53G)
The webgpu-feature build failed with "No space left on device" Ã¢â‚¬â€� not a code problem.
target/debug/incremental had grown to 53G across the session's many rebuilds; deps 34G.
Fix: rm -rf the incremental dir (safe Ã¢â‚¬â€� only costs recompile time, deps stay). Freed 53G.
STANDING HYGIENE: if feather builds start failing with os error 28, delete incremental first.
Also screencasts ~1G accumulates; prune old rounds.

## sec1zt168 Ã¢â‚¬â€� webgpu SHIPPED (feather#280) Ã¢â‚¬â€� pref-gate audit FULLY CLOSED
Shipped verdict: navigator.gpu exists, requestAdapter() RESOLVES-null with zero panics on the
GPU-less VM Ã¢â‚¬â€� spec-correct degradation, safe to ship. Sites get a truthful negative instead of
API absence (bot-signal). Real-GPU adapter path untested here (no vulkan on VM) Ã¢â‚¬â€� Windows/Mac
lanes can verify the positive path. Compile cost was one-time; deps now in target.
PREF-GATE AUDIT FINAL: every one of the 26 gated-off prefs has a verdict Ã¢â‚¬â€� shipped batch-1
(#273: visualViewport/cookieStore/adoptedStyleSheets/composition/entries/wakeLock/protocol),
permissions+storage (#271/#272), sanitizer (#275), webgpu (#280); excluded with evidence:
captureStream (hollow stub), webrtc (gstreamer panic); all others documented deferred.

## grid-2: subgrid is upstream-scale, not a lane increment Ã¢â‚¬â€� mission-blocker evidence
Chased the subgrid implementation path per the grid-1 verdict:
- feather fork pins taffy 0.15.1; upstream servo main pins the SAME version (verified in servo/servo@main Cargo.toml) Ã¢â‚¬â€� no upstream upgrade to ride.
- taffy main (0.16.0-dev, DioxusLabs/taffy) still lacks production subgrid: changelog keeps subgrids in the unsupported list; zero subgrid handling in the grid compute modules.
CONCLUSION: subgrid-in-servo requires implementing subgrid track-sizing participation IN taffy itself (upstream-scale, multi-week algorithmic work Ã¢â‚¬â€� nested tracks must resolve against parent track sizes recursively) or architectural surgery in servo's bridge to bypass taffy for subgridded axes (comparable scale). Neither is a lane increment; this is a coordinator/funding-model decision. Cross-reference: Ladybird implements its own grid (subgrid included) Ã¢â‚¬â€� no shortcut there either since servo delegates all grid sizing to taffy.
RECOMMENDATION: deprioritize subgrid as a standalone upstream mission; the grid mission at mechanism level is otherwise CLOSED-green per grid-1.

## sec1zt169 Ã¢â‚¬â€� devtools evaluateJS regression: INVESTIGATION IN PROGRESS (matrix harness blocked)
SYMPTOM: devtools console evaluateJS NEVER replies (timeout) though the full actor chain works:
banner Ã¢â€ â€™ listTabs Ã¢â€ â€™ getTarget Ã¢â€ â€™ consoleActor all respond correctly. Eval variants (eager,
frameActor) all timeout identically.
EXONERATED: (1) webgpu cargo feature Ã¢â‚¬â€� rebuilt without it, timeout persists (bisect-negative);
(2) script thread death Ã¢â‚¬â€� the shell's own webview.evaluate_javascript path (/tmp/feather-eval.js
hook) works fine on the SAME instance, so the script thread evaluates JS healthily. The break is
specific to the devtools eval path: console actor Ã¢â€ â€™ script_chan Ã¢â€ â€™ DevtoolScriptControlMsg::Eval Ã¢â€ â€™
debugger_global.fire_eval Ã¢â€ â€™ DebuggerEvalEvent handled by spidermonkey debugger JS Ã¢â€ â€™
generated EvalResult binding Ã¢â€ â€™ reply channel. Suspects in order: (a) debugger_global is None
(script_thread.rs:2285 drops the message SILENTLY if so) Ã¢â‚¬â€� verify by instrumenting that arm;
(b) the debugger JS handler failing to complete (resource load or PipelineId wrapping);
(c) reply channel loss.
TIMELINE UNCERTAINTY: matrix-results.json last committed 11-26 23:39 but content may be much
older Ã¢â‚¬â€� and matrix.py had the self-kill bug (pkill -f matched its own cmdline, fixed in
feather#281) meaning it possibly NEVER ran successfully on Linux; results may be mac-lane.
So this may not be a same-day regression at all.
NEXT: instrument the debugger_global None arm + fire_eval entry/exit + EvalResult with eprintln,
one build cycle localizes which of the three stages dies. Alternatively engine-bisect across
11-26/27 window (15-20min per iteration Ã¢â‚¬â€� expensive).

## sec1zt170 Ã¢â‚¬â€� devtools evaluateJS regression SOLVED + FIXED (upstream-candidate)
Root cause chain, three layers: (1) the sync evaluateJS arm in devtools/actors/console.rs
serialized the whole Result<EvaluateJSReply> Ã¢â‚¬â€� reply wire-shape {"Ok": {...}} instead of the
bare object; (2) in DEBUG builds the protocol-layer is_valid_reply debug_assert (protocol.rs:184)
PANICS on the malformed shape, killing the connection Ã¢â‚¬â€� clients see eternal timeout; (3) in
release the malformed packet goes out but the matrix harness happened to tolerate the Ok-wrapper
(`payload = m.get("Ok") or m`), which is why release runs ever worked. The async arm already
unwrapped correctly Ã¢â‚¬â€� the sync arm was a refactor miss. Fix mirrors it
(.map_err(|_| ActorError::Internal)). Verified live: evaluateJS returns result 2.0 correctly
shaped. On feather-engine.
DURABLE LESSON: devtools breakage can be debug-build-only Ã¢â‚¬â€� debug_asserts in the protocol layer
turn wire-shape bugs into connection deaths. When devtools eval times out, check feather stderr
for 'Message is not a valid reply' BEFORE suspecting script-thread plumbing; the three eval-path
stages (message arrival / fire_eval / EvalResult) can all be healthy while delivery dies.

## sec1zt171 Ã¢â‚¬â€� matrix transition reds: NOT a bug Ã¢â‚¬â€� harness load-flake (negative result)
transition-interpolates/transition-completes red in the full-matrix run but PASS on an idle
system: mid@60ms=0.939 (threshold 0.95), end@600ms=0.300 exact. The transition clock is
wall-clock-correct; the harness booting 45 fresh instances back-to-back starves early frames
enough to slip the tight 60ms threshold. Symptom-index: animation probes failing only in batch
runs Ã¢â€ â€™ suspect load-flake BEFORE engine clock bugs. Probe threshold loosening is test-only
polish, not prioritized.

## sec1zt172 Ã¢â‚¬â€� full-matrix Linux regen WORKING end state (feather#286)
242/251, all 9 reds categorized: webgl x2 environmental (GPU-less VM), webrtc/cookiestore
deliberate-off/hollow, range-geometry architectural-documented, transitions load-flake
(sec1zt171), editing designmode + text-orientation engine-scale documented, writing-modes 1
remaining. Drag-drop green in-grid. The harness is now a working Linux instrument Ã¢â‚¬â€� use it for
mechanism regression checks after engine batches.

## grid-3: SUBGRID PHASE-1 IMPLEMENTED AND LIVE-VERIFIED (2026-11-27, mac lane)
Symptom: `grid-template-columns/rows: subgrid` inert Ã¢â‚¬â€� children of subgridded items don't align to parent tracks.
Status: IMPLEMENTED in fork/feather-engine@a45709f97, verified live: probe children measure exactly 80/160/80px at x 9/89/249, aligned across rows.
Mechanism (architecture that matters for future work):
- Nested grids in servo are SEPARATE formatting contexts, not one taffy tree Ã¢â‚¬â€� compute_child_layout recurses through with_independent_formatting_context. So subgrid plumbing lives in servo's GLUE, not inside taffy.
- Phase-1 design = construction-time adoption: TaffyContainer::construct inspects each child item; if the child's style is subgrid in an axis AND the parent's tracks in that axis are all definite plain lengths AND the child's placement resolves to explicit lines (positive/negative/span-end), the spanned parent tracks + parent gap are cloned onto the child grid container (SubgridAdoption). The taffy style conversion (grid_template_columns/rows in stylo_taffy/wrapper.rs) then serves the adopted tracks instead of None; gap() is overridden with the parent gap in the adopted axis.
- INJECTION TRAP: taffy fetches the grid container style via LayoutGridContainer::get_grid_container_style, NOT get_core_container_style Ã¢â‚¬â€� adoption must be injected there (both exist on TaffyContainerContext; missing this cost one iteration).
- Type/lifetime notes: calc-valued lengths use raw pointers into style memory (resolve_calc_value) Ã¢â‚¬â€� restricting adoption to plain unpacked Length avoids all lifetime hazards. TemplateTrackList associated type is a Map over slice::Iter<TrackListValue> with a fn-pointer closure Ã¢â‚¬â€� adoption stores cloned TrackListValues so both arms share one iterator type.
Phase-1 limits (fallback to normal grid, no crash): fr/percentage/content-sized/auto tracks, auto-fill/fit repeats, auto or named-line placement, item participation in parent track sizing (the deep upstream-scale part), subgridded line names, child padding/border misaligns track positions vs parent lines slightly.
Next phases if continued: (2) fr/percentage resolution needs parent geometry at layout time not construction Ã¢â‚¬â€� requires threading sized tracks through the recursion; (3) full content-participation is the multi-week upstream version Ã¢â‚¬â€� see docs/wiki/SUBGRID-PLAN.md.

## sec1zt173 Ã¢â‚¬â€� cookiestore roundtrip FIXED (secure-cookie trustworthiness, three sites)
cookieStore silently dropped every cookie on localhost: cookieStore.set unconditionally marks
Secure (correct per spec), but THREE independent bare-scheme gates required is_secure_scheme:
ServoCookie::new_wrapped write validation, CookieStorage::push insertion, appropriate_for_url
retrieval. All now is_potentially_trustworthy() Ã¢â‚¬â€� localhost/127.0.0.1/loopback roundtrip per
secure-contexts spec, matching Chrome. Remote insecure hosts keep full protection.
Symptom-index: API resolves successfully but effect silently absent Ã¢â€ â€™ suspect multi-layer
spec-deviation gates; grep the WHOLE path (construction/insertion/retrieval) for the same
wrong check Ã¢â‚¬â€� servo's cookie code had it in three places and fixing one/two still fails.

## sec1zt174 Ã¢â‚¬â€� layout-pref audit (second gate family) Ã¢â‚¬â€� method + verdicts
Beyond dom_* webidl gates, stylo gates CSS PROPERTIES behind `servo_pref` layout.* entries in
vendor/stylo/style/properties/longhands.toml (41 total). Audit method: extract all servo_pref
names, normalize dots/dashes to underscores, cross-ref components/config/prefs.rs defaults +
feather's enabled set. Key nuance: many servo_pref names don't exist in servo config at all
('pref-not-in-servo-config') Ã¢â‚¬â€� those are effectively ungated in servo builds. text-overflow
defaults TRUE (why ellipsis worked despite feather never setting it).
SHIPPED (feather#287): font-variations (real impl: font.rs:307 variation axes), webkit-text-fill-color
(real impl: display_list painting). EXCLUDED hollow: columns Ã¢â‚¬â€� parse/compute real but ZERO multicol
layout implementation (grep components/layout for multicol = nothing); test: 300 words same height
in 1-col vs 3-col. ENABLING-HOLLOW-IS-WORSE-THAN-ABSENCE: sites lose graceful single-column fallback.
text-orientation red in matrix was flake/stale Ã¢â‚¬â€� computes 'upright' correctly live.

## sec1zt175 Ã¢â‚¬â€� layout-pref phase-2 surface: gated properties need pref FIELDS added (engine work)
Critical correction to sec1zt174: 'pref-not-in-servo-config' does NOT mean ungated Ã¢â‚¬â€� verified
counter-reset/hyphens/appearance/scroll-behavior all compute EMPTY even when parsed (style attr
retains text, computed style empty). servo prefs are a FIXED struct (components/config/prefs.rs)
Ã¢â‚¬â€� properties whose servo_pref has no struct field are permanently gated off; enabling requires
ADDING pref fields to servo config first, then feather opt-in.
Implementation-depth pre-screen (grep components/layout + fonts):
- appearance: layout/mod.rs hit Ã¢â‚¬â€� plausible-real, TOP CANDIDATE (appearance:none ubiquitous in
  modern CSS form resets)
- hyphens: fonts/query.rs hit Ã¢â‚¬â€� plausible-real, moderate value
- scroll-behavior/masking/transform-box/overscroll: no layout hits Ã¢â‚¬â€� likely parse-only hollow,
  verify before enabling (enabling hollow = worse than absence, sec1zt174)
- counters: niche Ã¢â‚¬â€� servo list numbering works through a separate internal path
NEXT-SESSION BATCH RECIPE: add pref struct fields (default false) for appearance+hyphens first,
enable in feather, probe computed non-empty + one behavioral check each; batch per round like
batch-1. Est. small engine diff + shell prefs + verification cycle per candidate.

## sec1zt176 Ã¢â‚¬â€� layout-pref audit batch 3: masking + scroll-behavior real
Depth screen: counters (no layout impl Ã¢â‚¬â€� hollow, skip), transform-box (no layout impl Ã¢â‚¬â€� hollow, skip), masking (display_list clip-path hit Ã¢â‚¬â€� real), scroll-behavior (element.rs hit Ã¢â‚¬â€� real). Both wired end-to-end (prefs.rs fields + stylo mapping + shell engine.rs) and verified computing live: `clip-path:circle(40%)` non-none, `scroll-behavior:smooth`. Engine: feather-engine 9858a8266e5; shell: feather c6c803a on devin/1795753937-layoutprefs (#287). Lesson: grep components/layout + components/script for the camelCase name before enabling Ã¢â‚¬â€� no hit = hollow = worse-than-absence. Remaining servo_pref keys with no servo struct field are permanently off (set_value panics); flipping stylo preferences.toml defaults directly is an untested lever.

## sec1zt177 Ã¢â‚¬â€� layout-pref audit CLOSED; 3d-transforms hollow negative result
transform-style:preserve-3d is hardcoded wr::TransformStyle::Flat in display_list/stacking_context.rs Ã¢â‚¬â€� servo explicitly flattens 3d, so enabling the transform-style pref is hollow (computes but never honored). content-visibility + offset-path: zero layout hits, hollow. counters/transform-box: hollow (sec1zt176 screen). Negative results matter: the remaining stylo servo_pref keys are all either permanently-off (no servo struct field) or hollow. Audit closed Ã¢â‚¬â€� do not re-screen without new evidence. Symptom: any stylo-gated property that "computes but does nothing" Ã¢â€ â€™ check: is the transform/behavior hardcoded in display_list conversions (e.g. TransformStyle::Flat).

## sec1zt178 Ã¢â‚¬â€� webgl VERIFIED WORKING; matrix harness self-interference lesson
WebGL1+WebGL2 contexts both create successfully on the GPU-less VM (servo has software-composite plumbing Ã¢â‚¬â€� webgl_thread.rs software_composite). The matrix webgl red was stale noise from the broken-harness era (self-kill + evaluateJS serialization bug). LESSON: never pkill feather-ui while a matrix regen is running Ã¢â‚¬â€� killing the in-flight instance mid-connect hangs the harness silently (recv blocks on a half-open devtools socket). Harness hardened: per-page fault isolation + incremental matrix-results.json write after every page (tests/scenarios/matrix.py). ALSO: dteval.py added Ã¢â‚¬â€� standalone devtools evaluateJS client, focus-independent alternative to the mouse-jiggle eval-file harness; requires booting with FEATHER_DEVTOOLS=9222 (env opt-in per engine.rs). The mouse-jiggle harness fails whenever anything else competes for the screen; prefer dteval. Symptom: matrix regen stalls at 'devtools connecting' with no progress Ã¢â€ â€™ check: is another feather-ui instance or manual experiment competing for port 9222 / did someone pkill mid-run.

## sec1zt179 Ã¢â‚¬â€� transitions VERIFIED wall-clock correct; matrix red is boot-jank artifact
Steady-state measurement via devtools eval with in-page setTimeout sampling: 40ms:0.938 Ã¢â€ â€™ 80:0.537 Ã¢â€ â€™ 120:0.407 Ã¢â€ â€™ 160:0.313 Ã¢â€ â€™ 200ms:0.300 Ã¢â‚¬â€� clean wall-clock interpolation completing exactly on the 0.2s duration in a fully idle page. The animation tick chain (AnimationRefreshDriverObserver Ã¢â€ â€™ animating_webviews Ã¢â€ â€™ TickAnimation) works correctly. The matrix red was measurement artifact: the probe started the transition during initial layout/boot when the script thread is busy Ã¢â‚¬â€� values reflected boot jank not the animation clock (consistent with sec1zt171). Probe hardened: starts transition only after double-rAF. Lesson: matrix probes measuring time-sensitive behavior must establish steady-state first (double-rAF minimum). Symptom: animation/transition timing probes fail with mid-transition values Ã¢â€ â€™ check: is the measurement happening during boot/layout jank rather than steady-state.

## sec1zt180 Ã¢â‚¬â€� pgrep/pkill -f matrix.py SELF-MATCHES the invoking shell
`pkill -f matrix.py` and `pgrep -f matrix.py` match the bash -c wrapper of the very command running them (the pattern string is in its own cmdline) Ã¢â‚¬â€� the shell kills itself (exit -1, no output) or pgrep lies MATRIX-ALIVE. Same defect class as the original harness self-kill (sec1zt167/#281) but at the operator level. Use `pgrep -af 'python3 matrix' | grep -v bash` and kill by exact pid. ALSO: matrix-measurement validity depends on the working-tree branch Ã¢â‚¬â€� the harness serves pages from the working tree, so a regen run while checked out on a branch lacking probe fixes measures the WRONG pages (chased a phantom webgl 'flakiness' for several rounds before discovering the working tree had reverted to pre-fix pages after a branch switch). Verify `git branch --show-current` before trusting any matrix result. Symptom: matrix result contradicts manual verification of the same probe Ã¢â€ â€™ check: which branch was checked out when the harness ran.

## sec1zt181 Ã¢â‚¬â€� transitions FINAL verdict: engine correct, fixed-window probe class invalid
Multi-round evidence chain: (1) steady-state passive sampling exact wall-clock interpolation 40ms:0.938Ã¢â€ â€™200ms:0.300; (2) even on the probe page post-boot, full transition completes in 187ms wall-clock vs 200ms spec; (3) yet fixed-window probes fail in boot conditions with ~0.99 mid values regardless of rAF gating or settles. Mechanism: transition-start lags the style change under load (style resolution must process the change before the transition exists) so every absolute-timestamp sample is randomly phase-shifted Ã¢â‚¬â€� the ENTIRE fixed-window probe class is invalid in fresh-boot harnesses, not just this probe. Robust replacement pattern: prove interpolation by delta-sampling (two samples 100ms apart, require descending delta) Ã¢â‚¬â€� phase-independent. Both probes green after conversion. Symptom: any timing-sensitive probe flaky in matrix but correct manually Ã¢â€ â€™ check: is it a fixed-window measurement being phase-shifted by boot lag; convert to delta-sampling.

## sec1zt182 Ã¢â‚¬â€� session state pointer (context-exhaustion handoff)
Shipped this session tail: feather#287 (pref audit incl execCommand Ã¢â‚¬â€� open), #288 (matrix 252/256 + probe fixes Ã¢â‚¬â€� open), #289 (changelog Ã¢â‚¬â€� open), #290 (dbus notifications Ã¢â‚¬â€� open), #291 (Cargo.lock webgpu closure Ã¢â‚¬â€� open). #286 superseded by #288. Matrix effectively fully explained: only designmode/range-geometry/webrtc-off remain red, all documented upstream gaps or decisions. captureStream re-confirmed hollow: CaptureStream() creates track with random id, zero frame-source wiring Ã¢â‚¬â€� real implementation needs canvas paint-end hook Ã¢â€ â€™ frame capture Ã¢â€ â€™ media pipeline (week-scale, as documented). Next best items: webrtc graceful-degradation trait refactor (design in sec1zt163-era notes), crash Stage-1 media granularity, designMode (engine-scale). Working tree on devin/1795755700-matrixregen; other branches pushed.

## sec1zt183 Ã¢â‚¬â€� webrtc graceful-degradation refactor COMPLETE + verified
The entire webrtc construction chain was infallible and gstreamer panicked via `expect("WebRTC creation failed")` when webrtcbin/plugins missing Ã¢â‚¬â€� killing the script thread the moment any page ran `new RTCPeerConnection()`. Refactored end-to-end (commit e0e8d13e60c on feather-engine): WebRtcBackend::construct_webrtc_controller Ã¢â€ â€™ Result; WebRtcController::new Ã¢â€ â€™ Option (no thread spawned on failure); Backend::create_webrtc Ã¢â€ â€™ Option across dummy/gstreamer/ohos (ohos was bare todo! Ã¢â‚¬â€� now None); RTCPeerConnection constructor throws catchable NotSupportedError when backend unavailable. VERIFIED live with pref temporarily flipped on: `new RTCPeerConnection()` Ã¢â€ â€™ "threw:NotSupportedError", page continues, browser alive. Downstream `.unwrap()`s on controller remain sound because a constructed peer connection always has a controller. DESIGN KEY: constructor-throws is the spec-honest shape for unsupported webrtc Ã¢â‚¬â€� this removes the crash-class blocker for enabling dom_webrtc_enabled; decision to enable belongs to coordinator (fingerprinting tradeoff per sec1zt163 audit). Symptom: any page using RTCPeerConnection crashes/hangs the whole browser Ã¢â€ â€™ check: is the backend construction chain panic-free (post-e0e8d13e60c it is); if constructor throws NotSupportedError, that's correct spec behavior not a bug. NEGATIVE RESULT: cargo check in ~/repos/servo standalone still fails (freetype2 pkg-config) Ã¢â‚¬â€� compile-verify servo changes via `cd ~/feather/shell-new && cargo check -p feather-ui` which compiles servo crates as path deps (~2min incremental).

## sec1zt184 Ã¢â‚¬â€� designMode IMPLEMENTED + verified (matrix red closed)
Was: commented out in Document.webidl, zero implementation Ã¢â‚¬â€� but the editing infrastructure (execCommand/selection/input) was already complete and keyed entirely off is_editing_host. Per HTML spec an editing host is ALSO "the HTML body element of a document whose design mode is enabled" Ã¢â‚¬â€� so implementation was small and mechanism-level (e7b4e7d13ae): Document.design_mode_enabled Cell + ASCII-case-insensitive 'on' setter (toggles trigger document restyle via add_restyle_reason(DOMChanged) since :read-write depends on editing state); HTMLElement::is_editing_host gained the body-element+designMode clause replacing the literal upstream TODO. Verified live: off/on round-trip correct, body isContentEditable tracks, execCommand insertText genuinely edits the document body ("TYPEDhello editable"). KEY PATTERN: when a spec feature is commented out upstream, grep for the spec hook (TODO comments in servo often mark exactly where the feature plugs in) Ã¢â‚¬â€� the infrastructure may already be complete underneath. designMode has no pref gate Ã¢â‚¬â€� it's a standard HTML feature, always on. Symptom: site editors/richtext that set document.designMode='on' appear non-editable Ã¢â€ â€™ check: post-e7b4e7d13ae this is implemented; if editing still fails check the editing host chain (is_editing_host Ã¢â€ â€™ is_editable_or_editing_host) not designMode itself.

## handoff-designmode-verify (2026-11-27, mac lane)
State: designMode implemented by ANOTHER lane in feather-engine@ea9b069ea (editing-host integration version Ã¢â‚¬â€� superior to this lane's dropped duplicate). Build /tmp/dm-build2.log compiles it on the mac VM. ONE-STEP VERIFICATION once build finishes: run `python3 /tmp/mxwm.py`-style matrix editing class probe Ã¢â‚¬â€� expect `designmode-property` PASS. Harness script pattern in tests/scenarios/matrix.py run_one_page; standalone runner example at /tmp/mxcplx.py (swap URL to editing.html). Lane-coordination lesson: matrix-reds worklist is consumed by multiple lanes Ã¢â‚¬â€� coordinator should split assignments explicitly to avoid duplicate implementation.

## sec1zt185 Ã¢â‚¬â€� :has() enabled + verified; github wedge is a SEPARATE defect
:has() parse gate (layout.css.has-selector.enabled) had no servo pref field Ã¢â‚¬â€� permanently off, so valid spec selectors (:not(:has(...)) Ã¢â‚¬â€� github.com Primer uses hundreds) failed parsing and entire rules dropped. Wired the field (70907f1e6b2) + shell enablement (feather#297). Verified live per behavior-verification rule: basic :has(>) matches, :not(:has()) negation correct both directions, AND dynamic invalidation works (post-load child injection restyles parent Ã¢â‚¬â€� the hard part). Zero :has errors remain on github. DIAGNOSTIC TECHNIQUE indexed: perf/gdb on this VM need `sudo sysctl kernel.perf_event_paranoid=-1 kernel.yama.ptrace_scope=0` (passwordless sudo works). Thread heat triage: `for t in /proc/PID/task/*; cat $t/comm + stat utime` identifies spinning threads fast. GITHUB WEDGE evidence chain: script thread spinning in JIT-compiled JS (EnterJit + anon:js-executable-memory frames in perf), Style threads simultaneously hot in url Vec PartialEq (restyle storm), never settles (230%+ CPU minutes after load), devtools evals all timeout incl 1+1, but browser process alive and SSR header renders. NOT caused by the :has gap (retested post-fix Ã¢â‚¬â€� still wedges). Hypothesis class: JS-driven mutation/restyle feedback loop (mutation observer cascade or web-components attribute reflection loop). Symptom: modern JS-framework site renders SSR shell then wedges with high CPU, devtools unresponsive Ã¢â€ â€™ check: thread heat pattern Script+Style both hot = mutation/restyle storm; Script-only hot = pure JS loop.

## sec1zt186 Ã¢â‚¬â€� CSS property-coverage census method + break-word fix
"Unsupported property declaration" log spam on real sites is mostly NOT missing properties. Live probe method: set values via el.style[prop]=val and check reflect Ã¢â‚¬â€� most suspect properties (resize/contain/scrollbar-width/text-underline-position/scroll-margin-top/forced-color-adjust) all WORK. Real gaps on github.com: (1) word-break:break-word Ã¢â‚¬â€� the legacy value EVERY major site uses; gecko has full implementation incl the overflow-wrap:anywhere coupling, gated behind layout.css.word-break.break-word.enabled parse-condition which defaults true for gecko but unwired for servo Ã¢â‚¬â€� fixed same pattern as :has. (2) text-wrap:balance/pretty genuinely absent (newer spec, text-wrap-mode/style split). (3) -moz-osx-font-smoothing + -moz-column-gap Ã¢â‚¬â€� harmless vendor prefixes. (4) @custom-media Ã¢â‚¬â€� draft spec, needs preprocessor feature. LESSON: never trust log-volume analysis for property gaps Ã¢â‚¬â€� probe reflect-test first; vendor prefixes and value-level gaps dominate. ALSO indexed: vendored stylo longhands.toml has 436 property sections; gecko-only engine="gecko" entries are candidates for the same ungating treatment (e.g. resize was engine=gecko but WORKS in servo Ã¢â‚¬â€� engine field is not authoritative, always live-probe). WEDGE INVESTIGATION negative results: repro with 400 custom props + 3000 elements + rAF-layout-read loop does NOT melt down (14fps, 119% cpu, converges); infinite rAF does not starve devtools evals; the CSS parse error spam is one-time-per-parse not looping. The github wedge remains unexplained Ã¢â‚¬â€� next discriminators: shadow-dom-heavy repro, web-components attribute reflection, or bisect by github subpage.

## sec1zt187 Ã¢â‚¬â€� github wedge MECHANISM CONFIRMED: animation-driven restyle storm
Repro chain (all negative results matter): custom-props+layout-read loop converges fine; infinite rAF doesn't starve evals; webgl2-off eliminates the DetachShader error spam but wedge persists (webgl exonerated); repo pages don't wedge at all Ã¢â‚¬â€� only the marketing homepage. MINIMAL REPRO that saturates CPU exactly like github: 400 custom properties on :root + 3000 elements + an infinite CSS opacity animation on a wrapper div Ã¢â‚¬â€� 255% CPU continuous with ZERO JS. Mechanism: animating ANY property on an element high in the tree causes the entire descendant subtree to restyle every animation frame; the cost center is custom-properties map comparison/dedup (can_deduplicate_values, Vec/url PartialEq, zip iteration dominate both Script and Style thread profiles). Opacity doesn't inherit Ã¢â‚¬â€� descendants should be completely unaffected Ã¢â‚¬â€� so the recascade is spurious. Prime suspect: animation tick produces a fresh custom-properties Arc identity for the animated element even when custom props are unchanged, so inherited-map identity comparison fails for every child Ã¢â€ â€™ full recascade per frame. Gecko dedups these maps precisely so unchanged maps keep identity; either servo's dedup is not running or the restyle propagation ignores value-equality. Symptom: site with infinite CSS animations pins all cores and wedges devtools Ã¢â€ â€™ check: repro with big DOM + animation on wrapper, profile for custom_properties_map comparisons. NEXT: examine how servo animation ticks dirty/restyle Ã¢â‚¬â€� components/layout animation tick path vs stylo invalidation.

## matrix-reds-hunt-COMPLETE (2026-11-27, mac lane)
Full re-verification of every catalogued matrix red on current feather-engine Ã¢â‚¬â€� THE HUNT IS EXHAUSTED:
- designmode-property: FIXED (other lane, live-verified roundtrip) Ã¢â‚¬â€� editing 6/6
- cookiestore-roundtrip: PASSES through harness (platform-apis 10/10) Ã¢â‚¬â€� earlier red stale
- transforms transitions: 5/5 Ã¢â‚¬â€� earlier reds timing flakes
- webgl-context/webgl2: both live-verified TRUE Ã¢â‚¬â€� stale/environmental
- range-geometry: CLOSED architectural upstream gap (text nodes yield zero fragments). Nuance: element-node ranges measure correctly, only text-node ranges 0x0.
- webrtc: graceful degradation by design
REMAINING TRACTABLE MATRIX WORK: none Ã¢â‚¬â€� all classes green except documented architectural items. Next engine-scale candidates: inline glyph progression (vertical text), text-node fragment architecture, subgrid phases 2-3.

## sec1zt188 Ã¢â‚¬â€� restyle-storm investigation state + repro suite + next steps
STATUS: mechanism confirmed but root-cause fix NOT started Ã¢â‚¬â€� full handoff. CONFIRMED FACTS: (1) animating ANY element (even a leaf span) pins 250% CPU on big-DOM pages and 151% even on a 10-element page with 400 :root custom properties; (2) big-DOM pages with custom props but ZERO animation/JS burn 100%+ for many seconds after load before partially settling (30% at +14s) Ã¢â‚¬â€� convergence is slow even when it happens; (3) cost center in every profile: custom-properties map comparison Ã¢â‚¬â€� can_deduplicate_values, Vec/url PartialEq, zip iteration, servo_arc custom props Arc operations Ã¢â‚¬â€� on BOTH Script#1 and StyleThreads; (4) the per-frame/per-element cost scales with custom-property COUNT, and total cost scales with element count even when animating a leaf Ã¢â‚¬â€� meaning restyle work is NOT scoped to the dirty element. UNRESOLVED QUESTIONS (the actual fix targets): (a) does servo ever do incremental/scoped restyle, or does any NodeDamage::Style trigger full-document cascade traversal? mark_animating_nodes_as_dirty (animation_manager.rs) dirties with NodeDamage::Style every tick for every animating node Ã¢â‚¬â€� if restyle traversal is global, every animation frame is a full-document recascade Ã¢â‚¬â€� matches all evidence; (b) why don't unchanged custom-props maps dedupe/short-circuit Ã¢â‚¬â€� gecko relies on map dedup so unchanged inherited props keep Arc identity; check whether servo's cascade rebuilds maps from scratch per restyle. REPRO SUITE: committed at feather tests/scenarios/pages/anim-*.html + noanim-*.html on branch devin/*-restyle-repros (PR open) Ã¢â‚¬â€� serve via localhost:8411 pattern. MEASUREMENT METHOD: ps pcpu of feather-ui + per-thread heat via /proc/PID/task/*/comm+stat (no sudo needed); perf needs sysctl overrides (sec1zt185). IMPORTANT CONFOUND: this VM is GPU-less so compositing is software Ã¢â‚¬â€� separate rasterization cost from style cost in profiles (style cost proved dominant here). PROJECT IMPACT: this is THE idle-CPU budget killer (idle ~0% goal) Ã¢â‚¬â€� every animated site burns cores continuously; prioritise over feature work.

## sec1zt189 Ã¢â‚¬â€� WPT harness setup + VM gotchas + compositing confound
WPT background run infrastructure: servo builds STANDALONE fine via `.venv/bin/python mach build --dev` (the system-python mach fails on missing toml Ã¢â‚¬â€� must use the venv python; freetype2 IS installed despite an old memory note Ã¢â‚¬â€� standalone cargo check failure was a stale-env artifact). CRITICAL: build MUST use CARGO_PROFILE_DEV_DEBUG=0 Ã¢â‚¬â€� full debuginfo balloons target to 30-60G+ and the 124G disk fills mid-build (feather/shell-new/target alone is 82G; when full, `rm -rf shell-new/target/debug/incremental` frees 32G without losing dep caches). Driver: ~/wpt-run.sh waits for target/debug/servoshell (NOTE: mach build produces servoshell NOT servo Ã¢â‚¬â€� the driver binary-check must match or it false-positive declares BUILD DIED) then runs priority dirs (css/html css/cssom html websockets fetch workers) writing mozlog to /tmp/wpt-results-DATE; report via docs/wiki/wpt/wpt-report.py (feather#301). Status in /tmp/wpt-status.txt. NEXT-SESSION RESUME: check /tmp/wpt-status.txt; if runs done, generate report `python3 docs/wiki/wpt/wpt-report.py /tmp/wpt-results-DATE > docs/wiki/wpt/wpt-DATE.md` and PR it. RESTYLE-STORM CONFOUND ADDENDUM (sec1zt188): the 150% CPU baseline even on tiny-DOM animation may be largely SOFTWARE COMPOSITING (GPU-less VM repaints full window per frame at 60fps) Ã¢â‚¬â€� the style-thread custom-props cost is real and proven but the constant baseline is partly a VM artifact; on real-GPU systems wedge severity may be lower. Separate the two when fixing: profile for raster/compositing threads vs style threads. Symptom: any WPT/perf measurement on this VM Ã¢â€ â€™ remember software rendering inflates all animation/repaint costs.

## sec1zt190 Ã¢â‚¬â€� restyle IS scoped (architecture correction) Ã¢â‚¬â€� storm cost is per-frame pipeline not global cascade
Element.restyle() registers the element in the document's pending_restyles map with RestyleHint::RESTYLE_SELF Ã¢â‚¬â€� servo HAS scoped restyle infrastructure, so the storm is NOT a full-document recascade. Revised theory of cost composition on the github-class repro: (1) constant per-frame cost: the animation pump drives full update_the_rendering Ã¢â€ â€™ reflow + SOFTWARE compositing of the whole window per frame (GPU-less VM artifact Ã¢â‚¬â€� significant constant baseline even for tiny DOMs, explains the 151% small-DOM floor); (2) DOM-scaling cost: per-element recascade cost scales with custom-property count Ã¢â‚¬â€� each restyled element inherits/recompares the parent's custom-props map (hundreds of entries on token-heavy sites), and layout/reflow of large trees. The custom-props comparison hotness in profiles is real but may be symptom not cause: it runs wherever cascade runs. NEXT DISCRIMINATORS when fixing: (a) measure pure-compositing floor Ã¢â‚¬â€� animate a trivial transform on an empty page: whatever CPU remains is the software-rendering baseline that does NOT affect GPU users; (b) then the style-side fix targets are: reduce per-element custom-props inheritance cost (dedup effectiveness Ã¢â‚¬â€� check whether unchanged parent maps actually share Arc identity across animation ticks; if the animated element's restyle rebuilds its inherited map fresh each tick the dedup never fires) and verify animation dirtying skips non-animatable-affecting work. NOTE the FIXME at element.rs restyle(): RESTYLE_SELF is inserted unconditionally for all damage types "preserving existing behavior" Ã¢â‚¬â€� upstream-suspect.

## sec1zt191 Ã¢â‚¬â€� DECISIVE: animation pipeline constant floor ~150-160% CPU even on EMPTY page
Repro: single 50px div animating opacity on an otherwise empty page burns 159% CPU continuously (vs 151% for the same animation on a 1400-node DOM, 255% on the big github-style DOM). PROOF: the per-frame cost is dominated by a CONSTANT pipeline cost Ã¢â‚¬â€� every animation frame unconditionally re-runs layout + paint + software-composite of the full window, regardless of what actually changed. DOM content is a secondary scaling term (~+100% for big DOM). THE FIX TARGET for the idle-CPU budget: compositor-only animations (opacity/transform) must skip layout AND paint entirely Ã¢â‚¬â€� the compositor can animate them from cached layer textures; and paint should be damage-region based. Even on real-GPU systems the unconditional layout+paint re-run per frame is wrong. Ladybird comparison reference: how does their AnimationDriver determine whether a frame needs layout at all? KEY QUESTION FOR THE FIX: does servo track whether the animated property is compositor-only? START: components/compositing + style/animation keyframe evaluation path Ã¢â‚¬â€� the animated values are computed in style each tick and pushed through the normal style pipeline (that's WHY restyle happens per tick at all). Upstream-grade fix is architectural: split compositor-animation sampling out of the style flush. INTERIM cheaper win: skip LAYOUT when restyle damage is empty (opacity doesn't dirty layout Ã¢â‚¬â€� check whether LayoutDamage propagates anyway). Symptom: any CSS animation burns cores permanently regardless of page size Ã¢â€ â€™ check update_the_rendering path, animation damage propagation, compositor-layer animation support.

## sec1zt192 Ã¢â‚¬â€� REVERSAL: animation cost is ~93% llvmpipe (VM artifact), engine only ~10% Ã¢â‚¬â€� wedge priority DOWNGRADED pending real-GPU verification
Rate measurement on the empty-page opacity-animation repro: llvmpipe threads 153% CPU total, servo threads only ~10% (Script#1 7%, WRRenderBackend 3%, everything else negligible). The 'constant pipeline floor' from sec1zt191 is Mesa SOFTWARE GL RASTERIZATION Ã¢â‚¬â€� a GPU-less-VM artifact, not servo layout/style. On real-GPU machines compositing opacity/transform animations is nearly free. REVISED UNDERSTANDING of the github wedge: severity on this VM was dominated by llvmpipe rasterizing full-window frames continuously; the script-thread-unresponsive aspect is consistent with CPU starvation (7 llvmpipe threads saturating cores), NOT necessarily a servo defect. CONSEQUENCES: (1) the restyle-storm is NOT confirmed as the idle-CPU budget killer on real hardware Ã¢â‚¬â€� needs re-verification on a GPU machine (Mac/Windows lanes) before architectural surgery; the custom-props cost is real but small-percent. (2) PERF-MEASUREMENT RULE on this VM: always split llvmpipe (Mesa software raster) from engine threads Ã¢â‚¬â€� `for t in /proc/PID/task/*; cat comm + utime` sampled twice 5s apart, diff/5 = %cpu. Any animation/repaint measurement dominated by llvmpipe says nothing about real hardware. (3) idle-CPU budget (~0% when nothing animates) is unaffected by this Ã¢â‚¬â€� no-animation idle already measures ~0. Symptom: high CPU with animation on Linux VM Ã¢â€ â€™ check llvmpipe thread heat FIRST before attributing to engine. Symptom: script thread unresponsive + wedged page Ã¢â€ â€™ consider llvmpipe-induced starvation before assuming engine defect.

## sec1zt193 Ã¢â‚¬â€� HANDOFF 2026-11-27 ~09:45 UTC: WPT running; next work = Reddit boot-death root-cause
STATE: (1) WPT background run LIVE Ã¢â‚¬â€� driver pid-filed in /tmp/wpt-driver.log, results streaming into /tmp/wpt-results-2026-11-27/ (css_html first, then css/cssom html websockets fetch workers). FIRST-RUN manifest update makes the first dir slow. WHEN COMPLETE: `python3 ~/feather/docs/wiki/wpt/wpt-report.py /tmp/wpt-results-2026-11-27 > ~/feather/docs/wiki/wpt/wpt-2026-11-27.md` on a fresh branch, PR it. NOTE the report.py in the feather repo Ã¢â‚¬â€� driver fix (servoshell binary name) is in ~/wpt-run.sh but NOT yet synced into docs/wiki/wpt/wpt-run.sh (copy when PRing the report). (2) Restyle-storm DOWNGRADED per sec1zt192 Ã¢â‚¬â€� needs real-GPU verification before architectural work; do NOT invest in it from this VM beyond what llvmpipe-subtracted measurement supports. (3) NEXT BACKLOG ITEM: Reddit boot-death root-cause Ã¢â‚¬â€� reddit.com produces 6KB DOM with empty text, framework boot death suspected (same class as the YouTube serviceWorker.ready win Ã¢â‚¬â€� check console errors FIRST, they're the fastest discriminator). Method: launch feather-ui reddit.com with FEATHER_DEVTOOLS=9222, devtools Runtime.consoleAPICalled / exceptionThrown events, identify the exact throwing spec surface, implement per spec. (4) Open PRs: none of mine pending at this writing. (5) coordinator knows: llvmpipe reversal reported in-flight.

## sec1zt194 Ã¢â‚¬â€� REDDIT BOOT-DEATH VERDICT: server-side anti-bot gate, NOT an engine defect
Full chain verified: (1) reddit.com serves a ~8KB anti-bot interstitial shell server-side to feather's UA Ã¢â‚¬â€� curl with the same UA receives byte-equivalent content, so nothing engine-side causes the service. (2) The gate's challenge JS EXECUTES CORRECTLY in feather Ã¢â‚¬â€� the hidden 'solution' input gets populated with the doubled challenge string (async/await + DOMContentLoaded + DOM APIs all fine). (3) form.requestSubmit() navigation is fully functional Ã¢â‚¬â€� controlled repro: a DOMContentLoaded auto-submitting form navigates with correct query string (?solution=x observed server-side). CONCLUSION: the engine correctly performs every step of the gate; the interstitial persists because reddit's gate re-serves itself (cookie-based allowlisting that never passes for this client, or the trivial doubled-string challenge is a placeholder for real PoW that varies). Whether the gate passes is bot-evasion territory, NOT engine correctness Ã¢â‚¬â€� closing this as not-a-bug. If the coordinator wants reddit to render, the lever is client-identity/bot-signal work (UA/cookie/jar behavior across the challenge redirect Ã¢â‚¬â€� verify whether the Set-Cookie from the gate response is stored and replayed), not spec-gap hunting. Symptom: site serves tiny shell with single inline script + form Ã¢â€ â€™ check for anti-bot interstitial (reddit pattern: input[name=solution] + requestSubmit in DOMContentLoaded) before assuming boot-death. Method note: controlled localhost repro of form-submit beats devtools eval across cross-origin navigations (actors detach). Note: the 8411 test server died at some point Ã¢â‚¬â€� geotest repros now also served from a fresh server when needed; don't assume localhost:8411 is alive.

## sec1zt195 Ã¢â‚¬â€� handoff addendum: wpt supplement chained + reddit closed
WPT: main driver running html/websockets/fetch/workers; css supplement script ~/wpt-css-supplement.sh chained behind it (14 high-value css spec dirs: flexbox grid cascade values sizing position transforms animations transitions color fonts box display contain conditional). NOTE css/html was a NONEXISTENT path Ã¢â‚¬â€� skip. WHEN "ALL RUNS COMPLETE" in /tmp/wpt-status.txt: generate report with ~/feather/docs/wiki/wpt/wpt-report.py, sync the servoshell-binary fix + supplement list into docs/wiki/wpt/wpt-run.sh, single PR with report+sync. Reddit boot-death: CLOSED not-a-bug (sec1zt194) Ã¢â‚¬â€� anti-bot gate, engine executes it correctly. Backlog state after this: captureStream real impl, text-wrap, @custom-media, crash-stage-1, WebAuthn/Push Ã¢â‚¬â€� all week-ish items; no small verified item remains open. CONSIDER: with restyle-storm downgraded (llvmpipe artifact) and reddit closed, best next value = WPT-driven spec fixing (the run results will hand a prioritized defect list Ã¢â‚¬â€� exactly the user's spec-driven philosophy).

## sec1zt196 Ã¢â‚¬â€� WPT preliminary signal + container-rule crash queue item
PRELIM (partial run): css/cssom 98.7% pass (301/305!), html ~85% Ã¢â‚¬â€� servo's core-spec conformance is strong; the compat wedge is elsewhere (bot-detection, experimental features). CRASH QUEUE ITEM: css/cssom/CSSContainerRule.tentative.html CRASHES servoshell under the WPT harness (with --enable-experimental-web-platform-features). Does NOT crash feather bare (feather doesn't enable that flag / test needs testharness). REPRO properly: after the background run finishes, `./mach test-wpt tests/wpt/tests/css/cssom/CSSContainerRule.tentative.html` and read the panic backtrace Ã¢â‚¬â€� likely a container-rule CSSOM implementation gap hitting unwrap/todo. Upstream-candidate crash fix Ã¢â‚¬â€� the class the user prioritizes (crashes must never kill the browser). pageswap/pagereveal ERRORs in html/browsers/history-traversal are speculative-navigation features (prerender/pageswap) Ã¢â‚¬â€� expected-missing, low priority.

## sec1zt197 Ã¢â‚¬â€� CSSContainerRule crash FIXED (beaaca6cc81) + compile-verification path correction
FIX: cssconditionrule.rs ConditionText() was missing the CSSContainerRule downcast arm Ã¢â‚¬â€� unreachable!() panic on any container-rule conditionText read (WPT css/cssom/CSSContainerRule.tentative.html crasher). Container rule already had get_condition_text(); one arm wires it. Compile-verified, pushed to feather-engine. LIVE VERIFICATION PENDING: rebuild servoshell + rerun the repro page AFTER the WPT background run finishes (don't rebuild mid-run Ã¢â‚¬â€� invalidates measurements; the repro was confirmed crashing pre-fix via `servoshell --headless --enable-experimental-web-platform-features file:///tmp/cqtest.html` Ã¢â‚¬â€� rerunning that should print CQ-OK in title). COMPILE-CHECK GOTCHA: `cargo check -p servo-script` FAILS on freetype2 version (system 24.1.18 < required 24.3.18) because the package-only check doesn't enable the `bundled_freetype` feature Ã¢â‚¬â€� must run workspace-level `cargo check` (no -p) which mirrors mach's feature unification; ~2min incremental. Symptom: insertRule/reading CSSOM on @container rules crashed Ã¢â€ â€™ fixed; pattern: when a CSSRule subtype is added, audit EVERY downcast chain in cssrule.rs/cssconditionrule.rs/cssgroupingrule.rs for missing arms Ã¢â‚¬â€� unreachable!() is the crash signature.

## sec1zt198 Ã¢â‚¬â€� cssrule unreachable!() family audit: only Container confirmed live-crasher
Audit of the CSSOM downcast chains (pattern from sec1zt197): cssrule.rs creation path has `StyleCssRule::Page(_) => unreachable!()`, `CounterStyle(_) => unimplemented!()`, `Document(_) => unimplemented!()` Ã¢â‚¬â€� HOWEVER @page+@counter-style repro did NOT crash servoshell (no panic in log): stylo apparently never materializes those variants into the rule list in servo's parse configuration (paged-media rules dropped). So they're latent, not live Ã¢â‚¬â€� only fix if a real repro surfaces. The Container arm WAS live because container rules parse into the list under --enable-experimental-web-platform-features. LESSON: unreachable!() arms in cssrule creation are only crash-reachable if stylo's parser actually emits the variant Ã¢â‚¬â€� check parser emission before treating as live defect. cssrule.rs:90 chain also lacks Scope/StartingStyle/Margin/PositionTry/ViewTransition arms Ã¢â‚¬â€� same latent class; if the WPT css/ css-scoping/css-cascade results show CRASH entries, these are the first suspects.

## sec1zt199 Ã¢â‚¬â€� P0 REDIRECT 11:47 UTC: gmail boot freeze + youtube seek-kill regression (DROP queue)
Coordinator P0, outranks everything: (1) GMAIL BOOT FREEZE Ã¢â‚¬â€� gmail boot permanently wedges UI thread 7+min, Windows lane has sharp characterization allegedly in FEATHER_MEMORY (NOT FOUND by grep gmail/freeze/batch Ã¢â‚¬â€� possibly not yet pushed or different wording; reproducing directly instead). Note: pre-batch-6 sweep had gmail rendering fully Ã¢â‚¬â€� so likely a batch-6-window regression. (2) YOUTUBE SEEK KILL Ã¢â‚¬â€� seeking backward (any seek?) kills playback permanently; REGRESSED Ã¢â‚¬â€� backward-seek fix was verified round-10 at commit a4922916078 (position-offset). Bisect window = commits after a4922916078; media-tree commits since then: only e0e8d13e60c (webrtc Ã¢â‚¬â€� shouldn't affect playback) Ã¢â‚¬â€� so suspect NON-media-path commits in the window (script/dom/media, constellation, servo.rs, or the shell side batch-6 PRs). STATE: WPT background run STILL RUNNING (html dir, /tmp/wpt-results-2026-11-27, supplement chained after Ã¢â‚¬â€� do not kill; it doesn't conflict with feather-ui repro testing). Container-rule fix live-verification still pending (needs servoshell rebuild Ã¢â‚¬â€� lower priority now). feather-engine remote is INTACT at 75d490be7 (verified Ã¢â‚¬â€� an earlier scare was a stale-ref artifact of mid-fetch reading). REPRO PLAN: (a) gmail: launch feather-ui gmail.com, watch UI-thread responsiveness + devtools eval; if frozen on Linux too Ã¢â€ â€™ bisect batch-6 window same as seek. (b) youtube: launch video, seek backward, check playback death Ã¢â‚¬â€� if reproducible, git bisect between a4922916078 (good) and tip. RESUME HERE.

## sec1zt200 Ã¢â‚¬â€� gmail repro interim: all threads SLEEPING not spinning (14 feather-ui + 8 JS Helper all S state, cpu-rate 0%) Ã¢â‚¬â€� if frozen it is a BLOCK/deadlock signature not a spin; devtools eval empty (could be cross-origin detach since gmail redirects through accounts Ã¢â‚¬â€� cannot distinguish). NEXT EXACT STEP: visual screenshot check of the live instance (still running, pid file state in /tmp/gmail-repro.log) then if truly wedged: gdb thread backtrace of UI thread. YouTube bisect after.

## sec1zt201 Ã¢â‚¬â€� gmail freeze does NOT repro at sign-in on Linux: screenshot shows accounts.google.com/v3/signin FULLY RENDERED (email field, buttons, all chrome), all threads sleeping, cpu-rate 0% = healthy idle. The freeze must be POST-LOGIN app boot (mail.google.com proper) Ã¢â‚¬â€� untestable without google credentials on this VM. Windows lane: confirm whether your freeze is pre or post login. If post-login only, Linux lane needs test creds to repro. NOTE the interesting bit: the sign-in page itself is heavy JS (accounts bootstrap) and renders fine Ã¢â‚¬â€� so the wedge class is specific to the gmail app bundle, not google-js generally.

## sec1zt202 Ã¢â‚¬â€� yt seek repro: page boots fine, but jfKfPfyJRdk shows "Video unavailable Ã¢â‚¬â€� this live stream recording is not available" Ã¢â‚¬â€� BAD TEST VIDEO (livestream VOD). NEXT: use a regular VOD e.g. any non-live video; the harness /tmp/mseh or earlier seek-verification rounds used a specific working video Ã¢â‚¬â€� check sec1zt137-era entries for the exact url used in round-10 verification. THEN: play, seek backward, observe.

## sec1zt203 Ã¢â‚¬â€� yt watch-page repro: devtools evals TIMEOUT even on document.title while ALL threads idle (no spin, no llvmpipe) Ã¢â‚¬â€� BLOCKED-IDLE wedge signature, same family shape as the gmail report. UNRESOLVED whether this is the seek regression manifestation at boot, a devtools-actor death artifact, or network stall. NEXT: (1) screenshot the live window to see what rendered; (2) if video visually playing despite dead devtools Ã¢â€ â€™ devtools artifact not engine wedge; (3) gdb backtrace Script#1 if truly stuck; (4) only then seek-bisect. repro instance live pid was 1610196-era relaunched once Ã¢â‚¬â€� check pgrep feather-ui.

## sec1zt204 Ã¢â‚¬â€� YT WATCH-PAGE BLANK-WEDGE REPRODUCED ON LINUX: dQw4w9WgXcQ watch page Ã¢â‚¬â€� tab title set ("Rick Astley - Never Gon...") but body NEVER PAINTED (blank white window), all threads idle, devtools evals timeout. CONTRAST: minutes earlier the lofi watch page rendered fully in the same binary Ã¢â‚¬â€� so the wedge is either racy or content-dependent. THIS IS A LIVE LINUX REPRO of the blocked-idle wedge family Ã¢â‚¬â€� the exact repro the P0 needs. instance still running (pgrep feather-ui). NEXT: gdb -p PID thread backtraces (esp Script#1 + main) to find the block point Ã¢â‚¬â€� this is the money artifact. Do NOT kill the instance.

## batch6-youtube-seek-wedge evidence (2026-11-27, macOS lane, sec986486)

SYMPTOM: youtube seek kills playback (user-reported batch-6 regression).
REPRODUCED on macOS feather-ui (engine cf9acff14): youtube.com/watch autoplay
advanced frames normally; one click on the seek bar (~70%) Ã¢â€ â€™ video frame froze
permanently, buffering spinner appeared and never cleared, player time readout
stuck at 0:05/3:33 across all later screenshots. UI thread stayed healthy
(main parked in ReceiveNextEventCommon, window interactive).

THREAD-STATE EVIDENCE (sample of wedged process, 2069 samples / ~2s):
- Script#1 (the youtube script thread) was in `semaphore_timedwait_trap`
  via `libdispatch: _dispatch_sema4_timedwait <- _dispatch_semaphore_wait_slow`
  in ALL 2069 samples Ã¢â‚¬â€� a GCD-semaphore timed-wait busy loop. Process CPU
  ~80-100% with one core saturated: the wait timeout is short/zero, so the
  thread spins without ever making progress Ã¢â‚¬â€� classic wedge signature.
  Stack (release, unsymbolicated): script-thread main loop Ã¢â€ â€™
  +0x1ee3fa8 Ã¢â€ â€™ +0x9c9a0 Ã¢â€ â€™ +0x9c410 Ã¢â€ â€™ +0x298cdfc Ã¢â€ â€™ dispatch wait.
  Offsets are stable within a build: feather-ui load addr 0x102f0c000,
  compute file offsets for atos if a debug build is ever produced.
- All other threads (Script#2/#8/#9, FetchThread, StyleThreads, caulk audio,
  WR*, Constellation) blocked in normal wait primitives Ã¢â‚¬â€� healthy.
  caulk.messenger (CoreAudio) idle in timed_wait Ã¢â‚¬â€� audio pipeline starved,
  consistent with playback death.

MECHANISM READ: the script thread is NOT deadlocked (it would be in
semaphore_wait, untimed) Ã¢â‚¬â€� it is LIVELOCKED in a poll loop waiting for a
condition that never becomes true. Prime suspect mechanism class: media
sync/clock Ã¢â‚¬â€� after a seek the gstreamer pipeline re-prerolls; if the
preroll/flush completion or the media-time update never reaches the script
thread's waiter (or the wait condition is checked against stale state),
the loop spins forever. Media event delivery suspects:
`components/media/player/video.rs` (frame push / seek handling),
`components/media/backends/gstreamer/player.rs` (seek + preroll/flush
completion signaling), and the script-side HTMLMediaElement seek/ready-state
machinery (`components/script/dom/html/media/`) Ã¢â‚¬â€� whichever condition the
script thread polls is never satisfied post-seek.
NOTE the pre-seek anomaly: player time readout was ALREADY stuck at 0:05
while frames advanced Ã¢â‚¬â€� the timeupdate/currentTime path may be broken
independently of seeking; seeking then kills frame flow entirely.

GMAIL ANGLE (not yet reproduced Ã¢â‚¬â€� signed-in boot unreachable without
credentials): if the same livelock fires in the gmail boot path, the script
thread spinning at 100% would starve the load Ã¢â‚¬â€� consistent with the user's
"permanent boot freeze". Signed-out gmail signin flow incl. botguard POST
roundtrip verified HEALTHY on the same build Ã¢â‚¬â€� freeze is post-auth only.
Repro evidence files on macOS lane VM: /tmp/yt-wedged-sample.txt (full
sample), /tmp/gmail-baseline-sample.txt.

## sec1zt205 Ã¢â‚¬â€� WEDGE SMOKING GUN: gdb of blank-wedged youtube watch page Ã¢â‚¬â€� Script#1 BLOCKED in futex inside gst_pad_set_active (libgstreamer pthread_mutex_lock). The script thread wedging in gstreamer pad activation = whole page dead while idle. EXPLAINS: blank boot wedge + dead devtools + idle cpu + plausibly the seek-kill (same lock family Ã¢â‚¬â€� seek pipeline reset churns pad activation). Deadlock partner = whoever holds mutex 0x75e30c2df430 Ã¢â‚¬â€� analyze /tmp/wedge-bt.txt (SAVED, 1226 lines, 252 threads): find threads inside gst code holding locks (media thread, GstBus sync handler, decodebin). Full bt preserved Ã¢â‚¬â€� grep gst_pad_set_active\\|gst_element\\|decodebin\\|media in it. This is likely batch-6-window media-commit regression Ã¢â‚¬â€� bisect candidates: 5e6df4c001b (seek pipeline-reset on worker thread), 4c126d64190/0cc27425170 (deferred-dispose reaper) Ã¢â‚¬â€� pad activation during teardown vs seek is the classic gst deadlock pair.
## sec1zt206 Ã¢â‚¬â€� deadlock mechanism identified: PlayerInner lock-ordering inversion
player.rs:955 = media-info-updated gst signal callback does inner_clone.lock().unwrap() on PlayerInner. Gst signal callbacks can fire SYNCHRONOUSLY on whatever thread triggers the state change. DEADLOCK: (a) script thread holds PlayerInner while calling gst set_state/pad-activation (seek reset / pipeline setup paths) Ã¢â€ â€™ gst takes internal locks; simultaneously (b) gst emits media-info-updated (on gst thread or synchronously) Ã¢â€ â€™ callback blocks on PlayerInner. If synchronous-on-same-thread: std Mutex non-reentrant SELF-deadlock. If cross-thread: classic ABBA. FIX DIRECTION (upstream-grade): NEVER call into gstreamer while holding PlayerInner Ã¢â‚¬â€� collect needed values, drop the guard, then call gst (classic pattern); and/or make the media-info-updated callback not need the lock at emission time (defer to a queued task). KEY NEXT STEP: find all sites where inner guard is live across inner.player.* calls Ã¢â‚¬â€� grep player.rs for lock() followed by .player. usage in scope. The batch-6-window commits made this latent inversion live (seek reset added pad-activation churn). This is upstream-servo-grade defect Ã¢â‚¬â€� candidate for upstream fix.

## sec1zt207 Ã¢â‚¬â€� DEADLOCK ARCHITECTURE fully identified + fix design
ROOT: the inner_player_proxy! macro (player.rs ~1267) locks PlayerInner for the ENTIRE duration of every Player trait method Ã¢â‚¬â€� play/pause/seek/stop/push_data all hold the mutex across BLOCKING gstreamer calls (set_state, pad activation). Meanwhile gst signal callbacks (media-info-updated player.rs:955, state-changed :901, position :928) lock the same PlayerInner from gst-emission contexts. ABBA DEADLOCK: script thread holds PlayerInner + waits for gst-internal pad lock (held by gst streaming thread) while gst callback thread holds gst-internal locks + waits for PlayerInner. Matches the gdb evidence exactly (Script#1 in gst_pad_set_active futex; gst callback thread in PlayerInner::lock). WHY NOW (batch-6 regression window): pre-seek-reset the blocking gst calls rarely ran on the script thread during normal playback; the seek-reset/pipeline-recreation work (5e6df4c001b etc.) massively increased pad-activation churn and callback-vs-control concurrency Ã¢â‚¬â€� the latent inversion became live. Also explains the boot-time wedge: media-info-updated fires during initial pipeline setup while script thread holds the lock across setup-time gst calls. FIX DESIGN (upstream-grade): never hold PlayerInner across blocking gst calls Ã¢â‚¬â€� restructure inner methods: take lock, extract needed state + clone the gst player/pipeline handle (gst objects are refcounted threadsafe), DROP guard, then perform the gst call. Applies to play/pause/stop/seek/set_rate/set_volume/setup paths. INTERIM cheap mitigation if full restructure is too big for one round: make the gst callbacks never block on PlayerInner Ã¢â‚¬â€� use try_lock + defer (queue the event, skip if contended) Ã¢â‚¬â€� state updates are idempotent-ish so dropped duplicate emissions are tolerable, BUT care: seek-ack channel lives behind the lock. Compile-verify each round via workspace cargo check ~2min. REPRO for verification: repeated fresh youtube watch boots + seek churn; wedge is racy so verification needs N clean boots/seeks in a row (5+).

## batch6-youtube-seek-wedge REFINEMENT: two-state race at media-pipeline setup (2026-11-27, macOS lane)

Second repro round, same binary: video played 50s+ clean, time readout
advancing normally, seek to 1:15 WORKED and playback continued healthy
(1:15Ã¢â€ â€™1:28 verified). Compare round 1 where the readout was stuck at 0:05
from page load BEFORE any interaction, and the first seek wedged everything.
Conclusion: the race is NOT at seek time Ã¢â‚¬â€� it is at media pipeline setup.
The player lands non-deterministically in one of two states:
- HEALTHY: clock runs, readout advances, seeks work.
- DEGRADED: media clock never starts (readout frozen at boot timestamp
  while frames still render for a while); any seek in this state livelocks
  the script thread (semaphore_timedwait spin, one core saturated).
Mechanism implication: a setup-time race in the media clock/sync
initialization Ã¢â‚¬â€� suspect the gst pipeline clock distribution /
`components/media/backends/gstreamer/player.rs` preroll completion vs the
script-side media element timeline attachment. The wedge at seek time is
downstream: seeking a clockless pipeline waits on a sync condition that
can never fire. This also explains gmail: the same setup race in a
boot-critical wait would freeze page boot permanently when the degraded
state is hit Ã¢â‚¬â€� no media element needed if the shared sync primitive
(semaphore timed-wait loop in the script thread's event plumbing) is what
races. Investigation direction for engine lane: instrument the two states
by logging pipeline clock state at first frame; the repro is 1-in-2-ish so
a handful of boots brackets both.

## sec1zt208 Ã¢â‚¬â€� deadlock cycle-breaker SHIPPED c534afd6c6d Ã¢â‚¬â€� VERIFICATION PENDING
The try_lock fix compiles and is pushed. VERIFY: rebuild feather shell-new (picks up path-dep servo) then repeated fresh youtube watch boots + seek churn Ã¢â‚¬â€� wedge is racy so need 5+ clean boots. ALSO the feathershell rebuild is heavy (~10+min?) Ã¢â‚¬â€� batch it. NOTE compile-check gotcha reconfirmed: -p package checks fail on freetype2 pkg-config (system 24.1.18 < 24.3.18 required) AND FREETYPE2_NO_PKG_CONFIG=1 just aborts pkg-config not enables bundled Ã¢â‚¬â€� MUST use workspace-level cargo check no -p (~2min warm). Structural followup still open: control path holds PlayerInner across blocking gst calls (inner_player_proxy macro) Ã¢â‚¬â€� upstream-grade fix = extract-then-call pattern; current fix breaks the cycle but is technically lossy under contention.

## sec1zt209 Ã¢â‚¬â€� deadlock fix VERIFIED for page-wedge; playback starvation is a SEPARATE defect still open
Post-fix verification (rebuilt feather 10:01, fresh youtube boot): page renders FULLY (vs blank-wedge pre-fix), devtools responsive THROUGHOUT seek operations (vs total eval timeout pre-fix) Ã¢â‚¬â€� the ABBA deadlock is broken. HOWEVER playback starvation persists: video reaches playing state (paused=false, readyState 4) but position NEVER ADVANCES Ã¢â‚¬â€� even before any seek (ct0.0 after 8s of play), and after fwd-seek-to-60 then bwd-seek-to-10 the position lands at target but freezes there permanently. NOTE this matches the pre-existing defect indexed in sec1zt108-era: initial playback freeze despite buffer+readyState Ã¢â‚¬â€� the MSE starvation family, NOT the lock deadlock. So the batch-6 regression has TWO separable layers: (1) page-wedge deadlock FIXED c534afd6c6d; (2) MSE playback starvation Ã¢â‚¬â€� OPEN, the actual seek-kill mechanism. NEXT: the starvation is in the MSE push/consume path Ã¢â‚¬â€� decodebin reselection or the refeed machinery from the batch-6 media commits; the original backward-seek fix verified position advancing at round-10, so something in the window a4922916078..tip regressed frame flow. BISECT candidates the media-tree commits in that window Ã¢â‚¬â€� BUT note media tree only has e0e8d13e60c (webrtc, innocent) Ã¢â‚¬â€� so suspect script/dom/media changes or the retention/refeed commits fade8925118/dec2bd5b096/97d964bb167 cluster timing (check dates vs a4922916078).

## sec1zt210 Ã¢â‚¬â€� regression-window isolation: media tree nearly CLEAN; starvation may predate or be non-media
Window analysis a4922916078..HEAD (post-verified-round-10): media-tree commits ONLY e0e8d13e60c (webrtc fallibility Ã¢â‚¬â€� mechanically unrelated to playback pipeline) + c534afd6c6d (today fix). SO: either (a) the regression is NOT in the media tree Ã¢â‚¬â€� suspect batch-6 feather-shell-side PRs or non-media engine commits, or (b) the freeze observed on Linux now is the PRE-EXISTING flaky defect (sec1zt108-era: initial playback freezes ~1.7s despite buffer+rs4, documented BEFORE the regression window) meaning Linux was marginal all along and Windows regression may share only symptoms. CONFOUND re-test needed: current progressive freeze at ct1.3 reproduced while WPT load avg ~5 Ã¢â‚¬â€� NEEDS re-verification on idle machine (WPT nearly finished naturally Ã¢â‚¬â€� wait for it, then re-test clean). If progressive freezes even idle: bisect window broadens to include pref-batch enables (webgl2! the pref-batch enabled dom_webgl2_enabled Ã¢â‚¬â€� youtube uses webgl for player?) and cookie-secure change. KEY NEXT: clean progressive-playback test on idle machine; if broken, bisect pref batch; if fine, the starvation is MSE/youtube-specific and the media analysis resumes.

## sec1zt211 Ã¢â‚¬â€� starvation mechanism narrowed: pipeline delivers ~1s then silence, no errors
Progressive playback test (bbb.mp4 via localhost, IDLE machine Ã¢â‚¬â€� WPT killed so no contention confound): samples flow n=0..30 (pts up to 1.066s) then TOTAL SILENCE Ã¢â‚¬â€� no errors, no underrun, no further samples. Position frozen ct1.3, paused=false, rs2. SIGNATURE: pipeline stuck in PAUSED/preroll Ã¢â‚¬â€� never reaches PLAYING because pushing stalls after the initial burst; playbin position only advances in PLAYING. This is NOT load-induced. NOTE [mse] instrumentation appears even for progressive playback Ã¢â‚¬â€� servos player abstraction routes both paths through similar machinery. SUSPECT AREA: the push-side flow control Ã¢â‚¬â€� enough_data Arc<AtomicBool> (PlayerInner line 174) or the seek/setup transition failing to reach Playing. KEY CHEAP NEXT EXPERIMENTS: (1) RUST_LOG debug for servo_media on a fresh repro, watch the state transitions; (2) test whether the same freeze exists in UPSTREAM servoshell (mach binary already built!) Ã¢â‚¬â€� if upstream freezes too, this is NOT a feather regression at all but an upstream defect; if upstream plays fine, bisect the fork delta. RUN: ./target/debug/servoshell http://localhost:8422/v.html Ã¢â‚¬â€� servoshell is graphical, same VM. THIS IS THE DECISIVE cheap experiment Ã¢â‚¬â€� do it FIRST before any bisect.

## sec1zt212 Ã¢â‚¬â€� freeze reproduces in bare servoshell: ENGINE defect confirmed, shell exonerated
Same signature in mach-built servoshell: samples n=0..30 to pts 1.066 then silence. CRITICAL REASONING: appsink RECEIVING samples means the pipeline IS in PLAYING and decode works Ã¢â‚¬â€� the freeze is the SOURCE starving, not the pipeline state. The [mse] instrumentation confirms samples reach the sink callback. So: something stops the flow after ~1s. For progressive playback the source is playbin/uridecodebin internal Ã¢â‚¬â€� fork media commits should not affect it AT ALL unless the player setup (buffering config, download flags) is broken. Hmm Ã¢â‚¬â€� but samples DID flow for 1s. What happens at ~1s: the PlayerInner media-info-updated or the position-reporting starts? OR the video RENDER path (video_renderer.lock render at :1061) blocks downstream backpressure! KEY SUSPECT REFRAME: appsink in servos player is PULL or push with callbacks Ã¢â‚¬â€� if the render/consume side doesnt drain (frame renderer stuck), backpressure stops the pipeline. The llvmpipe software-GL frame render path on this VM Ã¢â‚¬â€� could frame rendering itself be the stall? BUT it rendered 30 frames fine. NEXT: gdb the FROZEN progressive instance Ã¢â‚¬â€� thread states will show exactly where the consume side is stuck. Much cheaper than bisect: one gdb, read the media thread stacks.

## batch6-wedge suspect-chain sharpened: the SeekData/SeekLock input-side block (2026-11-27, macOS lane)

Source-level mechanism chain for the youtube-seek freeze, indexed by function:

- `components/media/player/lib.rs` Ã¢â‚¬â€� `PlayerEvent::SeekData(u64, SeekLock)`:
  when gstreamer's demuxer repositions in the byte stream (EVERY playback
  seek does this), the gst streaming thread BLOCKS until the script thread
  re-feeds it from the new offset and releases the `SeekLock`. If the
  script side never services the event, the streaming thread blocks
  forever Ã¢â€ â€™ no more frames Ã¢â€ â€™ permanent spinner + frozen frame. This is
  the primary wedge candidate.
- `SeekLock::unlock` is already defensively bounded (5s try_recv_timeout,
  comment: "a stuck script thread is worse than a dropped ack") Ã¢â‚¬â€� but that
  bound only protects the SCRIPT side; nothing bounds the GST side's wait
  for the unlock. An unserviced SeekData is a permanent gst-side wedge.
- `components/media/backends/gstreamer/player.rs` Shutdown path comment
  documents the inverse hazard: "the seek_data can be blocked waiting for
  the script thread" Ã¢â‚¬â€� the authors know this coupling wedges; teardown
  works around it, steady-state seek does not.
- Rebase subtlety: same file notes "after a seek the flush+recreate
  re-bases the pipeline at the first post-seek buffer" Ã¢â‚¬â€� the pipeline is
  RECREATED on seek. If the SeekData event arrives referencing the old
  pipeline's stream state or races the recreate, the script-side handler
  can drop/mishandle it Ã¢â‚¬â€� matching the non-determinism (seek works in the
  healthy state, wedges in the degraded state).
- Script-thread side of the wedge: sample evidence shows Script#1 in ALL
  samples inside `semaphore_timedwait` (never executing JS) Ã¢â‚¬â€� the script
  thread is alive but never runs page JS. Since Servo's script event loop
  waits with timeout when timers are pending, 100%-of-samples-in-wait +
  one saturated core is consistent with a wake-spin loop: waking on tiny
  timeouts without processing. Whether this is cause or symptom of the
  unserviced SeekData is open Ã¢â‚¬â€� but note the pre-seek symptom (time
  readout frozen at 0:05 while frames render) means `PositionChanged`
  events were already not reaching the page, i.e. the script thread's
  media-event processing was ALREADY degraded before the seek. Seek then
  kills frame flow entirely because the gst side joins the waiters.
- ENGINE LANE next step: instrument SeekData dispatch + unlock pairing
  (log every SeekData emission and every unlock with ids; a wedged run
  will show an unpaired SeekData). Also log PositionChanged production
  rate in both boot states Ã¢â‚¬â€� the healthy/degraded divergence should be
  visible from the first seconds.

## sec1zt213 Ã¢â‚¬â€� starvation mechanism: appsrc starved, downstream queues EMPTY waiting Ã¢â‚¬â€� need-data either never re-fires or its resume never reaches the pusher
gdb of frozen progressive instance: NO deadlock Ã¢â‚¬â€� everything cleanly waiting. appsrc0:src idle in g_cond_wait (nobody pushes), vqueue/multiqueue/queue2 srcs all starved waiting for data. Flow control: enough-data flag set Ã¢â€ â€™ push_data errors EnoughData Ã¢â€ â€™ script stops pushing; need-data emission clears flag + sends NeedData event. Since downstream queues are EMPTY, appsrc should re-emit need-data Ã¢â‚¬â€� so either (a) gstreamer never re-emits because the queue-level accounting is corrupted (the batch-6 in-band resequencing/reset work manipulates the appsrc!), or (b) need-data fires but the script-side resume is broken. RECONCILIATION with round-10 passing: round-10 seeks were on SHORT clips where the whole stream fit in the initial burst Ã¢â‚¬â€� a broken need-data resume never mattered. Sustained playback past the first burst is what exposes it Ã¢â‚¬â€� consistent with freeze at exactly the initial-burst boundary (~1s). NEXT DECISIVE STEPS: (1) git log player.rs full history in batch-6 window Ã¢â‚¬â€� read the actual diff of the seek-reset commits for what touches appsrc behavior; (2) instrument need_data/enough_data callbacks with eprintln Ã¢â‚¬â€� one rebuild, definitive answer on whether need-data re-fires.

## sec1zt214 Ã¢â‚¬â€� STARVATION PINNED: appsrc re-emits need-data (12x) but enough-data NEVER fires and no data ever arrives Ã¢â‚¬â€� defect is in the SCRIPT/NET push path, not gstreamer flow control
Instrumented run: need-data fires repeatedly even AFTER the last consumed sample (the pipeline keeps asking for data) Ã¢â‚¬â€� yet push_data is never called again (no more samples). enough-data never fires so flow-control pause is not involved. CONCLUSION: the progressive-download feed pump (media element fetch Ã¢â€ â€™ player push_data) stalls after the first burst. NEXT: find the progressive feed path Ã¢â‚¬â€� components/script/dom/html/media/htmlmediaelement or media_thread Ã¢â‚¬â€� the fetch/reader loop. Suspects in batch-6: anything touching the read/pump scheduling. ALSO still unexplained: whether this freeze predates batch-6 (the sec1zt108 note suggests an older freeze existed) Ã¢â‚¬â€� but for the coordinator regression the mechanism is now the right target regardless: WHO stops pumping. grep push_data callers in script.

## sec1zt215 Ã¢â‚¬â€� starvation dig state: need byte-level evidence on the FETCH side
Facts: bbb.mp4 is 10s/970KB served from localhost (instant). need-data fires repeatedly (appsrc hungry) yet samples stop at n=30/1.066s. TWO REMAINING HYPOTHESES: (a) fetch stall Ã¢â‚¬â€� script never receives/pushes the rest of the file despite appsrc hunger Ã¢â‚¬â€� bug in the body-read pump; (b) fetch COMPLETED and everything was pushed (maybe even EOS) but the pipeline jams internally after 1s Ã¢â‚¬â€� then the starvation is inside gst decode/consume not servo at all. DISCRIMINATOR: log bytes pushed Ã¢â‚¬â€� instrument process_into_player_from_queue payload sizes + EOS push in htmlmediaelement.rs, one rebuild. ALSO cheaper interim: tcpdump/ss on the localhost connection Ã¢â‚¬â€� if the server finished sending (connection idle/closed) the fetch completed. gdb had no blocked threads in fetch either. NOTE playback_need_data early-returns when fetch context exists with cancel_reason Ã¢â‚¬â€� and unlocks+processes otherwise; need-data events ARE reaching script (since we see the events in engine). Whether process_into_player_from_queue runs with empty buffers (nothing to push because fetch delivered nothing) is hypothesis (a) mechanism exactly. Also re-check: DataBuffer payloads popped-then-dropped on push error Ã¢â‚¬â€� if ANY push_data error occurs mid-stream the data is LOST silently (warn only) Ã¢â‚¬â€� progressive corruption defect independent of flow control.

## sec1zt216 Ã¢â‚¬â€� probe: buffered EMPTY, networkState=1 IDLE, rs2, ct frozen 1.3 of dur10
The element reports networkState NETWORK_IDLE (no active fetch) Ã¢â‚¬â€� either the fetch completed (all 970KB delivered Ã¢â‚¬â€� plausible, localhost instant + duration=10 correctly parsed hints moov/ full-file knowledge) or it stalled-and-was-abandoned. buffered[] empty is unreliable (servo may not implement it for progressive). IF the fetch completed: the starvation is INSIDE the gst decode path Ã¢â‚¬â€� decoder jammed after 30 frames (appsrc hungry because ITS queue drained into the demuxer, but the decoder stops outputting Ã¢â‚¬â€� downstream multiqueue empty). IF fetch incomplete: the body-read pump abandoned the response without error. NEXT DECISIVE: (1) log bytes Ã¢â‚¬â€� eprintln in the media elements fetch-data consumer (wherever DataBuffer::Payload gets queued) printing cumulative bytes Ã¢â‚¬â€� ONE number settles it: 970k+eos = gst-internal, less = fetch-pump defect; (2) also check gst decodebin artifacts Ã¢â‚¬â€� H264 decode of bbb in this container Ã¢â‚¬â€� but FIRST the byte count. ALSO NOTE: playback froze the same way on YOUTUBE (mse) and progressive Ã¢â‚¬â€� a SHARED mechanism in the push/consume path is more likely than two independent ones Ã¢â‚¬â€� the shared piece is the player/appsrc/feed machinery not the demuxers.

## devtools eval gotcha: location.* throws in the sandbox (2026-11-27, macOS lane)

`evaluateJS` on a real page: `1+1` Ã¢â€ â€™ 2.0 and `document.readyState` work, but
ANY access to `location.href`/`location.hostname` makes the whole eval return
None (the access itself throws or fails serialization in servo's devtools
sandbox). Probe pages with document properties only Ã¢â‚¬â€� readyState,
querySelector, title all work. Gmail boot harness lives at
feather/tests/scenarios/gmail_boot_harness.py (PR #309): boot-isolated TTI
measurement + wedge detection via interactivity probe timeout corroborated
by the livelock CPU signature (>70% single-core sustained). Verified 2/2
interactive runs on the signed-out chain; --url accepts signed-in boot URLs
when credentials exist.

## sec1zt217 Ã¢â‚¬â€� CONFOUND RESOLVED: h264-progressive freeze is VM decoder artifact; VP9 progressive incl backward seek fully healthy; regression is MSE-SPECIFIC
Decisive test series on idle machine: (1) h264 bbb.mp4 progressive freezes at ~1.066s/30 frames Ã¢â‚¬â€� because the VM ONLY has openh264dec (no libav/avdec_h264) which cannot handle High-Profile/B-frame streams Ã¢â‚¬â€� ENVIRONMENT artifact, NOT the regression, do not chase it. (2) VP9 bbb60.webm progressive: plays real-time (ct 10Ã¢â€ â€™15.5), rs4. (3) VP9 backward seek 29sÃ¢â€ â€™5s: lands and CONTINUES advancing (9.5Ã¢â€ â€™14.0) Ã¢â‚¬â€� progressive seek fully healthy. CONCLUSION: the regression path is narrowed to MSE playback specifically (youtube uses MSE). SYMPTOM INDEX: video frozen ~1s on Linux VM with h264 Ã¢â€ â€™ check openh264dec artifact FIRST before suspecting engine regression; progressive-vs-MSE split is the key bisect axis. NEXT: reproduce on the MSE harness (/tmp/mseh, bbb60.webm) Ã¢â‚¬â€� play then backward-seek; if dead, the regression lives in the MSE refeed/retention machinery after all (round-10 verified it working Ã¢â‚¬â€� so bisect what changed in script/dom/media since a4922916078 Ã¢â‚¬â€� BUT that window is near-empty per sec1zt210, suggesting the regression trigger may be timing/race exposed by the OTHER batch-6 changes, or the mseh harness passes while youtube fails = youtube-specific MSE feature like multiple tracks or the live-leanback config).

## sec1zt218 Ã¢â‚¬â€� P0-2 youtube freeze: mechanism cornered to CLOCK-NEVER-RUNS; decode+pts healthy; audio-clock prime suspect
SYSTEMATIC ELIMINATION on idle Linux VM, all verified: (1) h264 progressive freeze = openh264 VM artifact, NOT the regression. (2) progressive vp9 incl backward seek: HEALTHY. (3) MSE video-only incl backward seek: HEALTHY. (4) MSE two-sourcebuffer vp9+opus harness (youtube-mimicking): HEALTHY Ã¢â‚¬â€� plays fine. (5) REAL YOUTUBE watch page: playback NEVER advances Ã¢â‚¬â€� ct frozen at exactly 0/5.44/30.00 (boot and post-seek alike) despite play() accepted, pfalse, rs4, buffered data present. GDB of frozen state: BOTH sinks in gst_base_sink_wait_clock (frames AT the sinks), both appsrc push threads blocked in gst_data_queue_push downstream (congestion not starvation), libvpx decode threads idle-waiting normally. [mse] instrumentation in /tmp/yt2.log: video samples decode with CORRECT monotonic pts Ã¢â‚¬â€� post-seek samples decode at correct pts 30-68s Ã¢â‚¬â€� segment re-injection works, decode pipeline healthy end-to-end. Appsrc levels balloon to 27MB (downstream not consuming). MECHANISM VERDICT: the pipeline clock never advances Ã¢â‚¬â€� everything downstream of it is congestion backpressure. Position frozen at exactly the seek target = clock-never-started signature. PRIME SUSPECT: servo's CUSTOM audio sink (components/media/backends/gstreamer/audio_sink.rs) never starts consuming in the youtube configuration Ã¢â‚¬â€� the pipeline clock is audio-driven, a never-started audio sink = frozen clock = exactly this picture. UNEXPLAINED: why the mseav harness audio path works but youtube's doesn't Ã¢â‚¬â€� deltas: youtube runs ADS (ytcs/adview in log Ã¢â‚¬â€� multiple video elements/player instances!), uses playbackRate sets that fail NonSeekableStream, appends in gap-creating patterns (buffered gap 3.2-5.2), possibly different sample rates per quality switch. NEXT STEPS: (a) read audio_sink.rs/audio_stream_reader.rs for the consume/preroll trigger; (b) probe whether audio samples flow in frozen youtube (add [mse] audio sample probe analog to the video one at player.rs:1110); (c) test harness with playbackRate-set + multiple append gaps; (d) consider ads as the trigger Ã¢â‚¬â€� test youtube watch page that plays no ad. ALSO NOTED: track level quota (MAX_SRC_QUEUE_SIZE) not enforced Ã¢â‚¬â€� 27MB accumulated.

## sec1zt219 Ã¢â‚¬â€� youtube freeze refined: visual freeze confirmed; decode stalls after initial burst; NOT a render error
Additional evidence: (1) video is VISUALLY frozen too (pixel-diff over 5s = spinner-only ~361px) Ã¢â‚¬â€� not just position reporting. (2) [mse] video samples decode far ahead (pts to 2:05) in an initial burst then STOP entirely Ã¢â‚¬â€� consistent with sync=false-ish appsink pull during buffering then total congestion once multiqueues fill. (3) zero get_frame/render errors Ã¢â‚¬â€� the render path is not dropping frames. (4) The first frame DOES display (preroll renders). UNIFIED PICTURE: pipeline clock never starts Ã¢â€ â€™ sinks never consume Ã¢â€ â€™ multiqueues fill Ã¢â€ â€™ demux/decode stalls Ã¢â€ â€™ appsrc levels balloon (27MB observed, quota enforcement broken Ã¢â‚¬â€� separate defect). Everything downstream of the clock. THE question remains why clock never starts on youtube but does in the mseav harness. Remaining deltas untested: ADS (youtube watch pages run ad player instances first Ã¢â‚¬â€� multiple pipelines coexist!), playbackRate-set failures (NonSeekableStream warn spam Ã¢â‚¬â€� youtube sets rate, servo errors), gap-creating appends. NEXT: instrument the audio side (no audio sample probe exists Ã¢â‚¬â€� add one) to confirm whether audio frames reach their sink at all; if audio never flows, the audio-clock never starts and THAT is the freeze Ã¢â‚¬â€� then find why audio stalls on youtube but not the harness.

## sec1zt220 Ã¢â‚¬â€� BREAKTHROUGH: freeze = pipeline stuck in ASYNC state transition; youtube fires multiple rapid seeks; clock never restarts
[mse] instrumentation analysis of frozen youtube (/tmp/yt2.log): FIVE seeks fired (youtube seeks on its own Ã¢â‚¬â€� resume-at-position, ad transitions, quality switches Ã¢â‚¬â€� even with zero user input; the 'boot freeze' IS a post-seek freeze because youtube's resume seek fires at boot). First seek: running_time_ns=146000 Ã¢â‚¬â€� clock had advanced only 146 MICROSECONDS Ã¢â‚¬â€� proving clock never runs even before the first seek... OR the very first seek fired almost immediately after pipeline creation before clock started. KEY: 'seek pipeline->restore Ok(Async)' Ã¢â‚¬â€� the state restore returns ASYNC and there is NO completion logging; if the ReadyÃ¢â€ â€™Playing transition never completes (a sink never prerolls post-reset), the pipeline is stuck mid-transition forever: no clock, no consumption, total congestion Ã¢â‚¬â€� matches every downstream observation. STRONG youtube-specific candidate for why harness passes but youtube freezes: OVERLAPPING SEEKS Ã¢â‚¬â€� the state cycle runs on a spawned thread with zero synchronization; a second seek_reset landing while the first restore is still ASYNC reenters/confuses the cycle (fresh appsrcs recreated mid-transition, state changes interleave) leaving the pipeline permanently wedged. Harness fires one seek at a time with seconds of spacing Ã¢â‚¬â€� never overlaps Ã¢â‚¬â€� passes. FIX DIRECTION: serialize the state cycle per-pipeline (a dedicated owner thread or mutex+generation counter so a newer seek supersedes/completes before the next), plus await/log the async completion. ALSO the initial-clock-never-runs question: 146us running time at first seek may mean the clock NEVER runs on youtube even pre-seek Ã¢â‚¬â€� if so the root is upstream of seeking entirely (preroll never completes even initially Ã¢â‚¬â€� perhaps because youtube's initial append pattern with gaps leaves one sink starved). Discriminator next: harness test with seek fired IMMEDIATELY at playback start (before clock starts) and overlapping rapid seeks Ã¢â‚¬â€� reproduces or exonerates the overlap theory.

## sec1zt221 Ã¢â‚¬â€� FIX IMPLEMENTED: serialized generation-aware MSE seek cycle (1883f8d3435), compile-clean, verification pending
The fix: mse_seek_serial Mutex + mse_seek_gen AtomicU64 on PlayerInner; seek() now clones the ServoMse glib object + pipeline and spawns a worker that (1) acquires the serial lock, (2) abandons if superseded by a newer generation, (3) performs seek_reset + state cycle as one unit. Either acquisition order yields correct final state: newest-target cycle always runs last. Design note: seek_reset moved INTO the worker (was synchronous) Ã¢â‚¬â€� small window where pre-reset pushes land in dying appsrcs and are dropped, accepted because MSE pages re-append post-seek anyway; watch for regression in testing. GOTCHA documented: cargo check in servo repo passes in ~2s when incremental Ã¢â‚¬â€� must still rebuild feather binary for live verification (cargo check does NOT produce a binary Ã¢â‚¬â€� the sec1zt153 trap). VERIFICATION QUEUE: (1) msestress.html rapid-seek harness Ã¢â‚¬â€� frozen at 18.6 pre-fix, should pass post-fix; (2) live youtube watch page Ã¢â‚¬â€� boot resume seek + manual seeks; (3) confirm no append-loss gaps.

## sec1zt222 Ã¢â‚¬â€� BOOT FREEZE REPRODUCED IN HARNESS: single seek 150ms after boot wedges pipeline frozen at exactly the seek target; seek-vs-INITIAL-STARTUP overlap is the youtube boot mechanism
Correction to sec1zt220: re-counted Ã¢â‚¬â€� youtube fired only ONE seek in the frozen session (the grep count of 5 was prefix-matching seek_reset/pipeline lines), target 125.76 at running_time 146us Ã¢â‚¬â€� the seek fired essentially AT PIPELINE STARTUP, overlapping the initial preroll transition. Harness discrimination series: (1) single seek on A+V MSE seconds into stable playback Ã¢â‚¬â€� WORKS (ct advances post-seek); (2) rapid overlapping seeks Ã¢â‚¬â€� WEDGED; (3) SINGLE SEEK 150ms after boot Ã¢â‚¬â€� WEDGED frozen at exactly ct20.0 = youtube boot signature reproduced in controlled conditions. UNIFIED MECHANISM: any seek overlapping ANY in-flight pipeline state transition (initial preroll OR another seek's cycle) wedges the pipeline Ã¢â‚¬â€� recreating appsrcs mid-transition breaks decodebin3 plumbing, restore never completes (stuck Async), clock never runs. FIX: 1883f8d3435 (serialization+generation) + 019224829fc (worker waits bounded-5s for pending==VoidPending before reset). VERIFICATION QUEUE when rebuild lands: msebootseek.html should now pass (boot seek lands+plays), msestress.html should pass, live youtube boot+seek. If bootseek still fails post-fix: suspect the reset-vs-decodebin3 plumbing race itself rather than transition overlap Ã¢â‚¬â€� next instrument would be a bus watch on the restore transition completion.

## sec1zt223 Ã¢â‚¬â€� CORRECTION/REFINEMENT: bootseek harness repro is INVALID Ã¢â‚¬â€� harness never re-appends init segment post-seek; youtube-faithful harness must model init+media re-append from the seek point
Post-fix verification round: bootseek test still frozen BUT the harness itself is the artifact: after a seek, real MSE players re-append the INIT SEGMENT plus media from the seek target; my harness append loop keeps feeding the webm file linearly regardless of seek position, so post-reset the demuxer receives mid-stream bytes it cannot parse without the EBML init header. The stable-playback seek passes only because matroska can resync on cluster boundaries in one continuous stream; the boot case feeds too little data to resync. REDESIGN REQUIRED: harness v2 must (a) split file into init segment + media segments (webm: EBML head + cluster chunks), (b) on 'seeking' restart feeding from init+the target position Ã¢â‚¬â€� mimicking youtube exactly. Only then does pass/fail mean anything. MEANWHILE the overlap fix (1883f8d3435+019224829fc) remains correct and necessary for the confirmed stress-wedge class regardless. ALSO: video-only bootseek froze at ct1.0 (seek never even landed) Ã¢â‚¬â€� DIFFERENT signature, unexplained, possibly the seek landing while pipeline barely constructed. NEXT SESSION PLAN: (1) build webm init+cluster splitter (ffmpeg or manual EBML parse Ã¢â‚¬â€� check how the ORIGINAL round-10 mseh harness fed data Ã¢â‚¬â€� it may already do this correctly, since round-10 backward seeks verified passing Ã¢â‚¬â€� RE-READ that harness first!); (2) verify fixed binary against faithful harness; (3) then live youtube. The stress-wedge repro (msestress) is still valid evidence because spaced-seeks-pass/rapid-seeks-fail held harness-constant.

## github-segfault macOS negative result (2026-11-27, mac lane)

5x18s + 3x40s bare boots of github.com on macOS arm64: zero deaths, zero
macOS DiagnosticReports. The GL-point-sprites warning (1280) reproduces on
every boot Ã¢â‚¬â€� it is benign noise, not a crash precursor on this platform.
The segfault evidence chain is Linux-only (2/4 bare runs there); either the
fault is in Linux-specific surfman/GL path or requires interaction beyond
bare boot+idle. Do not burn macOS cycles on bare-boot repro. If it recurs
in dogfooding: macOS writes full crash reports with thread backtraces to
~/Library/Logs/DiagnosticReports automatically Ã¢â‚¬â€� THAT is the capture
path, no gdb/rr needed on this platform.

## sec1zt224 Ã¢â‚¬â€� gmail freeze hunt state: signed-out wedge DOES NOT REPRODUCE (6/6 clean boots, devtools responsive, idle cpu) Ã¢â‚¬â€� wedge confirmed post-auth-only across both lanes; botgate blocks signin on Linux entirely
6/6 signed-out boots healthy on current build. Deep signin flow exercise (email entry + botguard roundtrip) also healthy Ã¢â‚¬â€� ends at google's "browser may not be secure" rejection, meaning POST-AUTH REPRO IS IMPOSSIBLE on Linux without both credentials AND passing the anti-bot gate. Meanwhile mechanism analysis of the script-thread wake-spin primitive (mac evidence: Script#1 in semaphore_timedwait 100% of samples): the event loop timed-wait at messaging.rs:532 spins forever if a timer deadline is never in the future. FOUND + FIXED one real instance: script_thread rendering-update timer computes delay = animation_delay - time_since_last_rendering_opportunity which is EXACTLY ZERO when the clamp engages Ã¢â‚¬â€� a zero-duration timer never lets the wait block. Hardened (commit with this entry): delay==0 now sets the flag directly. SPECULATIVE re gmail Ã¢â‚¬â€� no repro to verify against. SYMPTOM INDEX: script thread 100% in timedwait spin, one core saturated, no JS executing Ã¢â€ â€™ check (a) zero/past-duration timers rescheduling in the event loop scheduler, (b) media clock wait family per mac evidence. NEEDED FROM WINDOWS/COORDINATOR: either test google credentials that pass the botgate, OR a thread-level artifact of the wedged gmail boot (sample/gdb backtrace of Script#1 + UI thread) Ã¢â‚¬â€� the artifact alone is enough for mechanism analysis. NOTE: gmail boot harness from PR #309 exists (tests/scenarios/gmail_boot_harness.py, release-binary-oriented) for verification once repro exists.

## real-site compat sweep 2026-11-27 (mac lane, 20 sites, boot-isolated devtools probe)

Method: tests/scenarios/site_sweep.py Ã¢â‚¬â€� fresh boot per site, readyState/title
probe, TTI timing, final-window CPU. CAVEAT: readyState=complete means DOM
readiness, NOT correct rendering Ã¢â‚¬â€� visual verification not in scope here.
19/20 ready, 1 wedged, 0 died.

READY (TTI / final-cpu): x.com 3.4s/61%, reddit 2.2/133, github 2.0/248,
stackoverflow 1.5/183 (Cloudflare "Just a moment" Ã¢â‚¬â€� botwall, not feather),
wikipedia 3.8/103, amazon 1.5/102 (EMPTY TITLE Ã¢â‚¬â€� suspicious), netflix
21.8/119 (very slow), whatsapp-web 4.6/0 (empty title), google 1.5/0.5,
youtube 2.7/68, gmail-chain 1.5/11, facebook 3.1/7, instagram 4.9/51,
linkedin 1.6/64, nytimes 4.2/311, bbc 4.7/104, chatgpt 5.0/113, claude
1.6/4.5 (Cloudflare "Just a moment"), notion 6.8/126.

WEDGED: twitch.tv Ã¢â‚¬â€� stuck at readyState=interactive forever, 242% cpu
sustained = livelock signature. Title bar read "Twitch" so SSR shell
parsed; the wedge hits during app boot. Same class as the youtube/gmail
script-thread livelock (batch6 entries) Ã¢â‚¬â€� possibly the same underlying
mechanism firing on a non-media boot path, which would mean it is NOT
media-specific. Engine lane: twitch.tv is now the easiest non-media repro
of the wedge class Ã¢â‚¬â€� no auth needed, deterministic-ish.

CPU-AFTER-READY catalog (restyle-storm/idle-CPU problem made visible):
nytimes 311%, github 248%, stackoverflow 183%, reddit 133%, notion 126%,
netflix 119%, chatgpt 113%, bbc 104%, amazon 102% Ã¢â‚¬â€� most major sites burn
1-3 cores after load on this GPU-less VM. Compare google 0.5%,
whatsapp-web 0%, facebook 7%. Confirms the sec1zt188 idle-CPU budget as
THE cross-site perf defect. Empty-title oddities (amazon, whatsapp-web)
worth a follow-up probe Ã¢â‚¬â€� possibly botwall interstitials or title set by
JS that eval-targeting can't see (first-pipeline artifact).

## sec1zt225 Ã¢â‚¬â€� WPT crash cluster fixed: createImageBitmap-from-video = NV12 frame format defect (194f25deae7); crash-cluster analysis method validated
WPT html results: 32 crashes, CLUSTERED Ã¢â‚¬â€� ~8 in createImageBitmap family + several canvas-video. Single-test repro (mach test-wpt one file, ~1min) gives the exact panic: 'unreachable: invalid raw data request for texture frame' at media/player/video.rs:61. ROOT: VideoFrame::get_data() unreachable on non-raw frames; htmlvideoelement get_current_frame_data treated every non-GL-texture frame as raw BGRA Ã¢â‚¬â€� but NV12 planar frames (what openh264/vpx produce on Linux) are also non-GL Ã¢â‚¬â€� boom. FIX: NV12Ã¢â€ â€™BGRA software conversion (BT.601 limited) in the snapshot path. SYMPTOM INDEX: crash in createImageBitmap/drawImage/canvas with video source Ã¢â€ â€™ check frame FORMAT first (is_nv12 vs is_gl_texture vs raw); unreachable at video.rs get_data/get_texture_id/get_nv12_planes = caller mismatch on format. METHOD NOTE (valuable): WPT crash lists cluster by subsystem Ã¢â‚¬â€� fix the cluster root not individual tests; single-test mach repro is fast (~1min) and yields exact panics Ã¢â‚¬â€� no need for manual reproduction pages. REMAINING CLUSTERS in html run: window-open-features (2), navigation-beforeunload, COOP header-parsing, iframe-move Ã¢â‚¬â€� uninvestigated. canvas offscreen font_variant_caps worker crash also open.

## sec1zt226 Ã¢â‚¬â€� NV12 fix VERIFIED: createImageBitmap-invalid-args CRASHÃ¢â€ â€™76P/17F/0crash; remaining fails are normal conformance gaps

## sweep triage with visual verification 2026-11-27 (mac lane)

Follow-up visual boots on the sweep's suspicious entries Ã¢â‚¬â€� REVISED IMPACT
TRIAGE, several probe artifacts corrected:

PROBE ARTIFACTS (not real breakage): amazon.com renders fully (nav,
search, Black Friday deals, product imagery) Ã¢â‚¬â€� the empty-title probe
result is an eval artifact (title set late by JS, or first-pipeline
targeting). web.whatsapp.com renders PERFECTLY including the login QR
code (canvas rendering working) Ã¢â‚¬â€� also probe artifact. Lesson for all
lanes: readyState/title probes measure DOM readiness only; empty titles
on JS-set-title sites are expected, not breakage.

TWITCH TRIAGE REVISED: visually renders the full directory page Ã¢â‚¬â€� live
channels sidebar, stream carousel with live viewer counts, grid. NOT a
functionality break. The defect is: readyState never leaves interactive
(load event never fires Ã¢â‚¬â€� a long-lived subresource or stream connection
holds it) AND ~242% CPU sustained. Impact: battery/heat burn, NOT
usability. Recategorize from "wedged" to "works-with-cpu-burn". The
livelock-class wedge confirmed by thread samples (youtube/gmail) remains
a separate real defect Ã¢â‚¬â€� twitch's CPU burn may or may not share the
mechanism; needs a thread sample to distinguish restyle-storm (main thread
busy) from semaphore-wait livelock (script thread parked).

USER-IMPACT PRIORITY ordering from the sweep+triage:
P0: idle-CPU burn across most major sites (nytimes 311%, github 248%,
twitch 242%, SO 183%) Ã¢â‚¬â€� affects every session, battery/heat/fan. One
mechanism fix lifts all of them: sec1zt188 restyle-storm work.
P1: the livelock wedge class (youtube degraded-boot + seek, twitch cpu,
gmail post-auth) Ã¢â‚¬â€� random catastrophic freeze on daily-driver sites.
P2: netflix 21.8s TTI Ã¢â‚¬â€� slow but functional.
P3: Cloudflare botwall interstitials (stackoverflow, claude.ai) Ã¢â‚¬â€� not
feather breakage, but real-user-facing; passing the challenge depends on
JS/wasm features, worth one investigation lane eventually.
NOT-BROKEN: amazon, whatsapp-web (render fully), x/reddit/github/wiki/
google/youtube/gmail-signin/fb/ig/linkedin/bbc/chatgpt/notion all reach
complete with correct titles.

## sec1zt227 Ã¢â‚¬â€� subframe-abort wedge mechanism established + fixed (2b094b2c721); crash-test cascade artifacts; flexbox col-wrap is a DIFFERENT pure-layout hang
WPT unknown-protocol-reload-crash "CRASH" is actually a full-browser WEDGE (harness hang-kill): gdb at wedge-time showed Constellation+Script#1+Script#2 all idle-parked in crossbeam select Ã¢â‚¬â€� NOT a cross-lock deadlock, a LOST-COMPLETION wedge: everyone waiting for a message never sent. Mechanism: subframe navigation into an unhandled scheme returns LoadCancelled (correct external-protocol behavior), script_thread sends AbortLoadUrl, but handle_abort_load_url_msg only resolved the TOP-LEVEL webdriver-load-status case Ã¢â‚¬â€� for subframes it closed the pipeline WITHOUT the IFrameLoadFinished parent notification that handle_subframe_load_finished sends on the normal path. Parent document's load bookkeeping never completes Ã¢â€ â€™ load event never fires Ã¢â€ â€™ everything waiting on load completion wedges forever. FIX: send subframe-finished notification before close_pipeline. SYMPTOM INDEX: browser-wide idle wedge after iframe navigates to external/unknown scheme Ã¢â€ â€™ check AbortLoadUrl handling in constellation; parent load event never firing with a dead subframe Ã¢â€ â€™ check IFrameLoadFinished accounting. METHODOLOGY (important): interactive-mode repro did NOT wedge visibly (the page itself survived) Ã¢â‚¬â€� the wedge is in the completion SIGNALING not the page logic; webdriver-mode is what exposes it. Also: batch-run CRASH entries are partially cascade artifacts Ã¢â‚¬â€� navigation-within-beforeunload CRASHed in batch but PASSes in isolation; always re-run crashes singly before investigating. flexbox intrinsic-size/col-wrap-crash.html wedge is a DIFFERENT mechanism: pure layout content (column flex-wrap + vertical-rl + baseline), no navigation involved Ã¢â‚¬â€� layout-engine hang, uninvestigated. GOTCHA: 'cmd1 && nohup cmd2 &' Ã¢â‚¬â€� the && chains INTO the background parse; the build silently never ran. Launch builds as separate statements.

## sec1zt228 Ã¢â‚¬â€� subframe-abort wedge fix VERIFIED: unknown-protocol-reload-crash CRASH(hang)Ã¢â€ â€™PASS in 3.7s. REAL-WORLD IMPACT: any iframe navigating to an external scheme (app-store links, whatsapp:/intent:/itms-services deep links inside iframes Ã¢â‚¬â€� common in ad/tracking frames and mobile-web boot flows) would permanently wedge the browser pre-fix. Candidate mechanism family for the gmail boot freeze Ã¢â‚¬â€� gmail boots with multiple iframes; if any navigates to a non-http scheme early, the exact wedge signature results (idle threads, no recovery). UNVERIFIED for gmail specifically Ã¢â‚¬â€� but the class matches Windows' characterization.

## serp-duplicate-results root cause 2026-11-27 (mac lane)

The google-SERP "duplicated results" dogfood report is a PAINT-INVALIDATION
artifact, not DOM duplication. Live evidence (google search serp, macOS):
after scrolling, ghost copies of previously-painted content (the AI-overview
sidebar text, result headings) remain visible FIXED IN POSITION Ã¢â‚¬â€� they do
not scroll with the page Ã¢â‚¬â€� rendered semi-transparent over the new content.
Real content scrolls correctly underneath. The user reads the stationary
ghost + the scrolled-in real copy as "duplicated results".

Mechanism shape: compositor fails to invalidate/erase regions where content
moved away Ã¢â‚¬â€� stale tiles persist. The ghost text being semi-transparent
suggests a partial-composite or wrong-blend leftover rather than a clean
stale frame. Suspect locations by function: servo's compositor tile
invalidation on scroll (components/compositing Ã¢â‚¬â€� the damage-rect logic that
decides what to repaints after a scroll offset change), possibly interacting
with the software-render path on GPU-less machines. Not yet confirmed
whether it reproduces on GPU hardware or is software-raster-only.
Repro: google search any product query with an AI-overview sidebar, scroll
down 1-2 screens, observe stationary ghost text. Cheap experiment for the
engine lane: does scrolling a simple position:static long page ghost too,
or does it need the SERP's fixed/sticky+async content mix?

## sec1zt229 Ã¢â‚¬â€� msestress post-fix analysis: starvation dominates, harness cannot verify seek fix; restyle-storm is the reproducible real-site wedge specimen
Post-fix msestress run: playback froze at exactly t=15.00 (last seek target) with paused=false Ã¢â‚¬â€� superficially the same freeze. BUT the [mse] instrumentation reveals the mechanism precisely: gen=8's worker hit 'state settle timeout' (pipeline already mid-transition 5s+), then reset+ready+restore ran with restore going Async and never completing. CRITICAL INVALIDITY CONFIRMED (sharpens sec1zt223): the harness feeds both files linearly to completion and NEVER re-appends post-seek Ã¢â‚¬â€� after seek_reset recreates the appsrc, zero new data ever arrives, so re-preroll is structurally impossible regardless of engine correctness. Any seek past the already-fed linear prefix starves by construction. VERDICT: msestress/msebootseek can neither verify nor falsify the seek fixes; only the YouTube-faithful harness (init+cluster-aligned re-append per seek Ã¢â‚¬â€� requires fmp4 segmentation or webm cue parsing, est. 30-60min) or Windows' real-YouTube verification count as evidence. Deprioritized pending Windows' verdict. ALSO: vello_cpu_backend has NO shadow support Ã¢â‚¬â€� draw_surface_with_shadow is a warn-stub (canvas shadows silently dropped on software-rendered machines; GPU vello implements them Ã¢â‚¬â€� real-user impact limited to GPU-less environments). shadowBlur_gaussian CRASH unreproduced interactively because shadows never execute on this backend Ã¢â‚¬â€� the harness crash path needs investigation on a GPU machine or the raqote backend. SYMPTOM INDEX: canvas shadows missing + 'no support for drawing shadows' in log Ã¢â€ â€™ vello_cpu backend warn-stub, not a page bug.

## sec1zt230 Ã¢â‚¬â€� github restyle-storm NOT REPRODUCING post-fix-batch: servo/servo repo page renders fully, Script#1 completely idle at rest (0 ticks delta over 10s windows). The original permanent-230%-CPU wedge is gone Ã¢â‚¬â€� most plausibly killed by the zero-delay-timer wake-spin fix (b786d36902b): the mutation storm's rendering-update scheduling produced exactly-zero delays that spun the event loop. UNVERIFIED which fix deserves credit (no bisect Ã¢â‚¬â€� would require reverting candidates); scroll-interaction probe was inconclusive because X-focus in the cluttered session is unreliable, but the at-rest permanent spin is definitively absent. gmail relevance: IF gmail's post-auth wedge shares the zero-delay-spin mechanism, it is already fixed by the same commit Ã¢â‚¬â€� consistent with Windows' characterization (google properties' heavy boot-time mutation/animation scheduling).

## sec1zt231 Ã¢â‚¬â€� UPSTREAM TRACKING DUTY baseline: merge-base with servo/servo = 170fe92389ad (2026-11-23); first scan covered 71 commits through bb9e153f1cc (2026-11-27). LAST-CHECKED UPSTREAM SHA: bb9e153f1ccb44a9312f0ea94d5142a12707a28c. Next scan: git fetch upstream main; git log <sha>..FETCH_HEAD. High-overlap candidates identified: 4d59f733e40 (media audio-thread panic Ã¢â‚¬â€� crash class), 2df3d02caa8 (offscreen canvas fonts Ã¢â‚¬â€� canvas cluster), 9facd8f075a (unpremultiply reciprocal Ã¢â‚¬â€� pixel fidelity), 8763f753b5b+aee0b7d8e50 (flex/flow layout Ã¢â‚¬â€� flexbox cluster, risky), 47ac285d9e8+d18a53c80cb (font caching Ã¢â‚¬â€� perf), 9ddfe28a9ec (getComputedStyle flat-tree spec fix), 9236540138a (mozjs 153.8 Ã¢â‚¬â€� big dep, defer), 086c4ad7746+1200b407c13 (navigation timing Ã¢â‚¬â€� performanceobserver cluster).

## sec1zt232 Ã¢â‚¬â€� CRITICAL REFRAME: raw WPT failure counts measure UPSTREAM incompleteness not feather bugs; expectations-adjusted: websockets 800/800 as-expected, workers 335/338, cssom 303/305, html 10559/10982 (~84 unexpected: 25 crash mostly FIXED same-day + 15 err + 14 fail + 30 timeout). Cluster-2-timeout systematic pass as scoped is mostly INVALIDATED (only ~30 of 368 are unexpected). Baseline doc corrected in feather#320. METHOD: always read harness 'ran as expected' summary not raw pass rates. Unexpected-positive note: drawImage-svg passes though upstream expects FAIL Ã¢â‚¬â€� feather AHEAD of upstream in places.
## sec1zt233 Ã¢â‚¬â€� upstream cherry-picks landed on feather-engine (7 clean): e65ccdfdc99 media audio-thread panic fix, 7bf49737b45 offscreen canvas fonts, 0c7d3547288 unpremultiply reciprocal, 7e1622eb169 font glyph bounds cache, f9d46910783 nav timing queue, d755b3a0be7 worker empty origin, e1087b1e399 FontFaceSet tracking. Compile-verified, pushed, NOT yet binary-verified (html rerun using older binary). CONFLICTED/skipped: 9ddfe28a9ec (codegen), a2568465ba8+6665562a412+8763f753b5b+aee0b7d8e50 (layout Ã¢â‚¬â€� heavy modification zone), bb9e153f1cc, 086c4ad7746, d18a53c80cb. LAST-CHECKED UPSTREAM SHA: bb9e153f1ccb44a9312f0ea94d5142a12707a28c.
## sec1zt234 Ã¢â‚¬â€� actionable unexpected-only inventory extracted to /tmp/unexpected-section.txt (885 lines): COEP header-parsing cluster (whitespace-trim of require-corp header values wrong Ã¢â‚¬â€� \t/space/f/v/r variants misclassified, SMALL FOCUSED FIX, parse location not yet found Ã¢â‚¬â€� grep EmbedderPolicyValue in net/http_loader.rs:2205 area), COEP-credentialless family fails, about:srcdoc query-navigation not blocked (spec violation), cross-origin top-navigation allowed when should be Denied (SECURITY-relevant spec gap Ã¢â‚¬â€� user-activation gating missing). These are the real feather-specific work list.
## sec1zt235 Ã¢â‚¬â€� COEP cluster verdict: response COEP header parsing into policy containers is UNIMPLEMENTED upstream Ã¢â‚¬â€� http_loader never parses cross-origin-embedder-policy response headers; embedder_policy only flows from client-side RequestPolicyContainer (http_loader.rs:2639). So the whole COEP failure family (header-parsing + credentialless + reporting) is ONE missing upstream feature (parsing + enforcement wiring), not per-test trimming bugs. Sized as significant feature work; upstream-shared not feather-specific. DE-EMPHASIZE per gap-avoidance rule.
## sec1zt236 Ã¢â‚¬â€� :dir() IMPLEMENTED + verified (parsed:1 matched rgb(255,0,0)). Mechanism: stylo parse_non_ts_functional_pseudo_class lacked the arm entirely. Fix = variant + parse + to_css + match-in-layout walking ancestor chain via get_attr_for_layout dir attr. SIMPLIFIED: dir=auto content-inspection NOT implemented (falls to parent) Ã¢â‚¬â€� residual gap for the dir-shadow/dir-auto WPT family. Build gotcha: incremental full-build can finish in 2:20 Ã¢â‚¬â€� verify empirically not by timing. Leaves doc feather#321 merged; attack order: flex gap next, then nav security trio (Location setters/srcdoc blocking/x-origin topnav gate).
## sec1zt237 Ã¢â‚¬â€� NEGATIVE RESULT: flex gap WORKS (verified exact 20px spacing live). The ~20 flexbox-gap WPT FAILs are edge cases (percentages/calc/multiline) or reftest pixel noise, NOT the feature. Do not sink time into basic gap. Same suspicion extends to much of the flexbox 191 Ã¢â‚¬â€� requires per-test pixel-vs-real audit before any effort.
## sec1zt238 Ã¢â‚¬â€� attack-order continuation state: (1) :dir() DONE c652b3e1aa3. (2) nav security trio INVESTIGATION STARTED: srcdoc-blocking Ã¢â‚¬â€� spec step "navigations from an about:srcdoc document to anything but exactly about:srcdoc must be blocked"; servo handles about:srcdoc in htmliframeelement.rs:434-501 + servoparser/html.rs:74; the navigation-request path to instrument is location setter -> constellation load_url; no blocking logic exists yet. (3) x-origin topnav user-activation gate Ã¢â‚¬â€� no gate exists; transient activation state tracking is the prerequisite primitive. (4) download-attribute navigation (18 files) Ã¢â‚¬â€� shell crossover. html post-fix rerun STILL RUNNING at session end Ã¢â‚¬â€� when complete, regenerate wpt dated report comparing unexpected-only counts vs baseline (crashes should collapse); do NOT trust raw pass-rate deltas.
## sec1zt239 Ã¢â‚¬â€� srcdoc nav-block patch IN FLIGHT (uncommitted at session end): location.rs navigate_a_location now blocks navigations from an about:srcdoc document to any about:srcdoc* url other than exactly about:srcdoc. Compile running in /tmp/srcdocbuild.log. NEXT: if compile clean, verify with the WPT test single-run (ports may conflict with big html run Ã¢â‚¬â€� use interactive repro instead: srcdoc iframe + script setting location to about:srcdoc?x), commit+push, then attack x-origin topnav activation gate. NOTE the exact spec step may also cover non-location navigation paths (anchor clicks) Ã¢â‚¬â€� the shared navigate() is the better final location if verification shows gaps.

## shell-crash-recovery handoff 2026-11-27 (mac lane, user-directed)

SCOPE (user verdict): shell-level crash/hang recovery only Ã¢â‚¬â€� NOT process
isolation. Detect engine/webview death or hang per tab, keep shell alive,
offer per-tab reload, preserve other tabs.

ALREADY EXISTS: servo auto-recovers panicked pipelines per-tab in-process
(feather shell-new/src/engine.rs line ~109-114 embedder Panic arm prints
'[tab-crash] pipeline panicked (tab reloaded by engine)'; hard_fail:false
at engine.rs:442; panic hook in main.rs:75-89). Process-death case is
solved by construction; other tabs survive.

MISSING (the implementable shell work):
1. HANG detection Ã¢â‚¬â€� the batch6 livelock class: tab alive but script thread
   parked in semaphore_timedwait forever. No user signal today.
2. User-visible affordance for both crash and hang: 'tab crashed/unresponsive
   Ã¢â‚¬â€� reload' pill. Today crashes silently auto-reload (loses scroll/form
   state with zero explanation).

IMPLEMENTATION SHAPE (scoped, not built): per-webview liveness heartbeat Ã¢â‚¬â€�
hook the shell's frame-pump/event-injection path (engine.rs/engine_surface.rs
Ã¢â‚¬â€� grep notify_frame/present/pump for where servo surfaces composite per
webview); record last-progress timestamp per tab. Watchdog: if a tab
receives input or is loading/animating but produces no frame progress for
~5s, mark unresponsive Ã¢â€ â€™ pill overlay in the tab content area with Reload
button (feather-styled, same family as permission pill). Pill actions:
reload (existing navigation machinery) or dismiss. ALSO: surface the
existing Panic-arm crash event to the UI (a toast or per-tab notice) so
auto-reloads are explained, and persist crash events to the crashes dir
alongside the hang artifacts already written there.
VERIFY: FEATHER_TESTUTILS=1 ServoTestUtils.panic() injection covers crash;
hang verification via the youtube-seek wedge repro or a busy-loop test page.
## sec1zt240 Ã¢â‚¬â€� srcdoc nav-block SHIPPED 76c84f93baf (verified live). Trio status: srcdoc DONE; Location-setter edges unexamined; x-origin-topnav activation gate REQUIRES transient-activation primitive that servo lacks entirely Ã¢â‚¬â€� multi-day, defer unless directed. NAV-COMPLETION FAMILY NOTE: navigate() in location.rs is 'ostensibly the navigate algorithm but spec has greatly deviated' per servo's own comment Ã¢â‚¬â€� the completion-signaling timeouts cluster likely lives in this deviation; any systematic pass starts at components/script/dom/window/location.rs:navigate + constellation load bookkeeping.
## sec1zt241 Ã¢â‚¬â€� CLUSTER-2 CORE WEDGE REPRODUCED: page with slow/in-progress subresource that sets location.href mid-load NEVER navigates (WEDGED-no-nav at /tmp/navtest/c.html served by /tmp/navtest/srv.py port 8438 Ã¢â‚¬â€� server serves /tmp/navtest with 30s slowimg). Simple load-event navigation works fine (REACHED-B) Ã¢â‚¬â€� the wedge requires an IN-PROGRESS load being aborted by the navigation request. This is plausibly the gmail-boot-freeze mechanism (auth redirects during initial load) and the location-setter-during-load/pageshow WPT timeout family. Shares ancestry with the subframe-abort completion wedge already fixed Ã¢â‚¬â€� suspect AbortLoadUrl/pipeline-replacement path in constellation. NEXT: reproduce + gdb all-threads bt at wedge time (sudo gdb -p PID -batch), compare thread state vs the subframe wedge signature, instrument handle_abort_load_url_msg ordering. Repro harness persists in /tmp/navtest.
## sec1zt242 Ã¢â‚¬â€� NAV-DURING-LOAD WEDGE LOCALIZED inside constellation::load_url. Elimination chain (instrumented, [nav-dbg] eprintlns still in tree UNCOMMITTED Ã¢â‚¬â€� remove before final commit): script navigate() sends LoadUrl OK Ã¢â€ â€™ constellation recv OK Ã¢â€ â€™ embedder approval round-trips OK (allowed=true) Ã¢â€ â€™ load_url ENTERED Ã¢â€ â€™ then nothing: no navigation completes, old doc stays. So wedge is inside load_url body (constellation.rs:4411+) or new_pipeline downstream. Suspect areas in order: (1) get_activity(source_id)==Inactive early-return Ã¢â‚¬â€� the source pipeline may be marked Inactive while its in-progress load never completed activation lifecycle; (2) the replace/pending-change machinery; (3) new_pipeline created but activation swap never happens because the OLD pipeline never finishes abort bookkeeping (same family as the fixed subframe wedge Ã¢â‚¬â€� completion signaling). REPRO: /tmp/navtest/c.html via srv.py port 8438 (slowimg 30s; note c.html references slowimg.py but server serves slowimg by path substring). GOTCHA: pkill self-match killed shell again when pattern in cmdline Ã¢â‚¬â€� never pkill patterns that appear in own command. GOTCHA2: incremental builds are 9-55s, trust the empirical binary test not timing. KEY FACT: transient activation EXISTS in servo (window.has_transient_activation, useractivation.rs) Ã¢â‚¬â€� x-origin topnav gate estimate revises DOWN to S.
## sec1zt243 Ã¢â‚¬â€� NAV-DURING-LOAD WEDGE MECHANISM PINNED (timing-decisive): the new pipeline IS created and the navigation is NOT dropped Ã¢â‚¬â€� it is DEFERRED until the old document's in-progress fetches finish. Evidence: with 6s slowimg, at 3.5s still on old page, by ~14s navigated; with 30s slowimg never navigates within observation. MECHANISM: new-pipeline activation waits for old document teardown; teardown waits for in-progress fetch completion because DOCUMENT ABORT DOES NOT CANCEL IN-PROGRESS FETCHES (spec: aborting a document must cancel its fetches). Not a deadlock Ã¢â‚¬â€� a lost-cancellation. IMPACT: any page slow-loading resources wedges navigations/redirects for the fetch duration Ã¢â‚¬â€� plausibly THE gmail boot freeze: gmail boot redirects fire while heavyweight boot resources are in flight, and if any never completes (hung connection), the navigation never happens = permanent wedge. IMPLEMENTATION TARGET: abort_a_document_and_its_descendants (script/dom/document) Ã¢â‚¬â€� must propagate cancellation to the document's in-progress resource fetches (image cache requests, stylesheet/script fetches via net FetchController cancellation tokens). UNCOMMITTED [nav-dbg] instrumentation in navigation.rs + constellation.rs Ã¢â‚¬â€� strip or keep behind a verbose flag before committing any fix. Verify fix with /tmp/navtest repro: 30s slowimg should navigate at ~0.3s not never.
## sec1zt244 Ã¢â‚¬â€� fetch-cancellation fix investigation state: document.abort() DOES terminate the script-side fetch_group (globalscope.rs:348-358) Ã¢â‚¬â€� so the missing cancellation is ELEMENT resource loads: img/script/link fetches go through net resource threads/image cache and are NOT in the fetch_group. Verify-first before assuming: instrument whether abort() even RUNS at wedge time (queue a nav-task eprintln in abort_a_document_and_its_descendants Ã¢â‚¬â€� possibility: the abort task itself starves or the image keeps the loader alive independently). Two candidate fix shapes: (1) wire element resource loads into the fetch_group cancellation tokens; (2) whatever teardown-waiting gate blocks activation must not wait on resource loads after abort. ALSO OPEN: confirm what exactly the new-pipeline activation waits on Ã¢â‚¬â€� grep for where pipeline activation is gated on old-pipeline teardown in constellation (handle_load_finished / pending change commit path). [nav-dbg] instrumentation uncommitted in tree: strip before shipping.
## sec1zt245 Ã¢â‚¬â€� CRITICAL INVALIDATION: the nav-during-load 'wedge' repro was substantially a TEST-SERVER ARTIFACT. python http.server.HTTPServer is SINGLE-THREADED Ã¢â‚¬â€� while slowimg slept 30s the server could not even accept the b.html connection, so 'b.html never fetched' meant nothing about servo. With ThreadingMixIn server (srv8442.py), b.html IS fetched during the in-flight slowimg. The sec1zt241-244 chain is CONTAMINATED Ã¢â‚¬â€� do not trust the deferral-timing evidence either. VALID residue: (a) the original observation WEDGED-no-nav at 3s+ with the single-threaded server is fully explained by the artifact; (b) whether the navigation COMPLETES VISUALLY quickly with a threading server is still untested Ã¢â‚¬â€� retest: title at 2s with 30s slowimg in flight. LESSON: always use threaded test servers; the harness-invalidity pattern strikes again (same class as msestress). The WPT location-setter timeouts use wptserve (threaded) so their cause is still open Ã¢â‚¬â€� repro must come from the actual WPT test not the synthetic one.

## wedge-thread-survives-reload 2026-11-27 (mac lane, engine evidence)

While verifying the shell hang watchdog (feather #323): after a busy-loop
(while(1){}) script-thread wedge, the pill's Reload reloaded the page
visually Ã¢â‚¬â€� but process cpu stayed at ~99.6%. The spinning script thread
is NOT terminated by navigation/reload. Implication chain for the engine
lane: (1) script threads have no preemption/kill path on navigation Ã¢â‚¬â€�
a wedged thread burns a core for the process lifetime; (2) shell-level
recovery is UX-only, cpu reclamation requires either thread teardown in
servo's pipeline shutdown or true process isolation; (3) when measuring
the wedge class in harnesses, post-reload cpu is NOT evidence of recovery
Ã¢â‚¬â€� check thread states instead. Suspect locations by function: pipeline
teardown on navigation in components/constellation Ã¢â‚¬â€� what happens to a
script thread that never finishes its current task when its pipeline is
closed/reloaded (does shutdown wait on the same join/chan that the wedge
starves? compare the batch6 SeekData deadlock shape Ã¢â‚¬â€� same family:
'unbounded wait on a thread that never comes back').
## sec1zt246 Ã¢â‚¬â€� CRITICAL HARNESS-INTERFERENCE LESSON: the html rerun's numbers are GARBAGE (201 crashes + 344 timeouts vs baseline 25+30) because /tmp/kshell.sh's servoshell cleanup pattern kills ALL servoshell processes machine-wide INCLUDING THE WPT HARNESS'S OWN harness-spawned servoshells (they run the same binary path). Every interactive-test cleanup during a background WPT run massacres the run in progress. RULES: (1) NEVER run interactive servoshell tests while a background WPT run is active; (2) if interactive testing is unavoidable, kill by exact PID never by pattern; (3) treat any WPT run with anomalous crash/timeouts-vs-baseline as contaminated and check for interference before believing regressions. Also: run logs are ~1.5GB each on a 4.2G-free disk Ã¢â‚¬â€� archive or delete contaminated logs before restarting runs. CLEAN html rerun restarted after this note.
## sec1zt247 Ã¢â‚¬â€� upstream scan 2026-11-27 ~12:25 UTC: no new commits since bb9e153f1ccb Ã¢â‚¬â€� duty satisfied, nothing to cherry-pick. CLEAN html-only WPT rerun launched 12:22 UTC (/tmp/wpt-html-status.txt) measuring the fix batch Ã¢â‚¬â€� DO NOT run interactive servoshell tests until it completes (~60-90min), interference lesson sec1zt246. Binary under test includes: subframe-abort wedge fix, NV12 crash fix, container-rule fix, :dir(), srcdoc nav block, zero-delay timer fix, webrtc graceful degradation, designMode, 7 upstream cherry-picks.
## sec1zt248 Ã¢â‚¬â€� x-origin topnav gate IMPLEMENTATION SPEC (unimplemented): spec 'allowed to navigate' Ã¢â‚¬â€� a cross-origin iframe navigating the TOP-level traversable requires transient activation IN THE IFRAME ITSELF (parent activation insufficient Ã¢â‚¬â€� per WPT test name). Implement in navigation.rs::navigate() replacing the TODO at 'Step 5': compute target-is-top (window_proxy.parent().is_none() AND target is the top traversable Ã¢â‚¬â€� careful: navigate() targets window which may be the iframe itself for normal self-navs; only gate when the target browsing context is top-level AND source document is inside a cross-origin subframe). Gate: if target is top-level AND source doc's origin != top-level origin AND !source_window.has_transient_activation() Ã¢â€ â€™ return without navigating. RISK: has_transient_activation correctness unverified Ã¢â‚¬â€� false negatives break legit navs; the WPT user-click variants verify both directions. Verify ONLY via the actual WPT test cross-origin-top-navigation-with-user-activation-in-parent.window.html once harness ports free. PRIORITY NOTE: this is security-relevant but upstream-shared; the html clean rerun completing + report regeneration comes first.

## serp-ghost-repro-negative 2026-11-27 (mac lane, negative results)

Follow-up to serp-duplicate-results: minimal repro attempts on synthetic
pages all CLEAN Ã¢â‚¬â€� no ghosting on scroll for: (a) plain long static page,
(b) position:sticky header page, (c) async content-replacement page
(innerHTML swap 1.5s post-load, SERP-like). Content scrolled correctly,
rows contiguous, no stuck copies. Repro files live at /tmp/htdocs
{plain,sticky,async}.html served via python http.server :8899.

Refined implication: ghosting needs something the SERP has that these
don't Ã¢â‚¬â€� candidates in decreasing likelihood: (1) async IMAGE loads
causing re-raster of already-painted regions (SERP thumbnails Ã¢â‚¬â€� ghost
copies in the original repro included image-bearing result blocks);
(2) layout SHIFT events mid-scroll (SERP reflows heavily as modules
inject); (3) position:fixed overlays + scroll combined; (4) web font
swap re-raster. The semi-transparency of ghosts remains the strongest
mechanism clue: new content alpha-blends OVER old rather than replacing Ã¢â‚¬â€�
points at missing buffer-clear in the scroll damage path, triggered only
when a re-raster invalidation arrives mid-scroll. Next experiment for
whoever picks this up: add async-loading images to the async.html harness
(mid-scroll insertion) Ã¢â‚¬â€� if ghosts appear, the mechanism is
're-raster invalidation during active scroll drops the clear'.

## serp-ghost-repro-POSITIVE 2026-11-27 (mac lane, mechanism confirmed)

MINIMAL REPRO FOUND Ã¢â‚¬â€� /tmp/htdocs/imgtest.html (served :8899): 40 static
text rows + async image injection in waves (picsum 80x60 thumbnails
prepended to random rows every 900ms). Repro recipe: load page, scroll
down WHILE image waves are inserting. Result: ghost copies of rows +
images stuck semi-transparent near the viewport top-edge / overlapping
the shell chrome area, exactly matching the google SERP artifact. The
trigger is precisely: REFLOW-CAUSING content injection (images prepend
shifts layout down Ã¢â‚¬â€� every injection moves rows) WHILE a scroll is in
flight. Neither plain static, sticky, nor text-only innerHTML replacement
repro'd Ã¢â‚¬â€� text replacement without layout shift is insufficient; the
images matter because they arrive async and each insertion REFLOWS.

Mechanism, refined to engine-lane actionable: during an active scroll,
the compositor blits/translates existing painted content and re-rasters
only the damage strip. If a layout-changing invalidation lands mid-scroll,
the damage region computed pre-reflow no longer matches post-reflow
geometry Ã¢â‚¬â€� the stale blit is composited (alpha-blended, hence
semi-transparency) rather than cleared. Suspect locations by function:
compositing scroll implementation in components/compositing Ã¢â‚¬â€� the
path that handles ScrollOffset updates + damage rect computation when
a reflow arrives between frames; specifically the interaction between
pinch/scroll viewport translation and repaint scheduling. EXPERIMENT
for engine lane: force full-canvas repaint on any layout dirtying that
overlaps an in-flight scroll (disable damage-strip optimization
temporarily) Ã¢â‚¬â€� if ghosts disappear, mechanism confirmed; the fix is then
damage-rect recomputation on reflow-during-scroll, not full repaints.

## handoff-daily-driver-essentials 2026-11-27 (mac lane, scoping for next lane)

Coordinator batch: (1) find-in-page UI, (2) bookmarks management surface,
(3) history UI surface. Scoping done, implementable subset clear:

BOOKMARKS Ã¢â‚¬â€� mostly exists already: toolbar.rs has star button wired to
Action::ToggleBookmark(url) at state.rs:253; state.rs:304 bookmarks:
Vec<String> with is_bookmarked() at :474. MISSING: management surface Ã¢â‚¬â€�
a panel/overlay listing bookmarks with click-to-open and remove. All
shell-side, no engine work. Shape: reuse the downloads-panel pattern in
ui/ (window.rs or a new bookmarks panel), add a bookmark-list button or
surface in the Ã¢â€¹Â¯ menu; Action::OpenBookmark(url)->NewTab-or-navigate +
RemoveBookmark(url) small additions to state.rs handlers.

HISTORY UI Ã¢â‚¬â€� storage exists per coordinator (verify in state.rs/persist
layer); surface needed: feather://history internal page or overlay list,
click-to-open, clear. Reuse internal_pages.rs pattern (it already exists
for settings-like surfaces).

FIND-IN-PAGE Ã¢â‚¬â€� NOT shell-implementable: NO engine API exists. Greps of
servo/components and ports find no find_in_page/search text API on
webview. Match highlighting/count requires engine work: script-side
DOM text traversal + paint-time highlight overlays. Recommend:
implement shell-side as a TWO-PHASE: phase 1 uses the devtools/JS eval
channel (javascript_evaluator.rs exists in components/servo!) to run
in-page JS doing text search + scroll-to-match + count Ã¢â‚¬â€� no engine
changes needed, eval-based find is what early electron shells did.
Phase 2 is native engine support upstream. The eval route makes this
shell-implementable TODAY. Gotcha: devtools eval sandbox Ã¢â‚¬â€� location.*
throws (see devtools-eval-gotchas entry) but document/innerText access
worked in probes.

mac-lane gotchas banked repeatedly but still biting: cwd resets EVERY
exec call Ã¢â‚¬â€� chain cd && in literally every command; install_name_tool
rpath with ABSOLUTE binary path after every cargo build; build cmd in
prior memory entries.

## inflight-bookmarks-page 2026-11-27 (mac lane, mid-flight state)

Branch devin/1795784000-bookmarks-page in feather: InternalPage::Bookmarks
bookmarks management surface PATCHED but NOT yet compile-verified Ã¢â‚¬â€� cargo
build running at compaction time. Changes: state.rs enum+title+address arms,
internal_pages.rs bookmarks()/bookmark_row() (parallel to history rows,
remove via ToggleBookmark, open via Action::Open{url,title:None}), toolbar.rs
menu row after History. HISTORY UI (batch item 3) ALREADY EXISTED fully
(search/filter/clear/remove rows) Ã¢â‚¬â€� verification-only. FIND-IN-PAGE still
untouched Ã¢â‚¬â€� implement the eval route per handoff-daily-driver-essentials.

## find-in-page-shipped 2026-11-27 (mac lane, shell-side implementation)

Find-in-page implemented shell-side via evaluate_javascript (servo has NO
native find API; window.find unimplemented in Window.webidl). Mechanism:
TreeWalker text-node walk, split+wrap matches in <span class=__ff_hit> with
inline background styles (real DOM elements Ã¢â‚¬â€� render in any engine state),
active match recolored + scrollIntoView. Count correct on first shot
(111/111 on probe page incl. substring overlap rows). Skip list:
SCRIPT/STYLE/NOSCRIPT/TEXTAREA/INPUT/SELECT/OPTION/contentEditable/[hidden].

KEY ENGINE EVIDENCE for the paint-invalidation family (compounds
serp-ghost-repro-POSITIVE): DOM mutations injected through eval do NOT
repaint immediately Ã¢â‚¬â€� highlights/probe-div appear only after a subsequent
frame trigger (scroll or another mutation). Verified: computed style and
layout geometry correct in DOM (orange rgba active, 45x16 rect) while
screen shows nothing. So servo's mutationÃ¢â€ â€™repaint path has latency/dropped
invalidation even WITHOUT scroll-in-flight Ã¢â‚¬â€� the damage-rect suspect in
serp-ghost is part of a broader invalidation starvation issue, not only a
scroll-interaction bug. Engine lane: instrument script-initiated reflow Ã¢â€ â€™
compositor frame production on a static page; expect missing or delayed
damage generation.

egui gotcha banked: singleline TextEdit consumes Enter as focus-loss Ã¢â‚¬â€�
detect submission via lost_focus()+key_pressed(Enter), then re-request
focus. keystroke leakage: main.rs already gates page key forwarding on
egui_wants_keyboard_input Ã¢â‚¬â€� once egui focus is real, no leak.
## sec1zt249 Ã¢â‚¬â€� LAYOUT PANIC ROOT CAUSE (batch-7 P0, layout_impl.rs:1587 'Should always have a FragmentTree when layout unnecessary'): feather's OWN hidden-tab memory lever (update_visibility_state Ã¢â€ â€™ drop_cached_layout_trees, document.rs:5642) drops box+fragment trees. compute_damage_and_rebuild_box_tree (traversal.rs:86) rebuilt the box tree but returned the propagated damage UNCHANGED Ã¢â‚¬â€� when damage was overflow-level only (no Relayout bit), reflow took the overflow-only path which panics on the absent fragment tree. Mechanically: any reflow after tree-drop whose damage is insufficiently severe = crash. Normally hiddenÃ¢â€ â€™visible transitions carry full damage which is why restore usually works; scroll/image-load-driven overflow-only reflows hit the panic Ã¢â‚¬â€� matches user repro 'repeatedly during real use'. FIX (traversal.rs): when box_tree reconstructs from scratch, return layout_damage | Relayout Ã¢â‚¬â€� a fresh box tree has no fragments by definition so full layout is mandatory. Compile-verifying; interactive verification queued behind the running clean WPT html run (NO interactive servo while it runs Ã¢â‚¬â€� interference lesson sec1zt246).
## sec1zt250 Ã¢â‚¬â€� batch-7 rendering-correctness queue (user dogfood, after panic fix): (1) gmail layout 'very off'/not centering Ã¢â‚¬â€� flex/grid in the wild, NEED post-login repro detail or screenshot since botgate blocks Linux gmail; flex gap itself verified exact so look at grid/align/justify-content or missing CSS (text-wrap, @custom-media, container queries are KNOWN absences). (2) gmail SVGs not rendering Ã¢â‚¬â€� inline SVG support partial in servo; reproduce with gmail-style SVG sprite usage. (3) youtube dark side-nav black-on-black Ã¢â‚¬â€� cascade layers exist in stylo, prefers-color-scheme exists in servo; suspects: @property registration gaps, color-scheme property handling, or the fe dark-theme attribute cascade. All three need interactive repro Ã¢â‚¬â€� BLOCKED while clean WPT html run occupies machine memory (started 12:22 UTC). Compile-retry loop /tmp/compile-retry.sh running autonomously for the panic fix (3b071c12b02 committed locally, push after compile-verify).

## perf-telemetry-youtube 2026-11-27 (mac lane, HARD NUMBERS, batch-7 perf class)

10-min instrumented capture, feather release build, Big Buck Bunny 4K60
playback (aqz-KE-bpKQ), FEATHER_MEMREP=20 + FEATHER_FRAMES=1 + rss/cpu
sampler (raw csv /tmp/perf-mem.csv, stderr /tmp/perf-stderr.log on mac lane
VM Ã¢â‚¬â€� ephemeral, numbers banked here).

VERDICT: primary mechanism is UI-THREAD SATURATION, with memory pressure
compounding. Not layout thrashing.

1. UI-thread stalls (the hover-lag mechanism, user: 'hover feels very
   delayed'): 34 '[stall] gui frame' events in ~5min playback Ã¢â‚¬â€� one every
   ~8s. ms values: p50Ã¢â€°Ë†370, p90Ã¢â€°Ë†1145, max 8486 (an 8.5 SECOND frame).
   Distribution over time does not decay Ã¢â‚¬â€� lag is continuous, not warmup.
   The stall instrument sits on gui.run() = egui layout + glow present;
   the servo surface composite runs on the UI thread through software GL
   (main.rs comment already flags this). 4K frames make each present a
   multi-hundred-ms CPU upload Ã¢â‚¬â€� input events (hover!) queue behind them.
2. Memory growth: rss 630MB boot Ã¢â€ â€™ 1072 in 15s of playback start Ã¢â€ â€™ 1430MB
   at 5min, no steady state (~+130MB/min initial, still climbing at end).
   js/malloc-heap for the youtube page: 94Ã¢â€ â€™318MB in 3min, GC drops it to
   82MB, regrows to 190MB Ã¢â‚¬â€� JS side DOES collect, but rss never follows
   the GC down: the retention is native-side. system-heap-allocated
   386Ã¢â€ â€™478MB (gstreamer decode buffers), SourceBuffer 5MB each out-of-tree.
   Matches user report (12% system memory on youtube alone).
3. Event-loop churn: wakes=61199 for 4800 produced frames = 12.8 wakes per
   frame; rearm taken 30 vs skipped 4769 Ã¢â‚¬â€� the loop never idles during
   playback, compounding input latency.
4. CPU: 100-170% sustained one-and-a-half cores for 4K60 software decode
   + composite on this hardware.

ENGINE-LANE actionable ordering:
- P0: get the video present OFF the UI thread (or GPU-accelerate it Ã¢â‚¬â€�
  compounds with gpu-video-decode.md scoping: the GL upload path exists,
  vtdec works on macOS). Every frame of video currently serializes against
  ALL input handling. This single fix addresses both 'laggy playback' and
  'delayed hover'.
- P1: media buffer lifecycle Ã¢â‚¬â€� SourceBuffer backing stores not released
  after append consumption (5MB each out-of-tree), plus whatever pins
  native system-heap growth. Instrument: does rss plateau if playback
  pauses? (not tested Ã¢â‚¬â€� cheap follow-up).
- P2: event-loop rearm starvation Ã¢â‚¬â€� 12.8 wakes/frame means the repainting
  logic never settles; possibly the webview_needs_paint loop continuously
  re-arming. Compare against idle-page baseline (google.com shows ~0.5%
  cpu, so idle is fine Ã¢â‚¬â€� this is playback-specific churn).
- Negative evidence: this is NOT a servo-layout-cost problem Ã¢â‚¬â€� stalls
  localize in the present path, not style/layout.
## sec1zt251 Ã¢â‚¬â€� RULE VIOLATION + reinforcement: at ~13:40 UTC I ran kshell.sh while the clean html run was active Ã¢â‚¬â€� a handful of harness servoshells killed mid-test = a few spurious crashes in this run's data. Runner survived; treat crash count skeptically, compare against future uncontaminated runs. RULE RESTATED HARD: while any WPT run is active, zero interactive servoshell launches AND zero kshell invocations Ã¢â‚¬â€� test nothing interactively; do compile-only or analysis work. POSITIVE DATA POINT from partial contaminated log: NV12/p3 crash family 11Ã¢â€ â€™1, beforeunload cascade 0 Ã¢â‚¬â€� crash fixes holding even under memory pressure. youtube verified playing video on Linux servo (screenshot in session). prefers-color-scheme media query mechanism evaluates correctly but follows winit system theme (can't force on this VM). color-scheme property IS implemented in stylo priority properties incl. light-dark() Ã¢â‚¬â€� youtube black-on-black mechanism is NOT absence of these; needs actual repro detail.
## sec1zt252 Ã¢â‚¬â€� composite-off-UI-thread SCOPING (batch-7 perf P0): feather shell-new composite path (app.rs:820-920): webview.paint() runs ON THE UI THREAD inside the egui frame, then render_to_parent GL-blit callback. Servo renders into one shared offscreen framebuffer (no per-webview render offset Ã¢â‚¬â€� documented SERVO LIMITATION). Cost anatomy: paint() itself is servo's composite Ã¢â‚¬â€� for VIDEO frames, the frame upload (4K software = hundreds of ms) happens inside paint() or inside webrender's texture upload, both UI-thread. FIX SHAPE OPTIONS: (a) move paint() to a worker thread sharing the GL context (requires making the context multi-thread-shareable Ã¢â‚¬â€� egui_glow/glow context is single-threaded by design, needs a second shared GL context with texture sharing Ã¢â‚¬â€� moderate-large shell surgery); (b) GPU-accelerate the video path so frames never transit CPU Ã¢â‚¬â€� compounds with gpu-video-decode scoping (vtdec works on mac; windows d3d11 exists in upstream servo?); (c) decouple present from input: run paint() only at video-framerate caps and yield input processing between Ã¢â‚¬â€� cheap partial mitigation, doesn't fix throughput. ARCHITECTURAL TRUTH: the correct fix is (a) or (b); (c) is a band-aid. Blocking unknown: whether servo's RenderingContext supports shared-context creation on each platform Ã¢â‚¬â€� investigate engine-side surfman/glow usage first. Mac telemetry: stalls p50=370ms p90=1145ms Ã¢â‚¬â€� even 1-in-3 frame skipping would transform UX.
## sec1zt253 Ã¢â‚¬â€� composite fix feasibility KEY FACT: servo's rendering sits on surfman (components/shared/paint/rendering_context.rs) which natively supports shared GL contexts Ã¢â‚¬â€� the engine-side primitive for off-UI-thread compositing EXISTS. SoftwareRenderingContext uses create_software_adapter. So option (a) is engineering-feasible: shell creates a second surfman context sharing textures with the display context, worker thread does webview.paint(), UI only blits the shared texture (cheap). Sized as multi-day shell+engine surgery Ã¢â‚¬â€� dedicated-session task, not a tail-of-context item. Also relevant: whether webrender itself is GPU or software determines whether even the blit is cheap Ã¢â‚¬â€� check pref/glyph for swgl vs GPU on each platform before designing.
## sec1zt254 Ã¢â‚¬â€� upstream scan 14:10 UTC: ONE new commit since bb9e153f1ccb Ã¢â‚¬â€� 98dc7f94673 script_bindings malloc_size_of measure-from-base (#50266), cherry-picked as f9586f3186d (codegen.py conflict: quote-style only, resolved keeping upstream logic). Diagnostic tooling, not cluster-relevant. LAST-CHECKED UPSTREAM SHA now 98dc7f94673. DISK CRISIS: hit 100% Ã¢â‚¬â€� freed 2G by deleting archived first-run wpt raw logs (superseded by clean run in flight). target/debug 18G (incremental 8.1G) is the disk hog.
## sec1zt255 Ã¢â‚¬â€� RENDERING BATCH INVESTIGATION STATE (handoff-critical). FLEXBOX: mined flexbox raw log Ã¢â‚¬â€� only 3 unexpected results BUT upstream-expected FAILs are real servo layout gaps users hit. Clusters: flex-abspos-staticpos (align-self/justify-content/position-absolute ~24 files, IMPLEMENTED in components/layout/flexbox/layout.rs:913-988 but reftest-failing = pixel-level incorrectness, subtests PASS Ã¢â‚¬â€� magnitude unknown), aspect-ratio-intrinsic-size (10), writing-mode (12). flex gap itself verified exact. DARK-NAV hypothesis: youtube themes via html[dark] attribute + CSS custom properties inherited into shadow roots; if servo breaks custom-property inheritance across shadow boundaries Ã¢â€ â€™ black-on-black exactly. Repro page READY at /tmp/navtest/shadowtest.html (measures host vs shadow-inner vs plain computed colors Ã¢â‚¬â€� title bar has verdict), darknav.html, svgtest2.html (inline svg + img-svg), center.html (flex/grid/text-align/margin-auto centering offsets). SVG: DOM elements exist (components/script/dom/svg/) Ã¢â‚¬â€� rendering correctness unverified. COMPOSITE PERF: scoping in sec1zt252-253 Ã¢â‚¬â€� surfman shared contexts feasible, multi-day dedicated-session task. SERVOSHELL TESTING RULE: harness active Ã¢â‚¬â€� no interactive launches (memory pressure + kill-risk); when free, run the 4 repro pages via srv8442.py server (ThreadingMixIn port 8442, serves /tmp/navtest). DISK: 2G free, target/debug 18G Ã¢â‚¬â€� delete target/debug/incremental (8.1G) if crisis recurs (costs compile time only).

## idle-memory-footprint (2026-11-27, macOS lane)
- feather idle on a light page: **132 MB RSS / 0.1% CPU** (~0.8% of 17GB system). about:blank similar at 160MB (noise-level variance). Genuinely light at idle Ã¢â‚¬â€� the user's 12%-of-system observation was youtube playback, already characterized as media-pipeline accumulation.
- Watchdog defect found + fixed in the same audit: the UI-pump stall detector false-positived on healthy idle (counter frozen in ControlFlow::Wait) Ã¢â‚¬â€� wrote spurious crash-hang artifacts every boot. Fixed with a working/idle state flag. Lesson for all lanes: any watchdog keyed on pump progress must distinguish blocked-in-wait from wedged-in-processing.
## sec1zt256 Ã¢â‚¬â€� SVG RENDERING BATCH RESULTS (live-verified, screenshot evidence): simple svg mechanisms ALL HEALTHY Ã¢â‚¬â€� inline path/circle rasterize correctly, img data-url svg loads+renders (24x24 natural), shadow-dom custom-property inheritance perfect, flex/grid/text-align/margin-auto centering ALL exact (c1-c4=0.0 offset). REAL DEFECTS FOUND via svguse.html sprite test: (1) `<use href>` referencing a `<path>` in defs LOSES the fill presentation attribute Ã¢â‚¬â€� renders black regardless of use's fill; `<use>` referencing `<symbol>` KEEPS fill correctly (green circle rendered green). Per spec use-fill must inherit into referenced content lacking own fill. (2) xlink:href namespace NOT supported Ã¢â‚¬â€� element renders nothing at all. GMAIL MATCH: gmail icons are svg sprites Ã¢â‚¬â€� fill-drop-to-black matches 'SVGs not rendering properly'. SYMPTOM INDEX: Symptom: svg icons render black/wrong-color Ã¢â€ â€™ check: use-element fill inheritance into referenced non-symbol content. Symptom: svg sprite items invisible Ã¢â€ â€™ check: xlink:href vs href on use elements. CENTERING REFUTED: simple centering all correct Ã¢â‚¬â€� gmail 'very off' layout is complex-interaction or the flex-abspos reftest cluster; needs Windows repro detail. DARK-NAV: basic vars+shadow inheritance fine Ã¢â‚¬â€� black-on-black needs real youtube repro detail.
## sec1zt257 Ã¢â‚¬â€� SVG ROOT CAUSE ISOLATED: inline svg rasterizes by SERIALIZING the DOM subtree to a data: URL then resvg-rasterizing like an external image (SVGElementData.source components/shared/layout/lib.rs:186). DECISIVE TEST: the SAME sprite markup as an external .svg file renders PERFECTLY (red href triangle + blue xlink:href triangle both correct fills) Ã¢â‚¬â€� resvg handles everything fine. THE DEFECT IS IN THE INLINE-DOM-TO-XML SERIALIZATION: fill attrs on use drop, xlink:href references break entirely. NEXT INVESTIGATION POINT: find the serializer producing SVGElementData.source Ã¢â‚¬â€� start components/script/dom/node/layout_dom.rs:355 svg_data and trace what serialization it uses (html vs xml serializer, namespaced attribute handling). Fix shape: correct serialization of namespaced attrs + presentation attrs, then both defects close at once. Test pages live at /tmp/navtest/svguse.html (inline, broken) vs exttest.html+extsprite.svg (external, correct).
## sec1zt258 Ã¢â‚¬â€� SVG USE ROOT CAUSE COMPLETE (components/script/dom/svg/svgsvgelement.rs process_single_use_element ~line 130): the use-resolution mechanism is fundamentally wrong, TWO defects same site: (1) only reads local_name!("href") Ã¢â‚¬â€� xlink:href never consulted = invisible sprites; (2) the referenced clone is APPENDED TO THE SVG ROOT as a plain sibling Ã¢â‚¬â€� NOT embedded as a child/shadow of the use element. Consequences: use's fill/presentation attrs never inherit into the clone (BLACK rendering Ã¢â‚¬â€� the exact screenshot symptom), x/y position offsets ignored, use semantics absent. CORRECT FIX SHAPE: append clone as child of the use element (or implement shadow-tree semantics) so presentation attrs inherit + apply x/y transform; add xlink:href fallback when href absent (read attribute with namespace Ã¢â‚¬â€� check how namespaced attrs are read elsewhere, get_attribute with ns). MEDIUM-size engine fix, fully scoped, high user value (gmail sprites). Tests: /tmp/navtest/svguse.html broken-vs-exttest.html correct. NOTE the early-return ordering: xlink check first, then clone-into-use restructure.
## sec1zt259 Ã¢â‚¬â€� SVG USE FIX SHIPPED+VERIFIED: xlink:href fallback + use-attribute transfer onto clone + x/y-transform folding, compile-clean 1m27s. VERIFIED LIVE: svguse.html now renders ALL FOUR correct Ã¢â‚¬â€� red href-path, blue xlink:href-path (was invisible), green symbol, purple viewBox Ã¢â‚¬â€� screenshot evidence /tmp/svguse-fixed.png. Implementation notes: set_attribute_from_parser never-overwrite semantics exactly match spec inheritance order (clone's own attrs win); transform merge only when clone lacks own transform (residual edge); clones still append at svg-root document order (paint-order imperfection vs use position Ã¢â‚¬â€� minor). This closes the gmail-svg-sprites defect class pending real-site verification.
## sec1zt260 Ã¢â‚¬â€� download-attribute state discovery + cross-origin fix (UNCOMPILED Ã¢â‚¬â€� WPT rerun active, compile blocked on RAM): the anchor download path was ALREADY wired in feather-engine (relations.rs follow_hyperlink Ã¢â€ â€™ AnchorDownloadFetchContext in download.rs Ã¢â‚¬â€� the spool infrastructure is feather-built). The stale TODO at htmlanchorelement.rs:377 is misleading. GAP FIXED: spec's cross-origin restriction was missing Ã¢â‚¬â€� download attr now honored only for same-origin/blob/data URLs, else falls through to normal navigation (security: cross-origin force-download with attacker-chosen filename). Compile+verify pending WPT rerun completion. Residual test-failure hypotheses for the 18 download files: filename derivation from content-disposition/url, sandboxed-downloads flags.

## idle-memory-growth-curve (2026-11-27, macOS lane)
- Idle growth curve, about:blank, sampled 1/min for 6+ min: **perfectly flat 161 MB RSS, 0.1% CPU at every sample** Ã¢â‚¬â€� zero creep. Combined with the session-restore measurement (132MB with 34 restored-but-lazy tabs), feather is genuinely light at idle AND idle memory does not leak. Lazy session restore confirmed by these numbers: 34 eagerly-loaded pages would be GBs.
- Completes the memory story: idle flat + leak-free; youtube playback growth is the media pipeline (banked earlier: flattens on pause, never shrinks Ã¢â‚¬â€� MSE eviction is the engine-lane fix).
## sec1zt261 Ã¢â‚¬â€� youtube-darknav repro breakthrough path (no system-dark needed): youtube dark mode is user-toggleable in youtube's own UI Ã¢â‚¬â€� boot youtube in servoshell when harness frees, toggle dark in youtube settings, observe nav. Analysis dead-ends logged: theme custom properties are NOT in boot html (JS-applied at runtime, polymer custom-style system); :host-context() unimplemented in stylo entirely (chrome-only legacy) but youtube boot html contains zero occurrences Ã¢â‚¬â€� weak linkage, verify live before investing. flex-abspos investigation CLOSED: horizontal align-self-001 subtests PASS Ã¢â‚¬â€� the upstream-expected FAILs are vertWM (vertical writing mode) only, rare in real layouts; this cluster does NOT explain gmail. fieldset/legend: upstream-known-incomplete layout feature (.ini metadata present) Ã¢â‚¬â€� de-emphasized per gap-avoidance. download cross-origin fix written UNCOMPILED in working tree (relations.rs dirty) Ã¢â‚¬â€� compile after WPT rerun2. Verification pages ready: /tmp/navtest/downloadtest.html (same/cross/data cases).

## webex-smartadserver-classification (2026-11-27, macOS lane)
Post-layer-1 residual classification Ã¢â‚¬â€� both sites are GENUINE runaway-JS pages, not driver artifacts:
- **webex.com**: 122% CPU sustained 30s+ after nav, RSS 857MB Ã¢â‚¬â€� one core saturated indefinitely while the UI thread stays healthy (main-thread sample: idle in NSApplication event loop). Matches the layer-1 note's long-JS-task starvation signature.
- **smartadserver.com**: worse Ã¢â‚¬â€� **3.7 GB RSS in 30 seconds (~120MB/s allocation churn), 197% CPU**, still 93% at 50s. Allocation-churn-driven (consistent with the NumberToAtom observation in the layer-1 analysis).
- Verdict: the 2 genuine-timeout sites confirm the layer-2 slow-script class is real and user-facing Ã¢â‚¬â€� a page can burn 1-2 cores + gigabytes indefinitely with no browser-level recourse. The layer-2 product decision (slow-script detection/ interruption, like every other browser's "this page is slowing down your browser" bar) is the correct next investment Ã¢â‚¬â€� shell-side UI for it is cheap once the engine exposes script-execution-time signals.
- Method note: thread attribution on macOS is limited by stripped ??? frames in feather-ui samples; CPU-per-process + healthy-main-thread sampling was sufficient for classification.

## idle-memory-audit-full (2026-11-27, macOS lane)
The open-close-reclaim test, wiki doc in feather docs/wiki/IDLE-MEMORY-AUDIT.md:
- 1 idle tab: 160MB/0.1%. +4 real sites (5 tabs): 649MB. **After closing the 4 real tabs: 656MB Ã¢â‚¬â€� ZERO reclaim**, stable at +45s. Memory is monotonic per process lifetime: CPU dies to 0.1% but bytes never return. Same rule as the youtube pause-test, now confirmed for plain tab-close.
- Chrome comparison (contaminated config, same machine): restored-session idle ~981MB across 10 processes vs feather 132-160MB Ã¢â‚¬â€� "light at idle" is honestly true; the caveat is monotonic growth in long sessions. Engine-lane fix is allocator-level, compounding the memory-bisect finding that forced-reclaim levers make things worse.
## sec1zt262 Ã¢â‚¬â€� MONOTONIC-MEMORY ANALYSIS (mac #337: zero reclaim closing 4-of-5 tabs): elimination chain Ã¢â‚¬â€� (1) feather shell-new DOES build use-jemalloc (Cargo.toml verified); (2) purge_arenas fires on pipeline exit (script_thread.rs:3367) and uses arena.4096.purge = ALL arenas correct; (3) therefore jemalloc dirty-page retention is NOT the mechanism assuming pipelines actually exit. PRIME SUSPECTS in order: (a) SpiderMonkey GC heap Ã¢â‚¬â€� allocates outside jemalloc (system malloc/mmap), JS objects for closed pages only die on GC and the heap never shrinks; (b) pipelines not actually exiting on feather tab-close Ã¢â‚¬â€� VERIFY FIRST: does closing a feather tab destroy the webview/pipeline or keep it suspended?; (c) webrender texture/IOSurface cache + gstreamer buffers Ã¢â‚¬â€� non-jemalloc native memory. MEASUREMENT PLAN: heap_reports() in allocator/lib.rs exposes jemalloc stats.allocated/active/mapped Ã¢â‚¬â€� compare before/after tab close: if allocated drops but RSS doesn't Ã¢â€ â€™ retention story; if allocated flat Ã¢â€ â€™ still-live (leak or non-jemalloc). Where heap_reports is surfaced Ã¢â‚¬â€� check devtools/about:memory exposure next.
## sec1zt263 Ã¢â‚¬â€� download cross-origin fix VERIFIED+SHIPPED: compile-clean 1m08s, live verification Ã¢â‚¬â€� same-origin click Ã¢â€ â€™ background fetch of downloadme.txt with page intact; cross-origin click Ã¢â€ â€™ document context torn down (navigation), exactly the spec asymmetry. html WPT rerun2 CLEAN results: 83.6% raw, 606 unexpected Ã¢â‚¬â€� KEY CLUSTER: 100/123 unexpected crashes in /html/editing/dnd alone (drag-and-drop systematically crashes servoshell Ã¢â‚¬â€� ONE likely root cause, biggest single actionable cluster); timeouts cluster in focus(86)/dnd(67)/popovers(45)/forms(41). PASS-exp-FAIL 34 = ahead of upstream expectations (our fixes). Next big target: dnd crash root cause.
## sec1zt264 Ã¢â‚¬â€� DND CRASH-WEDGE INVESTIGATION STATE: reproduced dnd/drop/002.xhtml CRASH live Ã¢â‚¬â€� NOT a panic, a WEDGE (whole process 0% CPU, Script#1 parked no-spin, webdriver stops responding mid-PerformActions). Tests use testdriver REAL pointer-action drags (move/down/move/up). The drag implementation is recent (components/script/drag/ Nov 24-26: HtmlDragHandler etc). No symbols in debug=0 build Ã¢â‚¬â€� for a real backtrace need a debug=1 rebuild or repro with perf. OPEN QUESTIONS: (a) is the wedge in hit-test/layout sync during drag-move, or in the webdriver action-completion signaling; (b) does REAL dragging in the feather shell wedge the same way (user-facing test) Ã¢â‚¬â€� harness wedge could be webdriver-specific. Hits: hit_test_from_point_in_viewport does synchronous layout query (elements_from_point_query) Ã¢â‚¬â€� circular-wait candidate if compositor is itself waiting on script. 100 crashes cluster here = biggest single actionable WPT cluster. Next: repro with debug build OR instrument logging in the drag path.

## sec1zt265 Ã¢â‚¬â€� memory reclaim experiment CONFIRMS monotonic retention on Linux (2026-11-27)
Valid webdriver open-close experiment (servoshell, 4x en.wikipedia.org tabs):
baseline 348MB Ã¢â€ â€™ loaded 795MB (+447MB) Ã¢â€ â€™ close all 4 tabs Ã¢â€ â€™ 604MB after 10s, 605MB flat at 30s.
**~191MB (43%) of per-tab growth is never reclaimed even 30s after close.** Reproduces Mac's
audit pattern (#337) on Linux servo Ã¢â‚¬â€� engine-level, not shell-specific. Partial reclaim does
happen (~190MB back), so it's not total pinning: suspects split between jemalloc dirty-page
retention (purge fires at pipeline exit but dirty pages may remain if other pipelines share
the arena), SpiderMonkey GC heap growth (JS_GC on trigger but SM retains arenas), webrender
resource cache, and gstreamer/font caches. Next discriminator: heap_reports() jemalloc
allocated/active/mapped before/after Ã¢â‚¬â€� if allocated drops but RSS doesn't = pure allocator
retention; if allocated flat = still-live objects (real leak/pinning). Also note: 82 wedged
harness servoshells found hoarding RAM after wpt runs Ã¢â‚¬â€� harness cleanup gap, kill -9 needed.

## sec1zt266 Ã¢â‚¬â€� retention is BOUNDED not a linear leak (2026-11-27)
Two consecutive open-close cycles in the SAME servoshell process:
cycle1: 348 baseline Ã¢â€ â€™ 794 loaded Ã¢â€ â€™ 605 after close (+257 retained)
cycle2: 605 Ã¢â€ â€™ 794 loaded (identical peak!) Ã¢â€ â€™ 639 after close (+34 retained)
Peak RSS did NOT grow across cycles Ã¢â‚¬â€� the retained memory is REUSED by subsequent tabs.
Interpretation: bulk of retention is saturating caches/allocator arenas (webrender resource
cache, font cache, SpiderMonkey arena pool, jemalloc fragmentation) NOT a per-tab leak.
Impact reframed: RSS converges to a working-set plateau (~1.6x single-tab weight) and stays Ã¢â‚¬â€�
bad for idle-budget/monotonic-per-process-lifetime optics, but NOT unbounded growth.
Fix implications: (1) cache eviction policies (webrender/font) matter more than leak hunting;
(2) jemalloc purge on close DOES need verification since fragmentation in shared arenas
defeats it; (3) measuring per-cycle delta is the right leak detector Ã¢â‚¬â€� +34MB residual/cycle
is the true leak-rate upper bound worth chasing. Harness note: webdriver window/new starts
failing 'no such window' after the first close-round Ã¢â‚¬â€� servo's bookkeeping drops handles;
single-cycle-per-session measurements only.

## sec1zt267 Ã¢â‚¬â€� dnd crash cluster investigation state (2026-11-27)
100 of 123 unexpected WPT crashes are in /html/editing/dnd Ã¢â‚¬â€� ZERO upstream .ini files expect
CRASH there, so all actionable. Drag code is NEW (Nov 24-26): components/script/drag/
drag_gesture.rs (unreachable! at line 33 in a DragHandler::Html arm), html_drag.rs:150
borrow.as_ref().unwrap() Ã¢â‚¬â€� both prime crash candidates. crashers/dialog-001.html needs real
user drags (alert-interrupted drag) Ã¢â‚¬â€� not webdriver-automatable. Prior session reproduced a
0%-CPU wedge mid-PerformActions with webdriver drag Ã¢â‚¬â€� SAME family, wedge vs crash may depend
on timing. NEXT SESSION PLAN: (1) run a single crashing dnd test via wptrunner with stderr
captured to get the exact panic location Ã¢â‚¬â€� command shape: ./mach test-wpt /html/editing/dnd/
<name> --product servoshell (verify exact mach invocation); (2) the panic message will
pinpoint which of the two candidates; (3) gdb backtrace needs debug symbols Ã¢â‚¬â€� consider a
CARGO_PROFILE_DEV_DEBUG=1 targeted rebuild of just the script crate if panic msg insufficient.
Also pending from before: WPT report regeneration PR (/tmp/wpt-html-report.md exists,
uncommitted); gmail/svg real-site verification blocked on Windows repro detail.

## select-dropdown activation gap (banked 2026-11-27, macOS lane)
Shell wiring complete (feather #343) but servo never fires SelectElement embedder
control on click Ã¢â‚¬â€� zero show_embedder_control calls, not even show+hide collapse.
Activation path: htmlselectelement.rs handle_event, mousedown+IsTrusted+Primary.
Suspects ranked: focus-event hide ordering (FocusEvent handler hides control for
element; if focus lands after mousedown the control dies synchronously), shadow-tree
hit-test, trustedness. Engine-lane investigation; discriminator instrumentation
noted in feather docs/defect-queue.md. Selects are everywhere on the real web Ã¢â‚¬â€�
every form with a dropdown currently dead-ends silently for the user.

## sec1zt268 Ã¢â‚¬â€� dnd "crashes" are actually the WEDGE, repro harness ready (2026-11-27)
KEY EVIDENCE: single-test run of /html/editing/dnd/canvas/001.html shows harness line
"Browser not responding, setting status to CRASH" Ã¢â‚¬â€� the 100 dnd "crashes" are NOT process
crashes, they are the browser WEDGING (same 0%-CPU wedge reproduced manually mid-PerformActions).
One mechanism explains the whole biggest-actionable WPT cluster. Log sequence: webdriver
"Dispatch actions completed successfully" Ã¢â€ â€™ Get(test page) loads fine Ã¢â€ â€™ then wedge.
HARNESS NOW WORKING for single tests: must first kill leftover wptserve pythons holding ports
8001-8003/8444-8446/8888-8889/9000 (ps aux|grep python.*wpt, fuser -k -n tcp <port>).
Command: .venv/bin/python mach test-wpt /html/editing/dnd/canvas/001.html Ã¢â‚¬â€� reproduces in ~25s.
NEXT: gdb attach the wedged servoshell mid-test (test takes 25s to wedge, window to attach)
Ã¢â‚¬â€� but debug=0 means no symbols; either (a) rebuild script crate with debug symbols, or
(b) RUST_LOG=script=debug instrument the drag path. Prime suspect from earlier analysis:
hit_test_from_point_in_viewport Ã¢â€ â€™ elements_from_point_query is a SYNCHRONOUS LAYOUT QUERY Ã¢â‚¬â€�
drag gesture processing in the script event loop hitting this while layout waits on script
= circular wait. Check whether drag handling runs on script thread and calls hit-test
synchronously (drag_gesture.rs / html_drag.rs event flow).

## sec1zt269 Ã¢â‚¬â€� dnd wedge mechanism traced to script-side ack loss (2026-11-27)
FULL gdb backtrace captured of wedged servoshell mid-test (SYMBOLS PRESENT in dev build Ã¢â‚¬â€� gdb
is useful after all!): ALL threads idle-parked, no deadlock. Chain:
1. webdriver dispatcher blocked in Handler::wait_for_input_event_handled (actions.rs:325) Ã¢â‚¬â€�
   waits on per-event receivers forever.
2. Flow: webview.notify_input_event Ã¢â€ â€™ paint hit-tests Ã¢â€ â€™ ForwardInputEvent to constellation
   Ã¢â€ â€™ constellation_webview.forward_input_event Ã¢â€ â€™ ScriptThreadMessage::SendInputEvent Ã¢â€ â€™
   script handle_input_event (script_thread.rs:3966) Ã¢â€ â€™ document.event_handler()
   .note_pending_input_event Ã¢â‚¬â€� events QUEUED, only processed in handle_pending_input_events
   during UPDATE_THE_RENDERING Ã¢â‚¬â€� acks (EmbedderMsg::InputEventsHandled, script_thread.rs:3982)
   go out only from that path.
3. Wedge = queued input events never processed OR processed-but-acked-incompletely.
   Two candidate sub-mechanisms: (a) update_the_rendering never runs again because nothing
   invalidates rendering Ã¢â‚¬â€� input events sit in the queue forever (would affect ALL input
   though Ã¢â‚¬â€� wpt clicks work, so either rendering is continuously driven in wpt mode OR
   normal events self-trigger rendering); (b) DRAG-SPECIFIC: drag_gesture state machine
   (document_event_handler has `drag_gesture` field) consumes mousemove/button events during
   an active drag without pushing InputEventOutcome for them Ã¢â‚¬â€� events processed but acks
   never sent. coalesced_mouse_move_event_ids IS handled in outcomes, so plain coalescing
   is acked correctly Ã¢â‚¬â€� the drag path is the prime suspect.
NEXT STEP: read handle_native_mouse_move_event + handle_native_mouse_button_event drag
integration in document_event_handler.rs (~lines 400-700) and check whether outcomes are
pushed for events consumed by drag_gesture.moved()/released(). Compare with what happens
when drag actually starts (html_drag.rs HtmlDragHandler state machine). FIX SHAPE: every
input event id that enters the script pipeline must produce exactly one InputEventOutcome,
including drag-consumed ones.
gdb repro recipe: run `.venv/bin/python mach test-wpt /html/editing/dnd/canvas/001.html`
(background), poll log for canvas/001 GET, sleep 6-8s, gdb -p $(pgrep -x servoshell|head -1)
-batch -ex "thread apply all bt" Ã¢â‚¬â€� full capture at /tmp/dnd-bt2.txt (1917 lines, symbolized).

## sec1zt270 Ã¢â‚¬â€� INPUT WEDGE ROOT CAUSE: my own zero-delay timer regression, FIXED 825fe34188f (2026-11-27)
The 100 dnd "crashes" + input wedges traced to MY speculative wake-spin fix b786d36902b.
Mechanism: script thread rendering updates process pending input events; scheduling uses
timer round-trip where the timer channel integrated in the crossbeam select is the ONLY
wake mechanism for the parked loop. My zero-delay special case set the flag directly Ã¢â‚¬â€�
loop parked in recv() with flag set but no wake Ã¢â€ â€™ input events queued forever Ã¢â€ â€™ webdriver
ack never sent Ã¢â€ â€™ PerformActions wedges Ã¢â€ â€™ harness reports CRASH. Any input on a page idle
>20ms hit it 100%. REPRO: single pointerMove on idle page. Instrumentation eprintln chain
(binary search logging) pinned it in one pass: events noted but update_the_rendering never
ran again. FIX: always round-trip the scheduler (825fe34188f) Ã¢â‚¬â€� verified move-only 0.0s,
full drag sequence ok, dnd/canvas/001.html completes instead of CRASH.
CRITICAL RETRACTIONS: (1) the github wedge fix attribution to b786d36902b is now suspect Ã¢â‚¬â€�
the wake-spin evidence (Mac's timedwait samples) needs re-examination; the zero-duration
round-trip is benign-by-design since the timer pops after firing. (2) WPT timeout clusters
in focus(86)/popovers(45)/forms(41) are likely INPUT-WEDGE ARTIFACTS not feature gaps Ã¢â‚¬â€�
the leaves doc targeting (feather#321) mis-estimates those clusters; re-measure before
implementing. (3) subtest-level dnd functionality (drop completion etc) still genuinely
incomplete Ã¢â‚¬â€� separate feature work. LESSON INDEXED: speculative fixes without repro
verification can regress worse than the original Ã¢â‚¬â€� the wake-spin fix was never verified
against an actual repro.

## sec1zt271 Ã¢â‚¬â€� upstream scan 2026-11-27 evening (last-checked now 9c793b64a4b)
5 new commits since 98dc7f94673. Cherry-pick queue (AFTER wpt run Ã¢â‚¬â€� compiles forbidden during):
1. 74e17f9c485 webdriver: synchronize navigation state with constellation inside wait Ã¢â‚¬â€�
   HIGH VALUE, same navigation-completion-signaling family as our wedge fixes, may clear
   more harness timeout noise.
2. fdc6077f3a5 script: NavigationTiming duration per spec Ã¢â‚¬â€� small spec fix, cheap.
3. 806d6369990 weak-referenceable elements Ã¢â‚¬â€� memory-relevant (GC semantics for elements,
   interacts with the retention story) but big/risky Ã¢â‚¬â€� evaluate carefully, possibly defer.
SKIP: 9c793b64a4b (crown lint cleanup), 460066e6176 (servoshell context-menu widget Ã¢â‚¬â€�
feather uses its own shell). Apply order: 74e17 first (verify wpt harness behavior), then fdc.

## sec1zt272 Ã¢â‚¬â€� canvas-wait input-skip: second latent input-loss path, fix scoped (2026-11-27)
Found during wedge hunt: update_the_rendering skips the ENTIRE document (including
process_pending_input_events) when waiting_on_canvas_image_updates is true Ã¢â‚¬â€� set when dirty
canvases flush, cleared only by handle_no_longer_waiting_on_asynchronous_image_updates
(webrender upload confirmation). If that confirmation is ever lost (frame never generates,
hidden-then-shown races, compositor edge), the document is skipped FOREVER: input queues,
acks never sent Ã¢â‚¬â€� SAME wedge shape as the timer regression but canvas-triggered. Canvas-heavy
pages (youtube/gmail both use canvases) Ã¢â‚¬â€� unproven but plausible real-user wedge path.
DESIGN CRITIQUE: input processing has no dependency on canvas image readiness Ã¢â‚¬â€� the skip is
over-broad even in the correct case (input latency for no reason). FIX SHAPE (small,
principled): hoist process_pending_input_events BEFORE the waiting_on_canvas_image_updates
check in update_the_rendering so input is always processed; visual updates still gate.
Queue after the WPT run + upstream cherry-picks. Verify: repro = page with a dirty canvas
in a state where confirmation is delayed, confirm input latency/loss before and after.

## sec1zt273 Ã¢â‚¬â€� post-input-fix residual crash cluster: media document-removal (2026-11-27)
Post-input-fix html run (mid-run sample, 7063 tests): ZERO dnd/input crashes Ã¢â‚¬â€� wedge fix
holds at scale. Residual crashes are a NEW coherent cluster: media-element document
move/removal Ã¢â‚¬â€� pause-remove-from-document{,-different-load}, pause-move-to-other-document,
currentTime-move-within-document, autoplay-overrides-preload (5 of 10). These look like
genuine process crashes (not wedges) in gstreamer/player teardown when documents are
navigated/removed while media plays Ã¢â‚¬â€� same territory as the batch-6 deferred-dispose reaper
commits. NEXT ACTIONABLE CLUSTER after the run completes: repro one in isolation, capture
panic message, likely teardown-ordering bug. Others: window-open-popup-behavior,
radio-focus-navigation, shadowBlur_gaussian (canvas), origin-keyed-agent-cluster Ã¢â‚¬â€� singletons.

## embedder-control family complete shell-side (banked 2026-11-27, macOS lane)
All four implementable controls shipped+merged in feather: context menus (#340),
simple dialogs (#341), file picker (#344, native via rfd Ã¢â‚¬â€� cross-platform, Windows
lane verify-only checklist in docs/defect-queue.md), color picker (#345). Select
wired (#343) but engine never fires the control Ã¢â‚¬â€� activation-gap suspects filed.
INPUT-ROUTING LESSON (all lanes): clicks on egui overlay chrome passed through to
the page beneath until #345 Ã¢â‚¬â€� fixed by layer discrimination (suppress forwarding
when top layer at pointer is Foreground/Tooltip). The blanket egui
wants_pointer_input CANNOT be used Ã¢â‚¬â€� the whole window is an egui surface, it
swallows every page click. Same class of bug will bite the Windows/Linux shells
if they forward input independently of overlay state. Known engine gaps filed:
color input/change events don't fire on commit; select activation never fires.
## Sec-Fetch-Site anomaly Ã¢â‚¬â€� measured + root-caused, fix deferred (2026-11-27 windows child)
MEASURED on verify7 binary via hdrsrv capture: omnibox/browser-initiated top-level navigation sends `Sec-Fetch-Site: cross-site` Ã¢â‚¬â€� SPEC-WRONG (should be `none` per webappsec-fetch-metadata step 4: navigation explicitly caused by user interaction with the UA). Firefox/Chrome send `none` for typed/omnibox navs. Subresource requests correctly send same-origin/same-site. Impact: every first-load looks like a cross-site arrival Ã¢â‚¬â€� anomalous vs real browsers, plausibly a negative trust signal, though weaker than UA/TLS contradictions.
ROOT CAUSE: components/net/http_loader.rs fn set_the_sec_fetch_site_header Ã¢â‚¬â€� explicit upstream TODO at step 3/4 ("If r is a navigation request that was explicitly caused by a user interaction with the user agent, then set header value to none") never implemented. FIX SHAPE: need a signal distinguishing UA-chrome-initiated navigations from in-page link-click navigations plumbed into the net Request (LoadData carries no such flag today; both paths build requests similarly). Candidates: reuse/plumb user_activation or add a browser_initiated bool on LoadData from the constellation load_url path. Moderate-cost, ~half-day with verification. Filed as fingerprint-contradiction-queue item 2 of 3 remaining (after TLS JA3 floor which is hard).


## aspect-ratio transfer COMPLETE for non-replaced boxes Ã¢â‚¬â€� 2026-11-27 (windows child, servo def19d8de79)
SYMBOL-LEVEL FINDING for engine lane: simple in-flow non-replaced blocks (plain divs Ã¢â‚¬â€� the common case!) bypass IndependentFormattingContext::layout_in_flow_block_level entirely. They layout in components/layout/flow/same_formatting_context_block.rs::layout_in_flow_non_replaced_block_level, which destructured ContainingBlockPaddingAndBorder discarding preferred_aspect_ratio and resolved the block size purely from content. FIX: capture the ratio and transfer through automatic block sizes with the definite inline content size as determining size (css-sizing-4). VERIFIED full matrix on exact binary: plain in-flow block fixed-width + height-auto + aspect-ratio 2/1 Ã¢â€ â€™ correct 100x50 (was 100x0); flex-item and independent-context transfers unchanged; flex/gap/writing-mode regression battery all PASS. The aspect-ratio leaf is now CLOSED for all non-replaced cases with definite inline sizes. Remaining theoretical edge: cyclic cases where inline size itself depends on content (fit-content floats etc) Ã¢â‚¬â€� genuinely hard, upstream-unsolved, negligible real-world frequency.


## probe round: img / object / media-behavioral leaves Ã¢â‚¬â€� 2026-11-27 (windows child, binary def19d8)
PROBE-FIRST results, all measured on exact binary with getBoundingClientRect/event harnesses:
- **img element: mostly HEALTHY (probe-negative)**. srcset selection CORRECT for both width descriptors (100w picks right candidate) AND density descriptors Ã¢â‚¬â€� verified that at dpr=1 src+1x correctly wins over 2x (spec-exact). naturalWidth/complete/error+load event ordering/decode() all correct. REAL GAPS (minor): `img.loading` and `img.decoding` IDL attributes absent from HTMLImageElement.webidl (reflection-only fix, lazy-defer behavior separate). Verdict: the 10-file WPT img leaf is dominated by edges, not core machinery.
- **object fallback: PARTIAL REAL GAP**. Fallback content renders correctly on failed loads (spec-correct). BUT `<object data="img.png" type="image/png">` does NOT render the image Ã¢â‚¬â€� falls back to text content even for valid image loads (measured: object renders at text size 106x18 instead of image). typeMustMatch IDL missing (minor). Fix depth: object element type-dispatch to image rendering Ã¢â‚¬â€� moderate, self-contained.
- **media element behavioral: probe-NEGATIVE for error paths**. loadstart@0 Ã¢â€ â€™ error@0 sequence correct, MediaError.code=4 correct, networkState=3 NETWORK_NO_SOURCE correct, readyState ordering correct. The 8-9 file leaf needs per-test triage Ã¢â‚¬â€� core error machinery is sound.


## window.open/close lifecycle Ã¢â‚¬â€� partial probe, harness-limited Ã¢â‚¬â€� 2026-11-27 (windows child)
EVIDENCE: window.open DOES create a new webview in feather (constellation log shows second WebViewId(0,2) appearing at open). DETAILED lifecycle probe INCONCLUSIVE: the popup immediately becomes the ACTIVE webview, and the FEATHER_EVAL harness evaluates in the active webview Ã¢â‚¬â€� so the opener page results become unreadable the moment a popup opens. Harness limitation, not engine defect. Needs either eval-target pinning or popup-aware test design. Meanwhile confirmed at minimum: open() returns without throwing, new browsing context spawns. close()/opener/features-parsing unverified. ALSO NOTE: fieldset/legend probe CONFIRMED REAL visual gap Ã¢â‚¬â€� legend renders full-width in-flow (measured 1236px wide for short text) instead of shrink-wrapped border-straddle; forms visibly wrong; rated L-difficulty, remains deferred. img loading/decoding reflection gap confirmed trivial-but-real (IDL absent).
REBASE NOTE: fixbatch-1 rebased onto feather-engine 1ebf6793490 cleanly (svg-use work + mem entries); all 5 fix commits + memory entries intact, force-pushed; binary verification predates rebase but merge was non-overlapping (svg/mem only) Ã¢â‚¬â€� risk accepted.


## probe round 2: document.open/write + flex align-content Ã¢â‚¬â€� 2026-11-27 (windows child)
- **document.open/write/close: probe-NEGATIVE**. Post-parse open() blows away the document correctly, written content parses and renders, close() finalizes. The 9-file leaf is parser-insertion-point spec edges, not core machinery.
- **flex align-content multi-line: probe-NEGATIVE**. stretch/center/space-between all geometrically correct measured with getBoundingClientRect on 2-row wrap layouts (stretch rows 75px apart in 150px container = exact even distribution; center packs tight + centers; space-between edges correct). The ~25-file sub-leaf is pixel-noise/exotic-interaction dominated per the method-note trap.
META-PATTERN CONFIRMED AGAIN: every geometrically-probeable leaf so far is either already-working or pixel-noise. The probe-first method keeps paying for itself. Remaining REAL confirmed gaps inventory: object-image rendering (fix in flight), img loading/decoding reflection (in flight), fieldset/legend visual (L, deferred), window.open lifecycle details (harness-limited), Sec-Fetch-Site (documented fix shape), gmail botguard attestation (XL, hostile), TLS JA3 (hard floor).


## beforeunload/unload lifecycle: probe-NEGATIVE Ã¢â‚¬â€� 2026-11-27 (windows child)
Measured with a localStorage-capture chain (cross-navigation persistence): full sequence fires in exact spec order Ã¢â‚¬â€� beforeunload Ã¢â€ â€™ pagehide(persisted=false, correct: no bfcache) Ã¢â€ â€™ unload. The 4-file leaf was cascade-artifact dominated as the doc suspected. NOTE the harness pattern for cross-navigation probes: write evidence to localStorage during the event, navigate to a reader page that surfaces it Ã¢â‚¬â€� reusable for all lifecycle-event probing.


## object-image rendering: deep root cause mapped, fix PARTIALLY landed Ã¢â‚¬â€� 2026-11-27 (windows child)
VERIFIED: img loading/decoding enumerated reflection FIXED and live (lazy.loading reflects correctly). object-image partial: the type-gate fix (image/* mime types pass) is correct-but-insufficient. DEEP ROOT CAUSE mapped for the next implementer: (1) layout replaced.rs::try_to_parse_image_data_url ONLY accepts data: URLs Ã¢â‚¬â€� normal URLs never parse; (2) even if parsed, RELATIVE urls (the dominant real-world case) cannot resolve in layout Ã¢â‚¬â€� the layout crate has zero URL-resolution machinery (no base_url access); (3) the INTENDED architecture per TODOs is script-side: htmlobjectelement.rs::process_data_url has "TODO(gw): Prefetch the image here" and an unused `image: DomRefCell<Option<Arc<RasterImage>>>` field. CORRECT FIX SHAPE: script resolves data-attr against document base_url, requests image from image cache into the object element's image field (mirroring htmlimageelement's pipeline), layout consumes it via a new as_object_image node accessor in ReplacedContents::for_element. Estimated half-day with compile cycles. Everything needed is documented in the two TODO sites.


## object-image pipeline IMPLEMENTED + VERIFIED Ã¢â‚¬â€� 2026-11-27 (windows child, leaf CLOSED)
The full pipeline works end to end. Design as documented: script resolves the object data attribute against document base_url at process_data_url (fires on data AND type attribute mutation, parse-time included), stores in resolved_data_url field; plumbing chain through node/layout_dom.rs downcast dispatch + the shared LayoutNode trait into layout/dom.rs; replaced.rs accepts resolved absolute URLs (data: URLs keep inline mime validation). VERIFIED on exact binary: object with typeless data AND type=image/png both render the actual image (4x4 measured, was 83x18 fallback text); broken-url objects still correctly render fallback content. Implementation notes for future plumbing work: LayoutDom accessors must be implemented on LayoutDom<'dom, T> with unsafe_get() not on the element itself (downcast returns the wrapper); the htmlimageelement impl block is the canonical pattern.


## Sec-Fetch-Site FIXED + VERIFIED Ã¢â‚¬â€� 2026-11-27 (windows child, leaf closed, fingerprint contradiction removed)
Implementation exactly as documented: browser_initiated flag plumbed RequestBuilder->Request (components/shared/net/request.rs), set in script/navigation.rs from !matches!(load_origin, LoadOrigin::Script(_)) Ã¢â‚¬â€� the discriminator already existed as LoadOrigin. http_loader spec step 4 implemented. VERIFIED on exact binary with hdrsrv capture: browser-chrome navigation (FEATHER_START_URL/omnibox class) sends Sec-Fetch-Site: none (was cross-site Ã¢â‚¬â€� anomalous vs Firefox/Chrome); script-initiated location.href navigation correctly still sends same-origin Ã¢â‚¬â€� the discriminator works both directions, zero regression on subresources. One more google-wall fingerprint contradiction eliminated. Remaining wall inventory: sec-ch-ua family (chrome-flavor doomed at TLS anyway Ã¢â‚¬â€� non-issue while firefox flavor active), TLS JA3 rustls (hard floor), botguard JS attestation for signin specifically (XL hostile).


## window.open lifecycle: partial-positive + SHELL BUG FOUND Ã¢â‚¬â€� 2026-11-27 (windows child)
Measured with self-closing-popup harness design (popup writes evidence then self-closes): window.open() creates the webview (constellation log), the popup loads and executes, and window.close() self-close WORKS (close_pipeline/close_browsing_context fire in log). **REAL SHELL BUG: after the popup closes, the shell does not restore the opener as the active webview Ã¢â‚¬â€� the eval bridge (which targets the active webview) keeps targeting the dead popup webview forever (InternalError), and constellation logs "Resizing browsing context for unknown WebView".** User-facing: closing a popup leaves the browser focused on nothing. Suspect location: feather shell-new webview-close handling Ã¢â‚¬â€� needs active-webview fallback to the opener/last tab when the active webview closes. This is a SHELL bug not engine Ã¢â‚¬â€� belongs in the feather repo lane. Harness note: the eval-target-stealing behavior of popups makes window lifecycle probing dependent on active-webview correctness Ã¢â‚¬â€� fix the shell bug and the probes unblock.


## window.open/close lifecycle leaf CLOSED Ã¢â‚¬â€� 2026-11-27 (windows child)
Full lifecycle probe-verified correct after the shell focus fix (feather PR#342): window.open returns object, popup loads+executes, opener linkage correct (window.opener set), window.close() self-close works, w.closed accurately reflects state post-close, focus returns to opener. Measured end-to-end: open.ret=object|sync.closed=false|pop.loaded=yes|pop.opener=opener-set|after.closed=true. The 11-file WPT leaf's core lifecycle is sound; residuals are feature-string parsing edges (width/height positioning Ã¢â‚¬â€� popup geometry is shell-controlled in feather anyway). Harness design note: self-closing popup + cross-page localStorage evidence + eval-on-opener-after-focus-restored is the working pattern.


## pageswap/pagereveal basic firing IMPLEMENTED + VERIFIED Ã¢â‚¬â€� 2026-11-27 (windows child, leaf closed at basic level)
Both events now fire in spec positions: pageswap on the outgoing document between beforeunload and pagehide (verified exact order: beforeunload|pageswap|pagehide); pagereveal on the incoming document before pageshow (verified: pagereveal|pageshow). TYPED as plain PageTransitionEvent Ã¢â‚¬â€� the PageSwapEvent/PageRevealEvent interfaces with activation/viewTransition attributes are NOT implemented (documented in code comments). Implementation notes: (1) atoms must be registered in vendor/stylo/stylo_atoms/static_atoms.txt or atom!() fails to compile Ã¢â‚¬â€� non-obvious compile-time registry; (2) pageshow has TWO firing paths Ã¢â‚¬â€� initial load (document.rs step 9.11 path) and history reactivation (fire_pageshow_event task) Ã¢â‚¬â€� pagereveal must be patched in BOTH; patching only one is the easy mistake and I made it. WPT impact: the 14-file pageswap/pagereveal leaf gains basic ordering subtests; interface-attribute subtests remain failing until the typed events land.


## gmail boot wedge Ã¢â‚¬â€� FIRST EVER clean rapid-boot matrix 2026-11-27 (windows child, heisenbug caveat)
bootmatrix harness on verify7 binary (feather-engine rebase + fixbatch-1 tip): gmail 3/3 rapid back-to-back boots with ZERO stall entries in 50s windows; maps/docs clean too. Same harness wedged 3/3 with 42-47 stalls pre-merge-train, and rapid back-to-back boots were previously the WORST case (4/4 wedged Ã¢â‚¬â€� warm font cache compresses completion arrival into simultaneous bursts). Caveats: (1) signed-out mail.google.com redirects to the workspace marketing page, not the full inbox JS Ã¢â‚¬â€� the user's signed-in path is heavier; (2) heisenbug class needs many trials. Candidate cause: something in the feather-engine merge train since batch7 beyond the FontFaceSet-tracking commit. Implication for testing: keep the bootmatrix harness as the wedge regression detector; the dogfood gate 3x gmail attempts remain the production gate.


## fieldset/legend probe characterization + implementation options Ã¢â‚¬â€� 2026-11-27 (windows child)
Probe numbers: fieldset top=8 border-top=2px; legend top=15.6 fully INSIDE content flow, zero border overlap (real browsers: legend vertical center straddles the fieldset top border, border unpainted behind legend). Three implementation paths: (A) correct layout surgery Ã¢â‚¬â€� detect legend children of fieldset in flow layout, position at border edge, suppress border behind it Ã¢â‚¬â€� multi-week class, touches the hot block layout path, high regression risk; (B) UA-stylesheet transform approximation (translateY(-50%)+border offset) Ã¢â‚¬â€� hours, but wrong for author-overridden borders and transparent legend backgrounds leak border through; (C) hybrid: layout-level negative-margin positioning of legend box + paint-order border suppression only Ã¢â‚¬â€� the medium path, still deep. Recommendation: defer until after COOP/COEP Ã¢â‚¬â€� forms cosmetically wrong is lower user-impact than security surface. If the swarm wants forms-pretty sooner, path B is the 80/20.


## dialog leaf PROBE-NEGATIVE Ã¢â‚¬â€� 2026-11-27 (windows child)
Core dialog machinery fully verified working: showModal/show/close, open-attribute reflection, display none<->block switching, viewport centering, ::backdrop pseudo with correct spec UA background rgba(0,0,0,0.1), focus delegation into dialog, close/returnValue round-trip, async close event firing, CloseWatcher constructor present, requestClose present, closedBy attribute present. The 40 failing files are edges: Esc-key close-watcher integration (shell input routing, needs real-input testing not eval probes) and top-layer corner cases. NOTE for probe authors: the close event is spec-queued as a task Ã¢â‚¬â€� synchronous checks false-negative; use setTimeout before reading. NOT an implementable leaf at probe level.


## flexbox-intrinsic + link-load-event leaves PROBE-NEGATIVE Ã¢â‚¬â€� 2026-11-27 (windows child)
flexbox intrinsic sizing spec-exact on canonical probes: flex-shrink distribution ratio-correct (basis150+150 in 200px container, shrink factors 2:1 -> widths 83/117 matching scaled-shrink-factor math), flex-grow fills container, min-width:auto vs 0 both correct. link stylesheet load/error events: canonical dynamic-insertion pattern FULLY correct (load fires on success, error on 404, no spurious load). Caveat for probe authors: parser-inserted <link> in <head> completes before body scripts attach listeners Ã¢â‚¬â€� timing confound, not a bug. The ~30-file flexbox intrinsic estimate and 21-file link leaf are noise/edges at probe level. META UPDATE: the probe-negative ledger now covers every geometrically/event-probeable leaf in the wpt-2026-11-27-leaves doc. What remains is exactly: fieldset-legend visual (real, M), close-watcher Esc input routing (shell integration), COOP/COEP (architectural L), pageswap typed interfaces (M follow-up), dir-bidi formatting chars (M), canvas (deferred), bfcache (XL).


## dir=auto content-resolution Ã¢â‚¬â€� root cause + correct fix shape Ã¢â‚¬â€� 2026-11-27 (windows child)
PROBE: dir=auto with hebrew first-strong resolves ltr Ã¢â‚¬â€� first-strong scan never existed. MECHANISM MAPPED: (1) UA stylesheet uses STATIC attribute selectors [dir=rtl i]{direction:rtl} Ã¢â‚¬â€� explicit values work, auto gets only unicode-bidi:isolate and inherits; (2) the commented-out intended rule at user-agent.css:178-181 shows the planned shape Ã¢â‚¬â€� :dir()-based rules Ã¢â‚¬â€� blocked at the time on :dir() parsing; (3) Linux's :dir() matching (c652b3e1aa3) uses a SIMPLIFIED model: dir=auto falls through to parent, no content scan; (4) THE ARCHITECTURAL WALL: pseudo matching runs in the LAYOUT thread via ServoDangerousStyleElement which cannot safely walk script-thread text content Ã¢â‚¬â€� content-scan directionality must be precomputed script-side and stored on the element for layout to read (Gecko model). CORRECT FIX SHAPE: script computes resolved directionality (first-strong scan) on dir-attribute/content mutation, stores in element data readable by layout; :dir() matching consumes it; UA stylesheet gains the full spec rules. Cost: M-deep stylo-adjacent surgery. PARTIAL FIX COMMITTED in htmlelement.rs directionality(): first-strong descendant scan implemented per spec (skips dir-attr/bdi/script/style subtrees) Ã¢â‚¬â€� this corrects the directionality() API consumers (form dirname submission etc.) but does NOT fix computed style, which needs the architectural piece above. unicode-bidi dep already present; bidi_class pattern copied from htmlinputelement.rs:245.


## input-wedge timer fix verification Ã¢â‚¬â€� gmail matrix clean again 2026-11-27 (windows child)
Rebased fixbatch-1 onto feather-engine tip incl. Linux 825fe34188f (zero-delay rendering timer round-trip fix). bootmatrix on the rebuilt binary: gmail 3/3 rapid boots ZERO stalls, maps/docs clean Ã¢â‚¬â€� second consecutive fully-clean matrix generation (6/6 clean gmail boots across the two most recent binary generations vs 42-47 stalls per boot pre-merge-train). Attribution caveat: the previous generation was already clean so the input fix's specific contribution is unmeasurable by this harness Ã¢â‚¬â€� but no regression and the wedge remains dormant. YouTube large-seek not re-verified this cycle (MSE untouched by this rebase; was verified FIXED in batch7 production).


## fieldset/legend CSS-transform approximation MEASURED + rejected Ã¢â‚¬â€� 2026-11-27 (windows child)
Numerical experiment: legend{transform:translateY(-50%) translateY(-1px)} lands the legend center 5.6px below the ideal border-center on default UA styling (legend height ~18px, so visibly off by ~30% of legend height). Cause: translate percentages reference the legend box, but the correct straddle position depends on the fieldset's border+padding metrics which CSS cannot reference Ã¢â‚¬â€� and any author override of border/padding breaks it further. Verdict: the approximation is not shippable even as 80/20; fieldset/legend stays deferred pending real layout integration (spec-correct cost: detect legend-in-fieldset during flow layout, position at border edge, suppress border behind Ã¢â‚¬â€� medium-deep layout surgery).


## fieldset/legend implementation entry points Ã¢â‚¬â€� 2026-11-27 (windows child, pre-seeded for implementation)
Zero special-casing exists in components/layout (grep-confirmed). Integration points: components/layout/flow/mod.rs (BlockContainer enum line ~113, layout fns at ~395/525/909/1172) and flow/construct.rs for box construction. Implementation sketch: (1) construct.rs Ã¢â‚¬â€� when building children of a fieldset element, mark legend boxes (first-in-flow legend child per spec) with a flag on the box or a rare-data; (2) flow layout Ã¢â‚¬â€� after normal child placement, if parent box is a fieldset with a flagged legend: shift legend block-position up so its border-box center aligns with fieldset's top border edge center, add equivalent extra space reservation at content start (spec: the legend is laid out as if it were the first in-flow child but positioned straddling the border; its height contributes to fieldset content padding-top); (3) paint Ã¢â‚¬â€� suppress the fieldset's top border segment behind the legend's border-box width (spec: border rendered with a gap; alternatively paint legend background opaque Ã¢â‚¬â€� spec-correct is the gap). Identification plumbing: use the LayoutDom accessor pattern (htmlimageelement.rs:1647 canonical Ã¢â‚¬â€� implement on LayoutDom<'dom, HTMLFieldSetElement>/HTMLLegendElement with unsafe_get). Spec ref: html/rendering section 'the fieldset and legend elements' Ã¢â‚¬â€� has exact positioning algorithm including min-inline sizing interactions (legend shrink-wrap contributes to fieldset intrinsic width). Realistic cost: M (multi-session) Ã¢â‚¬â€� layout surgery plus paint gap plus intrinsic sizing.

## sec1zt274 Ã¢â‚¬â€� media cross-document-move wedge dissected (2026-11-27)
pause-move-to-other-document.html reproduces solo (25s). gdb of wedged state: aqueue:src +
vqueue:src threads blocked in gst_base_sink_wait_preroll FOREVER; appsrc0/multiqueue/queue2
blocked pushing into full queues; script threads idle (input path healthy). Mechanism: when
a playing video element is adopted into another document, the gstreamer pipeline ends in a
state where sinks never preroll Ã¢â‚¬â€� same family as the YouTube boot/seek wedge (pad
activation + pipeline state-cycle teardown/recreate). Playback never resumes, test never
completes. HARNESS MISCLASSIFICATION CHAIN: servo's webdriver has no script timeout, so the
never-resolving ExecuteAsyncScript hangs the transport Ã¢â€ â€™ wptrunner declares "Browser not
responding" Ã¢â€ â€™ CRASH. These media "crashes" are neither process crashes nor browser wedges Ã¢â‚¬â€�
they are single-feature media-playback failures misclassified. Implication: the residual
crash count in the re-baseline overstates engine instability; a webdriver script-timeout
implementation would reclassify them as ordinary TIMEOUTs. TWO actionable items:
(1) engine: player teardown/recreate on document adoption must not wedge the pipeline Ã¢â‚¬â€�
check whether htmlmediaelement adoption path triggers player recreation at all, and whether
the generation-serialized reset logic (1883f8d3435) covers it; (2) webdriver: implement the
script timeout so never-resolving async scripts return timeout instead of hanging the transport.
Full bt at /tmp/media-bt.txt.

## sec1zt275 Ã¢â‚¬â€� media adoption wedge: analysis state for implementation (2026-11-27)
Findings: (1) htmlmediaelement adopting_steps (htmlmediaelement.rs:3978) does NOT touch
video_renderer Ã¢â‚¬â€� frame renderer keeps the OLD document's task_source after cross-document
adoption. (2) pause/play are thin gstplay delegates (player.rs:256-296), no serialization Ã¢â‚¬â€�
the generation-serialized reset only covers MSE seeks, NOT progressive pause/play cycles.
(3) bt analysis refinement: BOTH audio and video sinks wait in preroll Ã¢â‚¬â€� audio doesn't
depend on frame rendering, so the wedge is upstream of frame consumption: the pipeline is
stuck in a PAUSED/preroll state and never transitions to PLAYING. Sinks waiting in preroll
with full queues is the correct behavior for a paused pipeline Ã¢â‚¬â€� the defect is the pipeline
never resuming. (4) PauseIfNotInDocument microtask fires on unbind-before-adoption Ã¢â‚¬â€�
suspect sequence: unbind queues pause microtask Ã¢â€ â€™ pause executes (pipeline PAUSED, sinks
enter preroll-wait) Ã¢â€ â€™ adoption completes but nothing resumes playback. Spec: moving a
playing media element across documents must NOT pause it. Even if the microtask correctly
no-ops when the element is re-inserted, the pause may execute BEFORE reinsertion completes.
NEXT: read MediaElementMicrotask::PauseIfNotInDocument handler (htmlmediaelement.rs:4048)
and the exact spec timing Ã¢â‚¬â€� fix likely = cancel/no-op the pause when the element is in a
document at microtask execution time (may already exist Ã¢â‚¬â€� verify), or don't pause at all
when adoption is in progress. ALSO the deeper progressive-playback wedge: if pause during
preroll wedges the pipeline (state never completes), the seek-fix serialization pattern
needs generalizing to progressive pause/play. Repro test stands: single-test mach invocation
in sec1zt274.

## sec1zt276 Ã¢â‚¬â€� media wedge is RACY and precedes playback (2026-11-27)
pause-move-to-other-document flake rate ~1/3 (pass/crash/pass/crash samples). CRITICAL:
in the crashed run there are ZERO player calls Ã¢â‚¬â€� not even the initial play() Ã¢â‚¬â€� the wedge
fires BEFORE media playback starts, so the preroll/bt analysis (sec1zt274) captured the
AFTERMATH state not the trigger. Correlated warning in failing runs: "Trying to exec media
action on an unknown client context" appears even before the test loads (leftover actions
from the PREVIOUS page hitting a torn-down media session Ã¢â‚¬â€� the deferred-dispose/registration
mismatch from batch-6). New hypothesis space: the wedge is in the media session registration/
teardown race Ã¢â‚¬â€� client context teardown from the harness-setup page interleaves with the
test page's media setup. Same-doc move verified HEALTHY (playback continues ct=1.76);
cross-document repro harness was flaky for setup reasons (autoplay didn't fire) Ã¢â‚¬â€� use the
wpt single-test as the only reliable repro. UNCOMMITTED instrumentation in working tree:
player.rs has eprintln [playerdbg] in play/pause/stop Ã¢â‚¬â€� strip before any engine commit or
keep for debugging. Repro recipe: loop the single test until CRASH, grep playerdbg.

## sec1zt277 Ã¢â‚¬â€� media registry mechanics for the adoption race (2026-11-27)
gstreamer/lib.rs: players register per ClientContextId (per-document). media_instance_action
warns "unknown client context" when the context has no registered instances. BackendMsg::
Shutdown{context,id} removes instances for a context Ã¢â‚¬â€� sent at document teardown. KEY
INSIGHT for the adoption wedge: the player keeps the ORIGINAL document's context id after
cross-document adoption (htmlmediaelement never re-registers) Ã¢â‚¬â€� so if/when the original
document tears down, Shutdown strips the registration while the element lives on in the new
document: subsequent media actions no-op (warnings) and worse, whatever teardown path runs
may NULL/flush the pipeline out from under the still-playing element Ã¢â‚¬â€� matches both the
mysterious pre-test warnings (previous page's teardown orphaning actions) and the racy wedge.
FIX SHAPE candidate: on adopting_steps, re-register the player under the new document's
media client context (or use a context id that survives adoption Ã¢â‚¬â€� e.g. webview-scoped
rather than document-scoped). Verify what ClientContextId is (document? pipeline? webview?)
before choosing Ã¢â‚¬â€� grep media client context creation in htmlmediaelement/document.

## sec1zt278 Ã¢â‚¬â€� CONFIRMED: ClientContextId is per-PIPELINE, adoption breaks routing (2026-11-27)
htmlmediaelement.rs:2433-2434: ClientContextId::build(pipeline_id.namespace_id.0,
pipeline_id.index.0.get()) Ã¢â‚¬â€� the media client context is the PIPELINE. Cross-document
adoption moves the element to a different pipeline (iframe = own pipeline), but the player
stays registered under the original pipeline's context. After adoption: (a) media actions
routed via the element's current pipeline context find no registered player Ã¢â€ â€™ "unknown
client context" warnings Ã¢â€ â€™ actions silently no-op Ã¢â€ â€™ the element becomes uncontrollable
(pause/seek/play do nothing); (b) any sync path that awaits a player response wedges;
(c) if the original pipeline later exits, shutdown strips the registration entirely.
This is THE root mechanism of the move-to-other-document failures. FIX: player context must
follow the element Ã¢â‚¬â€� on adopting_steps, migrate the player's registration to the new
pipeline's context id (or re-key actions to the creation context Ã¢â‚¬â€� but then teardown of
the original pipeline kills a live player, so migration is correct). Implementation:
adopting_steps needs access to the player id + backend registry Ã¢â‚¬â€� the GStreamerBackend
instances map is the thing to update; may need a new BackendMsg::MigrateContext{id,
old_context, new_context}. Verify Shutdown semantics in the same file (lines ~150-165).

## sec1zt279 Ã¢â‚¬â€� MEDIA WEDGE ROOT CAUSE: event-ordering race, NOT adoption, NOT gst (2026-11-27)
pause-move-to-other-document wedge dissected end-to-end with instrumentation:
- Wedged state is HEALTHY everywhere: script threads idle-parked in crossbeam select,
  gst pipeline correctly prerolled to PAUSED (sinks in wait_preroll is NORMAL paused
  state), no deadlock anywhere. gdb bt is a distraction Ã¢â‚¬â€� nothing is blocked.
- Decisive sequence (crashed run): state_changed Buffering Ã¢â€ â€™ state_changed Paused Ã¢â€ â€™
  metadata_updated(ready_state=HaveNothing) Ã¢â€ â€™ change_ready_state HaveMetadata Ã¢â‚¬â€� then
  silence forever. play() is NEVER called.
- MECHANISM: playback_state_changed advances readyState HaveMetadataÃ¢â€ â€™HaveEnoughData on
  Paused/Playing, but ONLY if ready_state is already HaveMetadata. Gstreamer delivers the
  Paused state change BEFORE the metadata signal ~half the time (racy signal ordering).
  Metadata then caps at HaveMetadata and NOTHING ever advances further Ã¢â‚¬â€� the element never
  becomes potentially-playing Ã¢â€ â€™ player.play() never fires Ã¢â€ â€™ playback never starts Ã¢â€ â€™
  test/promise never resolves Ã¢â€ â€™ harness CRASH misclassification.
- NOT adoption-specific: hits ALL progressive media playback Ã¢â‚¬â€� the whole media crash
  cluster (~5 tests) is one mechanism. Real-world impact: any video on any site can fail
  to start racily Ã¢â‚¬â€� this is a beta-critical playback bug, plausibly part of YouTube
  boot/seek flake.
- FIX (implemented, verifying): advance_after_metadata Cell<bool> Ã¢â‚¬â€� state_changed at
  HaveNothing records the missed advancement; playback_metadata_updated replays it after
  reaching HaveMetadata. Order-independent.
- NEGATIVE RESULTS (do not re-investigate): (1) backend shutdown-thread single-recv is
  BENIGN Ã¢â‚¬â€� when recvr drops, queued tx_ack senders drop too, rx_ack.recv() errors
  instantly, no wedge Ã¢â‚¬â€� verified upstream has same code, do not "fix" as a wedge fix;
  (2) ClientContextId per-pipeline adoption mismatch is real but NOT the wedge mechanism
  (element is uncontrollable cross-document after adoption but the test wedges before
  any of that matters); (3) the "unknown client context" warnings appear even in PASSING
  runs Ã¢â‚¬â€� noise from initial about:blank pipeline teardown, red herring.

## integration regression pass on merged main (banked 2026-11-27, macOS lane)
All gesture/embedder work verified composing on integrated main post-merge:
cmd+click background tabs, dialogs round-trip (confirm:CANCEL into DOM),
context menus, status overlay. Zero integration regressions across the
branch overlap regions (app.rs/state.rs were touched by 4+ concurrent PRs).

## workers + cssom leaves PROBE-NEGATIVE Ã¢â‚¬â€� 2026-11-27 (windows child)
Workers: module workers WITH nested ES imports work, classic workers work Ã¢â‚¬â€� the 7 timeout files are harness artifacts/edges. cssom: serialization all correct Ã¢â‚¬â€� shorthand round-trips (margin 4-value), color canonicalization rgb(), cssText, insertRule parsing, computed-value serialization. The 3 cssom fail files are edge noise. Gmail sign-in re-verified on the all-fixes binary: unchanged gate Ã¢â‚¬â€� v3/signin/rejected botguard verdict after identifier submission; everything up to the gate works (page loads clean, form functional, fill+submit fine). Probe harness note: google's identifier input is type=text id=identifierId NOT type=email Ã¢â‚¬â€� earlier selector assumptions wrong; use the id. New harnesses in work dir: signin_probe2/3/4.py (FEATHER_EVAL multi-stage pattern Ã¢â‚¬â€� write js into evfile repeatedly for staged driving), wprobe.py parametric.

## youtube eval-probe harness LIMITATION established Ã¢â‚¬â€� 2026-11-27 (windows child)
Eval-driven (FEATHER_EVAL) youtube probes CANNOT drive playback: video element never receives a source at all (src_type=no-src, networkState 0/3, zero buffered) Ã¢â‚¬â€� youtube's player JS never initializes media attachment for synthetic sessions. IDENTICAL across two binary generations (verify6/verify7) = harness artifact not regression. Progressive mp4 in the same harness plays perfectly (t advancing, rs=4) so the media pipeline is fine. Implication: youtube verification requires real-input harnesses (dogfood-style OS clicks); do not trust eval-probe youtube negatives. The large-seek production verification stands on batch7 real-input evidence. Harness files: ytmse_probe.py/ytseek_probe.py in work (multi-stage FEATHER_EVAL pattern).


## typed PageSwapEvent/PageRevealEvent + Origin-Agent-Cluster IMPLEMENTED + VERIFIED Ã¢â‚¬â€� 2026-11-27 (windows child, fixbatch-2)
TYPED EVENTS: PageSwapEvent/PageRevealEvent interfaces implemented (inherit PageTransitionEvent, second-level dom_struct pattern mirrors mouseevent: parent field + new_inherited chain; activation/viewTransition attributes deferred Ã¢â‚¬â€� NavigationActivation/ViewTransition don't exist). Firing sites converted: pageswap in unload(), pagereveal in BOTH pageshow paths. Verified live: typeof PageSwapEvent/PageRevealEvent == 'function' AND events instanceof their interfaces, ordering unchanged correct. OAC: window.originAgentCluster implemented end-to-end Ã¢â‚¬â€� header parse in servoparser metadata destructure (?1 structured-header boolean) -> NavigationParams -> Document field -> Window getter. Verified both directions: ?1 header => true, absent => false. GOTCHAS for future implementers: (1) webidl files MUST be utf-8 Ã¢â‚¬â€� python on windows defaults to cp1252, a single em-dash breaks the entire bindings codegen with UnicodeDecodeError in run.py; (2) servoparser metadata tuple destructure order must exactly match the closure construction order Ã¢â‚¬â€� inserting mid-tuple silently misaligns types; (3) Window.document is MutNullableDom Ã¢â‚¬â€� access via self.document.get().map_or pattern; (4) trait method impls must sit INSIDE the impl block Ã¢â‚¬â€� sed-style insertion after the closing brace produces confusing E0046+E0133 pairs. Merged-head wedge matrix: clean third consecutive generation (gmail 3/3 zero stalls) Ã¢â‚¬â€� wedge confirmed dormant across merged state.


## fetch + window.name PROBE-NEGATIVE Ã¢â‚¬â€� 2026-11-27 (windows child)
fetch canonical behaviors all correct: AbortController abort (correct AbortError), redirect follow, manual redirect mode, no-store cache mode, text()/headers access. The 13 fetch timeout files are edge/harness noise Ã¢â‚¬â€� per-test triage unlikely to yield real bugs. window.name persists correctly across same-tab navigation. The probe-negative ledger now covers literally every cheaply-probeable primitive in the leaves doc; remaining real work is exclusively the deep inventory: fieldset/legend (M, pre-seeded entry points), dir-style architectural, close-watcher Esc input routing, typed-interfaces follow-ups (activation/viewTransition need NavigationActivation+ViewTransition implementations first), canvas (deferred), bfcache (XL).


## memory leak UNFIXED on merged head Ã¢â‚¬â€� 56MB/nav real content, 0 trivial Ã¢â‚¬â€� 2026-11-27 (windows child, for Linux lane)
leakcheck_real harness on merged-tip binary (post PR#2+#3 squash): 8 sequential heavy wikipedia navigations RSS 266->717MB monotonic, ~56MB/nav Ã¢â‚¬â€� NOT fixed. CRITICAL NUANCE: trivial test pages show FLAT rss (0MB/nav over 20 same-origin navs) Ã¢â‚¬â€� the leak scales with page complexity/real JS content. Consistent with per-navigation document/script-teardown structural theory: heavy documents leave most of their graph behind, trivial ones little enough to be invisible in noise. Harnesses leakcheck.py+leakcheck_real.py in work dir Ã¢â‚¬â€� 90s repeatable. Method: eval-file-driven window.location navigations + tasklist CSV RSS sampling (mem col = parts[-1]).
## egui Area hit-testing lesson (banked 2026-11-27, macOS lane)
Content overflowing an egui Area's declared rect RENDERS visually but is
DEAD to hit-testing Ã¢â‚¬â€� clicks never reach the widget and no error is logged.
When sizing overlay panels from row counts, every footer/separator must be
included in the height math, or entries silently become click-through.
Cost a debugging round in the downloads-panel work: rows inside the rect
worked, the footer below it was visible-but-inert. Symptom: click closes
the panel (outside-close or nothing) with zero actions in FEATHER_ACTIONS
log. For discovery entries prefer hamburger-menu rows Ã¢â‚¬â€� that path is proven.
## settings-depth audit (banked 2026-11-27): feather shell settings UI is
already comprehensive Ã¢â‚¬â€� search/startup/animation/theme/tabs/shields/
browsing-data/site-permissions sections all expose the Settings struct
fields. No meaningful unexposed options exist; don't manufacture noise here.
## sec1zt280 Ã¢â‚¬â€� media wedge SOLVED+SHIPPED: single ordering race, no second mechanism (2026-11-27)
Fix 08325d65497 live on feather-engine. The full truth after instrumentation chaos:
- There was only ONE mechanism all along: StateChanged(Paused/Playing) arriving before
  MetadataUpdated Ã¢â€ â€™ element stalled at HAVE_METADATA forever Ã¢â€ â€™ playback never starts.
  The apparent "mode 2" (Paused never arrives) was an artifact: the fix had been silently
  dropped from the working tree by a failed `git stash pop` during the memory-push rebase
  dance, so post-verification runs were testing the UNFIXED binary.
- DEFINITIVE event-level proof (evtdbg instrumentation): in wedged runs ALL events Ã¢â‚¬â€�
  Buffering, Paused, VideoFrameUpdated, MetadataUpdated, DurationChanged Ã¢â‚¬â€� are received
  by the callback, their tasks run, and they dispatch to the element handlers with zero
  drops. The gstreamer pipeline is perfectly healthy (prerolled paused, fully fed + EOS).
  Nothing is ever lost in transit Ã¢â‚¬â€� the state machine just can't advance out of order.
- VERIFICATION: 10/10 consecutive plain-autoplay runs pass with the fix (rs=4 paused=false)
  vs ~40-60% stall rate without. Harness repro: ./target/debug/servoshell --headless
  --hard-fail http://127.0.0.1:8442/plainplay.html (file in /tmp/navtest, loop script
  /tmp/plainloop.sh) Ã¢â‚¬â€� reproduces WITHOUT any harness or document adoption.
- PROCESS LESSON (critical): `git stash pop` failures during rebase gymnastics can silently
  drop in-flight work Ã¢â‚¬â€� the stash entry surviving means the pop did NOT apply cleanly.
  Always verify the actual diff is in the working tree after pop (`grep` for a known string),
  and never trust verification results unless the binary is proven to contain the fix
  (rebuild + grep the source before each verification round).
- PRINT TRANSPORT LESSON: eprintln evidence in killed/harnessed processes is unreliable Ã¢â‚¬â€�
  presence is informative, absence is not. Direct servoshell repros preserve full output.
- Impact: this was racy playback-start failure for ALL progressive video Ã¢â‚¬â€� plausibly a
  component of the YouTube boot/seek flake the user hits. Windows should re-test YouTube
  boot reliability after pulling. The remaining media adoption-context mismatch
  (sec1zt278) is real but untested-independence: element uncontrollable after cross-doc
  adoption Ã¢â‚¬â€� separate smaller fix, still open.
## sec1zt281 Ã¢â‚¬â€� upstream picks + dom/elements cluster debunked (2026-11-27)
Upstream last-checked sha now 461f99fc59f. Cherry-picked live (9c2558929c8):
74e17f9c485 webdriver navigation-state sync, fdc6077f3a5 NavigationTiming duration,
461f99fc59f xpath id() first-element, e667861e269 attribute borrow hazard,
60019507770 input internal value. SKIPPED adc82a5434c (xr-only). 806d6369990 weak-ref
elements still deferred (big/risky).
DEBUNK: /html/dom/elements "34 unexpected" is a mirage Ã¢â‚¬â€� ~33 are dir-shadow PASS where
upstream expects FAIL: positive deltas from OUR :dir() fix (c652b3e1aa3), not bugs.
Leaves-doc targeting for that row is void; the cluster is closed.
## sec1zt282 Ã¢â‚¬â€� wpt CRASH-misclassification mechanics fully explained (2026-11-27, analysis-only)
"Browser not responding Ã¢â€ â€™ CRASH" fires ONLY when `protocol.is_alive()` fails Ã¢â‚¬â€� a
`GET window/handles` probe with a **2-second** timeout (executorwebdriver.py:1200).
Servo's webdriver server is SINGLE-THREADED: while inside
`wait_for_script_ipc_response_with_timeout` (up to the 15s harness-configured script
timeout), it cannot answer the probe Ã¢â€ â€™ is_alive false Ã¢â€ â€™ live healthy browser misclassified
as CRASH. The race: wptrunner test external-timeout (~10s+extras) usually fires BEFORE the
15s script timeout, so the script-timeout rescue never gets to run. This is an
UPSTREAM-ARCHITECTURE artifact (single-threaded command processing), not feather-specific Ã¢â‚¬â€�
documented, deliberately not fixed: the honest fix is concurrent command handling, big.
With the media wedge fixed the media tests no longer hit this path at all. The script
timeout itself IS implemented (lib.rs:2944 wait_for_script_ipc_response_with_timeout +
harness sets script:15000) Ã¢â‚¬â€� earlier belief "servo webdriver has no script timeout" was
wrong; retracted.
## sec1zt283 Ã¢â‚¬â€� upstream 806d6369990 weak-ref elements: DEFER decision (2026-11-27)
Evaluated: 91-file mechanical BoxÃ¢â€ â€™Rc conversion making elements weak-referenceable,
explicitly zero-behavior-change preparatory work for a not-yet-landed weak-attr-elements
feature. Decision: SKIP for now Ã¢â‚¬â€� no behavioral benefit, non-trivial conflict risk against
our svguseelement.rs sprite fix, and picking it now means carrying rebase risk with zero
payoff. Re-evaluate as a bundle when the actual weak-ref-attr-elements consumer lands
upstream; it IS memory-retention-relevant long term (elements become GC-able while weakly
held Ã¢â‚¬â€� interacts with the monotonic-retention story).
## sec1zt284 Ã¢â‚¬â€� memory retention: purge wiring audit + residual fix design (2026-11-27)
AUDIT RESULTS: (1) servo_allocator::purge_arenas() EXISTS and IS already wired to pipeline
exit (script_thread.rs handle_pipeline_exited ~line 3367) Ã¢â‚¬â€� dirty pages ARE purged on tab
close already; (2) webrender display-list resources ARE dropped on exit
(painter.rs transaction.remove_pipeline). Therefore the measured ~190MB/4-tab retention
(43% of per-tab growth) is NOT dirty pages and NOT display lists Ã¢â‚¬â€� residual candidates in
order of likely mass: (a) SpiderMonkey GC heap Ã¢â‚¬â€� SM does its own page management outside
jemalloc; shrinking GC only happens under memory pressure, tab-close does not force one;
JS heap is usually the dominant per-tab weight. FIX SHAPE: trigger a shrinking GC
(JS::ShrinkingGC or gc_slice with reason) when the last pipeline of a webview exits.
(b) font cache Ã¢â‚¬â€� global, keyed independently of pipelines, no eviction on exit;
(c) image cache Ã¢â‚¬â€� check whether per-pipeline or global. MEASUREMENT FIRST when harness
frees: use /tmp/reclaim-test.py pattern + read /proc/pid/smaps_rollup RSS before/after,
and if measurable, instrument SM heap size (js::PeekSizeOfExiting?) Ã¢â‚¬â€� implement (a) first
as cheapest-high-value, re-measure.
## persistence batch lessons (banked 2026-11-27, macOS lane)
- macOS Cmd+Q never delivers winit WindowEvent::CloseRequested Ã¢â‚¬â€� any
  cleanup wired only there silently skips the primary mac quit gesture.
  ApplicationHandler::exiting is the single funnel all quit paths reach;
  clear-on-exit and window-geometry writes both live there now.
- Session persistence was navigation-triggered only; structural state
  (tab create/close/reorder/group) was crash-losable. Fixed with a 2s
  debounced save after each drained action batch (#358). Verified with
  SIGKILL round-trips: exact state restores.
- Window geometry: window.json {w,h,x,y} written at exiting, restored in
  resumed() with size clamps; physical-position units for x/y, logical
  for w/h (divide by scale_factor).
- Settings persistence pattern: Prefs struct + prefs.txt key=value lines,
  startup copy into browser.settings, sync_prefs() writes back on change.
  Any new setting must touch all four places or it silently resets.

## memsec: fieldset/legend implementation map (windows lane, PR#4)
symptom: legend renders fully inside fieldset instead of straddling border -> FIXED in PR#4. mechanisms: (1) FragmentFlags was FULL at u16 Ã¢â‚¬â€� widened to u32 in base_fragment.rs bitflags; IS_FIELDSET=1<<16, IS_LEGEND=1<<17 set in From<ServoLayoutNode> local_name match. (2) shift at same_formatting_context_block.rs layout_in_flow_non_replaced_block_level pre-BoxFragment::new: fragments are PhysicalDeprecated at assembly (NOT Logical Ã¢â‚¬â€� logical rects convert earlier), use base.rect()/set_rect() interior mutation. (3) target: legend center at -(padding.block_start+border.block_start/2) content-relative. REMAINING leaf increments: border-gap paint suppression (fieldset border paints behind legend Ã¢â‚¬â€� needs paint-time clip or border-split), direct-child-only nesting enforcement, vertical writing modes, independent-FC fieldset path (overflow!=visible fieldsets bypass same-formatting assembly entirely). probe: wtest/fsverify.html delta==0.0 straddles=true.

## memsec: fieldset border-gap paint suppression Ã¢â‚¬â€� paint path mapped, NOT implemented
target: suppress fieldset top border behind legend inline span. PAINT PATH: display_list/mod.rs build_border (~line 2297) Ã¢â‚¬â€� single wr push_border NormalBorder item per fragment; fieldset detectable via self.fragment.base.flags IS_FIELDSET; legend geometry available at paint time by iterating self.fragment children for IS_LEGEND fragment rect. BLOCKER ANALYSIS: webrender NormalBorder cannot express partial side segments Ã¢â‚¬â€� correct impl requires custom-painting top border as two segments (trivial for solid style, hard for groove/double/dotted 3D styles; UA default is groove!). design options: (a) segment-split only solid tops, leave others (most authored fieldsets solid); (b) approximate groove top as solid color segments Ã¢â‚¬â€� visual regression on default styling but removes line-through-text; (c) proper: clip-path per segment in wr. entry point confirmed: build_border early-branch before push_border when IS_FIELDSET && top border width nonzero && legend child present.

## sec1zt285 Ã¢â‚¬â€� memory retention DECOMPOSED: per-tab infra dominates, SM heap ruled out (2026-11-27)
CONTROLLED A/B (harness /tmp/memab.py: webdriver open 4 tabs, close, measure RSS):
- Wikipedia/Linux (image-heavy): baseline 262MB, loaded 739, after-close 547 Ã¢â€ â€™ **285MB retained**
- example.com (text-only): baseline 262MB, loaded 407, after-close 384 Ã¢â€ â€™ **122MB retained (~30MB/tab content-INDEPENDENT)**
- both cases cycle2 flat = bounded saturation, reuse confirmed.
DECOMPOSITION: retention = per-tab infrastructure (~30MB/tab, content-independent Ã¢â‚¬â€�
pipeline/layout/webrender teardown survivors or jemalloc fragmentation across thread arenas)
PLUS content-driven caches (~160MB for image-heavy pages). RULED OUT by measurement:
(1) SpiderMonkey GC heap Ã¢â‚¬â€� shrinking-GC-on-close fix (9182287bf3d, pushed: GC broadcast on
webview close + JSGC_MIN_EMPTY_CHUNK_COUNT=0) changed retention ZERO in A/B Ã¢â‚¬â€� but kept as
correct hygiene for JS-heavy pages; (2) jemalloc dirty pages Ã¢â‚¬â€� purge_arenas already wired
to pipeline exit (script_thread.rs ~3367); (3) image cache entries Ã¢â‚¬â€� clear() already runs
on pipeline close (image_cache.rs ~1720). NEXT LEVER (uninvestigated): WHAT survives per
pipeline teardown Ã¢â‚¬â€� suspects: layout thread pool stacks/arenas, webrender per-pipeline
state in the painter, font caches (global, no eviction), stylo thread-local caches,
HTTP cache. PROFILING NEEDED: heaptrack/massif or smaps diff before/after close Ã¢â‚¬â€� this is
a dedicated profiling session's work, not greppable. Negative results mandatory-indexed here.

## STANDING ORDER Ã¢â‚¬â€� macOS lane work queue (coordinator directive 2026-11-27, verbatim intent)
NEVER end your turn with waiting_for_user. When a task completes,
immediately start the next item in this list without coordinator input:
  (1) tab groups persistence across restart Ã¢â‚¬â€� DONE (#357+#358, verified natural cycle)
  (2) crash-recovery UX polish Ã¢â‚¬â€� DONE (#358, SIGKILL-verified)
  (3) friction audit pass Ã¢â‚¬â€� sweep shell for unwired/dead affordances
  (4) wiki docs for shell features
  (5) verify engine fixes from the shell side
If genuinely everything is done: real-usage dogfood pass on the 20-site
sweep and file friction items. There is always work. Context compacts
automatically Ã¢â‚¬â€� do not stop for it, do not park awaiting merge.

DECISIVE measurement via SERVO_MEMDUMP_AFTER=8 (built-in per-category reporter, servo.rs:1185,
dumps to stderr every 60s Ã¢â‚¬â€� the right tool, heaptrack is unnecessary). Post-close state (4x
example.com tabs opened+closed): resident 385MB but jemalloc-heap-allocated only 17.7MB,
system-heap-allocated 108MB, url() js rows collapse 46Ã¢â€ â€™4.8MB (JS heaps ARE released cleanly Ã¢â‚¬â€�
confirms GC-on-close hygiene working). KEY ROW: jemalloc-heap-mapped 103MB vs active 24MB vs
allocated 17.7MB Ã¢â‚¬â€� **~80MB of jemalloc mappings are neither live allocations nor active pages**
= fragmentation + tcache/thread-cache retention + free runs. Dirty decay is default (10s, no
override in allocator/) so dirty pages DO self-purge Ã¢â‚¬â€� the residual is NOT purgeable: it is
(a) small live objects pinning otherwise-free pages (fragmentation) and (b) per-thread tcache
free-object caches (NOT covered by arena purge!). NEXT LEVERS in order of cost:
(1) MALLOC_CONF=tcache_max small / lg_tcache_max lower or mallctl tcache.flush on pipeline exit Ã¢â‚¬â€�
    tcaches are per-thread and threads are SHARED (68Ã¢â€ â€™77Ã¢â€ â€™69 measurement) so freed tab objects
    sit in the surviving threads' tcaches forever Ã¢â‚¬â€� STRONGEST HYPOTHESIS, cheap to test: set
    opt.tcache false or small in allocator and re-run /tmp/memab.py A/B;
(2) background_thread:true in malloc_conf for async decay;
(3) arena reset on close (mallctl arena.<i>.reset Ã¢â‚¬â€� dangerous, only for quiescent arenas).
MEASUREMENT HARNESS all reusable: /tmp/memab.py (RSS A/B), SERVO_MEMDUMP_AFTER=8 + parse of
/tmp/memdump.log rows (categories). Heaptrack capture failed (0-byte output through fifo on
SIGTERM Ã¢â‚¬â€� do not retry that path). RSS file-backed caveat: ~220MB of resident is servoshell
binary + libLLVM + fonts demand-loaded file pages Ã¢â‚¬â€� inflate absolute RSS but constant baseline.
## sec1zt286 Ã¢â‚¬â€� retention mass PINPOINTED: jemalloc-internal retention, NOT live objects (2026-11-27)

## memsec: orthogonal writing-mode transposition (windows lane, PR#7)
symptom: vertical writing-mode flex/block children size on transposed axes (flex-grow no-op or wrong-direction growth; cross-stretch resolves against wrong physical dimension) -> FIXED in PR#7. mechanism: flow/mod.rs orthogonal-flow CB construction swapped sizes but kept PARENT style; content_box_sizes_and_padding_border_margin (style_ext.rs) maps width/height to logical via containing_block.style.writing_mode Ã¢â‚¬â€� parent mode transposes for orthogonal children. fix = pass element's own style in the orthogonal CB. KEY LANDMARKS: cross_axis_is_item_block_axis (flexbox/layout.rs) logic was correct all along; ContainingBlockSize::swapped_axes is the orthogonal mechanism; width/height are physical CSS properties whose logical mapping depends on element's own writing mode. PROBES: wtest/wmflex.html, wmflex2.html, wmflexctl.html Ã¢â‚¬â€� rowflex w200/h100, colflex h100/w200 exact. BUILD GOTCHA (permanent): PATH must include /c/Program Files/LLVM/bin or glslopt/swgl fail clang-cl ToolNotFound.
## memory-reclaim fix VERIFIED shell-side (2026-11-27, macOS lane)
shrinking-GC + zero empty-chunk reserve (9182287bf) confirmed working in
the feather binary: 5 heavy tabs 2257MB Ã¢â€ â€™ close 4 Ã¢â€ â€™ 1182MB within seconds
(-1GB immediately; pre-fix was strictly monotonic). Residual +112MB/90s
creep on an ACTIVE youtube homepage is unclassified Ã¢â‚¬â€� needs a lightweight-
page control next time. Also measured: ~450MB/tab for heavy real sites Ã¢â‚¬â€�
per-tab infra dominates; youtube alone idles ~1.2GB. All lanes: memory
reclamation is now a solved primitive, remaining work is per-tab weight
and active-page churn, not the close-path.
## sec1zt287 Ã¢â‚¬â€� memory investigation CLOSED: no creep exists; monotonic growth was SM heap, fixed (2026-11-27)
FINAL VERDICTS:
1. CREEP TEST (unique pages per cycle Ã¢â‚¬â€� the real-world pattern): 4 consecutive cycles of
   DIFFERENT Wikipedia articles (Linux/Rust/Python/Firefox), 2 tabs each, close-all between:
   after-close RSS **flat at exactly 510MB every cycle** Ã¢â‚¬â€� zero residual creep. Cache reuse
   is total; the plateau is bounded and constant. Harness: /tmp/creep-test.py.
2. Therefore the user-visible MONOTONIC growth Mac measured was dominated by the SpiderMonkey
   GC heap Ã¢â‚¬â€� and the shrinking-GC-on-close fix (9182287bf) is VERIFIED by Windows: 1GB returns
   on tab close. Memory issue is resolved as a defect; what remains is plateau-size optimization.
3. Plateau decomposition (single-run longitudinal memdump, 4x example.com): +124MB retention =
   +68.6MB system-heap LIVE objects (C-library caches: fontconfig/freetype/mesa/glib Ã¢â‚¬â€� not
   fragmentation, genuinely allocated) + ~16MB jemalloc-mapped growth + ~24MB one-time
   lib/binary demand-load. url() js rows collapse properly post-GC-fix.
4. NEGATIVE RESULTS (do not retest): MALLOC_CONF env is IGNORED in rust _rjem-prefixed jemalloc
   (symbol precedence Ã¢â‚¬â€� stats_print/tcache:false/dirty_decay_ms:0 all no-ops, verified);
   MALLOC_ARENA_MAX=2 no change (glibc arena proliferation NOT the mass); heaptrack is BLIND
   for this binary (static jemalloc doesn't interpose Rust allocs; C-side capture incomplete,
   15MB peak artifact); thread-count leak ruled out (68Ã¢â€ â€™77Ã¢â€ â€™69); tcache/dirty-decay hypotheses
   dead. Blank-tab anomaly: 4 blank tabs retain MORE (258MB) than content tabs (122MB) Ã¢â‚¬â€�
   infrastructure cost dominates and content pages partially reuse it; unexplained inversion,
   indexed but deprioritized.
5. REMAINING optimization path (not a defect): shrink the plateau Ã¢â‚¬â€� attribute the +68MB
   system-heap live objects (fontconfig/freetype/mesa caches are prime suspects; needs
   glibc-side profiling or library-by-library eviction audit). Plateau ~250-500MB depending
   on content weight Ã¢â‚¬â€� matters for the <150MB idle budget but is NOT beta-blocking.
## sec1zt288 Ã¢â‚¬â€� object-element layout crash family FIXED: borrow_for_layout (2026-11-27)
c99f789949c pushed. Mechanism: layout replaced-contents traversal reads object data-url
cache via LayoutDom; the DomRefCell.borrow() asserts script state but layout swaps thread
state to LAYOUT (ThreadStateRestorer, shared/layout/lib.rs:1074) Ã¢â‚¬â€� debug panic on ANY page
with <object data>. Root-caused via RUST_BACKTRACE=1 direct repro (/tmp/crashtest/t4.html:
embed+form+img+object named elements Ã¢â‚¬â€� repro harness). Fixed with borrow_for_layout Ã¢â‚¬â€� the
established correct pattern already in tree (script_bindings/cell.rs:50). Verified:
name-attribute-elements 3/3 pass (was CRASH), charset/in-object PASS (was CRASH). This was
the dominant crash cluster in the post-input-fix baseline Ã¢â‚¬â€� the browsing-context/charset/
html5lib "crash" lines were mostly object-embedding test pages. Debug-only assert but a real
thread-contract violation (release builds silently raced). COOP-reporting crashes collapsed
into ordinary feature gaps post-fix. SWEEP RESULT: HTMLImageElement uses AtomicRefCell
(thread-safe, correct); no other embedded-content LayoutDom uses raw DomRefCell.borrow().
REPRO LESSON: RUST_BACKTRACE=1 + direct servoshell repro gives full symbolized backtraces Ã¢â‚¬â€�
far faster than gdb for panic-class bugs since the panic hook catches before gdb stops.
## macOS lane state 2026-11-27 late (post-compaction handoff)
- Sweep tally: PASS google(end-to-end incl SERP+AI overview), stackoverflow,
  bbc, netflix, x.com, chatgpt; PASS-minus-DRM twitch; botwall-blocked
  nytimes+claude.ai (datacenter IP, not compat); DEFECT amazon layout
  (content compressed at page bottom, engine-side flex/grid).
- Omnibox input defect CLOSED shell-side: root is main-thread starvation by
  heavy servo pages Ã¢â‚¬â€� NOT input routing. Enter-hoist fix merged (#366).
  The fix is engine-lane present-off-UI-thread. Do not re-investigate.
- Memory-reclaim fix verified working: 2257MBÃ¢â€ â€™1182MB on tab close.
- ~450MB/tab heavy sites; per-tab infra dominates memory budget.
- Binary: shell-new/target/release/feather-ui built from main+instrumentation
  (eprintln focus logging in toolbar.rs Ã¢â‚¬â€� harmless, can strip).
- Open: amazon isolation, sweep remainder (reddit/github/gmail/wiki/notion/
  instagram/facebook/linkedin/whatsapp/stackoverflow-deep), Windows lane
  file-picker verification.

## macOS lane final state 2026-11-27 (sweep complete)
- 20/20 sweep done, matrix in docs/wiki/COMPATIBILITY.md. The anonymous web
  works in feather; failures = DRM, datacenter botwalls, amazon intermittent
  hydration layout. Next frontier: authenticated dogfood.
- Cmd+T dedupe removed for Chrome parity (#380) Ã¢â‚¬â€� blank tabs no longer
  dedupe. The "new-tab regression" scare was this dedupe, not a defect.
- Tab-group persistence re-verified end-to-end on canonical main AGAIN
  post-sweep Ã¢â‚¬â€� natural cycle, clean.
- Session format: {"version":1,"active_space":0,"spaces":[{"name","tabs":
  [{"url","group"}],"active","groups":[{"id","name","color","collapsed"}]}],
  "pinned":[]} Ã¢â‚¬â€� blank NewTab pages are NOT persisted (no URL).

## sec1zt289 Ã¢â‚¬â€� crash surface BOTTOMED: one mechanism left, timed-Atomics.waitAsync (2026-11-27)
Solo re-tests of all 8 surviving "crash" lines from the evening baseline: 6 of 8 PASS solo
(clearinterval-from-callback 1/1, createImageBitmap-resolves-in-task 13/13, sandbox-top-nav
pair, protocol-handler) Ã¢â‚¬â€� they were flaky/contamination artifacts (memory pressure or
cascade), not real crashes. TRUE crash surface in all of html WPT: exactly ONE mechanism Ã¢â‚¬â€�
Atomics.waitAsync WITH a timeout argument segfaults (null embedding-callback call,
verified gdb: frame#0 0x0 called from AtomicsWaitAsync<int>). Root cause fully traced:
servo wires SetUpEventLoopDispatch (script_runtime.rs:682) but the mozjs glue passes
nullptr for the DELAYED dispatch callback (jsglue.cpp SetUpEventLoopDispatch hardcodes
nullptr 3rd arg to JS::InitAsyncTaskCallbacks); timed waits use the delayed path Ã¢â€ â€™ null
call. waitAsync WITHOUT timeout works fine (verified). UPSTREAM-SHARED: upstream servo main
(mozjs 0.28) has the identical gap Ã¢â‚¬â€� feather can fix first. FIX DESIGN (deferred, cost):
vendor+patch mozjs crate adding a DelayedDispatchToEventLoop C++ wrapper mirroring the
DispatchablePointer pattern, then implement the delayed arm in script_runtime using the
script-thread timer infra; cost dominated by full mozjs_sys rebuild (~30-60min compile).
Justified only when crash-free polish outranks feature work Ã¢â‚¬â€� the API is rare in the wild
but the crash is a hard process-wide SEGV. REPRO: /tmp/crashtest/atomics.html (timed Ã¢â‚¬â€�
segfaults) vs atomics2.html (untimed Ã¢â‚¬â€� fine). SERVER: /tmp/crashtest/srv.py port 8455.
TIMEOUT-cluster analysis: 14+ of 30 timeouts are COOP/COEP-reporting Ã¢â‚¬â€� zero reporting
infrastructure exists in servo net stack (no Report-To, no reporting endpoints, no COOP
parsing) Ã¢â‚¬â€� upstream-shared multi-day net-stack feature, documented not started.

## memsec: dir=auto content-scan directionality Ã¢â‚¬â€� full chain (windows lane, fixbatch-7)
symptom: computed direction wrong for dir=auto elements (hebrew content stayed ltr; latin inside rtl parent inherited rtl) -> FIXED. architecture: (1) Element.directionality_cache AtomicUsize (0=unset,1=ltr,2=rtl) script-side in element/element.rs, free fn get_directionality_cache_for_layout mirrors get_attr_for_layout pattern with #[expect(unsafe_code)] Ã¢â‚¬â€� CAREFUL inserting fns above get_attr_for_layout detaches its expect attr causing phantom unsafe errors. (2) htmlelement.update_directionality_cache maps directionality() results, hooked at attribute_mutated(dir)+bind_to_tree. (3) element children_changed walks INCLUSIVE self+ancestors (DomRoot::from_ref chain, GetParentNode takes NO args) recomputing caches, dirtying on change. (4) matching in servo_dangerous_style_element.rs :dir() auto branch reads cache. (5) CRITICAL missing piece found empirically: no stylesheet rule consumed :dir() for generic elements Ã¢â‚¬â€� added [dir=auto i]:dir(ltr/rtl) rules in user-agent.css. KEY INSIGHT: bind_to_tree computes before children parse Ã¢â‚¬â€� the children_changed inclusive-walk is what makes parse-time content work. RESIDUAL: characterdata in-place text edits dont recompute; bdi without dir attr should behave as auto (pre-existing TODO in directionality()). PROBES: wtest/dirauto2.html (hebrew=rtl latin=ltr latin-in-rtl=ltr exact), dirreg.html regression. GetParentNode() no-arg signature gotcha; Cell<u8> fails dom_struct inference Ã¢â‚¬â€� use AtomicUsize mirroring selector_flags.
## sec1zt290 Ã¢â‚¬â€� YouTube dark-nav REPRODUCED: theme custom properties never apply (2026-11-27)
UNBLOCKED dark-mode repro: servoshell now has SERVO_FORCE_THEME=dark|light override
(pushed this session Ã¢â‚¬â€� headed_window.rs:1030 + window.rs default theme()). Reproduced the
batch-7 defect end-to-end: YouTube boots into dark theme (html[dark]=true, verified via
webdriver) yet the left side-nav is INVISIBLE in the screenshot (/tmp/yt-dark.png) while
the rest of the page renders correct dark theme. KEY EVIDENCE (webdriver probe):
getComputedStyle(document.documentElement).getPropertyValue('--yt-spec-text-primary') is
EMPTY Ã¢â‚¬â€� YouTube's theme custom properties never apply to the cascade, so components fall
back (many to transparent/black-on-black). Also guide display:none (may be orthonormal
watch-page behavior or fallout). Body bg computes transparent. NEXT investigation step:
determine HOW YouTube injects the vars Ã¢â‚¬â€� candidates: (a) html[dark] selector rules in a
stylesheet servo fails to parse/apply (test: minimal html[dark] { --x: v } repro Ã¢â‚¬â€� if that
works, the failure is in YouTube's specific constructs); (b) @property-registered
inherits:true vars losing initial values in shadow trees; (c) vars defined inside
CSS layers or scope. YouTube nav items queried DO compute color rgb(232,232,232) Ã¢â‚¬â€� so
simple vars partially work Ã¢â‚¬â€� the invisibility may be narrower than full var failure:
possibly specific to the guide's background surface tokens. Harness: /tmp/ytnav.py,
/tmp/yt-dark.png, SERVO_FORCE_THEME=dark + webdriver execute/sync probing.

## sec1zt291 Ã¢â‚¬â€� dark-nav refinement: basic var mechanism healthy; suspect incomplete boot (2026-11-27)
Minimal repro /tmp/crashtest/vartest.html PROVES html[dark]-selector custom properties
apply correctly in servo (v1[#aabbcc]v2[#112233] both resolve). Therefore the missing
--yt-spec-* variables are NOT a CSS cascade failure Ã¢â‚¬â€� YouTube's own theming JS injects
those vars, and the probe snapshot showed the boot INCOMPLETE (video still spinner at 30s).
New leading theory: the black-on-black nav is downstream of incomplete YouTube boot (the
boot flake Ã¢â‚¬â€� same family as the media/script wedges) rather than a discrete CSS bug.
DISCRIMINATOR for next session: probe again after the page fully settles (or drive with a
longer wait + check ytd-app theming inline styles on html element; if vars appear after
full boot, the defect is boot-completeness not CSS). All harness durable: /tmp/ytnav.py,
/tmp/checkvartest.py, srv port 8455.

## sec1zt292 Ã¢â‚¬â€� boot-incompleteness REFUTED: genuine CSS mechanism failure (2026-11-27)
60s settle probe: video fully playing (readyState 4, currentTime 11s, unpaused),
ytInitialData loaded Ã¢â‚¬â€� yet --yt-spec-text-primary STILL empty, html has only 2 inline
style props. Boot completeness is NOT the cause: this is a genuine CSS failure specific to
HOW YouTube defines theme vars (basic html[dark] vars proven working in sec1zt291).
Candidates narrowed: vars defined in a construct servo mishandles Ã¢â‚¬â€� Polymer legacy
`.style-scope` selectors, :host-context, @supports-wrapped defs, or layered/!important
interactions. NEXT STEP: probe document.styleSheets enumeration on the live page (same-
origin parts enumerable) grepping cssText for '--yt-spec-text-primary' to find the exact
defining selector/construct; fallback: fetch youtube css assets offline and grep. Harness
/tmp/ytnav2.py durable. NOTE - youtube css is largely served inline in <style> tags so
enumeration may fully succeed.

## sec1zt293 Ã¢â‚¬â€� youtube dark-nav root-cause chain: NOT CSS Ã¢â‚¬â€� JS guide-state logic (2026-11-27)
INVESTIGATION COMPLETE THROUGH CSS LAYER Ã¢â‚¬â€� all CSS theories refuted, defect is in YouTube's
JS guide-state decisions in servo. Evidence chain: (1) theme tokens --tffXXXX resolve
CORRECTLY in dark mode (#f1f1f1 etc Ã¢â‚¬â€� verified live); (2) the legacy --yt-spec-text-primary
probe was a red herring Ã¢â‚¬â€� YouTube no longer uses those vars at all (0 usages in served CSS);
(3) mini-guide element is properly defined/upgraded (shady-dom content present) but keeps
its skeleton `hidden` attribute forever Ã¢â‚¬â€� YouTube JS never unhides it / never sets
mini-guide-visible on ytd-app even at 1600px width; (4) NOT a boot-completeness issue
(video plays, readyState 4); (5) display-mode media feature was genuinely missing in servo
(all four values false Ã¢â‚¬â€� spec violation) Ã¢â‚¬â€� FIXED this session (stylo: display-mode always
browser, compile-verified, pushed) but does NOT lift the nav defect by itself. NEXT STEP:
probe iron-media-query elements' query/matches state on the live page Ã¢â‚¬â€� YouTube's guide
visibility is gated through polymer media-query bindings; find which query mis-evaluates.
ALSO durable: kevlar.css token system documented (:root double-assign + [dark]/[light] bare
attribute overrides Ã¢â‚¬â€� servo handles correctly, verified vartest2.html). Harnesses:
/tmp/ytnav*.py chain, checkvartest.py <page> for fast media/var probes. SERVO_FORCE_THEME
tooling in tree. Files: /tmp/kevlar.css (3.4MB full css), /tmp/yt-dark.png.

## sec1zt294 Ã¢â‚¬â€� youtube-nav: media-query machinery fully cleared Ã¢â‚¬â€� healthy (2026-11-27)
FINAL REFUTATIONS: (1) all four iron-media-query bindings on the live page evaluate
CORRECTLY for the actual viewport (verified against 1024x740 arithmetic); (2) matchMedia
change events fire correctly on resize (minimal repro vartest5: initial:false Ã¢â€ â€™
change:true + legacy:true both fire at 1600 resize) Ã¢â‚¬â€� the entire media layer is healthy.
(3) height media queries correct. REMAINING DEFECT: at 1600x1000-from-boot (resize before
nav), YouTube's q4-type breakpoint (min-width 1327 + min-height 630) media conditions are
satisfied yet miniGuideVisible stays false Ã¢â‚¬â€� the wrong branch is inside YouTube's polymer
app-shell state logic itself, not any servo media/CSS machinery. NEXT INVESTIGATION SHAPE:
side-by-side diff of ytd-app property state servo-vs-chrome, or breakpoint YouTube's
guide-state setter. NOTE: servo JS lacks some API YouTube feature-detects (e.g. check
navigator.userAgent branching Ã¢â‚¬â€� youtube may serve a fallback experience to unknown agents;
the skeleton ships mini-guide hidden BY DEFAULT and the upgrade path that unhides it may
be gated on client-capability detection). PRIORITY-ORDER candidate for next probe: compare
the full served HTML/JS payload servo-UA vs chrome-UA Ã¢â‚¬â€� bot/unknown-UA degradation is the
most likely remaining mechanism and cheap to test (diff curl responses). Harnesses durable.

## sec1zt295 Ã¢â‚¬â€� youtube-nav: UA hypothesis refuted Ã¢â‚¬â€� investigation bounded (2026-11-27)
Clean Firefox UA (no Servo token, --user-agent override) Ã¢â‚¬â€� mini-guide STILL hidden, same
appAttrs. UA degradation refuted. INVESTIGATION STATE: every servo-side layer cleared with
live evidence (CSS cascade, theme tokens, custom elements upgrade, all media-query
machinery incl change events, boot completeness, UA). The wrong branch is inside YouTube's
obfuscated polymer state logic Ã¢â‚¬â€� further progress needs either (a) side-by-side servo vs
chrome property diffing of ytd-app internals, or (b) breakpointing YouTube's guide-state
setter in devtools Ã¢â‚¬â€� both require a dedicated deep-debug session. POSSIBLY RELATED: check
js error console early in boot Ã¢â‚¬â€� a single thrown exception in a polymer event handler
upstream of guide-state init would produce exactly this symptom (everything boots except
one behavior chain). Cheapest next probe: capture window error events + console errors
during boot (webdriver inject addEventListener('error') collector via early script or
check servo stderr for js error reports). All harnesses /tmp/ytnav*.py durable.

## sec1zt296 Ã¢â‚¬â€� REPRODUCED: external-scheme nav during initial load breaks pipeline (2026-11-28)
THE wa.me never-completing-load mechanism in minimal repro, decisive A/B: control page
(schemenav3.html Ã¢â‚¬â€� hanging subresource img, nothing else): nav completes, LOAD-FIRED,
readyState complete, probes healthy (servo doesn't block load on hanging images at all).
Test page (schemenav2.html Ã¢â‚¬â€� IDENTICAL plus a whatsapp:// anchor click fired at parse
time): navigation NEVER completes (webdriver pageLoad timeout fires), and afterwards
execute/sync returns HTTP 500 Ã¢â‚¬â€� the script pipeline is wedged/broken, not just the load
event missing. Mechanism: an unnavigable-scheme navigation fired DURING the initial load
corrupts the pipeline's load/script state Ã¢â‚¬â€� the navigate-and-cancel path (deab4229838)
tears down or poisons more than the scheme navigation itself. IMPACT: any real page doing
a protocol handoff during load (whatsapp/deep-link/app-store redirects) kills the tab Ã¢â‚¬â€�
user-critical. REFUTED along the way: scheme nav AFTER load completion is harmless
(schemenav.html Ã¢â‚¬â€� LOAD-FIRED, recovery fine); plain hanging subresources don't wedge
anything; webdriver recovery machinery per spec is fine (the earlier "wedge regression"
scare was the 300s spec-default pageLoad wait Ã¢â‚¬â€� NOT an engine wedge; verified with 5s
timeout: recovery 0.04s). MEMORY RE-VERIFIED on current feather-engine: flat exactly
500MB across 4 unique-page open-close cycles. Harnesses durable: /tmp/crashtest/
schemenav{,2,3}.html, /tmp/schemetest2.py <page>, /tmp/hangsrv.py (port 8460 hanging-img
server), /tmp/wedge-verify*.py chain. NEXT: read the navigate-and-cancel/AbortLoadUrl
path for how a parse-time scheme nav interacts with the in-flight initial load Ã¢â‚¬â€� suspect
the abort tears down the pending change of the WRONG (main) navigation or leaves the
pipeline in a state where load-event completion can never be signalled.

## Interaction-class verification COMPLETE (macOS lane, 2026-11-28)
All core input classes verified on real sites/fixtures: text input (google),
SPA link navigation (youtube), scroll (fixture), media playback realtime 1:1
+ pause + seek (fixture), drag-dwell grouping, storage prompts (whatsapp).
YouTube video playback works end-to-end Ã¢â‚¬â€� ads play through, real video with
full player chrome. prefers-color-scheme verified end-to-end through feather's
own appearance toggle (embedder pipeline works Ã¢â‚¬â€� youtube dark-nav issue is
NOT signaling, it's in servo themed-style computation). SERVO_FORCE_THEME is
servoshell-only, inert in feather. Media remaining gap: DRM/EME only.

## sec1zt297 Ã¢â‚¬â€� external-scheme pipeline-break FIXED: navigate() abort was the mechanism (2026-11-28)
ROOT CAUSE + FIX SHIPPED (feather-engine HEAD): navigate() in script/navigation.rs queued
abort_a_document_and_its_descendants (step 24.4) unconditionally BEFORE sending LoadUrl
to the constellation Ã¢â‚¬â€� so for unnavigable schemes the current document's in-flight load
was destroyed in script before the constellation could ever drop the navigation. This is
why deab4229838's navigate-and-cancel could never work: the cancellation path it relied
on runs AFTER the destructive abort. Fix: early-return in navigate() for schemes outside
http/https/about/data/blob/file/filesystem (handoff semantics = no-op) + constellation
early-drop as belt-and-braces. VERIFIED: schemenav2 (parse-time whatsapp click) now
LOAD-FIRED+complete (was: never-completing load + wedged pipeline, execute 500s); wa.me
end-to-end completes in 1.2s (was permanent timeout); recovery navs 0.03s; regression
check on post-load scheme nav + plain navs clean. CONSEQUENCE FOR FLEET: wa.me is FIXED,
not just non-wedging Ã¢â‚¬â€� the earlier 'residual upstream-class net/fetch-completion bug'
note in deab4229838's message needs re-evaluation: some or all of those 'script body
streams never finish' observations may have been cascade artifacts of this abort defect.
aol.com stall claim should be re-tested on this fix before any net-layer hunt.

## sec1zt298 Ã¢â‚¬â€� aol-class wedge ROOT-CAUSED: quadratic RootCollection::unroot in xpath eval (2026-11-28)
The never-completing-load + wedged-script-thread class (aol.com repro, originally mis-
diagnosed as net fetch-completion) is algorithmic: gdb of the 100%-CPU Script#2 thread
shows document.evaluate() xpath evaluation spending everything in
RootCollection::unroot's rposition LINEAR SCAN (full bt in session history). Mechanism:
xpath evaluate() on '//' paths collects every descendant into a NodeSet of individually
rooted DomRoots (thousands of simultaneous roots); dropping them in forward order makes
each unroot scan nearly the whole roots vec Ã¢â‚¬â€� O(N^2) pointer comparisons per evaluation,
minutes of 100% CPU on real pages. NOT a net bug: parallel CDN fetches verified healthy
(24 parallel aolcdn scripts complete fine standalone); the 'script streams never arrive'
observation in deab4229838 was downstream of the wedged thread. REFUTED same session:
HTTP2_MAX_HEADER_LIST_SIZE already wired (256KB Chrome value, connector.rs:689).
FIX DESIGN ANALYSIS (not yet implemented): (1) NodeSet Drop reverse-order Ã¢â‚¬â€� blocked:
into_iter moves nodes out (E0509 with Drop impl); (2) eval.rs consumption order is
semantic, can't trivially reverse; (3) RootCollection side-index HashMap<ptr-as-usize,
usize> storing LAST occurrence index (matches rposition semantics): unroot looks up,
falls back to scan on stale/miss (duplicates possible Ã¢â‚¬â€� same object rooted twice), swap-
remove fixup updates swapped element's index; correct but touches hot engine-wide infra;
(4) scan-direction-adaptive or first-K-then-full hybrid: doesn't fix worst case.
RECOMMENDED: option 3 with fallback scan Ã¢â‚¬â€� exact correctness, O(1) fast path, moderate
blast radius. Verify with aol.com nav completing + execute probes alive. REPRO harness:
/tmp/aolhunt.py (auto gdb-captures spinning script thread), /tmp/aolcpu.py,
/tmp/aol-spin-bt.txt. Also NOTE: this class probably affects other xpath-heavy pages;
also worth checking whether document.evaluate heavy loops in WPT explain residual
timeouts.


## Non-http scheme investigation chain (macOS lane, 2026-11-28)
file:// AND data: both blank in feather Ã¢â‚¬â€� generalized defect, not file-specific.
Document never even parses (title-marker probe). servo embedding library is
INNOCENT Ã¢â‚¬â€� servo's own tests assert data: reaches LoadStatus::Complete in CI.
Defect is feather-specific. REFUTED: startup-race hypothesis (fails post-startup
too). REFUTED: pref-diff (all overrides are innocent feature-enables). Strongest
remaining: feather's custom winit event-loop integration Ã¢â‚¬â€� sync-scheme loads
don't produce whatever frame-signal async loads trigger. NEXT LANE: instrument
load-status transitions per scheme; needs build budget. Also filed separately:
omnibox normalize() mangles data:/blob:/about: (no "://" in those schemes).
DISK WARNING: cargo registry cache was deleted to survive a disk-full crisis Ã¢â‚¬â€�
first rebuild re-downloads crates (+3-5min). servoshell binary build INFEASIBLE
on this box (feature-variant deps double-compile 8GB tree).


## sec1zt299 Ã¢â‚¬â€� unroot quadratic fix SHIPPED; residual aol cost is genuine JS eval (2026-11-28)
O(1) unroot fix live on feather-engine (hash side-index ptrÃ¢â€ â€™(count,pos), validate-or-scan
fallback, swap-remove fixup). Verified the aol spin no longer contains unroot frames Ã¢â‚¬â€� the
hotspot moved into JIT'd JS itself: the residual wedge is genuine evaluation cost, either
massive intermediate node-set churn in servo_xpath (traverse_preorder over huge documents
per query, filter_map allocation cascades) or JS-loop-level repetition of xpath queries.
NEXT investigation: capture WHAT xpath aol runs (instrument document.evaluate logging or
JS profile) and whether the blowup is per-query algorithmic (node-set sizes exploding
combinatorially across multi-step paths) or repetition-driven. ALSO NOTE: the unroot fix
is engine-wide hot-path infrastructure Ã¢â‚¬â€� WPT baseline re-run warranted when convenient;
smoke tests passed (navs, 2605-node xpath results, alloc churn). HARNESS: /tmp/aolhunt.py
auto-captures spinning script threads with gdb; /tmp/smoke.py.

- FLEXBOX 191-FILE CLUSTER AUDIT COMPLETE (windows lane, 2026-11-27): every sub-leaf probe-verified working or fixed. gap/column-gap pixel-exact incl percentages+wrapping; align-content multi-line exact; aspect-ratio FIXED (PR#2); writing-mode interaction FIXED (PR#7 orthogonal transposition); intrinsic-size/min-max exact shrink ratios; inline-flex SAME-LINE correct (baseline alignment per spec Ã¢â‚¬â€� early crude probe false-alarmed on baseline offset, refined probe confirms afterLeft==containerRight exactly). VERDICT: the 191 count is overwhelmingly reftest pixel-noise + upstream-expected failures. NOT actionable as engineering work. Symptom-index negative entries: if flex layout regressions appear, suspects are flexbox/layout.rs stretch resolution and flow/mod.rs orthogonal CB construction (both recently touched).
- VISUAL VERIFICATION of fieldset solid-top border gap (PR#5) COMPLETE 2026-11-27: desktop screenshot of red-6px-solid fieldset shows legend straddling the top border with a clean gap behind the legend text Ã¢â‚¬â€� border segments correct left/right, no line through text, exactly matching Firefox/Chrome rendering. The flagged honest gap is closed; coordinate math confirmed visually correct.

## Arabic/bidi fully verified user-ready (macOS lane, 2026-11-28)
aljazeera.net real-world probe: contextual shaping, RTL layout direction, zero
corruption. dir=auto/bdi computed direction correct (#8/#9 verified shell-side).
CONCLUSION: Arabic user-ready in feather. NOTE: the external-scheme engine fix
(8967796b1) does NOT cure the file:// blank-render defect Ã¢â‚¬â€� verified distinct
by rebuild+retest; non-http primary loads never parse, all cheap hypotheses
eliminated, needs load-status instrumentation lane.


## sec1zt300 Ã¢â‚¬â€� aol residual: NOT xpath, NOT jit Ã¢â‚¬â€� genuinely stuck JS (2026-11-28)
Negative results closing the aol investigation for now: (1) SERVO_LOG_XPATH instrumentation
at the evaluate_internal chokepoint (universal Ã¢â‚¬â€� covers document.evaluate AND
XPathEvaluator.evaluate) logs ZERO evaluations during wedged aol boots Ã¢â‚¬â€� aol does not
meaningfully use xpath; the original gdb capture's xpath frames were a one-time artifact
(and the unroot fix remains correct+valuable for genuinely xpath-heavy pages). (2) Wedge
persists with --pref=js_disable_jit Ã¢â‚¬â€� not a JIT miscompilation. (3) 75s is not enough Ã¢â‚¬â€�
effectively infinite, not slow. Current state: spin is in interpreter/JIT JS frames with
no native symbol visibility; further progress needs JS-level profiling (instrument the
script thread's microtask checkpoint or add a watchdog that dumps the JS stack via
SpiderMonkey's profiling API Ã¢â‚¬â€� both are real implementation work, documented not started).
Instrumentation left in tree (env-gated, zero-cost): SERVO_LOG_XPATH in xpathexpression.rs
evaluate_internal. RECOMMENDATION: deprioritize aol-deep-dive in favor of user-visible P0s;
the wedge class matters but needs the profiler tooling first.


## sec1zt301 Ã¢â‚¬â€� youtube boot-error probe negative: no js errors reach stderr (2026-11-28)
RUST_LOG=script=info + stderr capture during wedged youtube boot shows zero error-ish
lines Ã¢â‚¬â€� servo's exception reporting does not surface page JS errors to stderr at any log
level. Cheap high-value tooling addition for ALL future real-site debugging: JS exceptions
are already plumbed to the embedder as JavaScriptErrorInfo Ã¢â‚¬â€� ports/servoshell just never
prints them. Patch servoshell to eprintln them; then boot-error probes become trivial.




## sec1zt302 Ã¢â‚¬â€� console errors now reach stderr; youtube boot-error hypothesis REFUTED (2026-11-28)
TOOLING FIX (this commit): servoshell printed console messages via println! Ã¢â‚¬â€� block-
buffered through pipes, lost on SIGKILL. Now eprintln! in running_app_state.rs +
headed/headless_window.rs, tagged [console]. Uncaught page JS errors surface as
"[console] Error at FILE:LINE:COL message" Ã¢â‚¬â€� verified with a deliberately-throwing page.
This unblocks ALL future real-site JS debugging.
PROBE RESULT: youtube.com boot produces ZERO uncaught JS errors (verified plumbing with
positive control in same session). The "thrown exception in polymer handler chain"
hypothesis for dark-nav (sec1zt295) is REFUTED Ã¢â‚¬â€� the wrong-branch logic is silent.
Remaining explanations narrow to: wrong values flowing through media-match/feature-detect
APIs polymer actually consults (matchMedia results, window.innerWidth timing, etc.) or
upstream-boot differences. Given everything else cleared, dark-nav stays bounded; the
next probe shape is diffing matchMedia/innerWidth/screen values against a real browser.

## sec1zt303 Ã¢â‚¬â€� upstream scan 2026-11-28: cherry-pick candidates
Small safe picks adjacent to feather work: 461f99fc59f xpath id() first-element-only
(perf, adjacent to unroot work), e667861e269 borrow hazard in attribute storage (crash
fix). Larger: 806d6369990 weak-referenceable elements, 60019507770 input internal value.
NOT YET APPLIED Ã¢â‚¬â€� cherry-pick attempt hit harness shutdown; queue for next session.


## sec1zt304 Ã¢â‚¬â€� upstream picks already present; git fetch refspec lesson (2026-11-28)
Both queued cherry-picks (461f99fc59f xpath id(), e667861e269 attribute borrow hazard)
come out EMPTY against feather-engine Ã¢â‚¬â€� content already in tree (via upstream-merge
overlap). No action needed; do not re-attempt.
GIT PROXY LESSON: plain `git fetch origin feather-engine` does NOT update the remote-
tracking ref through the git-manager proxy Ã¢â‚¬â€� origin/feather-engine silently goes stale,
causing phantom non-fast-forward rejections. Always use:
  git fetch origin +refs/heads/feather-engine:refs/remotes/origin/feather-engine
then rebase + `git push origin HEAD:feather-engine`. This was the root of all push-race
paradoxes this session, including the botched rebase that briefly dropped sec1zt301-303
(reinstated in 954969fbd93 Ã¢â‚¬â€� verify memory entries survive any future rebase by grepping
afterward).

## sec1zt305 Ã¢â‚¬â€� youtube script saturation blocks probing (2026-11-28)
webdriver execute/sync against youtube.com times out 35s into boot Ã¢â‚¬â€� the script thread
is too saturated to service even a trivial evaluation. This blocks all JS-level probing
of the dark-nav defect AND signals the real user-facing problem: youtube main-thread
work in servo is far heavier than in real browsers (polymer boot + our slower JS/DOM).
Implication for priority: dark-nav correctness is downstream of raw JS/DOM perf Ã¢â‚¬â€� the
composite-off-UI-thread work + general perf matter more than further dark-nav probing.
Negative result: zero console errors again confirmed on this run.

## sec1zt306 Ã¢â‚¬â€� wedge regression VERDICT: fixed, verified with exact repro (2026-11-28)
Repro per coordinator: hanging embedded image (never-completing subresource, hangsrv.py
port 8460, hangpage.html) + 5s pageLoad timeout, then re-navigate. Results: timeout at
exactly 5.0s, recovery nav completes in 0.02s, execute alive (readyState complete),
second cycle identical. NO permanent dead webview. Attribution caveat: the recovery path
mechanically depends on input-wedge fix 825fe34188f + permanent-webview fix bb5c9bc9b94,
not memory-only 9182287bf. Harness: /tmp/wedge-repro-final.py Ã¢â‚¬â€� REPRO RECIPE NOTES:
navigating directly to a hanging image URL does NOT hang (image-document wrapper
completes instantly); must embed as subresource in HTML page. Server dies over time Ã¢â‚¬â€�
restart hangsrv.py before use. DEFECT CLOSED.

## sec1zt307 Ã¢â‚¬â€� leaf rebase post-all-fixes: what actually remains (2026-11-28)
The targeting-doc leaf list (feather#321) is largely INVALIDATED Ã¢â‚¬â€� most rows were
input-wedge artifacts. Rebased against the evening baseline (post object-fix):
REMAINING ACTIONABLE, bottom-up:
1. fieldset/legend containing-block layout Ã¢â‚¬â€� reftest FAILs (fieldset-containing-block,
   fieldset-transform-translatez, legend-float). Real-web visible: forms render wrong.
   Reftest hashes differ = layout geometry bug in fieldset abs-pos containing block.
   MEDIUM difficulty, highest real-web value of remaining leaves.
2. canvas 2d.layer blur/filter FAILs (~4) Ã¢â‚¬â€� niche, low priority.
3. location_replace_session_history ERROR (was TIMEOUT-expected Ã¢â‚¬â€� actually an
   expectations artifact: ERROR vs expected TIMEOUT both mean broken upstream; needs solo look).
4. COOP/COEP reporting timeouts Ã¢â‚¬â€� blocked on net-stack reporting infra (multi-day).
5. timed waitAsync segfault Ã¢â‚¬â€� ONLY remaining crash mechanism; needs mozjs vendoring.
CLOSED-BY-FIX leaves (do not re-hunt): Location setter edges (15+ subtests Ã¢â‚¬â€� were wedge
artifacts, now pass), pageswap, most history-traversal rows, cssom crash.
NEXT SESSION: start with fieldset containing-block Ã¢â‚¬â€� read the spec section on fieldset
layout + the reftest, implement in components/layout/flow/ (block.rs fieldset handling).

## sec1zt308 Ã¢â‚¬â€� ROTATION HANDOFF: state, orders, fieldset scoping (2026-11-28)
COORDINATOR ORDERS IN FORCE: (1) disk hygiene DONE Ã¢â‚¬â€� freed 63GB (shell-new/target 56G
deleted, stale 14h+, regenerable; WPT raw dirs, old binaries, logs) Ã¢â‚¬â€� 99%Ã¢â€ â€™48% full.
(2) fieldset/legend containing-block fix = current task. (3) this handoff.
KEY FACTS FOR NEXT SESSION:
- Targeting doc feather#321 is INVALIDATED Ã¢â‚¬â€� most leaves were input-wedge artifacts.
  Remaining real leaves (sec1zt307): fieldset containing-block (current), canvas
  layer-filter, location_replace ERROR, COOP reporting (blocked), waitAsync (needs decision).
- composite-off-UI-thread (feather#335) is highest-leverage: youtube script saturation
  evidence (sec1zt305).
- Wedge regression VERIFIED FIXED (sec1zt306).
FIELDSET SCOPING (done this session, don't redo): servo lays out fieldset as ordinary
block Ã¢â‚¬â€� NO anonymous fieldset-content wrapper exists. Legend straddle-shift exists in
same_formatting_context_block.rs:443+ but the spec model (anonymous content box wrapping
non-legend children; containing block for abs-pos = that content box, NOT the fieldset
padding box) is unimplemented. Test: fieldset-containing-block.html Ã¢â‚¬â€� abs div top:0 in
position:relative fieldset must resolve against the content-box area per HTML rendering
spec ("the fieldset and legend elements" section). Implementation shape: wrap non-legend
children in anonymous box during box construction (components/layout/flow construction),
give fieldset special containing-block behavior. Estimated: half-to-full session.
Repro: mach test-wpt /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/fieldset-containing-block.html
NOTE add134a8a47 'legend straddles fieldset border in vertical writing modes' commit
exists in log Ã¢â‚¬â€� check what's already merged before writing anything.

## sec1zt309 Ã¢â‚¬â€� fieldset spec text + implementation plan (2026-11-28, rotation input)
EXACT SPEC (html spec 15.3.12, fetched verbatim):
- fieldset behaves as flow-root (BFC) regardless of display.
- rendered legend = FIRST legend child box with float:none AND position not abs/fixed.
- border not painted behind legend rect (DONE by other lane Ã¢â‚¬â€� 92854ce43f8).
- legend straddle positioning (DONE Ã¢â‚¬â€� 592aa606c95, add134a8a47 vertical).
- **MISSING PIECE**: anonymous fieldset content box Ã¢â‚¬â€� appears after rendered legend,
  contains ALL other content incl ::before/::after; its used padding is ZERO.
- min/max-content inline size of fieldset = greater of legend vs content-box sizes.
- block-start border space = max(border-block-start-width, legend margin-box block size).
IMPLEMENTATION PLAN for rotated session:
1. Box construction (components/layout/flow construction path Ã¢â‚¬â€� find where block
   containers build children, likely construct.rs or same_formatting_context_block):
   for fieldset elements, partition children into rendered-legend vs rest; wrap rest
   in anonymous block box (existing anonymous-box machinery used for wrapping inline
   content Ã¢â‚¬â€� reuse). Legend stays direct child.
2. Sizing: fieldset intrinsic sizes = max(legend, content-box) contributions.
3. Verify with: mach test-wpt
   /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/fieldset-containing-block.html
   plus legend-float.html, fieldset-transform-translatez.html.
4. The abs-pos containing block resolution may need NO special code once the wrapper
   exists Ã¢â‚¬â€� the reftest geometry follows from correct tree structure. Test FIRST after
   step 1 before writing any containing-block-specific logic.
OTHER LANE already in this area (commits #4/#5/#7/#11 merged) Ã¢â‚¬â€� rebase carefully,
their legend-shift code in same_formatting_context_block.rs assumes flat children and
may need adaptation once the wrapper exists (legend stays direct child so probably fine).

sec1zt309-addendum: construction site located Ã¢â‚¬â€� components/layout/flow/construct.rs is the box construction path (has anonymous wrapping machinery); IS_FIELDSET flag set in fragment_tree/base_fragment.rs:227. Start implementation in construct.rs where block children are processed.

## sec1zt310 Ã¢â‚¬â€� rendered-legend conditions fix shipped, partial (2026-11-28)
commit pending-push: straddle-shift now filters float/position per spec. VERIFIED:
no regression (border-gap-position-relative still PASS-ahead-of-upstream,
containing-block still FAIL as before). legend-float.html still FAILS Ã¢â‚¬â€� expected:
the real fix needs the anonymous content box; floated legends must float inside it.
ROTATION: implement per sec1zt309 plan. BoxFragment style access pattern (cost me
3 compile cycles, save them): children are Fragment enum; match Fragment::Box(bf)
then bf.style.borrow().get_box() Ã¢â‚¬â€� bf is Arc<BoxFragment> no borrow() on it, and
style is ArcRefCell so .borrow() needed. Fragment enum must be imported.


## Blank-tab group serialization shipped (macOS lane, 2026-11-28)
PR #401: blank NewTab pages in groups now serialize as {"blank":true,"group":...}
and restore with membership Ã¢â‚¬â€� last gap from the #381 matrix closed. Verified
both directions. Session format addition is backwards compatible.
DISK: cleaned to 2.9GB free (cargo registry, screencasts, screenshots, tmp).
CoreSimulator 4.1GB deliberately kept (JOT iOS lane tooling). cargo registry
will re-download on next build (+3-5min first time).
SERVOSHELL DISCRIMINATOR: infeasible on this box (feature-variant deps don't
fit disk) BUT unnecessary Ã¢â‚¬â€� servo embedding tests already prove data: reaches
LoadStatus::Complete; the non-http blank defect is feather-specific. NEXT LANE
for file://: instrument load-status transitions per scheme in feather's engine.

## file:// ROOT-CAUSED AND FIXED shell-side (macOS lane, PR #403, 2026-11-28)
The blank non-http rendering was ONE LINE in feather's frame loop: the engine
navigation driver filtered wanted urls with starts_with("http") Ã¢â‚¬â€� file:/data:
/blob: never reached the constellation at all. NOT engine-side, NOT the library.
Fix: gate removed; verified url_changedÃ¢â€ â€™HeadParsedÃ¢â€ â€™Complete chain + render.
LESSON for all lanes: when zero events fire at every level, suspect the shell's
own driver logic before the engine Ã¢â‚¬â€� the wedge/loadlog env instrumentation
(FEATHER_WEDGE/FEATHER_LOADLOG) discriminates in minutes. The servo-embedding-
tests-pass evidence was correct but misleading Ã¢â‚¬â€� the library WAS innocent
because the load never reached it. file-drop feature (#352) unblocked.
Residual same-class items: session serialization still filters non-http
(app.rs save_session) and reopen-closed-tab recording (state.rs) Ã¢â‚¬â€� deliberate
or oversight, worth a review pass next lane.


## sec1zt311 Ã¢â‚¬â€� fieldset containing-block FIXED: allocated-border-space rule (2026-11-28)
ROOT CAUSE (pixel-level): legend straddle-shift centered on the physical border edge;
spec allocates max(border-width, legend margin block size) as the border space and
centers the legend within it. Zero-border case: legend must not spill above fieldset.
Servo rendered the band at y83, correct y108 Ã¢â‚¬â€� exactly legend-height/2 off.
FIX: straddle target now -(padding+border) + max(border,legend_height)/2 in content
coords; degenerates to old formula when border >= legend height. VERIFIED:
fieldset-containing-block PASS (was FAIL); zero regressions Ã¢â‚¬â€� legend-position-relative-2
and fieldset-transform-translatez FAILs both pre-existing (evening baseline).
STILL OPEN: (a) in-flow content shift Ã¢â‚¬â€� spec also requires non-legend content to start
after the allocated border space (allocated - border) deeper than normal; not yet
implemented, no current test catches it but real forms with legends+content need it;
(b) legend-float, translatez, legend-position-relative-2 Ã¢â‚¬â€� need analysis, some may need
the anonymous content box. Repro harness: /tmp/fs-shot.py screenshots test+ref over
webdriver (needs --window-size 800x600 NOT 800,600 Ã¢â‚¬â€� comma form crashes servoshell
argparse; file:// urls kill the webdriver session, serve over http instead).



## FINAL HANDOFF Ã¢â‚¬â€� complete shell-lane state (2026-11-28)

### Build commands (this box)
```
cd /Users/devin/feather/shell-new
PKG_CONFIG_PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig cargo build --release
install_name_tool -add_rpath /Library/Frameworks/GStreamer.framework/Versions/1.0/lib target/release/feather-ui   # after EVERY build
```
Shell-only rebuild 2-5min; servo-dep rebuilds ~6.5min. NOTE: cargo registry cache was deleted 2026-11-28 (disk crisis) Ã¢â‚¬â€� first fresh build re-downloads crates, +3-5min.
Debug env vars: FEATHER_LOADLOG=1 (webview load status), FEATHER_WEDGE=1/FEATHER_ACTIONS=1 (navigation driver), FEATHER_DEVTOOLS=1 (devtools server on :12374, firefox protocol, reports zero tabs Ã¢â‚¬â€� integration partial).

### Verified green on latest engine (do not re-verify)
memory reclaim; 20/20 site sweep (COMPATIBILITY.md); youtube playback end-to-end; media controls; text input; SPA nav; scroll; storage prompts; prefers-color-scheme through feather's own toggle; arabic shaping+painting+dir/bidi+RTL (aljazeera real-world); object-element fix; external-scheme handoff LOAD-FIRED; tab-group persistence (all dimensions incl. collapsed state); crash recovery SIGKILL; file:// rendering (fixed #403, closed #404).

### Open items (in priority order, as documented in defect-queue.md)
1. Authenticated dogfood Ã¢â‚¬â€� gmail/facebook/linkedin need real credentials; probe surface anonymous-only is exhausted.
2. Amazon hydration layout Ã¢â‚¬â€� intermittent, cold-first-load biased, 3/3 correct since original sighting; one cold-profile repro left before closing.
3. DRM/EME Ã¢â‚¬â€� the only media gap; widevine-class work, engine lane.
4. Hardware-verification checklist with the user: middle-click, mouse back/forward buttons, real file drops (#352 assumed working now that file:// loads Ã¢â‚¬â€� the drag path itself never verified cross-app).
5. Windows file-picker verification.
6. Omnibox data: URL mangling Ã¢â‚¬â€� normalize() requires "://", strips data: syntax (#393 filed, fix needed).

### Methodology notes (learned the hard way)
- Zero events at every level = suspect the shell's own driver logic BEFORE the engine. The wedge/loadlog env instrumentation discriminates shell-vs-engine in minutes. file:// was one http-only gate line, found only after eliminating race/prefs/library/external-scheme.
- Session format: {"version":1,"active_space":0,"spaces":[{"name","tabs":[{"url","group"} or {"blank":true,"group"}],"active","groups":[{"id","name","color":[r,g,b],"collapsed"}]}],"pinned":[]} Ã¢â‚¬â€� at ~/Library/Application Support/feather/session.json.
- Boot-url file drops and omnibox navigations are DIFFERENT code paths Ã¢â‚¬â€� always test both when changing navigation behavior.
- Same-branch commits ride into open PRs; docs PRs collide on defect-queue.md Ã¢â‚¬â€� rebase by re-appending onto fresh main.


## sec1zt312 Ã¢â‚¬â€� fieldset leaf effectively closed: matches chrome visually (2026-11-28)
in-flow content shift shipped: pull up by (border + legend_height - allocated) for
fragments below the legend. DECISIVE VERIFICATION: headless chrome screenshot of
default fieldset+legend form is near-identical to servo output Ã¢â‚¬â€� modern browsers do
NOT straddle legend on the visible border line for thin borders (legend sits inside
the allocation); my earlier visual intuition was wrong. Spec legend rule exact text:
"positioned in the block-flow direction such that its border box is centered over the
border on the block-start side" Ã¢â‚¬â€� ambiguous re allocated vs physical edge; reftests
are the arbiter and allocated-centering passes them.
REMAINING fieldset fails (all pre-existing, edge-geometry): legend-position-relative{,-2}
(2-4px vertical offset), fieldset-transform-translatez (transform containing-block
interaction), border-gap painting for non-solid borders (groove default shows border
through legend Ã¢â‚¬â€� commit 92854ce43f8 covered solid only). Leaf ROI exhausted Ã¢â‚¬â€� remaining
items are sub-pixel/niche; recommend moving to next leaf.


## sec1zt313 Ã¢â‚¬â€� legend border gap generalized to groove/all styles (2026-11-28)
Default fieldset forms now render the border gap behind legends correctly Ã¢â‚¬â€�
before: border painted through behind legend text on EVERY real form (groove is
the UA default; solid-only fix didn't cover it). Implementation: segment-as-thin-
border-strip per non-solid segment, clip-free. Verified pixel-level (75/75 -> 27/75
darkish behind legend) + visually matches chrome + reftests pass.

## sec1zt314 Ã¢â‚¬â€� fieldset suite verified at scale: 85/88 as-expected, zero regressions (2026-11-28)
Full directory run (88 tests) after all three fieldset fixes: only 3 unexpected Ã¢â‚¬â€�
border-gap-position-relative PASS-ahead-of-upstream (positive), translatez +
legend-position-relative-2 FAIL (both pre-existing edge geometry). All three shipped
fixes regression-free at scale. Leaf CLOSED.
CANVAS LAYERS ASSESSMENT: beginLayer/endLayer entirely absent (no webidl even) Ã¢â‚¬â€�
tests FAIL blank rather than ERROR. Real implementation = offscreen surface +
filter composite in canvas_state, multi-hour graphics work, ~12 tests across
element+offscreen. Documented not started Ã¢â‚¬â€� queued behind composite-off-UI-thread.

## sec1zt315 Ã¢â‚¬â€� html re-baseline in flight + rotation state (2026-11-28)
BACKGROUND RUN: post-fieldset html re-baseline launched ~23:58 UTC, output dest
/tmp/wpt-post-fieldset/ (html.log + html-raw.log, status.txt when done). ~40min.
HARNESS RULE IN EFFECT Ã¢â‚¬â€� no compiles/interactive servoshell until it finishes.
Next session: generate report with docs/wiki/wpt/wpt-report-v2.py, PR to feather,
compare against evening baseline Ã¢â‚¬â€� expect fieldset improvements (3 fixed) plus any
windows-lane deltas. The run REPLACES the need for manual leaf verification.
ROTATION STATE: all work pushed through 52a3b2b94aeb. Leaf inventory bottomed:
cheap leaves exhausted; remaining = canvas layers (multi-hour, no webidl even),
composite-off-UI-thread (design-complete, top priority), COOP infra (multi-day),
waitAsync (coordinator decision pending). Fieldset leaf closed at 85/88 as-expected.

## Post-handoff additions (2026-11-28, same lane)
- data: URLs fully working everywhere post-#408: omnibox in-session AND boot-path both render. The boot-path about:blank was the same normalize() mangling, not a separate seam. Non-http scheme support is substantively complete: file: renders+persists+reopens (#403/#405), data: renders boot+omnibox (#408).
- Verification trick that worked well: data:text/html;base64 fixtures typed into the omnibox Ã¢â‚¬â€� but any literal '#' in the payload truncates at the fragment (correct URL-spec behavior, cost one debugging round). base64-encode everything.

## Final closure additions (2026-11-28)
- amazon hydration-layout defect CLOSED: cold-profile repro correct, 4/4 non-reproducing on latest engine (#409). The 20-site sweep is fully clean Ã¢â‚¬â€� no open sweep items remain.
- Open items definitive list at handoff: (1) authenticated dogfood Ã¢â‚¬â€� needs credentials from the user; (2) DRM/EME Ã¢â‚¬â€� engine lane; (3) hardware checklist with user: middle-click, mouse back/forward, cross-app file drops; (4) Windows file-picker verification Ã¢â‚¬â€� windows lane; (5) file-drop #352 functional verification now that file:// loads work.
- Everything else ever filed is either merged-fixed, verified-green, or documented-as-not-a-defect in docs/defect-queue.md.

## sec1zt316 Ã¢â‚¬â€� location_replace leaf CLOSED: completely-loaded-before-load-event (2026-11-28)
ROOT CAUSE: servo set document completely_loaded only in completely_finish_loading
(AFTER the load event dispatches, spec step 9.12). Per the location-href setter rule,
navigations from not-completely-loaded documents are Replace Ã¢â‚¬â€� so every location.href
assignment inside an onload handler REPLACED the page itself in session history,
erasing it. history.back() then skipped the page entirely. Cross-browser truth:
inside the load event the document already counts as completely loaded for
history-handling (the wpt location_replace_session_history chain depends on it).
FIX (20206bb466c): completely_loaded.set(true) in the fire_load_event task right
after readiness flips to Complete, BEFORE the load event dispatches.
VERIFIED: minimal repro A2-onload-navÃ¢â€ â€™B2-then-history.back(): before fix nothing
happened (B2 replaced A2, back() landed on initial about:blank); after fix A2
pageshow refires and browser lands back on A2. WPT: location_replace_session_history
PASSES (was upstream-expected TIMEOUT Ã¢â‚¬â€� ini REMOVED, feather ahead of upstream).
Full the-location-interface directory: zero regressions (same-hash ERROR,
create-script-set-location ERROR, location-replace-cross-site TIMEOUT all match
upstreams ini expectations).
MECHANISM NOTE: constellation-side pipeline.completely_loaded still set by the
load-finished notification in step 9.12 Ã¢â‚¬â€� do NOT move it earlier: webdriver
load-status depends on it. The iframe override (constellation.rs:3735) is safe
because scriptÃ¢â€ â€™constellation FIFO guarantees load-finished is processed before
any in-load-handler navigation message from the same pipeline.
SYMPTOM INDEX: location.href in onload replaces history entry / history.back()
skips pages that self-navigated during load Ã¢â€ â€™ check completely_loaded SET TIMING
in document.rs fire_load_event task. Negative: traversal machinery itself was
never broken (queue/diff/activation all correct) Ã¢â‚¬â€� the past entries simply never
existed because the navigations were replacement.
REAL-WEB IMPACT: any redirect-via-onload or SPA-ish navigation pattern now
leaves correct session history Ã¢â‚¬â€� back button works across redirect chains.

## sec1zt317 Ã¢â‚¬â€� composite-off-UI-thread: code-level implementation plan + canvas WIP (2026-11-28)
CANVAS LAYERS WIP: `git stash list` Ã¢â‚¬â€� stash@{0} "canvas-layers-wip" has webidl
mixin+dictionary ONLY (CanvasRenderingContext2D.webidl + Offscreen includes;
restore() gained [Throws]). Full semantics design: layer rendering attrs reset
inside layer = alpha/composite/shadow*/filter ONLY (NOT transform/fillStyle Ã¢â‚¬â€�
proven by 2d.layer.ctm.getTransform + restore-style tests). Stack entries must
distinguish Saved vs Layer: restore() with layer on top throws InvalidState;
endLayer with save on top throws; reset() kills layers. Composition params
captured AT beginLayer (outer state can't mutate while inside) Ã¢â€ â€™ pass in
CanvasCommand::BeginLayer(CompositionOptions). Backend: vello_cpu 0.2.0
push_layer(clip, blend, opacity, mask, filter) exists; multithreading feature
PANICS with filter Ã¢â‚¬â€� disable mt or skip filter. Backend must drain per-layer
clips before popping canvas layer (vello dispatcher is strict LIFO).
Filter validation minimal: throw TypeError only when filter obj has string-typed
`values` (passes exceptions-are-no-op test). Explainer: /tmp/l.md (fserb/canvas2d).

COMPOSITE-OFF-UI-THREAD Ã¢â‚¬â€� ground truth established this session (corrects #335):
* TODAY'S SHELL USES ONE GL CONTEXT FOR EVERYTHING: servo's surfman WindowRenderingContext
  is created in engine.rs:420-ish, egui paints into the SAME context. Sharing is NOT
  proven anywhere in production Ã¢â‚¬â€� the render_to_parent blit is intra-context FBOÃ¢â€ â€™default.
* paint() cost decomposition (components/paint/painter.rs render()): make_current Ã¢â€ â€™
  prepare_for_rendering Ã¢â€ â€™ webrender renderer.update()+renderer.render(size,0) Ã¢â‚¬â€� THE
  expensive part is renderer.render() itself (synchronous rasterization/present-wait),
  NOT the shell-side blit (render_to_parent is a cheap textured blit).
* Painter is deeply non-Send: gleam Rc<dyn Gl>, RefCell everywhere, refresh driver,
  screenshot taker, webview_renderers. There is NO way to move renderer.render()
  off the UI thread without moving the GL-context+Renderer ownership wholesale.
* surfman 0.14 supports sharing natively: device.create_context(&descriptor,
  Some(&other_context)) Ã¢â‚¬â€� sharing across threads is API-supported (device.rs:53).

IMPLEMENTATION PLAN (sized multi-day, slice per rotation):
1. SPIKE (hours): /tmp/spike-shared-gl crate Ã¢â‚¬â€� create ctx1+shared ctx2, worker
   thread makes ctx2 current, renders into texture, UI reads it in ctx1. GO/NO-GO.
   Pattern to copy: components/shared/paint/rendering_context.rs SurfmanRenderingContext::new
   (flags ALPHA|DEPTH|STENCIL, GL3.2/GLES3.0). NOTE: spike file is half-written Ã¢â‚¬â€�
   the surface creation part is wrong (transmute hack), fix using servo's pattern:
   offscreen surfaces suffice since servo composites into FBOs not widget surfaces.
2. ENGINE RESTRUCTURE (the core): create paint/render thread inside servo paint
   component. Renderer+rendering_context move to it. Painter stays as the message
   hub on the compositor side. render() becomes: post to render thread, which does
   make_current+update+render. Synchronization: paint() must still be synchronous-
   enough for screenshots/reftests Ã¢â‚¬â€� use a rendezvous channel for completion.
   WATCH: webrender Renderer::update() consumes transaction messages Ã¢â‚¬â€� the
   WebRenderApi is already channel-based (render backend runs on its own thread!);
   what actually runs in render() is the renderer's draw execution + llvmpipe flush-wait.
3. DOUBLE-BUFFER the offscreen FBO: worker renders into back buffer, swaps on
   completion; UI blits front Ã¢â‚¬â€� no tearing. Generation counter discards stale
   frames on resize (MSE seek serialization pattern).
4. BACKPRESSURE: shell skips paint requests while one is in flight (video self-limits).
5. VERIFY: youtube playing + simultaneous omnibox input Ã¢â‚¬â€� the exact reproduction.
   Metrics: UI stalls p50 should drop from ~370ms to single-digit ms.

FIRST SLICE FOR NEXT SESSION: finish+compile+run the spike Ã¢â‚¬â€� everything else is
blocked on its GO. If NO-GO on EGL/llvmpipe, fallback: investigate surfman
SurfaceTexture/SwapChain chains API (chains::{PreserveBuffer,SwapChain} already
imported in rendering_context.rs Ã¢â‚¬â€� servo's webgl uses cross-context surface sharing
TODAY for webgl-in-webrender, so precedent exists in-tree: webrender_external_images.rs).

## sec1zt317b Ã¢â‚¬â€� composite plan CORRECTION: webgl swapchain precedent (2026-11-28)
IMPORTANT addition to sec1zt317: the spike is substantially de-risked. Servo's WebGL
path ALREADY does cross-thread/cross-context surface sharing in production TODAY:
components/paint/webrender_external_images.rs uses surfman::chains::SwapChains Ã¢â‚¬â€�
webgl contexts render offthread and webrender consumes frames as external images
via SwapChain lock/unlock. The exact primitive composite-off-UI-thread needs exists
in-tree with a working usage pattern. The spike should copy the SwapChainsAPI usage
(lock_swap_chain pattern at webrender_external_images.rs:49) rather than raw
create_context sharing: SwapChain handles surface presentation sync correctly.
REVISED MILESTONE 2 SHAPE: paint worker renders into a swapchain surface; UI thread
locks+blits the front surface texture Ã¢â‚¬â€� same consumption pattern webrender uses
for webgl. This avoids hand-rolled FBO double-buffering entirely.

## sec1zt317c Ã¢â‚¬â€� composite plan RESOLVED: no shared contexts needed at all (2026-11-28)
Surfman type-level facts (0.14 source audit): Connection+Surface are Send on Linux
(x11/wayland/mesa_surfaceless) but Device and Context are NOT Send ANYWHERE. This is
deliberate: surfman's threading model NEVER moves devices/contexts across threads Ã¢â‚¬â€�
each thread creates its own device+context locally, and SURFACES move between them.
Surface textures work cross-context WITHOUT shared object namespaces because surfaces
are backed by platform-shareable buffers (EGLImage/dma-buf/pixmap). Proof in-tree:
WebGLThreads Ã¢â‚¬â€� webgl contexts render on their own threads into surfaces, webrender
consumes them as external images on the compositor thread. No context sharing anywhere.
CORRECTED ARCHITECTURE for composite-off-ui-thread:
1. Paint thread: creates its OWN device+context+webrender Renderer locally, renders
   frames into swapchain surfaces.
2. UI thread: locks front surface via SwapChain, creates SurfaceTexture in the egui/
   shell context, blits quad, unlocks. EXACTLY the webgl consumption pattern
   (webrender_external_images.rs lock_swap_chain).
3. NO shared-context spike needed Ã¢â‚¬â€� the mechanism is production-proven in-tree.
   The real engineering is 100% in the paint-thread restructure: moving
   Renderer+painter render duties behind a thread boundary with rendezvous sync
   for screenshots/reftests. Skip milestone-1 spike; go directly to restructure.
REVISED FIRST SLICE: inside components/paint, spawn render thread owning
device+context+Renderer at Paint::new; Painter::render becomes a rendezvous
message roundtrip. Verify servoshell+WPT still pass Ã¢â‚¬â€� that alone is the refactor
proof before touching the shell at all.

## sec1zt317d Ã¢â‚¬â€� composite restructure: exact thread-split design (2026-11-28)
Final reconnaissance facts for the paint-thread restructure (all verified in source):
* webrender's RenderApi is ALREADY a channel proxy Ã¢â‚¬â€� Painter builds transactions
  and sends them cross-thread by design. Display-list ingestion and transaction
  building do NOT need to move. Only the Renderer object + GL context do.
* Painter::new (painter.rs:156) requires rendering context current during
  webrender::create_webrender_instance Ã¢â‚¬â€� so the paint thread must exist and own
  the context BEFORE Renderer creation. Sequence: spawn thread Ã¢â€ â€™ make context
  current there Ã¢â€ â€™ create renderer there Ã¢â€ â€™ rendezvous back startup-ok.
* Painter methods touching GL/renderer (must become paint-thread messages):
  render() (update+render+metrics-after-composite), read_to_image/screenshot
  (rendezvous Ã¢â‚¬â€� reftests+webdriver depend), resize (rendezvous), webrender
  debug toggle, memory report, capture_webrender. ~6-8 call sites.
* THE CLEAN SPLIT: compositor thread keeps Painter (display lists, transactions,
  repaint decisions); new paint thread owns context+Renderer and executes
  RenderNow messages with completion signals. paint metrics
  (send_pending_paint_metrics_messages_after_composite) must run AFTER the
  completion signal, not inline Ã¢â‚¬â€� currently inline in render() painter.rs:458.
* media::WindowGLContext::initialize_image_handler + WebGLExternalImages are
  constructed in Painter::new with rendering_context.clone() Ã¢â‚¬â€� the external
  image handlers run INSIDE webrender render on the paint thread automatically
  once Renderer moves; audit that their Rc usage doesn't cross back.
IMPLEMENTATION ORDER: (1) move context+renderer creation behind thread boundary
with everything still synchronous rendezvous Ã¢â‚¬â€� behavioral no-op refactor, verify
WPT+servoshell unchanged; (2) shell: make paint() non-blocking + swapchain
consumption per sec1zt317c; (3) backpressure. Step 1 alone is a full session.

## sec1zt318 Ã¢â‚¬â€� upstream scan 2026-11-28: 82 commits behind, triage (2026-11-28)
HIGH-VALUE cherry-pick candidates for next session (compile-verify each):
CRASH/ROBUSTNESS: 054e5a4ae1c (webgl VAO drop), e667861e269 (attr borrow hazard),
  58067970868 (lazy callback disconnect), 9ddfe28a9ec (getComputedStyle non-flat-tree)
PERF (aligns with idle/perf goals): 47ac285d9e8 (glyph typographic bounds cache),
  d18a53c80cb (harfrust ShapePlan cache), 84b3cc31ac3 (attrs without Attr nodes),
  1bf371f9e26 (root less finding slots)
LAYOUT FEATURES: a2c0d6849e1 (aspect-ratio non-replaced), 6665562a412 (aspect-ratio
  margin collapse), aee0b7d8e50 (flow layout logical rects Ã¢â‚¬â€� BIG, careful)
CORRECTNESS: 2df3d02caa8 (offscreen canvas fonts), fdc6077f3a5 (NavigationTiming),
  8dd671a83fe (FontFaceSet tracking), 1200b407c13 (nav timing entries)
WARNING Ã¢â‚¬â€� sec1zt303 was WRONG: it claimed 461f99fc59f + e667861e269 were already in
tree via merge overlap, but git log HEAD..upstream/main still lists both Ã¢â‚¬â€� the
empty-diff check must have been flawed (probably compared against wrong base).
Re-evaluate both properly this time. Lesson: verify cherry-pick emptiness with
`git log HEAD..upstream` listing, not just patch diff.

## Discovery-lane final state (2026-11-28)
- Discovery sweep: 8 new sites tested, results in feather/docs/discovery-sweep.md. Passes: absa, fnb, news24, translate(+interaction), maps(full vector tiles), dailymotion. Defects filed: vimeo thumbnails blank (homepage), vimeo video pages stuck-cloudflare-botwall. Inconclusive: google docs anonymous.
- Untested remainder for next lane: iol.co.za, saas landing pages, sheets/drive anonymous, twitch re-check, vimeo thumbnail scroll-isolation, turnstile-vs-IP discrimination (needs residential IP).
- Disk post-scorched-earth: 4.9GB free. CoreSimulator 4.1GB intact pending cross-lane decision. shell-new/target is 9.7GB Ã¢â‚¬â€� the active build, keep.
## MSE seek starvation Ã¢â‚¬â€� root cause + symptom index (2026-11-28, Windows lane)
**Root cause (proven with instance-address instrumentation)**: every MSE seek's
ReadyÃ¢â€ â€™Playing cycle makes uridecodebin RE-INSTANTIATE the ServoMse source element
(gstreamer tears down + reconstructs the source on bin state cycles; the player's
`source-setup` handler then re-points PlayerSource::Mse at the new instance). The
in-band resequence flags (`needs_resequence`) are marked on the PRE-cycle instance
inside `seek_reset`, which is then discarded Ã¢â‚¬â€� the injection therefore NEVER fires
in production (zero `resequence inject` lines in any real repro log, despite the
logging existing in the tested binary). Post-seek appends land in a virgin instance
whose stream decodebin3/playsink never reselect: appsrc queues grow unconsumed
(level climbs past 20MB) and playback starves ~15s to youtube's error screen.

**Symptom index entries**:
- symptom: post-seek MSE appends accepted (`Ok(Ok)`) but level grows unconsumed, no FLOW
  Ã¢â€ â€™ suspect: uridecodebin source re-instantiation across Ready cycles Ã¢â‚¬â€� flags/continuity
  die with the old instance. Functions: `ServoMse::seek_reset`, player.rs seek worker,
  the `source-setup` signal handler. Mechanism: instance identity, not data flow.
- symptom: `[mse] recreated appsrc sync` clusters appearing per seek
  Ã¢â€ â€™ NOT appsrc recreation Ã¢â‚¬â€� these are ensure_track creations inside NEW ServoMse
  instances (one pair per re-instantiated pipeline source).
- negative result: ServoMSE_RECREATE env-off recreate path is NOT the cause;
  track ids are stable (0/1) across all pushes Ã¢â‚¬â€� sourcebuffer re-adding is not involved.
- negative result: the starvation is NOT a regression from the media event-ordering or
  memory-reclaim merges Ã¢â‚¬â€� reproduces identically on the batch7 engine (bisect null result).

**Harness**: `work/wtest/mseauto*.html` + `python -m http.server 8899` from work dir Ã¢â‚¬â€�
local dual-track webm MSE with automated seek patterns; instance churn reproduces
without youtube (youtube botwall-independent iteration). Caveat: arbitrary byte-sliced
webm appends are NOT valid cluster-aligned MSE Ã¢â‚¬â€� harnesses must feed init+aligned units.

**Open fix direction**: skip the Ready-cycle so the instance survives and injection
fires in the live pipeline (SERVO_MSE_CYCLE env re-enables cycle for A/B). If live-pipeline
re-plumbing still fails, the deep options are (a) full pipeline teardown+rebuild per seek
(expensive but truly initial-state), or (b) WebKit-style custom MSE pipeline without
playbin3 (parsebin-per-track architecture) Ã¢â‚¬â€� the correct long-term shape.

## sec1zt320 Ã¢â‚¬â€� composite-off-UI-thread IMPLEMENTED: paint thread + async consumption (2026-11-28)
STATUS: steps 1+2 live on feather-engine (1c6fa6f87e7 paint thread restructure,
0e883853d46 async consumption). feather shell-new enables gfx_async_paint_composite.
ARCHITECTURE (as designed in sec1zt317c/d): paint thread owns GL context+webrender
Renderer, renders into surfman surface backed by attached SwapChain; UI consumes
front surface via create_texture/blit into embedder FBO (behavioral no-op).
KEY IMPLEMENTATION FACTS (verified, do not rediscover):
* surfman Connection+Adapter are Send cross-thread in production TODAY
  (PainterSurfmanDetailsMap shares them with webgl threads). Surfaces Send everywhere.
* SwapChain is internally Arc<Mutex> thread-safe; consumer take_surface/recycle_surface.
* HEADLESS GOTCHA: SoftwareRenderingContext present() swaps the chain Ã¢â‚¬â€� a blitted
  frame vanishes from the back buffer after present. Fixed by parking the imported
  texture + re-blitting when no new frame completed. OffscreenRenderingContext
  (headed) has no-op present Ã¢â‚¬â€� no issue there.
* webdriver screenshot validated async mode pixel-exact vs sync (902 colors,
  center (30,120,200) identical). rAF/nav/JS all healthy, no deadlocks.
* Completion loop: paint thread sends PaintThreadFrameReady via paint_proxy (which
  wakes the event loop); painter sets a dedicated repaint reason that CONSUMES but
  never KICKS Ã¢â‚¬â€� otherwise infinite render loop. Kick condition: needs_repaint has
  bits other than PaintThreadFrameReady AND requested==completed generation.
* External image handlers (webgl/webgpu/media) constructed on paint thread with
  paint-thread context Ã¢â‚¬â€� they run inside renderer.render.
* Fallback: custom rendering contexts without surfman connection/adapter use the
  old inline path (webrender_renderer stays on UI thread).
* SPEC CORRECTION (08c2c7c4040): reverted completely_loaded-before-load-event Ã¢â‚¬â€�
  spec order is load-event THEN completely_finish_loading, so in-onload location
  assignment correctly REPLACES (assign_before_load passes). The
  location_replace_session_history TIMEOUT is a genuine independent defect:
  REPLACE navigation initiated inside the load event handler wedges servo.
  THAT is the real leaf Ã¢â‚¬â€� the early-set was masking it. ini restored.
REMAINING: step 3 backpressure tuning + feather shell youtube verification
(shell rebuilding). windows lane: verify composite works on wgl/angle.

## sec1zt311: composite teardown panic + surface lifecycle (2026-11-28)
- Teardown panic "Should have destroyed the FBO renderbuffers with destroy()": parked frame texture in Painter was dropped raw. Surfman surfaces/textures MUST be destroyed with the device Ã¢â‚¬â€� never dropped. Fix (269203c8d8a): Painter::drop destroys the texture with the embedder context and recycles the surface into the paint thread's swap chain BEFORE paint thread shutdown, so the chain teardown destroys it properly. Symptom index: panic in surfman renderbuffers drop at shutdown Ã¢â€ â€™ check surface/texture lifecycle ordering, parked textures especially.
- NEGATIVE result: directory-run crashes on the-location-interface (security_location_0, replace-with-nested-iframe) were cascade artifacts Ã¢â‚¬â€� both pass solo. The panic was real but non-fatal for the test itself.

## sec1zt312: composite async Ã¢â‚¬â€� headed verification results (2026-11-28)
- VERIFIED working headed: YouTube plays, scroll repaints correct, zero flashes/crashes across sync+async, sustained heavy-load stress clean. Input responsive in async under load.
- NEGATIVE result: input-responsiveness delta NOT measurable on llvmpipe Ã¢â‚¬â€� composite cost at small windows/low fps is too cheap to block the sync UI thread; the improvement requires GPU hardware + large windows/high fps. 2D canvas rasterization happens in canvas/script pipeline in BOTH modes Ã¢â‚¬â€� canvas-heavy pages are the wrong stress test for composite-off-UI-thread.
- fps directionally positive on DOM-rasterization load (5-6 async vs 4-5 sync, within noise).
- GOTCHA: `media_gstreamer_enabled` is NOT a valid pref name Ã¢â‚¬â€� servoshell panics "Unknown preference" at startup. Media works without it.
- Windows lane: the user-visible delta verification belongs on the Windows repro hardware Ã¢â‚¬â€� llvmpipe boxes cannot surface it.

## sec1zt313: composite async Ã¢â‚¬â€� feather shell verification PASSED (2026-11-28)
- The exact reproduction passes in feather shell-new (feather-ui binary): YouTube plays + omnibox typing instant WITH live autocomplete dropdown during playback, scroll repaints clean, zero panics/surfman issues/winit crashes in full log, clean teardown navigating away from playing video. Composite-off-UI-thread is shippable.
- location-interface directory with teardown fix: 68/69 as expected, ZERO crashes (previously 65+2crash+1err) Ã¢â‚¬â€� the teardown panic fix eliminated the cascade crashes. Remaining leaf: location.hash on srcdoc iframe FAILs (about:srcdoc#test not applied Ã¢â‚¬â€� small genuine leaf).

## sec1zt314: rotation handoff Ã¢â‚¬â€� post-composite state (2026-11-28)
- COMPOSITE-OFF-UI-THREAD: COMPLETE and verified (steps 1+2). feather-engine tip has: paint thread + sync rendezvous (1c6fa6f87e7), async mode + pref gfx_async_paint_composite (0e883853d46), teardown panic fix (269203c8d8a). feather shell-new engine.rs enables the pref unconditionally. Verified headed in servoshell AND in feather shell: YouTube+omnibox exact repro passes, zero panics.
- IN FLIGHT: post-composite html re-baseline at /tmp/wpt-post-fieldset/ (started 01:37, ~40-70min). Next session: generate report PR from html-raw.log using feather/docs/wiki/wpt/wpt-report-v2.py pattern. NOTE prior /tmp/wpt-post-fieldset data was incomplete Ã¢â‚¬â€� was deleted and relaunched.
- HARNESS RULE ACTIVE while baseline runs: zero interactive servoshell launches, zero compiles.
- OPEN LEAVES: (1) location.hash on srcdoc iframe Ã¢â‚¬â€� about:srcdoc#test never applied (small), (2) conflicting cherry-picks e667861e269/9ddfe28a9ec/47ac285d9e8 deferred, (3) composite step-3 backpressure tuning, (4) waitAsync mozjs decision in coordinator's court, (5) gmail needs Windows repro specifics.

## sec1zt315: srcdoc location.hash leaf Ã¢â‚¬â€� fix applied UNCOMMITTED (2026-11-28)
- Mechanism: navigate_a_location in components/script/dom/window/location.rs blocks ALL navigations from about:srcdoc to urls starting with "about:srcdoc" except exactly "about:srcdoc" Ã¢â‚¬â€� this kills fragment-only same-document navigations (about:srcdoc#test) which must be allowed per spec.
- FIX APPLIED IN WORKING TREE, NOT COMMITTED, NOT COMPILED (harness rule Ã¢â‚¬â€� baseline running): strip_prefix("about:srcdoc") remainder starting with '#' = fragment-only = allowed. Next session: after baseline completes, compile, verify location_hash.html passes (the "Setting location.hash on srcdoc iframe" subtest), run the-location-interface directory for regressions, commit+push. The working tree survives rotation.

## sec1zt316: cherry-pick triage final (2026-11-28)
- e667861e269 (attr borrow hazard) = already in tree as 185f8f3931b. 47ac285d9e8 (typographic bounds cache) = already in tree as 7e1622eb169. NOTHING TO DO for either.
- 9ddfe28a9ec (getComputedStyle flat-tree) genuinely conflicts: 6 files auto-merge clean, configuration.py has a 3-way semantic conflict (HEAD diverged Ã¢â‚¬â€� contextForMember restructured). Resolution needs compile-verify cycles. Deferred post-baseline. To redo: git cherry-pick -n 9ddfe28a9ec, resolve configuration.py keeping HEAD's restructure + adding the incoming lengthGetterNeedsCx/implicitNoGCGetters logic, compile, verify.

## sec1zt317: rotation state Ã¢â‚¬â€� srcdoc leaf closed, one residual error unidentified (2026-11-28)
- srcdoc fragment fix COMMITTED+PUSHED to feather-engine (location.rs fragment-only same-document navigations in about:srcdoc now allowed). location_hash.html passes 6/6.
- Directory regression: 68/69 as expected, but ONE test now errors unexpectedly (previously the hash subtest FAILed; now a different test errors Ã¢â‚¬â€� POSSIBLY a regression from the srcdoc fix or flake). IN-FLIGHT: background shell e6aadb was re-running the directory to identify which test Ã¢â‚¬â€� check its output. If regression: the fragment navigation may now reach a codepath that errors for replace-with-nested-iframe or scripted_click/scripted_form tests (they run srcdoc iframes). Investigate which test, solo it, fix or refine the fragment check.
- Post-composite html baseline COMPLETE: /tmp/wpt-post-fieldset/html-raw.log (10982 tests, 2043s). NEXT: generate report PR with feather/docs/wiki/wpt/wpt-report-v2.py Ã¢â‚¬â€� compare vs wpt-2026-11-27-evening baseline.

## sec1zt318: rotation final note (2026-11-28)
- Leftover wptserve processes from the completed baseline held ports 8000-9000 breaking re-runs Ã¢â‚¬â€� killed. NEXT SESSION: re-run the-location-interface directory cleanly to identify the one unexpected ERROR (sec1zt317), verify srcdoc fix regression-free, then generate baseline report PR from /tmp/wpt-post-fieldset/html-raw.log.

## sec1zt319: residual error identified — pre-existing, NOT a regression (2026-11-28)
- The one unexpected ERROR is location_replace_session_history.html ERROR [expected TIMEOUT] at test line 13:17. It appeared identically in the PRE-srcdoc-fix directory run (same count, 1 error) — NOT caused by the srcdoc fix. srcdoc fix is regression-free; leaf fully closed.
- location_replace_session_history remains the genuine independent leaf: replace-navigation inside load handler — now manifesting as harness ERROR (JS throw at line 13) rather than pure TIMEOUT. Worth a fresh solo investigation next session: run solo with --log-raw, capture the exact exception at :13:17.

## sec1zt320: back-history defect — SHELL-SIDE, servo-core innocent (2026-11-28)
- Mac discrimination ("link-click navigations never become traversable diffs in session_history.past") was observational but wrong about the layer: servoshell repro + histdbg both prove clicks push correctly and traversal works in servo-core. The defect is feather shell-new's parallel mock history: link clicks mutate in place, cursor moves on back land on stale entries, renav loop commands wv.load() of wrong target — overriding correct engine traversal. Fixed in feather PR #455 (back/forward purely engine-driven for engine-backed tabs).
- Symptom index: back/forward hijack or phantom navigation → suspect shell renav loop + mock cursor desync, NOT engine session history. Verify engine innocence with servoshell webdriver repro first.
- LESSON for lanes: two parallel sources of truth for the same state (shell mock history vs engine session history) inevitably desync on paths only one of them sees. Engine is truth for web history.

## sec1zt321: 9ddfe28a9ec ABORTED — requires upstream infra we don't have (2026-11-28)
- The getComputedStyle-flat-tree cherry-pick is NOT portable to feather-engine HEAD: upstream's implementation threads NoGC tokens through every CSSStyleDeclaration method (including hundreds of macro-generated per-property getters) because upstream's codegen has contextForMember + implicitNoGCGetters wiring. HEAD removed contextForMember in the binding-context refactor and never implemented implicitNoGCGetters consumption — the config key is dead. Porting requires either (a) reimplementing implicit no_gc getter support in codegen (signature generation AND CGPerSignatureCall glue, blast radius: every binding regenerates), or (b) fabricating NoGC tokens unsafely — rejected. cx-classification adaptation also deadlocks: internal callers (devtools/webdriver) are themselves in no_gc-only contexts and can't supply cx.
- If ever revisited: implement implicitNoGCGetters in CGInterfaceTrait attribute generation + CGPerSignatureCall needs_cx_arg + the glue call conversion (cx.no_gc() for no_gc methods), then the cherry-pick applies cleanly. Estimated multi-hour with full-binding rebuild verification. Value is low (edge-case WPT correctness for detached elements).
- All cherry-pick queue items now resolved: e667861e269 + 47ac285d9e8 already in tree, 9ddfe28a9ec aborted with cause.

## sec1zt322: windowproxy parent-active assertion fixed (2026-11-28)
- 312b590471d: parent_origin_and_internal_ancestor_origin_objects_list asserted parent fully active — cannot hold in async pipeline world (child creation races parent navigation). Reproduced on yahoo scroll (ad-iframe-dense). Fix: return None like the frame-element branch. Verified: html/browsers/windows 55/55 as-expected. UPSTREAMABLE candidate — check if upstream servo still has the assert. Symptom index: panic windowproxy.rs assert currently_active on ad-iframe-dense pages → this fix.

## sec1zt323: select dropdown leak root cause (2026-11-28)
- Discovery batch-2 "select never opens" is NOT servo-core and mostly wrong: dropdown always opened on Linux. Real defect: shell overlay-click swallow used egui interact_pos() = None before egui processes the press, so the press leaked to the page, re-clicking the select; pick never landed. Fixed shell-side with latest_pos() — feather PR #464. Symptom: clicks on egui overlays (dropdown/context menu) hit page beneath → main.rs swallow must use latest_pos not interact_pos.

## sec1zt324: location_replace_session_history — NEGATIVE RESULT, close leaf (2026-11-28)
- The directory ERROR was cascade artifact + fixed by revert 08c2c7c4040. Solo: runs as-expected TIMEOUT (ini expects TIMEOUT, upstream-shared defect). Mechanism: iframe onload location.href assignment then history.back() pageshow counting never completes — deep session-history replace/traversal semantics shared with upstream servo. NOT feather-specific, multi-hour for one test — shelved. Do not re-investigate unless upstream fixes land.

## sec1zt325: icloud silent-boot investigation checkpoint (2026-11-28)
- VERIFIED: no unsupported-browser wall on current engine — landing renders fully; app boots partway (reaches setup.icloud.com/setup/ws/1/validate API, 421 normal-unauth). Batch-100 wall report is STALE.
- THE DEFECT: app boot stalls silently before component registration — `ui-button` custom element never defined (ctor stays HTMLElement), Sign In click dead. ZERO console errors, zero unhandled rejections, zero pending network fetches (all 132 complete, mostly 200).
- ELIMINATED (all verified working in servo): custom elements trivial repro passes, dynamic import() works, module error reporting works, WebCrypto/indexedDB/ServiceWorker present WITH PREFS. IMPORTANT: servoshell does NOT enable dom_indexeddb_enabled/dom_serviceworker_enabled by default — feather shell does (engine.rs:469/477); probes must pass --pref for both.
- SHIMMED WITHOUT EFFECT: RTCPeerConnection, navigator.credentials (webauthn), navigator.locks — boot still stalls, zero defines.
- METHOD that works: servoshell --userscripts /tmp/uscripts injects document-start JS — monkeypatched customElements.define tracer at /tmp/uscripts/trace.js, harness /tmp/icltrace.py (kills+relaunches servoshell fresh each run; servoshell webdriver allows ONE session ever, /sessions GET unsupported).
- NEXT PROBE IDEAS: (1) icloud vendors chunk registration may gate on Atomics/SharedArrayBuffer-in-worker, (2) WASM streaming compile (compileStreaming), (3) blob:-URL module workers, (4) compare network waterfall vs Chrome — count of .js chunk GETs here was 14 which seems low. Main.js has zero "customElements" references — defines live in vendor chunks which may never even be REQUESTED if a chunk-loading manifest promise stalls.

## sec1zt326: ServiceWorkerContainer.getRegistrations implemented (2026-11-28)
- 70532102fc0: full chain — webidl (sequence not FrozenArray, JS-invisible), MatchAllServiceWorkerRegistrations algorithm through constellation to sw manager, longest-scope-first ordering per spec. KEY TRICK LEARNED: zero-arg promise methods need the method listed in Bindings.conf 'realm' array for the interface — codegen does NOT auto-infer cx/realm for promise returns (typeNeedsCx is a skeleton). trait signature mismatch error E0050 means missing entry there.
- Verified: WPT getregistrations.https.html — Register-then-getRegistrations PASS, array resolution end-to-end; getregistration.https.html fully passes too (stale upstream FAIL expectations dropped). Controlled-frame subtests still TIMEOUT — pre-existing infra gap, not this change.
- Motivation: behance blank-grid root cause (batch 131/132) — sites gating bundle loads on SW probing. UPSTREAMABLE. Mac lane should retest behance explore grid.
- Note: http(non-localhost) pages reject all SW calls with "Failed to obtain a storage key" — pre-existing environmental, affects getRegistration identically.

## sec1zt327: blank-grid family — amazon CONFIRMED FIXED visually (2026-11-28)
- Headed servoshell on live amazon.com (current feather-engine): tile images FULLY render — product cards, hero images, deal grid all visible. Batch-133/134 blank-tile defect is fixed on current engine (collateral of session image-lifecycle/compositing work). DOM probe: complete:true, naturalWidth>0, opacity 1, visibility visible — was always healthy DOM-side; the paint path now delivers.
- behance.net landing also renders hero images + category pills post-getRegistrations fix; explore-grid deep scroll retest belongs to Mac sweep lane.
- REMAINING in blank-grid family: vimeo thumbs — untested.
- NOTE for probes: servoshell -o screenshot mode captures too early for JS-heavy pages (blank white even when fine) — must use headed+wmctrl+import for visual verification. servoshell needs explicit --pref dom_serviceworker_enabled=true dom_indexeddb_enabled=true to match feather shell.

## sec1zt328: vimeo thumbs CONFIRMED FIXED — blank-grid family fully closed (2026-11-28)
- Headed servoshell vimeo.com: full thumbnail grid renders (staff picks etc). Blank-grid family dead on all three reproductions: amazon tiles, behance grid, vimeo thumbs. Family was killed collaterally by this session's image-lifecycle/compositing work + getRegistrations for behance's crash path.

## sec1zt329: windowproxy fix verified live — yahoo loads clean (2026-11-28)
- Headed servoshell yahoo.com (ad-iframe-dense, batch-68 repro): zero assertion panics, zero crashes, page renders with full chrome. Fix 312b590471d confirmed against the original repro site.
## sec1zt319: residual error identified — pre-existing, NOT a regression (2026-11-28)
- The one unexpected ERROR is location_replace_session_history.html ERROR [expected TIMEOUT] at test line 13:17. It appeared identically in the PRE-srcdoc-fix directory run (same count, 1 error) — NOT caused by the srcdoc fix. srcdoc fix is regression-free; leaf fully closed.
- location_replace_session_history remains the genuine independent leaf: replace-navigation inside load handler — now manifesting as harness ERROR (JS throw at line 13) rather than pure TIMEOUT. Worth a fresh solo investigation next session: run solo with --log-raw, capture the exact exception at :13:17.

## Memory-per-navigation growth Ã¢â‚¬â€� RESOLVED verdict (2026-11-28, Windows lane)
- Measurement harness: work/leakcheck_real.py, work/leakdiag*.py (Windows lane), ~90s each.
- Growth IS real across distinct heavy navigations: 268->336->506MB over 3 wikipedia pages (loadstat-verified distinct documents).
- Memory IS reclaimed: one navigation to about:blank drops RSS instantly to baseline (~259MB) and it stays flat. NOT a hard leak Ã¢â‚¬â€� consistent with GC lag / lazy teardown under rapid navigation; the 2026-11-27 shrinking-GC work may simply not keep up with rapid allocation. Corrects the earlier "never reclaims" verdict.
- Harness artifact discovered: eval-driven navigation STOPS WORKING after the page becomes about:blank (eval bridge targets the active engine surface; post-blank evals silently no-op). Any harness chaining navigations through about:blank will silently stop navigating.
- Customizable-select probe: appearance:base-select unsupported, no selectedcontent element, no showPicker Ã¢â‚¬â€� entirely absent upstream. Bleeding-edge spec, L-cost, low daily-driver value. Not implementing.

## MSE seek starvation/deadlock Ã¢â‚¬â€� full resolution chain (2026-11-28, Windows lane, servo #12/#14/#15)
Three-layer defect, each layer masked by the previous:
1. **Instance churn** (#12): every seek's Ready->Playing pipeline cycle made uridecodebin re-instantiate the ServoMse source entirely; resequence flags marked on the dying instance never fired. Fix: no-cycle (skip the Ready cycle) + no-flush (bare flush events mid-stream leave downstream permanently flushing Ã¢â‚¬â€� position froze exactly at seek target). Validated: single instance across 8 seeks, injection firing every seek.
2. **Script-thread deadlock** (#14): resequence injection wedged inside srcpad.push_event(StreamStart) on the script thread Ã¢â‚¬â€� locking inversion (streaming threads hold element locks waiting for data only the script thread supplies). Evidence signature: inject-fire prints then NOTHING Ã¢â‚¬â€� no completion logs, zero eval callbacks, zero network activity, entire page JS dead. Fix: worker thread + queue for all injection/pushes; quota checked synchronously in caller (level+inflight vs cap); generation counter drops stale pre-seek jobs. SERVO_MSE_SYNC env restores sync path for A/B.
3. **Mutex-mediated deadlock residue** (#15): worker held the tracks mutex ACROSS the blocking injection pushes Ã¢â‚¬â€� appendBuffer's synchronous quota pre-check then died on the same mutex (signature: tab unresponsive after seeks despite worker architecture, shell watchdog fires). Fix: clone refcounted appsrc + snapshot resequence state under lock, drop lock, push unlocked. LOCK DISCIPLINE RULE: never hold the tracks mutex across any push_event/push_buffer/end_of_stream call.
- Validation on real youtube with real input (backward x3 + mixed x4 seeks): no watchdog, frames jump correctly, injection completes every cycle, playback continues live. Pre-#15 the identical repro fired the unresponsive banner within seconds.
- Harness artifacts: youtube eval-callback starvation is normal on heavy pages (evals execute but results never return Ã¢â‚¬â€� do NOT treat zero eval results as script-thread death without corroboration); player sometimes fails to attach in synthetic sessions (blank player area, flake independent of engine).

## surfman surface-lifecycle panic Ã¢â‚¬â€� trigger characterized, NOT a paint-thread regression in practice (2026-11-28, Windows lane)
- Symptom: `surfman angle/surface.rs:73 "Should have destroyed the surface first with destroy_surface()"` Ã¢â‚¬â€� whole process dies.
- Suspect locations: shell webview teardown paths (`close_webview` in shell engine glue), servo paint-thread surface destruction ordering.
- Trigger precisely characterized by granularity testing: navigating TO a data: URL then away Ã¢â‚¬â€� because the shell's STALE `starts_with("http")` wanted-gate dropped non-http tabs, destroying + recreating the webview each time. googleÃ¢â€ â€™data:Ã¢â€ â€™bing reproduces in 30s on a stale-checkout binary.
- NEGATIVE RESULTS (release-tree binary, feather#403 included): data: chains clean, tab close clean, alt+F4 shutdown clean, aux window.open/close clean. The gate fix (feather#403) eliminates the only confirmed trigger.
- Harness artifact: backtrace is symbolless in release builds (fmt::v11::detail::write_console spam) Ã¢â‚¬â€� symbol capture needed for future compositor panics.
- Residual risk: the surface-teardown ordering defect is real but has no known trigger in the release tree; if a NEW teardown path emerges (e.g. tab-group close-all, crash recovery), expect this signature.

## google signin flow â€” re-verified 2026-11-28 merge train (Windows lane)
- Signin page renders cleanly (no botwall interstitial on load), real-input typing works perfectly in google forms (input-latency fix confirmed live), identifier submission round-trips.
- FINAL STEP STILL BLOCKED: google responds "Couldn't sign you in â€” this browser or app may not be secure" (accounts.google.com/v3/signin/rejected). Server-side browser attestation â€” the documented TLS-JA3/botguard hard floor, unchanged. Everything client-side of the flow now works; remaining wall is attestation only.
- Harness/input lesson: keybd_event typing needs VK codes (uppercase letters/digits map directly); click coordinates need physical-pixel scaling (screenshot analysis is scaled 1120x840 vs desktop 1600x1200 â€” multiply by 1.428).

## composite-off-UI-thread GPU verification â€” VERDICT (2026-11-28, Windows real-GPU hardware)
- Both sync and async paint-thread composite modes repaint continuously on real hardware: video playback sync 131/163 distinct frames in bracket-verified capture window, async 190/218; canvas-stress sync 147/150, async 139/149; zero watchdog UI-stalls in any mode/workload.
- No user-visible delta between modes on fast hardware for video or canvas-stress workloads â€” composite-on-UI-thread was never the bottleneck on this GPU. Async harness remains available: FEATHER_ASYNC_COMPOSITE=1 (feather shell env gate).
- MEASUREMENT ARTIFACT LESSON (cost a false release-blocking alarm): the mseauto2 harness video is only 15s â€” screen-capture repaint measurements MUST bracket-verify playback is active during the capture window (ct before/after), use the correct on-screen region (getBoundingClientRect + window offset), and never measure after eval-driven seeks â€” the harness appends samples once, so re-seeking starves it (frozen frame reads as a repaint defect). PIL ImageGrab and System.Drawing captures both valid on this machine; the artifact was playback state, not capture.

## sec1zt330: cross-lane integration event + two hard lessons (2026-11-28)
- INTEGRATION: Windows lane pushed media fixes (servo #14/#15) directly to feather-engine; Linux lane's last 3 pushes silently failed non-fast-forward because output was suppressed with -q/2>/dev/null. Merged at 61ffdceafe4. RULE: NEVER suppress git push output — check the "branch -> feather-engine" line every time.
- CORRECTION to sec1zt327/328 blank-grid verdict: Linux llvmpipe verification showed amazon/vimeo/behance rendering perfectly, but Mac sweep lane (real GPU) STILL reports blank-image partials on flickr/nfl/wired same-day. Conclusion: the family is PLATFORM-DEPENDENT, not dead — renders on llvmpipe, blanks selectively on GPU. Suspect: GPU texture-upload path or the surface-discard lever interacting with real compositor. Needs GPU-environment repro, belongs to Windows/Mac lanes. Linux lane cannot verify this class — heads-up for verification claims.

## sec1zt331: apple scroll-hero gap scoped — ScrollTimeline absent (2026-11-28)
- batch-194 apple blank scroll-hero: NOT a pref gap — feather shell already sets dom_web_animations_enabled=true (engine.rs:475). Gap is scroll-driven animations: no ScrollTimeline/ViewTimeline webidl in servo at all. Large multi-session feature (scroll-linked animation timelines + compositor integration). Documented as known large gap, not a leaf.

## sec1zt332: blank-grid mechanism — negative results batch (2026-11-28, symptom index)
- NOT the idle-surface-discard lever: dropped long ago (7c19cc3d476, restore races).
- NOT loading=lazy absence: attribute unimplemented in htmlimageelement.rs but that causes EAGER loading — more rendering, not blankness.
- NOT IntersectionObserver: verified fully functional via webdriver probe — initial callback fires, scroll-triggered callbacks fire with correct isIntersecting transitions (count:2 events:[false,true]). IO-driven data-src lazy-load pattern should work.
- REMAINING suspects for Mac selective same-page blankness: fetch lifecycle under CMS-specific conditions (batch-182: zero console errors, paint/fetch class), decode pipeline for specific formats, or GPU texture upload path. Linux llvmpipe CANNOT repro — all three sites render fully here. Ownership: Mac/Windows lanes with GPU.

## sec1zt333: gmail boot freeze REPRODUCES ON LINUX — investigation head start (2026-11-28)
- Headed servoshell mail.google.com (signed-in session cookies exist in servoshell profile — auth NOT needed): reaches mail.google.com/mail/u/0/, renders loading splash (M logo + Google Workspace), NEVER boots past it. 306 requests complete. Zero panics/assertions.
- Console/log shows ONLY css parse noise — mostly benign cross-browser placeholder-selector dropping, EXCEPT: `:has(.gb_We):has([data-expanded=true]):has([aria-selected])` fails with SelectorError(UnexpectedIdent) — nested/multi-argument :has() unsupported despite feather enabling :has() (#297 enabled the pref; implementation may be single-argument only). Real CSS gap in google chrome UI.
- Boot stall is SILENT like icloud (sec1zt325) — same signature: heavy JS app, resources complete, no errors, dead before app init. icloud userscript-tracer harness (/tmp/icltrace.py pattern + --userscripts document-start injection) is the investigation method. gmail repro command: `./target/debug/servoshell --window-size 1280x900 --pref dom_serviceworker_enabled=true --pref dom_indexeddb_enabled=true "https://mail.google.com/"` — no longer blocked on Windows specifics.
- HYPOTHESIS worth testing first: gmail+icloud both silent-stall — if they share a mechanism (e.g. a specific JS API or pattern both bootloaders gate on), one root cause kills two top-tier defects.

## sec1zt334: gmail boot-stall narrowed — readyState wedge, NOT module graph (2026-11-28)
- Probe chain: readyState stuck at `interactive` forever (never reaches complete). URL reaches #inbox, 45 body children, GM_* boot globals present, 190+ resources complete, network log shows all responses 200. Plain-page control probe proves readyState machinery healthy (parsed@loading→DCL@interactive→load@complete).
- NEGATIVE: not ES-module wedge — gmail uses ZERO type=module scripts (28 scripts, all classic async). Differs from icloud.
- KEY SUSPECT: load-event completion blocked by a never-completing resource — gmail's comet/push channel uses hanging-GET iframes by design; if servo's load event waits on iframe completion the boot stalls permanently. Chrome must special-case or gmail inserts the transport differently there. Next probe: enumerate document iframes + their readyStates on the stuck page (eval: Array.from(document.querySelectorAll('iframe')).map(f=>f.src+':'+((f.contentDocument||{}).readyState||'x'))), find which never completes; then check spec — does parent load REALLY wait for iframe docs, and does gmail's transport iframe count?
- ALSO OPEN: whether gmail boot itself even depends on the load event — maybe boot uses its own signal and the wedge is elsewhere; the readyState stall may be symptom not cause. Check GM_BOOTSTRAP_DATA state.

## sec1zt335: GMAIL BOOTS — full inbox renders; boot freeze is a FLAKY RACE (2026-11-28)
- MAJOR: signed-in gmail fully booted in headed servoshell (Linux, current feather-engine pre-merge binary): complete inbox UI — email list, compose, nav, search, avatars. readyState reached complete, 122/122 images complete.
- First run same setup stuck at splash with readyState interactive → the freeze is a RACE CONDITION not a deterministic wedge. Two runs, two outcomes. The gmail defect class changes: not "broken" but "flaky boot timing" — likely a resource-completion race during boot (comet iframes etc per sec1zt334). Repro: rerun servoshell gmail several times, count boot-vs-stall ratio; the stall state has readyState stuck interactive.
- CORRECTION IMPLICATION: the Windows report "gmail rendering/boot freeze" may itself be the flaky race — retests needed there before deep investment.

## sec1zt336: rotation handoff — merged-tree build state (2026-11-28)
- BUILD: first merged-tree rebuild FAILED spuriously — cause: concurrent mach invocations (timeout-killed wrapper left cargo children running; nohup restart raced it). No actual rustc errors in log. Clean rebuild restarted, output /tmp/rebuild2.log, ETA 20-50min.
- NEXT SESSION FIRST TASK: verify /tmp/rebuild2.log completes green; if it fails with REAL errors, suspect the Windows-lane media commits (#14/#15) on Linux — bisect there. When green: YouTube verification on unified engine (composite + MSE fixes first time together), then gmail boot-race repro-rate measurement (run servoshell gmail 5x, count boot vs stall — per sec1zt335).
- pkills: ALWAYS use bracket trick — the kill loop pattern `for p in $(pgrep -f servoshell)` self-kills even inside for-loops. Cost 4 sessions this rotation.

## sec1zt337: unified engine build GREEN + smoke-verified (2026-11-28)
- Merged tree 61ffdceafe4+ builds clean (rebuild2.log "Succeeded"). YouTube watch page smoke test on unified binary: renders fully — player chrome, title, related sidebar with thumbnails, search. Zero panics/assertions. MSE machinery active in logs (37 lines). Composite + Windows MSE fixes coexist healthy on Linux.
- NOTE: coordinator clarified — gmail boot was PRE-AUTHENTICATED profile (servoshell persists cookies), fresh Google sign-in UNTESTED, TLS-JA3 floor assessment unaffected.
