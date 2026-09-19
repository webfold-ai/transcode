// transcode/src/image/mod.rs — visual analysis primitives for `Image`.
//
// Two primitives, both native Rust, both zero-dependency beyond the fold substrate:
//
//   * `ssim`     — Wiener 2004 structural similarity on luminance, 8x8 windows.
//   * `clusters` — two-pass connected-component labelling on a byte diff mask.
//
// These were first written under `webfold/src/image/` (the pixelhog Rust port);
// the cyclic `bedrock -> webfold -> bedrock` edge made that placement wrong —
// the right rung is `transcode`, where the `Image` decoder lives. Both webfold
// and bedrock now consume the same primitives through this module.
//
// Quality / speed / baseline regression lives in
// `fold/docs/specs/posthog-hardening/regression.md`.

pub mod clusters;
pub mod ssim;
