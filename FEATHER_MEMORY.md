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
