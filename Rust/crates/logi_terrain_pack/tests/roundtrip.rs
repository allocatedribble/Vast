//! Round-trip + determinism tests for the .logipack format and the HeightPage codec.

use logi_terrain_pack::{HeightPage, Package, PackageBuilder, PageKind};

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
