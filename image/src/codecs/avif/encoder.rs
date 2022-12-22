//! Encoding of AVIF images.
///
/// The [AVIF] specification defines an image derivative of the AV1 bitstream, an open video codec.
///
/// [AVIF]: https://aomediacodec.github.io/av1-avif/
use std::borrow::Cow;
use std::io::Write;
use std::cmp::min;

use crate::{ColorType, ImageBuffer, ImageFormat, Pixel};
use crate::{ImageError, ImageResult};
use crate::buffer::ConvertBuffer;
use crate::color::{FromColor, Luma, LumaA, Bgr, Bgra, Rgb, Rgba};
use crate::error::{EncodingError, ParameterError, ParameterErrorKind, UnsupportedError, UnsupportedErrorKind};

use bytemuck::{Pod, PodCastError, try_cast_slice, try_cast_slice_mut};
use num_traits::Zero;
use ravif::{Img, Config, RGBA8, encode_rgba};
use rgb::AsPixels;

/// AVIF Encoder.
///
/// Writes one image into the chosen output.
pub struct AvifEncoder<W> {
    inner: W,
    fallback: Vec<u8>,
    config: Config
}

/// An enumeration over supported AVIF color spaces
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ColorSpace {
    /// sRGB colorspace
    Srgb,
    /// BT.709 colorspace
    Bt709,

    #[doc(hidden)]
    __NonExhaustive(crate::utils::NonExhaustiveMarker),
}

impl ColorSpace {
    fn to_ravif(self) -> ravif::ColorSpace {
        match self {
            Self::Srgb => ravif::ColorSpace::RGB,
            Self::Bt709 => ravif::ColorSpace::YCbCr,
            Self::__NonExhaustive(marker) => match marker._private {},
        }
    }
}

impl<W: Write> AvifEncoder<W> {
    /// Create a new encoder that writes its output to `w`.
    pub fn new(w: W) -> Self {
        AvifEncoder::new_with_speed_quality(w, 1, 100)
    }

    /// Create a new encoder with specified speed and quality, that writes its output to `w`.
    /// `speed` accepts a value in the range 0-10, where 0 is the slowest and 10 is the fastest.
    /// `quality` accepts a value in the range 0-100, where 0 is the worst and 100 is the best.
    pub fn new_with_speed_quality(w: W, speed: u8, quality: u8) -> Self {
        // Clamp quality and speed to range
        let quality = min(quality, 100);
        let speed = min(speed, 10);

        AvifEncoder {
            inner: w,
            fallback: vec![],
            config: Config {
                quality,
                alpha_quality: quality,
                speed,
                premultiplied_alpha: false,
                color_space: ravif::ColorSpace::RGB,
            } 
        }
    }

    /// Encode with the specified `color_space`.
    pub fn with_colorspace(mut self, color_space: ColorSpace) -> Self {
        self.config.color_space = color_space.to_ravif();
        self
    }

    /// Encode image data with the indicated color type.
    ///
    /// The encoder currently requires all data to be RGBA8, it will be converted internally if
    /// necessary. When data is suitably aligned, i.e. u16 channels to two bytes, then the
    /// conversion may be more efficient.
    pub fn write_image(mut self, data: &[u8], width: u32, height: u32, color: ColorType) -> ImageResult<()> {
        self.set_color(color);
        let config = self.config;
        // `ravif` needs strongly typed data so let's convert. We can either use a temporarily
        // owned version in our own buffer or zero-copy if possible by using the input buffer.
        // This requires going through `rgb`.
        let buffer = self.encode_as_img(data, width, height, color)?;
        let (data, _color_size, _alpha_size) = encode_rgba(buffer, &config)
            .map_err(|err| ImageError::Encoding(
                EncodingError::new(ImageFormat::Avif.into(), err)
            ))?;
        self.inner.write_all(&data)?;
        Ok(())
    }

    // Does not currently do anything. Mirrors behaviour of old config function.
    fn set_color(&mut self, _color: ColorType) {
        // self.config.color_space = ColorSpace::RGB;
    }

