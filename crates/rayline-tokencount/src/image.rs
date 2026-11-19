//! What an image costs a vision model. GLM and Qwen image processors share
//! the Qwen2-VL-style `smart_resize`: the image is scaled to a multiple of
//! `patch_size * merge_size` within the processor's token limits; each merged
//! patch is one token. Gemma 4's processor scales each image to at most
//! `max_soft_tokens` tokens.

use anyhow::{Result, bail};
use base64::Engine;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSizing {
    Resized {
        factor: u64,
        min_tokens: u64,
        max_tokens: u64,
    },
    /// Every image is charged `tokens`, whatever its size.
    PerImage { tokens: u64 },
}

/// An image part's pixel size when the request carries its bytes; an image
/// only referenced by URL is charged the processor's maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Pixels { width: u64, height: u64 },
    Unknown,
}

impl Source {
    pub(crate) fn from_part(part: &Value) -> Self {
        let url = part
            .pointer("/image_url/url")
            .or_else(|| part.get("image_url"))
            .or_else(|| part.pointer("/source/data"))
            .and_then(Value::as_str);
        let Some(url) = url else { return Self::Unknown };
        let data = url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .map_or(url, |(_, data)| data);
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data.trim()) else {
            return Self::Unknown;
        };
        match imagesize::blob_size(&bytes) {
            Ok(size) if size.width > 0 && size.height > 0 => Self::Pixels {
                width: size.width as u64,
                height: size.height as u64,
            },
            _ => Self::Unknown,
        }
    }
}

impl ImageSizing {
    /// Reads `image_processor` (or the top level) of a processor config.
    /// A config with no `patch_size` anywhere describes no image input:
    /// `None`. One whose `patch_size` is elsewhere or not an integer
    /// describes images the counter cannot size: an error naming where.
    pub fn from_processor_config(config: &Value) -> Result<Option<Self>> {
        let processor = config.get("image_processor").unwrap_or(config);
        let Some(patch) = processor.get("patch_size").and_then(Value::as_u64) else {
            if let Some(pointer) = find_key(config, "patch_size", String::new()) {
                bail!("the image processor's `{pointer}` is not a `patch_size` the counter reads");
            }
            return Ok(None);
        };
        // Gemma 4 (`Gemma4ImageProcessor`) has no `merge_size`: an image
        // becomes at most `max_soft_tokens` tokens, which the processor
        // writes between a begin-image and an end-image token.
        if processor.get("merge_size").is_none()
            && let Some(budget) = processor.get("max_soft_tokens")
        {
            let Some(budget) = budget.as_u64().filter(|budget| *budget > 0) else {
                bail!("the image processor's `max_soft_tokens` is not a positive integer");
            };
            return Ok(Some(Self::PerImage {
                tokens: budget.saturating_add(2),
            }));
        }
        let Some(merge) = processor.get("merge_size").and_then(Value::as_u64) else {
            bail!(
                "the image processor has a `patch_size` but no integer `merge_size` or `max_soft_tokens`"
            );
        };
        let factor = patch * merge;
        let area = factor * factor;
        // Qwen2-VL-style processors name their pixel limits
        // `size.shortest_edge` (minimum) and `size.longest_edge` (maximum).
        let edge = |pixels_key: &str| match pixels_key {
            "min_pixels" => "shortest_edge",
            _ => "longest_edge",
        };
        let tokens = |tokens_key: &str, pixels_key: &str| {
            processor
                .get(tokens_key)
                .and_then(Value::as_u64)
                .or_else(|| {
                    processor
                        .get(pixels_key)
                        .and_then(Value::as_u64)
                        .map(|pixels| pixels / area)
                })
                .or_else(|| {
                    processor
                        .pointer(&format!("/size/{pixels_key}"))
                        .or_else(|| processor.pointer(&format!("/size/{}", edge(pixels_key))))
                        .and_then(Value::as_u64)
                        .map(|pixels| pixels / area)
                })
        };
        let (Some(min_tokens), Some(max_tokens)) = (
            tokens("min_image_tokens", "min_pixels"),
            tokens("max_image_tokens", "max_pixels"),
        ) else {
            bail!("the image processor has a patch size but no image token limits");
        };
        if factor == 0 || min_tokens == 0 || max_tokens < min_tokens {
            bail!("the image processor has inconsistent image limits");
        }
        Ok(Some(Self::Resized {
            factor,
            min_tokens,
            max_tokens,
        }))
    }

