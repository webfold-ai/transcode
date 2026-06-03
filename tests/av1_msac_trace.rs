// ═══════════════════════════════════════════════════════════════════════════
// av1_msac_trace — our msac Reader vs dav1d's per-symbol rng trace.
// Decodes libaom's reference tile (09 ac: a 64×64 DC-coeff KEY_FRAME) and
// asserts our normalized range after EVERY symbol equals dav1d's, captured by
// building dav1d with DEBUG_BLOCK_INFO=1. This pins the coefficient path
// (all_zero, eob, eob_base_tok, hi_tok/br_tok, dc_sign) to dav1d bit-for-bit.
// ═══════════════════════════════════════════════════════════════════════════
use transcode::av1_ec::Reader;
fn cdf(a: &[u16]) -> Vec<u16> { let mut v: Vec<u16>=a.iter().map(|&x|32768-x).collect(); v.push(0); v.push(0); v }
fn bcdf(a: u16) -> [u16;2] { [32768-a, 0] }

#[test]
fn reader_matches_dav1d_rng_trace() {
    // dav1d default CDFs (qctx=3), from src/cdf.c.
    let part=[20137u16,21547,23078,29566,29837,30261,30524,30892,31724];
    let ym=[15588u16,17027,19338,20218,20682,21110,21825,23244,24189,28165,29093,30466];
    let uv=[22631u16,24152,25378,25661,25986,26520,27055,27923,28244,30059,30941,31961];
    let eob=[6698u16,8334,11961,15762,20186,23862,27434,29326,31082,32050];
    let eobtok=[12358u16,24977];      // eob_base_tok[4][0][0]
    let brtok=[12162u16,18785,22648]; // br_tok[3][0][0]  (TX_64X64 → imin(ctx,3)=3)
    let mut r=Reader::new(&[0x09u8,0xac]);
    macro_rules! ck { ($name:expr,$want:expr) => { assert_eq!(r.rng(), $want, "rng diverged at {}", $name); } }
    let mut c=cdf(&part); assert_eq!(r.symbol(&mut c,10),0); ck!("partition",40248);
    let mut s=bcdf(31671); assert!(!r.bool_adapt(&mut s)); ck!("skip",38910);
    let mut c=cdf(&ym); assert_eq!(r.symbol(&mut c,13),0); ck!("ymode",37256);
    let mut c=cdf(&uv); assert_eq!(r.symbol(&mut c,13),0); ck!("uvmode",51506);
    let mut lz=bcdf(31539); assert!(!r.bool_adapt(&mut lz)); ck!("all_zero",49593);
    let mut c=cdf(&eob); assert_eq!(r.symbol(&mut c,11),0); ck!("eob_pt",41112);
    let mut c=cdf(&eobtok); assert_eq!(r.symbol(&mut c,3),2); ck!("dc_lo",38736);
    let mut br=cdf(&brtok);
    assert_eq!(r.symbol(&mut br,4),3); assert_eq!(r.symbol(&mut br,4),1); ck!("dc_hi(=7)",37216);
    let mut ds=bcdf(16000); assert!(r.bool_adapt(&mut ds)); ck!("dc_sign",37998);
    let mut cz=bcdf(4656);
    assert!(r.bool_adapt(&mut cz)); ck!("U_az",64980);
    assert!(r.bool_adapt(&mut cz)); ck!("V_az",56043);
}
