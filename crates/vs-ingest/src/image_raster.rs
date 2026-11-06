//! Raster formats: PNG, JPEG (one page) and TIFF (one page per frame).

use std::io::Cursor;

use image::{imageops::FilterType, DynamicImage, GrayImage, ImageDecoder, ImageReader, RgbImage, RgbaImage};
use tiff::decoder::{ifd::Value, Decoder as TiffDecoder, DecodingResult};
use tiff::tags::{ResolutionUnit, Tag};
use tiff::ColorType;
use vs_classify::PageSignals;
use vs_dag::{PageImage, TextLayer};

use crate::format::Format;
use crate::raster::{check_page, encode_jpeg, encode_png, IngestError, PageAssets, Rasterizer};

/// Used when the file carries no resolution (SPEC.md).
const ASSUMED_DPI: u32 = 300;

pub struct ImageRasterizer {
    format: Format,
    bytes: Vec<u8>,
    page_count: u32,
}

impl ImageRasterizer {
    pub fn open(format: Format, bytes: &[u8], max_pages: u32) -> Result<Self, IngestError> {
        let page_count = match format {
            Format::Tiff => tiff_frame_count(bytes)?,
            Format::Png | Format::Jpeg => 1,
            Format::Pdf => return Err(IngestError::UnsupportedFormat),
        };
        if page_count > max_pages {
            return Err(IngestError::TooManyPages(page_count));
        }
        let r = Self { format, bytes: bytes.to_vec(), page_count };
        // Fail on undecodable files at open time, like PDFs do.
        r.decode(1).map_err(|e| IngestError::Corrupt(e.to_string()))?;
        Ok(r)
    }

    /// Decoded frame, upright (EXIF orientation applied), and its source DPI.
    fn decode(&self, page_no: u32) -> Result<(DynamicImage, u32), IngestError> {
        check_page(page_no, self.page_count)?;
        let err = |reason: String| IngestError::Render { page: page_no, reason };
        match self.format {
            Format::Tiff => decode_tiff_frame(&self.bytes, (page_no - 1) as usize).map_err(err),
            _ => {
                let mut dec = ImageReader::new(Cursor::new(&self.bytes))
                    .with_guessed_format()
                    .map_err(|e| err(e.to_string()))?
                    .into_decoder()
                    .map_err(|e| err(e.to_string()))?;
                let orientation = dec.orientation().map_err(|e| err(e.to_string()))?;
                let mut img = DynamicImage::from_decoder(dec).map_err(|e| err(e.to_string()))?;
                img.apply_orientation(orientation);
                Ok((img, ASSUMED_DPI))
            }
        }
    }
}

impl Rasterizer for ImageRasterizer {
    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn render(&mut self, page_no: u32, dpi: u32) -> Result<PageImage, IngestError> {
        let (img, src_dpi) = self.decode(page_no)?;
        render(&img, src_dpi, dpi, page_no)
    }

    fn thumbnail(&mut self, page_no: u32, long_side_px: u32) -> Result<PageImage, IngestError> {
        let (img, src_dpi) = self.decode(page_no)?;
        thumbnail(&img, src_dpi, long_side_px, page_no)
    }

    fn text_layer(&mut self, page_no: u32) -> Result<TextLayer, IngestError> {
        check_page(page_no, self.page_count)?;
        Ok(TextLayer::default())
    }

    fn signals(&mut self, page_no: u32) -> Result<PageSignals, IngestError> {
        check_page(page_no, self.page_count)?;
        Ok(image_signals())
    }

    fn page(&mut self, page_no: u32, dpi: u32, thumb_long_side_px: u32) -> Result<PageAssets, IngestError> {
        let (img, src_dpi) = self.decode(page_no)?;
        Ok(PageAssets {
            image: render(&img, src_dpi, dpi, page_no)?,
            thumbnail: thumbnail(&img, src_dpi, thumb_long_side_px, page_no)?,
            text_layer: TextLayer::default(),
            signals: image_signals(),
        })
    }
}

/// A raster page is one full-page image without a text layer.
fn image_signals() -> PageSignals {
    PageSignals {
        text_chars: 0,
        text_layer_coverage: 0.0,
        ocr_text_layer: false,
        image_count: 1,
        image_area_ratio: 1.0,
        rotation_deg: 0,
    }
}

