//! `imagegen` plugin settings and the pure helpers its CLI needs: provider
//! and model from `[imagegen]` in `instances.toml`, the gateway key, output
//! sizes per aspect ratio, and image type/size sniffing. HTTP lives in the
//! binary (`agentdeck imagegen`).

use std::path::Path;

use crate::registry::Registry;
use crate::settings::{PROVIDER_OPENROUTER, PROVIDER_VERCEL};
use crate::usage::UsageKind;

pub const DEFAULT_PROVIDER: &str = PROVIDER_VERCEL;
pub const DEFAULT_VERCEL_MODEL: &str = "bytedance/seedream-5.0-pro";
pub const DEFAULT_OPENROUTER_MODEL: &str = "google/gemini-3.1-flash-image";
/// Vercel image model with a native `background: "transparent"` (Seedream,
/// Recraft and Gemini have none); open on the gateway's free tier.
pub const DEFAULT_TRANSPARENT_MODEL: &str = "openai/gpt-image-1-mini";

/// (ratio, width, height): about 3.7-4.2 MP, multiples of 16. Seedream
/// rejects small sizes and ignores `aspect_ratio`, so the size carries it.
pub const SIZES: [(&str, u32, u32); 8] = [
    ("1:1", 2048, 2048),
    ("16:9", 2560, 1440),
    ("9:16", 1440, 2560),
    ("4:3", 2304, 1728),
    ("3:4", 1728, 2304),
    ("3:2", 2496, 1664),
    ("2:3", 1664, 2496),
    ("21:9", 3024, 1296),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagegenSettings {
    pub provider: String,
    pub model: String,
    /// Used on Vercel for `--transparent`.
    pub transparent_model: String,
}

/// `[imagegen] provider / model / transparent_model`; an unknown provider falls back to Vercel,
/// a missing model to the provider's default.
pub fn read(reg: &Registry) -> ImagegenSettings {
    let get = |key: &str| {
        reg.extra
            .get("imagegen")
            .and_then(|t| t.as_table()?.get(key)?.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let provider = get("provider")
        .filter(|p| p == PROVIDER_VERCEL || p == PROVIDER_OPENROUTER)
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
    let model = get("model").unwrap_or_else(|| default_model(&provider).to_string());
    let transparent_model =
        get("transparent_model").unwrap_or_else(|| DEFAULT_TRANSPARENT_MODEL.to_string());
    ImagegenSettings {
        provider,
        model,
        transparent_model,
    }
}

pub fn default_model(provider: &str) -> &'static str {
    if provider == PROVIDER_OPENROUTER {
        DEFAULT_OPENROUTER_MODEL
    } else {
        DEFAULT_VERCEL_MODEL
    }
}

/// Gateway key: the provider's standard env var, then the matching
/// `[[usage]]` entry's `api_key_env` (loaded from usage.env), then the
/// router's `~/.config/model-route/<provider>.key`.
pub fn gateway_key(reg: &Registry, home: &Path, provider: &str) -> Option<String> {
    let (env, kind) = if provider == PROVIDER_OPENROUTER {
        ("OPENROUTER_API_KEY", UsageKind::Openrouter)
    } else {
        ("AI_GATEWAY_API_KEY", UsageKind::Vercel)
    };
    let nonempty = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    std::env::var(env)
        .ok()
        .and_then(nonempty)
        .or_else(|| {
            reg.usage
                .iter()
                .filter(|u| u.kind == kind)
                .filter_map(|u| u.api_key_env.as_deref())
                .find_map(|var| std::env::var(var).ok().and_then(nonempty))
        })
        .or_else(|| {
            std::fs::read_to_string(
                home.join(".config/model-route")
                    .join(format!("{provider}.key")),
            )
            .ok()
            .and_then(nonempty)
        })
}

/// Request size for `ratio`. GPT Image 1.x only takes 1024x1024, 1536x1024
/// and 1024x1536, so it gets the closest of those.
pub fn size_for(model: &str, ratio: &str) -> Option<(u32, u32)> {
    let (_, w, h) = SIZES.iter().find(|(r, ..)| *r == ratio)?;
    if model.starts_with("openai/gpt-image-1") {
        return Some(match w.cmp(h) {
            std::cmp::Ordering::Greater => (1536, 1024),
            std::cmp::Ordering::Less => (1024, 1536),
            std::cmp::Ordering::Equal => (1024, 1024),
        });
    }
    Some((*w, *h))
}

/// The table ratio closest to `w:h` (edits keep the source's shape).
pub fn nearest_ratio(w: u32, h: u32) -> &'static str {
    let target = (w as f64 / h.max(1) as f64).ln();
    SIZES
        .iter()
        .min_by(|a, b| {
            let d = |(_, w, h): &&(&str, u32, u32)| ((*w as f64 / *h as f64).ln() - target).abs();
            d(a).total_cmp(&d(b))
        })
        .map(|(r, ..)| *r)
        .unwrap_or("1:1")
}

/// File extension and MIME type from magic bytes.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}

/// Whether a PNG/WebP can carry transparency (PNG colour type with alpha or a
/// `tRNS` chunk; WebP `VP8X` alpha flag or lossless `VP8L` alpha hint). JPEG never does.
pub fn has_alpha(bytes: &[u8]) -> bool {
    match sniff(bytes).map(|s| s.0) {
        Some("png") => {
            matches!(bytes.get(25), Some(4 | 6))
                || bytes.windows(4).take(4096).any(|w| w == b"tRNS")
        }
        Some("webp") => match bytes.get(12..16) {
            Some(b"VP8X") => bytes.get(20).is_some_and(|f| f & 0x10 != 0),
            Some(b"VP8L") => bytes.get(24).is_some_and(|b| b & 0x10 != 0),
            _ => false,
        },
        _ => false,
    }
}

/// Pixel size of a PNG or baseline/progressive JPEG; `None` for anything else.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]) as u32);
    let be32 = |i: usize| Some(u32::from_be_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    match sniff(bytes)?.0 {
        "png" => Some((be32(16)?, be32(20)?)),
        "jpg" => {
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    return None;
                }
                let marker = bytes[i + 1];
                // SOF0-SOF15 except DHT (C4), JPG (C8) and DAC (CC).
                if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                    return Some((be16(i + 7)?, be16(i + 5)?));
                }
                i += 2 + be16(i + 2)? as usize;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_and_override() {
        let mut reg = Registry::default();
        assert_eq!(
            read(&reg),
            ImagegenSettings {
                provider: "vercel".into(),
                model: DEFAULT_VERCEL_MODEL.into(),
                transparent_model: DEFAULT_TRANSPARENT_MODEL.into(),
            }
        );
        reg.extra.insert(
            "imagegen".into(),
            toml::toml! { provider = "openrouter" }.into(),
        );
        assert_eq!(read(&reg).model, DEFAULT_OPENROUTER_MODEL);
        reg.extra.insert(
            "imagegen".into(),
            toml::toml! { provider = "nope"
            model = "x/y" }
            .into(),
        );
        assert_eq!(read(&reg).provider, "vercel");
        assert_eq!(read(&reg).model, "x/y");
    }

    #[test]
    fn sizes_and_ratios() {
        assert_eq!(size_for("x/y", "16:9"), Some((2560, 1440)));
        assert_eq!(size_for("x/y", "5:1"), None);
        assert_eq!(
            size_for("openai/gpt-image-1-mini", "21:9"),
            Some((1536, 1024))
        );
        assert_eq!(size_for("openai/gpt-image-1.5", "9:16"), Some((1024, 1536)));
        assert_eq!(
            size_for("openai/gpt-image-1-mini", "1:1"),
            Some((1024, 1024))
        );
        assert_eq!(nearest_ratio(1664, 2496), "2:3");
        assert_eq!(nearest_ratio(1376, 768), "16:9");
        assert_eq!(nearest_ratio(1000, 1010), "1:1");
        for (_, w, h) in SIZES {
            assert!(w % 16 == 0 && h % 16 == 0);
        }
    }

    #[test]
    fn sniff_and_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(sniff(&png), Some(("png", "image/png")));
        assert_eq!(dimensions(&png), Some((640, 480)));
        // SOI, APP0 (len 4), SOF0 with height 300, width 200.
        let jpg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01,
            0x2C, 0x00, 0xC8, 0x03,
        ];
        assert_eq!(dimensions(&jpg), Some((200, 300)));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(("webp", "image/webp")));
        assert_eq!(sniff(b"GIF89a"), None);
        assert!(!has_alpha(&png)); // IHDR ends before the colour type byte
        let mut rgba = png.clone();
        rgba.extend_from_slice(&[8, 6]);
        assert!(has_alpha(&rgba));
        rgba[25] = 2;
        assert!(!has_alpha(&rgba));
        assert!(!has_alpha(&jpg));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0".to_vec();
        webp.push(0x10);
        assert!(has_alpha(&webp));
        webp[20] = 0;
        assert!(!has_alpha(&webp));
    }
}