    pub(crate) fn tokens(&self, source: Source) -> usize {
        match (*self, source) {
            (Self::PerImage { tokens }, _) => tokens as usize,
            (Self::Resized { max_tokens, .. }, Source::Unknown) => max_tokens as usize,
            (
                Self::Resized {
                    factor,
                    min_tokens,
                    max_tokens,
                },
                Source::Pixels { width, height },
            ) => {
                let (height, width) = Self::resize(height, width, factor, min_tokens, max_tokens);
                ((height / factor) * (width / factor)) as usize
            }
        }
    }

    fn resize(
        height: u64,
        width: u64,
        factor: u64,
        min_tokens: u64,
        max_tokens: u64,
    ) -> (u64, u64) {
        let min_pixels = (min_tokens * factor * factor) as f64;
        let max_pixels = (max_tokens * factor * factor) as f64;
        let factor = factor as f64;
        let (h, w) = (height as f64, width as f64);
        let mut h_bar = (round_half_even(h / factor) * factor).max(factor);
        let mut w_bar = (round_half_even(w / factor) * factor).max(factor);
        if h_bar * w_bar > max_pixels {
            let beta = (h * w / max_pixels).sqrt();
            h_bar = ((h / beta / factor).floor() * factor).max(factor);
            w_bar = ((w / beta / factor).floor() * factor).max(factor);
        } else if h_bar * w_bar < min_pixels {
            let beta = (min_pixels / (h * w)).sqrt();
            h_bar = (h * beta / factor).ceil() * factor;
            w_bar = (w * beta / factor).ceil() * factor;
        }
        (h_bar as u64, w_bar as u64)
    }
}

/// The JSON pointer of the first `key` in `value`, at any depth.
fn find_key(value: &Value, key: &str, pointer: String) -> Option<String> {
    match value {
        Value::Object(object) => object.iter().find_map(|(name, child)| {
            let path = format!("{pointer}/{name}");
            if name == key {
                Some(path)
            } else {
                find_key(child, key, path)
            }
        }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, child)| find_key(child, key, format!("{pointer}/{index}"))),
        _ => None,
    }
}