    fn encode_as_img<'buf>(&'buf mut self, data: &'buf [u8], width: u32, height: u32, color: ColorType)
        -> ImageResult<Img<&'buf [RGBA8]>>
    {
        // Error wrapping utility for color dependent buffer dimensions.
        fn try_from_raw<P: Pixel + 'static>(data: &[P::Subpixel], width: u32, height: u32)
            -> ImageResult<ImageBuffer<P, &[P::Subpixel]>>
        {
            ImageBuffer::from_raw(width, height, data).ok_or_else(|| {
                ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::DimensionMismatch))
            })
        };

        // Convert to target color type using few buffer allocations.
        fn convert_into<'buf, P>(buf: &'buf mut Vec<u8>, image: ImageBuffer<P, &[P::Subpixel]>)
            -> Img<&'buf [RGBA8]>
        where
            P: Pixel + 'static,
            Rgba<u8>: FromColor<P>,
        {
            let (width, height) = image.dimensions();
            // TODO: conversion re-using the target buffer?
            let image: ImageBuffer<Rgba<u8>, _> = image.convert();
            *buf = image.into_raw();
            Img::new(buf.as_pixels(), width as usize, height as usize)
        }

        // Cast the input slice using few buffer allocations if possible.
        // In particular try not to allocate if the caller did the infallible reverse.
        fn cast_buffer<Channel>(buf: &[u8]) -> ImageResult<Cow<[Channel]>>
        where
            Channel: Pod + Zero,
        {
            match try_cast_slice(buf) {
                Ok(slice) => Ok(Cow::Borrowed(slice)),
                Err(PodCastError::OutputSliceWouldHaveSlop) => {
                    Err(ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::DimensionMismatch)))
                }
                Err(PodCastError::TargetAlignmentGreaterAndInputNotAligned) => {
                    // Sad, but let's allocate.
                    // bytemuck checks alignment _before_ slop but size mismatch before this..
                    if buf.len() % std::mem::size_of::<Channel>() != 0 {
                        Err(ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::DimensionMismatch)))
                    } else {
                        let len = buf.len() / std::mem::size_of::<Channel>();
                        let mut data = vec![Channel::zero(); len];
                        let view = try_cast_slice_mut::<_, u8>(data.as_mut_slice()).unwrap();
                        view.copy_from_slice(buf);
                        Ok(Cow::Owned(data))
                    }
                }
                Err(err) => { // Are you trying to encode a ZST??
                    Err(ImageError::Parameter(ParameterError::from_kind(
                        ParameterErrorKind::Generic(format!("{:?}", err))
                    )))
                }
            }
        }

        match color {
            ColorType::Rgba8 => {
                // ravif doesn't do any checks but has some asserts, so we do the checks.
                let img = try_from_raw::<Rgba<u8>>(data, width, height)?;
                // Now, internally ravif uses u32 but it takes usize. We could do some checked
                // conversion but instead we use that a non-empty image must be addressable.
                if img.pixels().len() == 0 {
                    return Err(ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::DimensionMismatch)));
                }

                Ok(Img::new(rgb::AsPixels::as_pixels(data), width as usize, height as usize))
            },
            // we need a separate buffer..
            ColorType::L8 => {
                let image = try_from_raw::<Luma<u8>>(data, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::La8 => {
                let image = try_from_raw::<LumaA<u8>>(data, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::Rgb8 => {
                let image = try_from_raw::<Rgb<u8>>(data, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::Bgr8 => {
                let image = try_from_raw::<Bgr<u8>>(data, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::Bgra8 => {
                let image = try_from_raw::<Bgra<u8>>(data, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            // we need to really convert data..
            ColorType::L16 => {
                let buffer = cast_buffer(data)?;
                let image = try_from_raw::<Luma<u16>>(&buffer, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::La16 => {
                let buffer = cast_buffer(data)?;
                let image = try_from_raw::<LumaA<u16>>(&buffer, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::Rgb16 => {
                let buffer = cast_buffer(data)?;
                let image = try_from_raw::<Rgb<u16>>(&buffer, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            ColorType::Rgba16 => {
                let buffer = cast_buffer(data)?;
                let image = try_from_raw::<Rgba<u16>>(&buffer, width, height)?;
                Ok(convert_into(&mut self.fallback, image))
            }
            // for cases we do not support at all?
            _ => Err(ImageError::Unsupported(UnsupportedError::from_format_and_kind(
                    ImageFormat::Avif.into(),
                    UnsupportedErrorKind::Color(color.into()),
                )))
        }
    }
}
