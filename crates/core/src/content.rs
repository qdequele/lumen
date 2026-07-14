//! Shared content-parts types for multimodal inputs.
//!
//! OpenAI overloads a message/input's content: it is either a bare string or an
//! array of typed *parts* (`text`, `image_url`, …). These types model the array
//! form and are shared by the embeddings input (M9) and — when it lands — chat
//! vision (M8), so the two endpoints speak one vocabulary.
//!
//! # Dispatch by field presence
//! `ContentPart::kind` (`"type"`) defaults to `"text"` when omitted, so a part
//! can be written as `{"text":"hi"}` or `{"image_url":{...}}` without spelling
//! out the type. Because the default means `kind` and the populated field can
//! disagree, image-vs-text decisions dispatch on **which field is set**
//! ([`ContentPart::image`] / [`ContentPart::text_str`]), never on `kind`.
//! `kind` is retained only for round-trip fidelity and forward-compat.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One element of a content-parts array.
///
/// Modelled as a typed struct with a `flatten`ed `extra` map (the same idiom as
/// [`crate::chat::ChatMessage`]) so unknown/future part types (e.g.
/// `input_audio`) survive round-trip verbatim rather than failing to parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPart {
    /// `"type"` defaults to `"text"` when omitted. Real OpenAI-shaped parts
    /// (which always send `type`) still parse; see the module docs for why the
    /// value is not used to decide text-vs-image.
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    /// Present for text parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present for image parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// Any other fields (and the payload of unknown part types) preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The default `type` for a content part with none specified.
fn default_kind() -> String {
    "text".to_owned()
}

impl ContentPart {
    /// The image reference, if this part carries one (dispatch by field
    /// presence, not `kind`).
    #[must_use]
    pub fn image(&self) -> Option<&ImageUrl> {
        self.image_url.as_ref()
    }

    /// Mutable access to the image reference, if any. Used by the fetch stage to
    /// rewrite a remote URL to an inline `data:` URI in place.
    pub fn image_mut(&mut self) -> Option<&mut ImageUrl> {
        self.image_url.as_mut()
    }

    /// The text of this part, if it is a text part. A part with an `image_url`
    /// is never treated as text even if it also carries a `text` field.
    #[must_use]
    pub fn text_str(&self) -> Option<&str> {
        if self.image_url.is_some() {
            None
        } else {
            self.text.as_deref()
        }
    }
}

/// An `image_url` content part payload (OpenAI shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// A `data:` URI (inline base64) or a remote `http(s)` URL.
    pub url: String,
    /// Optional provider hint (`"low"` | `"high"` | `"auto"`); preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_type_defaults_to_text() {
        let part: ContentPart = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(part.kind, "text");
        assert_eq!(part.text_str(), Some("hi"));
        assert!(part.image().is_none());
    }

    #[test]
    fn image_dispatch_is_by_field_not_kind() {
        // No type, but an image_url is present.
        let part: ContentPart =
            serde_json::from_str(r#"{"image_url":{"url":"data:image/png;base64,AA"}}"#).unwrap();
        assert!(part.image().is_some());
        assert!(part.text_str().is_none());
    }

    #[test]
    fn unknown_part_preserved_in_extra() {
        let part: ContentPart =
            serde_json::from_str(r#"{"type":"input_audio","input_audio":{"data":"x"}}"#).unwrap();
        assert_eq!(part.kind, "input_audio");
        assert!(part.extra.contains_key("input_audio"));
        let back = serde_json::to_value(&part).unwrap();
        assert_eq!(back["type"], "input_audio");
    }
}