/// Python's `round`.
fn round_half_even(value: f64) -> f64 {
    let rounded = value.round();
    if (value - value.trunc()).abs() == 0.5 && rounded % 2.0 != 0.0 {
        rounded - value.signum()
    } else {
        rounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn glm() -> ImageSizing {
        let config = json!({"image_processor": {"patch_size": 14, "merge_size": 2, "min_image_tokens": 16, "max_image_tokens": 8000}});
        ImageSizing::from_processor_config(&config)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn charges_what_the_glm_server_charged() {
        // A GLM-5.3-Flash server: a 2172x724 PNG added 2027 tokens to a
        // prompt beside its one placeholder, so the image is 2028 tokens.
        assert_eq!(
            glm().tokens(Source::Pixels {
                width: 2172,
                height: 724
            }),
            2028
        );
    }

    #[test]
    fn stays_within_the_processors_limits() {
        let sizing = glm();
        assert_eq!(
            sizing.tokens(Source::Pixels {
                width: 1,
                height: 1
            }),
            16
        );
        assert!(
            sizing.tokens(Source::Pixels {
                width: 6016,
                height: 3384
            }) <= 8000
        );
        assert_eq!(sizing.tokens(Source::Unknown), 8000);
    }

    #[test]
    fn rounds_like_python() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(77.57), 78.0);
    }

    #[test]
    fn reads_qwen_edge_limits_as_pixels() {
        let config = json!({"size": {"longest_edge": 16777216, "shortest_edge": 65536}, "patch_size": 16, "merge_size": 2});
        let sizing = ImageSizing::from_processor_config(&config)
            .unwrap()
            .unwrap();
        assert_eq!(
            sizing,
            ImageSizing::Resized {
                factor: 32,
                min_tokens: 64,
                max_tokens: 16384
            }
        );
    }

    /// google/gemma-4-e4b-it's processor_config.json, trimmed to its image
    /// keys.
    fn gemma4() -> Value {
        json!({
            "image_processor": {
                "image_processor_type": "Gemma4ImageProcessor",
                "image_seq_length": 280,
                "max_soft_tokens": 280,
                "patch_size": 16,
                "pooling_kernel_size": 3
            },
            "image_seq_length": 280,
            "processor_class": "Gemma4Processor"
        })
    }

    #[test]
    fn a_gemma4_image_costs_its_soft_token_budget_and_its_two_markers() {
        let sizing = ImageSizing::from_processor_config(&gemma4())
            .unwrap()
            .unwrap();
        for source in [
            Source::Pixels {
                width: 512,
                height: 512,
            },
            Source::Pixels {
                width: 4000,
                height: 50,
            },
            Source::Unknown,
        ] {
            assert_eq!(sizing.tokens(source), 282);
        }
    }

    #[test]
    fn a_merge_size_keeps_the_resize_sizing_beside_a_soft_token_budget() {
        let config = json!({"patch_size": 16, "merge_size": 2, "min_image_tokens": 64, "max_image_tokens": 16384, "max_soft_tokens": 280});
        let sizing = ImageSizing::from_processor_config(&config)
            .unwrap()
            .unwrap();
        assert_eq!(sizing.tokens(Source::Unknown), 16384);
    }

    #[test]
    fn a_soft_token_budget_that_is_not_a_positive_integer_is_an_error() {
        for budget in [json!(0), json!("280"), json!(280.5)] {
            let mut config = gemma4();
            config["image_processor"]["max_soft_tokens"] = budget;
            let error = ImageSizing::from_processor_config(&config).unwrap_err();
            assert_eq!(
                error.to_string(),
                "the image processor's `max_soft_tokens` is not a positive integer"
            );
        }
    }

    #[test]
    fn a_patch_size_without_a_merge_size_is_an_error_naming_it() {
        for processor in [
            json!({"patch_size": 14, "min_image_tokens": 16, "max_image_tokens": 8000}),
            json!({"patch_size": 14, "merge_size": "2", "min_image_tokens": 16, "max_image_tokens": 8000}),
        ] {
            let error = ImageSizing::from_processor_config(&processor).unwrap_err();
            assert!(error.to_string().contains("`merge_size`"), "{error}");
        }
    }

    #[test]
    fn a_patch_size_the_counter_does_not_read_is_an_error_naming_it() {
        for (processor, pointer) in [
            // Kimi-K3's preprocessor_config.json, trimmed to its sizing keys.
            (
                json!({"media_proc_cfg": {"in_patch_limit": 65536, "patch_size": 14, "merge_kernel_size": 2, "patch_limit_on_one_side": 512}}),
                "/media_proc_cfg/patch_size",
            ),
            (
                json!({"image_processor": {"merge_size": 2}, "patch_size": 14}),
                "/patch_size",
            ),
            (
                json!({"patch_size": {"height": 14, "width": 14}, "merge_size": 2}),
                "/patch_size",
            ),
        ] {
            let error = ImageSizing::from_processor_config(&processor).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "the image processor's `{pointer}` is not a `patch_size` the counter reads"
                )
            );
        }
    }

    #[test]
    fn a_processor_without_patches_takes_no_images() {
        assert_eq!(
            ImageSizing::from_processor_config(&json!({"feature_size": 80})).unwrap(),
            None
        );
    }
}