/// Downscales to `dpi` when the source is finer; never upscales.
fn render(img: &DynamicImage, src_dpi: u32, dpi: u32, page_no: u32) -> Result<PageImage, IngestError> {
    if dpi >= src_dpi {
        return encode_png(img, src_dpi, page_no);
    }
    let f = dpi as f32 / src_dpi as f32;
    let (w, h) = (((img.width() as f32) * f).round().max(1.0) as u32, ((img.height() as f32) * f).round().max(1.0) as u32);
    encode_png(&img.resize_exact(w, h, FilterType::Triangle), dpi, page_no)
}

fn thumbnail(img: &DynamicImage, src_dpi: u32, long_side_px: u32, page_no: u32) -> Result<PageImage, IngestError> {
    let thumb = img.thumbnail(long_side_px, long_side_px);
    let f = thumb.width() as f32 / img.width().max(1) as f32;
    encode_jpeg(&thumb, (src_dpi as f32 * f).round() as u32, page_no)
}

fn tiff_frame_count(bytes: &[u8]) -> Result<u32, IngestError> {
    let mut dec = TiffDecoder::new(Cursor::new(bytes)).map_err(|e| IngestError::Corrupt(e.to_string()))?;
    let mut n = 1;
    while dec.more_images() {
        dec.next_image().map_err(|e| IngestError::Corrupt(e.to_string()))?;
        n += 1;
    }
    Ok(n)
}

fn decode_tiff_frame(bytes: &[u8], index: usize) -> Result<(DynamicImage, u32), String> {
    let mut dec = TiffDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    dec.seek_to_image(index).map_err(|e| e.to_string())?;
    let (w, h) = dec.dimensions().map_err(|e| e.to_string())?;
    let color = dec.colortype().map_err(|e| e.to_string())?;
    // PhotometricInterpretation 0 = WhiteIsZero (common for fax/bilevel scans).
    let white_is_zero = matches!(dec.get_tag_u32(Tag::PhotometricInterpretation), Ok(0));
    let dpi = tiff_dpi(&mut dec);
    let data = dec.read_image().map_err(|e| e.to_string())?;

    let bad = || format!("unexpected sample buffer for {color:?}");
    let img = match (color, data) {
        (ColorType::Gray(1), DecodingResult::U8(packed)) => {
            let row = (w as usize).div_ceil(8);
            let mut px = Vec::with_capacity((w * h) as usize);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let bit = packed.get(y * row + x / 8).map_or(0, |b| (b >> (7 - x % 8)) & 1);
                    px.push(if bit == 1 { 255 } else { 0 });
                }
            }
            DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, px).ok_or_else(bad)?)
        }
        (ColorType::Gray(8), DecodingResult::U8(px)) => DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, px).ok_or_else(bad)?),
        (ColorType::Gray(16), DecodingResult::U16(px)) => {
            DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, px.iter().map(|v| (v >> 8) as u8).collect()).ok_or_else(bad)?)
        }
        (ColorType::RGB(8), DecodingResult::U8(px)) => DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, px).ok_or_else(bad)?),
        (ColorType::RGBA(8), DecodingResult::U8(px)) => DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, px).ok_or_else(bad)?),
        (ColorType::RGB(16), DecodingResult::U16(px)) => {
            DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, px.iter().map(|v| (v >> 8) as u8).collect()).ok_or_else(bad)?)
        }
        (other, _) => return Err(format!("unsupported TIFF color type {other:?}")),
    };
    let img = match (white_is_zero, img) {
        (true, DynamicImage::ImageLuma8(mut g)) => {
            image::imageops::invert(&mut g);
            DynamicImage::ImageLuma8(g)
        }
        (_, img) => img,
    };
    Ok((img, dpi))
}

fn tiff_dpi(dec: &mut TiffDecoder<Cursor<&[u8]>>) -> u32 {
    let per_unit = match dec.find_tag(Tag::XResolution) {
        Ok(Some(Value::Rational(n, d))) if n > 0 && d > 0 => n as f32 / d as f32,
        _ => return ASSUMED_DPI,
    };
    let unit = dec.get_tag_u32(Tag::ResolutionUnit).ok().and_then(|u| ResolutionUnit::from_u16(u as u16));
    let dpi = match unit {
        Some(ResolutionUnit::Centimeter) => per_unit * 2.54,
        Some(ResolutionUnit::None) => return ASSUMED_DPI,
        _ => per_unit, // default unit is inch
    };
    dpi.round().clamp(50.0, 2400.0) as u32
}
