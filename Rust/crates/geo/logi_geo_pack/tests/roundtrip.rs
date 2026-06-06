//! Round-trip + determinism tests for the .logipack format and the HeightPage codec.

use logi_geo_pack::{HeightPage, Package, PackageBuilder, PageKind};

fn sample_field(w: u16, h: u16) -> (Vec<f32>, Vec<bool>) {
    let mut heights = Vec::with_capacity(w as usize * h as usize);
    let mut holes = Vec::with_capacity(w as usize * h as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32;
            let fy = y as f32;
            heights.push(100.0 + 20.0 * (fx * 0.3).sin() + 15.0 * (fy * 0.2).cos());
            holes.push((x + y) % 37 == 0);
        }
    }
    (heights, holes)
}

#[test]
fn height_page_roundtrip_within_quant_error() {
    let (w, h) = (32u16, 24u16);
    let (heights, holes) = sample_field(w, h);

    let page = HeightPage::from_heights(w, h, &heights, &holes);
    let decoded = HeightPage::decode(&page.encode()).unwrap();

    assert_eq!(decoded.width, w);
    assert_eq!(decoded.height, h);
    assert_eq!(decoded.samples.len(), w as usize * h as usize);

    for i in 0..(w as usize * h as usize) {
        assert_eq!(decoded.is_hole(i), holes[i], "hole bit at {i}");
        let e = decoded.elevation(i);
        assert!(
            (e - heights[i]).abs() <= page.error_metric + 1e-3,
            "sample {i}: got {e}, want {} (err {})",
            heights[i],
            page.error_metric
        );
    }
}

#[test]
fn package_roundtrip_dedup_and_deterministic() {
    let (h16, ho16) = sample_field(16, 16);
    let page_a = HeightPage::from_heights(16, 16, &h16, &ho16).encode();
    let (h8, ho8) = sample_field(8, 8);
    let page_b = HeightPage::from_heights(8, 8, &h8, &ho8).encode();

    let mut builder = PackageBuilder::new(7);
    let id_a = builder.add_page(PageKind::Height, 1, &page_a);
    let id_a2 = builder.add_page(PageKind::Height, 1, &page_a); // identical -> dedup
    let id_b = builder.add_page(PageKind::Height, 2, &page_b);
    assert_eq!(id_a, id_a2);
    assert_eq!(builder.page_count(), 2, "identical pages must dedup");

    let bytes1 = builder.build();
    let bytes2 = builder.build();
    assert_eq!(bytes1, bytes2, "build must be deterministic");

    let pkg = Package::parse(&bytes1).unwrap();
    assert_eq!(pkg.page_count(), 2);
    assert_eq!(pkg.package_version, 7);

    let (desc_a, payload_a) = pkg.get(&id_a).expect("page a present");
    assert_eq!(desc_a.kind, PageKind::Height);
    assert_eq!(desc_a.license_id, 1);
    assert_eq!(payload_a, page_a.as_slice());
    assert_eq!(HeightPage::decode(payload_a).unwrap().width, 16);

    let (desc_b, _) = pkg.get(&id_b).expect("page b present");
    assert_eq!(desc_b.license_id, 2);

    assert!(pkg.get(&[0u8; 16]).is_none(), "unknown id");
    assert!(Package::parse(b"not a logipack").is_err(), "bad magic rejected");
}

/// H6 + Low: a truncated or directory-corrupt package must be rejected at parse / return `None` from a
/// lookup — never panic or hand back an out-of-bounds slice.
#[test]
fn truncated_and_bad_directory_are_rejected() {
    let (h, ho) = sample_field(16, 16);
    let page = HeightPage::from_heights(16, 16, &h, &ho).encode();
    let mut builder = PackageBuilder::new(1);
    let id = builder.add_page(PageKind::Height, 0, &page);
    let good = builder.build();

    // A package cut short of its directory fails to parse (the dir window runs past EOF).
    let truncated = &good[..good.len() - 8];
    assert!(Package::parse(truncated).is_err(), "truncated package must be rejected");

    // Cutting into the payload region while keeping the header's claimed dir_offset/len makes the
    // directory point out of bounds — parse must reject it, not index OOB.
    let chopped = &good[..good.len() / 2];
    assert!(
        Package::parse(chopped).is_err(),
        "mid-payload truncation must be rejected"
    );

    // Too short for even a header.
    assert!(Package::parse(&good[..4]).is_err(), "sub-header bytes must be rejected");

    // A well-formed package still round-trips its one page (sanity that the corruptions above are the
    // cause of rejection, not a builder bug).
    let pkg = Package::parse(&good).expect("intact package parses");
    assert!(pkg.get(&id).is_some(), "intact package still serves its page");
}

/// L8: an **all-holes** HeightPage (every sample no-data) must encode/decode and report every sample as a
/// hole with a collapsed (0..0) elevation range — no panic, no NaN.
#[test]
fn all_holes_height_page_roundtrips() {
    let (w, h) = (8u16, 8u16);
    let n = w as usize * h as usize;
    let heights = vec![123.0f32; n];
    let holes = vec![true; n]; // every sample is no-data

    let page = HeightPage::from_heights(w, h, &heights, &holes);
    // With no valid samples the range collapses to 0..0 and the error metric is exactly 0.
    assert_eq!(page.min_elevation, 0.0);
    assert_eq!(page.max_elevation, 0.0);
    assert_eq!(page.error_metric, 0.0);

    let decoded = HeightPage::decode(&page.encode()).unwrap();
    for i in 0..n {
        assert!(decoded.is_hole(i), "sample {i} must be a hole");
        assert!(decoded.elevation(i).is_finite(), "hole elevation must be finite");
    }
}

/// The no-holes fast path (used by the procedural lane) must produce the SAME page as `from_heights`
/// with an all-`false` mask — the optimization changes allocation, not output.
#[test]
fn no_holes_fast_path_matches_explicit_empty_mask() {
    let (w, h) = (16u16, 12u16);
    let (heights, _) = sample_field(w, h);
    let no_holes_mask = vec![false; w as usize * h as usize];

    let a = HeightPage::from_heights_no_holes(w, h, &heights);
    let b = HeightPage::from_heights(w, h, &heights, &no_holes_mask);

    assert_eq!(a.min_elevation, b.min_elevation);
    assert_eq!(a.max_elevation, b.max_elevation);
    assert_eq!(a.error_metric, b.error_metric);
    assert_eq!(a.hole_mask, b.hole_mask);
    assert_eq!(a.samples, b.samples);
}
