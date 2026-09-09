use filetwin_core::{
    Error, ErrorCode, Result, profile,
    worker_protocol::{Encoded, Request},
};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, imageops::FilterType};
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

fn decode_error(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::DecodeFailed, "image", e.to_string())
}
fn image_error(e: image::ImageError) -> Error {
    let code = if matches!(e, image::ImageError::Unsupported(_)) {
        ErrorCode::UnsupportedFormat
    } else {
        ErrorCode::DecodeFailed
    };
    Error::new(code, "image", e.to_string())
}
fn model_error(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::ModelIntegrity, "model", e.to_string())
}

pub struct Sscd {
    session: Session,
}
impl Sscd {
    pub fn load(request: &Request) -> Result<Self> {
        let path = request.model_dir.join(profile::SSCD_MODEL_FILE);
        let file = File::open(&path).map_err(model_error)?;
        let mut bytes = Vec::new();
        file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 128 * 1024 * 1024
            || format!("{:x}", Sha256::digest(&bytes)) != profile::SSCD_MODEL_SHA256
        {
            return Err(model_error(
                "SSCD ONNX checksum does not match the immutable profile",
            ));
        }
        let runtime = request
            .runtime
            .onnxruntime_path
            .as_ref()
            .ok_or_else(|| model_error("ONNX Runtime path is missing"))?;
        crate::verify_runtime(runtime, "onnxruntime")?;
        ort::init_from(runtime)
            .map_err(model_error)?
            .with_name("filetwin-worker")
            .with_telemetry(false)
            .commit();
        let session = Session::builder()
            .map_err(model_error)?
            .with_intra_threads(1)
            .map_err(model_error)?
            .with_inter_threads(1)
            .map_err(model_error)?
            .with_parallel_execution(false)
            .map_err(model_error)?
            .with_optimization_level(GraphOptimizationLevel::Disable)
            .map_err(model_error)?
            .commit_from_memory(&bytes)
            .map_err(model_error)?;
        Ok(Self { session })
    }

    pub fn tensor(&mut self, input: Vec<f32>) -> Result<Vec<f32>> {
        let input =
            Tensor::from_array(([1usize, 3, profile::IMAGE_SIZE, profile::IMAGE_SIZE], input))
                .map_err(model_error)?;
        let output = self
            .session
            .run(ort::inputs!["image" => input])
            .map_err(model_error)?;
        let (shape, values) = output["embedding"]
            .try_extract_tensor::<f32>()
            .map_err(model_error)?;
        if shape.as_ref() != [1, profile::IMAGE_DIMENSIONS as i64]
            || values.len() != profile::IMAGE_DIMENSIONS
        {
            return Err(model_error("SSCD output must have shape [1,512]"));
        }
        let mut vector = values.to_vec();
        profile::normalize(&mut vector)?;
        Ok(vector)
    }

    pub fn rgb(&mut self, rgb: &RgbImage) -> Result<Vec<f32>> {
        self.tensor(preprocess(rgb))
    }
}

pub fn preprocess(rgb: &RgbImage) -> Vec<f32> {
    let n = profile::IMAGE_SIZE;
    let resized = image::imageops::resize(rgb, n as u32, n as u32, FilterType::Triangle);
    let mut tensor = vec![0.0f32; 3 * n * n];
    for (i, pixel) in resized.pixels().enumerate() {
        for c in 0..3 {
            tensor[c * n * n + i] =
                (f32::from(pixel[c]) / 255.0 - [0.485, 0.456, 0.406][c]) / [0.229, 0.224, 0.225][c];
        }
    }
    tensor
}

pub fn read_rgb(request: &Request) -> Result<(RgbImage, serde_json::Value)> {
    if request.format == "heif_avif" {
        if request.profile_id == profile::image_profile_v1().profile_id {
            return Err(Error::new(
                ErrorCode::UnsupportedFormat,
                "image",
                "HEIF/AVIF requires the v2 image profile",
            ));
        }
        return crate::media::still_image(request);
    }
    let mut reader = ImageReader::open(&request.path)
        .map_err(decode_error)?
        .with_guessed_format()
        .map_err(decode_error)?;
    if request.format == "tga" {
        reader.set_format(image::ImageFormat::Tga);
    }
    let format = reader.format().ok_or_else(|| {
        Error::new(
            ErrorCode::UnsupportedFormat,
            "image",
            "Image codec unavailable under the selected profile",
        )
    })?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some((request.memory_bytes / 8).min(128 * 1024 * 1024));
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let dimensions = decoder.dimensions();
    // Reserve room for the decoder, orientation copy, RGB conversion and model.
    if decoder.total_bytes() > request.memory_bytes / 8 {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "image",
            "Decoded image exceeds memory allowance",
        ));
    }
    let orientation = decoder.orientation().map_err(image_error)?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(image_error)?;
    decoded.apply_orientation(orientation);
    let rgba = decoded.into_rgba8();
    let rgb = RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        image::Rgb([0, 1, 2].map(|c| ((u16::from(p[c]) * u16::from(p[3]) + 127) / 255) as u8))
    });
    Ok((
        rgb,
        json!({"coverage":"first_image","format":format!("{format:?}"),"source_width":dimensions.0,"source_height":dimensions.1,"orientation":format!("{orientation:?}"),"color_policy":"assume_srgb_ignore_icc_alpha_on_black","decoder":"image-0.25.10"}),
    ))
}

pub fn encode(request: &Request) -> Result<Encoded> {
    let (rgb, extraction) = read_rgb(request)?;
    let vector = Sscd::load(request)?.rgb(&rgb)?;
    Ok(Encoded {
        family: "image".into(),
        format: extraction["format"].as_str().unwrap_or("raster").into(),
        vector,
        extraction,
    })
}
