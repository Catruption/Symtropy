//! Visual match evaluation for candidate remote items.
//!
//! Local tags are only a search hint. A remote item is not allowed to enrich
//! local metadata until its pixels have been compared with the local file.

use crate::model::{RemoteItem, Result};
use crate::policy::SourcePolicy;
use image::GenericImageView;
use symtropy_tags::{propose_enrichment_from_match, MatchEvidence, Proposal};

const HASH_WIDTH: u32 = 9;
const HASH_HEIGHT: u32 = 8;

fn dhash(bytes: &[u8]) -> Result<Vec<u8>> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let image = image.to_luma8();
    let resized = image::imageops::resize(
        &image,
        HASH_WIDTH,
        HASH_HEIGHT,
        image::imageops::FilterType::Lanczos3,
    );
    let mut bits = Vec::with_capacity((HASH_WIDTH as usize - 1) * HASH_HEIGHT as usize);
    for y in 0..HASH_HEIGHT {
        for x in 0..(HASH_WIDTH - 1) {
            let left = resized.get_pixel(x, y).0[0];
            let right = resized.get_pixel(x + 1, y).0[0];
            bits.push(u8::from(right > left));
        }
    }
    Ok(bits)
}

pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::load_from_memory(bytes)
        .ok()
        .map(|image| {
            let (width, height) = image.dimensions();
            (width, height)
        })
}

pub fn similarity(a: &[u8], b: &[u8]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let matching = a.iter().zip(b).filter(|(left, right)| left == right).count();
    matching as f32 / a.len() as f32
}

/// Compare a local file with a downloaded remote preview and build the
/// evidence record consumed by the tag core.
pub fn evaluate_candidate(
    local_path: &std::path::Path,
    remote_bytes: &[u8],
    remote: &RemoteItem,
    policy: &SourcePolicy,
) -> Result<MatchEvidence> {
    let local_bytes = std::fs::read(local_path).map_err(|error| error.to_string())?;
    let local_hash = dhash(&local_bytes)?;
    let remote_hash = dhash(remote_bytes)?;
    let similarity = similarity(&local_hash, &remote_hash);
    let verdict = policy.verdict(similarity);

    Ok(MatchEvidence {
        remote_id: remote.remote_id.clone(),
        source_url: remote.source_url.clone(),
        site: remote.site.as_str().to_string(),
        similarity,
        verdict,
        method: "dhash".to_string(),
        observed_tags: remote.tag_names(),
        local_dimensions: dimensions(&local_bytes),
        remote_dimensions: dimensions(remote_bytes),
    })
}

/// Turn a usable match into a sandboxed enrichment proposal. Rejected matches
/// produce an empty proposal, so callers can safely preview/apply either way.
pub fn proposal_for_match(
    item_id: &str,
    evidence: &MatchEvidence,
    based_on_revision: u64,
) -> Proposal {
    propose_enrichment_from_match(item_id, evidence, based_on_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma};

    fn encoded_image(size: u32, offset: u8) -> Vec<u8> {
        let mut image = ImageBuffer::from_fn(size, size, |x, y| {
            Luma([(((x + y) % 17) as u8).wrapping_add(offset)])
        });
        if size > 2 {
            image.put_pixel(1, 1, Luma([offset.wrapping_add(240)]));
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageLuma8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn identical_pixels_score_exactly_one() {
        let bytes = encoded_image(32, 20);
        let hash = dhash(&bytes).unwrap();
        assert_eq!(similarity(&hash, &hash), 1.0);
    }

    fn checkerboard_image(size: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(size, size, |x, y| {
            Luma([if (x + y) % 2 == 0 { 0 } else { 255 }])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageLuma8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn different_pixels_score_lower() {
        let left = dhash(&encoded_image(32, 20)).unwrap();
        let right = dhash(&checkerboard_image(32)).unwrap();
        assert!(similarity(&left, &right) < 1.0);
    }

    #[test]
    fn rejected_match_produces_no_operations() {
        let evidence = MatchEvidence {
            remote_id: "post-1".into(),
            source_url: "https://example.invalid/1".into(),
            site: "e621".into(),
            similarity: 0.1,
            verdict: symtropy_tags::MatchVerdict::Rejected,
            method: "dhash".into(),
            observed_tags: vec!["fox".into()],
            local_dimensions: None,
            remote_dimensions: None,
        };
        assert!(proposal_for_match("a.webp", &evidence, 0).operations.is_empty());
    }
}
