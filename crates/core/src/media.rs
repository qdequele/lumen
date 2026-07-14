//! Media accounting for multimodal inputs (M9).
//!
//! Counts the media items in an embedding request and their **decoded** size in
//! bytes — a billing dimension alongside tokens. Measurement runs *after* the
//! image-fetch stage, when every image part is an inline `data:` URI (whether
//! the client supplied it or the gateway fetched it), so the size is always
//! available at the gateway with no extra I/O.

use crate::embed::{EmbedInput, EmbedItem};

/// Aggregated media usage for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaUsage {
    /// Total media items.
    pub count: u64,
    /// Total decoded media bytes.
    pub bytes: u64,
    /// Per top-level media type (`"image"`, future `"audio"`, …), in first-seen
    /// order. Used for the per-type Prometheus breakdown.
    pub by_type: Vec<MediaTypeUsage>,
}

/// Media usage for one top-level media type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaTypeUsage {
    /// Top-level media type, e.g. `"image"`.
    pub media_type: String,
    /// Items of this type.
    pub count: u64,
    /// Decoded bytes of this type.
    pub bytes: u64,
}

impl MediaUsage {
    /// Whether any media was measured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn add(&mut self, media_type: &str, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        if let Some(entry) = self.by_type.iter_mut().find(|t| t.media_type == media_type) {
            entry.count += 1;
            entry.bytes += bytes;
        } else {
            self.by_type.push(MediaTypeUsage {
                media_type: media_type.to_owned(),
                count: 1,
                bytes,
            });
        }
    }
}

/// Measure the media (count + decoded bytes, by top-level type) in `input`.
/// Text-only inputs return an empty [`MediaUsage`]. Intended to run after the
/// image-fetch stage, so image URLs are `data:` URIs; a non-`data:` image URL
/// (should not occur post-resolve) is counted with 0 bytes and type `unknown`.
#[must_use]
pub fn measure_media(input: &EmbedInput) -> MediaUsage {
    let mut usage = MediaUsage::default();
    let EmbedInput::Multi(items) = input else {
        return usage;
    };
    for item in items {
        if let EmbedItem::Parts(parts) = item {
            for part in parts {
                if let Some(image) = part.image() {
                    let (media_type, bytes) = measure_data_uri(&image.url);
                    usage.add(media_type, bytes);
                }
            }
        }
    }
    usage
}

/// Parse a `data:<mediatype>[;base64],<payload>` URI into its top-level media
/// type and decoded byte length. A non-`data:` URL yields `("unknown", 0)`.
fn measure_data_uri(url: &str) -> (&'static str, u64) {
    let trimmed = url.trim_start();
    // Case-insensitive `data:` scheme check without allocating.
    let Some(rest) = trimmed.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")) else {
        return ("unknown", 0);
    };
    let _ = rest;
    let after_scheme = &trimmed[5..];
    let Some(comma) = after_scheme.find(',') else {
        return ("unknown", 0);
    };
    let meta = &after_scheme[..comma];
    let payload = &after_scheme[comma + 1..];

    let is_base64 = meta
        .split(';')
        .any(|seg| seg.eq_ignore_ascii_case("base64"));
    let mediatype = meta.split(';').next().unwrap_or("");
    let top = top_level_type(mediatype);

    let bytes = if is_base64 {
        // Each base64 char carries 6 bits; padding excluded. bytes = chars*3/4.
        let chars = payload.trim_end_matches('=').len() as u64;
        chars * 3 / 4
    } else {
        // Non-base64 data URI: the payload is the (percent-encoded) bytes.
        payload.len() as u64
    };
    (top, bytes)
}

/// The top-level type of a MIME string (`"image/png"` → `"image"`), mapped to a
/// fixed set of `&'static str` labels to keep Prometheus cardinality bounded.
fn top_level_type(mediatype: &str) -> &'static str {
    match mediatype
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "image" => "image",
        "audio" => "audio",
        "video" => "video",
        "text" => "text",
        // `data:;base64,...` (no media type) defaults to octet-stream.
        "application" | "" => "application",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentPart, ImageUrl};
    use base64::Engine;

    fn image_part(url: &str) -> ContentPart {
        ContentPart {
            kind: "image_url".to_owned(),
            text: None,
            image_url: Some(ImageUrl {
                url: url.to_owned(),
                detail: None,
            }),
            extra: serde_json::Map::new(),
        }
    }

    fn data_uri(mime: &str, bytes: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("data:{mime};base64,{b64}")
    }

    #[test]
    fn text_only_input_has_no_media() {
        let input = EmbedInput::Batch(vec!["a".to_owned(), "b".to_owned()]);
        assert!(measure_media(&input).is_empty());
    }

    #[test]
    fn counts_images_and_decoded_bytes() {
        let png = vec![0x89u8, 0x50, 0x4e, 0x47, 1, 2, 3]; // 7 bytes
        let jpeg = vec![0u8; 10]; // 10 bytes
        let input = EmbedInput::Multi(vec![EmbedItem::Parts(vec![
            ContentPart {
                kind: "text".to_owned(),
                text: Some("caption".to_owned()),
                image_url: None,
                extra: serde_json::Map::new(),
            },
            image_part(&data_uri("image/png", &png)),
            image_part(&data_uri("image/jpeg", &jpeg)),
        ])]);
        let usage = measure_media(&input);
        assert_eq!(usage.count, 2);
        // Decoded bytes equal the original payload sizes exactly.
        assert_eq!(usage.bytes, 17);
        assert_eq!(usage.by_type.len(), 1); // both are "image"
        assert_eq!(usage.by_type[0].media_type, "image");
        assert_eq!(usage.by_type[0].count, 2);
        assert_eq!(usage.by_type[0].bytes, 17);
    }

    #[test]
    fn breaks_down_by_top_level_type() {
        let input = EmbedInput::Multi(vec![
            EmbedItem::Parts(vec![image_part(&data_uri("image/png", &[0u8; 3]))]),
            EmbedItem::Parts(vec![image_part(&data_uri("audio/mpeg", &[0u8; 8]))]),
        ]);
        let usage = measure_media(&input);
        assert_eq!(usage.count, 2);
        assert_eq!(usage.bytes, 11);
        let image = usage
            .by_type
            .iter()
            .find(|t| t.media_type == "image")
            .unwrap();
        let audio = usage
            .by_type
            .iter()
            .find(|t| t.media_type == "audio")
            .unwrap();
        assert_eq!(image.bytes, 3);
        assert_eq!(audio.bytes, 8);
    }

    #[test]
    fn base64_without_padding_measured_correctly() {
        // 4 raw bytes → 8 base64 chars with "==" padding; formula must ignore it.
        let uri = data_uri("image/png", &[1, 2, 3, 4]);
        let (ty, bytes) = measure_data_uri(&uri);
        assert_eq!(ty, "image");
        assert_eq!(bytes, 4);
    }

    #[test]
    fn non_data_url_is_unknown_zero() {
        assert_eq!(
            measure_data_uri("https://example.com/cat.png"),
            ("unknown", 0)
        );
    }
}
