use std::io::Cursor;

use ash_core::{Content, ToolError, ToolOutput};
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageReader};

pub const MAX_IMAGE_INGEST_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 2_000;
const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;
const JPEG_QUALITY: u8 = 80;

#[derive(Clone, Copy)]
struct Limits {
    max_dimension: u32,
    max_base64_bytes: usize,
}

const DEFAULT_LIMITS: Limits = Limits {
    max_dimension: MAX_IMAGE_DIMENSION,
    max_base64_bytes: MAX_IMAGE_BASE64_BYTES,
};

enum PreparedImage {
    Inline {
        media_type: String,
        data: Vec<u8>,
        notes: Vec<String>,
    },
    Omitted {
        reason: String,
    },
}

pub fn tool_output(media_type: &str, bytes: &[u8]) -> Result<ToolOutput, ToolError> {
    match prepare(bytes, media_type, DEFAULT_LIMITS)? {
        PreparedImage::Inline {
            media_type,
            data,
            notes,
        } => Ok(ToolOutput::with_attachments(
            render_notes(&format!("Read image file [{media_type}]"), &notes),
            vec![Content::Image { media_type, data }],
        )),
        PreparedImage::Omitted { reason } => {
            Ok(format!("Read image file [{media_type}]\n{reason}").into())
        }
    }
}

fn prepare(
    bytes: &[u8],
    source_media_type: &str,
    limits: Limits,
) -> Result<PreparedImage, ToolError> {
    let image = decode(bytes)?;
    let (source_width, source_height) = image.dimensions();
    if fits_inline(
        source_width,
        source_height,
        bytes.len(),
        source_media_type,
        &limits,
    ) {
        return Ok(PreparedImage::Inline {
            media_type: source_media_type.to_string(),
            data: bytes.to_vec(),
            notes: Vec::new(),
        });
    }

    let (width, height) = fit_dimensions(source_width, source_height, limits.max_dimension);
    let prepared = if (width, height) == (source_width, source_height) {
        image
    } else {
        image.resize_exact(width, height, FilterType::Triangle)
    };
    let data = encode_jpeg(&prepared)?;
    if base64_len(data.len()) > limits.max_base64_bytes {
        return Ok(PreparedImage::Omitted {
            reason: format!(
                "[Image omitted: could not be resized below the {} inline image size limit.]",
                crate::truncate::format_size(limits.max_base64_bytes)
            ),
        });
    }

    let media_type = "image/jpeg".to_string();
    let mut notes = Vec::new();
    if media_type != source_media_type {
        notes.push(format!(
            "[Image converted from {source_media_type} to {media_type}.]"
        ));
    }
    if (width, height) != (source_width, source_height) {
        notes.push(format!(
            "[Image resized from {source_width}x{source_height} to {width}x{height}.]"
        ));
    }
    Ok(PreparedImage::Inline {
        media_type,
        data,
        notes,
    })
}

fn decode(bytes: &[u8]) -> Result<DynamicImage, ToolError> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| ToolError::Execution(format!("cannot decode image: {error}")))?
        .decode()
        .map_err(|error| ToolError::Execution(format!("cannot decode image: {error}")))
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>, ToolError> {
    let mut buffer = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY)
        .encode_image(image)
        .map_err(|error| ToolError::Execution(format!("cannot encode image: {error}")))?;
    Ok(buffer.into_inner())
}

fn fits_inline(width: u32, height: u32, bytes: usize, media_type: &str, limits: &Limits) -> bool {
    is_inline_media_type(media_type)
        && width <= limits.max_dimension
        && height <= limits.max_dimension
        && base64_len(bytes) <= limits.max_base64_bytes
}

fn is_inline_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

fn fit_dimensions(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let longest = width.max(height);
    if longest <= max_dimension {
        return (width, height);
    }
    (
        scale_dimension(width, longest, max_dimension),
        scale_dimension(height, longest, max_dimension),
    )
}

fn scale_dimension(value: u32, longest: u32, max_dimension: u32) -> u32 {
    u32::try_from(u64::from(value) * u64::from(max_dimension) / u64::from(longest))
        .unwrap_or(1)
        .max(1)
}

const fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3).saturating_mul(4)
}

fn render_notes(headline: &str, notes: &[String]) -> String {
    if notes.is_empty() {
        headline.to_string()
    } else {
        format!("{headline}\n{}", notes.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, Rgb([0xcc, 0x33, 0x00]));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode png");
        bytes.into_inner()
    }

    fn bmp_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, Rgb([0x00, 0x66, 0xcc]));
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Bmp)
            .expect("encode bmp");
        bytes.into_inner()
    }

    fn jpeg_size(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes)
            .expect("decode prepared jpeg")
            .dimensions()
    }

    #[test]
    fn keeps_small_supported_images() {
        let bytes = png_bytes(8, 4);
        let output = tool_output("image/png", &bytes).unwrap();

        assert_eq!(output.text, "Read image file [image/png]");
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/png" && data == &bytes
        ));
    }

    #[test]
    fn converts_bmp_even_when_small() {
        let output = tool_output("image/bmp", &bmp_bytes(4, 4)).unwrap();

        assert!(output.text.contains("Read image file [image/jpeg]"));
        assert!(output
            .text
            .contains("[Image converted from image/bmp to image/jpeg.]"));
        assert!(matches!(
            output.attachments.as_slice(),
            [Content::Image { media_type, data }]
                if media_type == "image/jpeg" && jpeg_size(data) == (4, 4)
        ));
    }

    #[test]
    fn resizes_images_that_exceed_the_dimension_limit() {
        let prepared = prepare(
            &png_bytes(40, 20),
            "image/png",
            Limits {
                max_dimension: 20,
                max_base64_bytes: MAX_IMAGE_BASE64_BYTES,
            },
        )
        .unwrap();

        match prepared {
            PreparedImage::Inline {
                media_type,
                data,
                notes,
            } => {
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(jpeg_size(&data), (20, 10));
                assert!(notes
                    .iter()
                    .any(|note| { note == "[Image converted from image/png to image/jpeg.]" }));
                assert!(notes
                    .iter()
                    .any(|note| note == "[Image resized from 40x20 to 20x10.]"));
            }
            PreparedImage::Omitted { reason } => panic!("resized image was omitted: {reason}"),
        }
    }

    #[test]
    fn omits_images_that_remain_over_the_encoded_size_limit() {
        let prepared = prepare(
            &png_bytes(8, 8),
            "image/png",
            Limits {
                max_dimension: 8,
                max_base64_bytes: 1,
            },
        )
        .unwrap();

        match prepared {
            PreparedImage::Omitted { reason } => {
                assert!(
                    reason.contains("could not be resized below the 1B inline image size limit"),
                    "unexpected reason: {reason}"
                );
            }
            PreparedImage::Inline { .. } => panic!("oversized image was kept"),
        }
    }

    #[test]
    fn rejects_undecodable_bytes() {
        let error = tool_output("image/png", b"not-an-image").unwrap_err();
        assert!(matches!(
            error,
            ToolError::Execution(message) if message.contains("cannot decode image")
        ));
    }
}
