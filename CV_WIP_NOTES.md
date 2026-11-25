# cv-wip branch — content-visibility paint skipping (Stage 1, verified)

## What this is
Stage 1 of the content-visibility implementation per feather `docs/content-visibility-milestone.md`:
relevance determination (spec hook at update-the-rendering step 16.2) + paint-only skipping of
non-relevant `content-visibility: auto` subtrees and all `content-visibility: hidden` subtrees.
Layout still runs for everything (CPU win only, no memory win yet — that is Stage 2 fragment eviction).

Verified on synthetic pages: hidden never paints in viewport, deep auto content unskips and paints
correctly when scrolled into view in both directions.

## CRITICAL BUILD DEPENDENCY — read before building this branch
This branch does NOT compile against upstream pinned stylo. It requires patched stylo with:
1. `[content-visibility]` and `[contain-intrinsic-*]` ungated for the servo engine in
   `style/properties/longhands.toml` (currently `engine = "gecko"` upstream)
2. `ComputeSquaredDistance` + `ToAnimatedZero` derives on the ContentVisibility enum in
   `style/values/specified/box.rs`
3. `layout.css.contain.enabled = true` in `stylo_static_prefs/preferences.toml`

The patched copy lives at `C:\Users\Administrator\repos\stylo-local` on the Windows build machine
and is wired in via a LOCAL-ONLY `[patch."https://github.com/servo/stylo"]` section in
`feather/shell/Cargo.toml` (the workspace root for shell builds — the patch section in this
repo's root Cargo.toml is inert for shell builds since cargo only applies patches at the root).
All NINE stylo workspace crates must be patched together (style, stylo_atoms, stylo_dom,
stylo_malloc_size_of, stylo_static_prefs, stylo_traits, selectors, servo_arc, to_shmem) —
partial patching splits resolution into crates.io duplicates and breaks servo_arc serde features.

This is temporary infrastructure until the baigojahanzaib/stylo fork exists, at which point the
fork gets the patch branch and servo repins to it directly.
