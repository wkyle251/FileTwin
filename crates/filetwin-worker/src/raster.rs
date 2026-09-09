use filetwin_core::{
    Error, ErrorCode, Result, profile,
    worker_protocol::{Encoded, Request},
};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, imageops::FilterType};
use ort::{
    ep::{self, ExecutionProvider},
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, fs::File, io::Read, time::Instant};

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
    inference_seconds: f64,
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
        let backend = profile::inference_backend(&request.profile_id)?;
        let threads = if backend == profile::Backend::Reference {
            1
        } else {
            request.runtime.inference_threads as usize
        };
        let mut builder = Session::builder()
            .map_err(model_error)?
            .with_intra_threads(threads)
            .map_err(model_error)?
            .with_inter_threads(1)
            .map_err(model_error)?
            .with_parallel_execution(false)
            .map_err(model_error)?
            .with_optimization_level(if backend == profile::Backend::Reference {
                GraphOptimizationLevel::Disable
            } else {
                GraphOptimizationLevel::Level3
            })
            .map_err(model_error)?;
        let unavailable =
            |message: String| Error::new(ErrorCode::RuntimeUnavailable, "inference", message);
        let mut _cache_lock = None;
        match backend {
            profile::Backend::Coreml => {
                if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
                    || !ep::CoreML::default().is_available().unwrap_or(false)
                {
                    return Err(unavailable(
                        "CoreML requires Apple Silicon macOS and a CoreML-enabled ONNX Runtime"
                            .into(),
                    ));
                }
                let mut provider = ep::CoreML::default()
                    .with_model_format(ep::coreml::ModelFormat::MLProgram)
                    .with_compute_units(ep::coreml::ComputeUnits::All)
                    .with_static_input_shapes(true)
                    .with_low_precision_accumulation_on_gpu(false);
                if let Some(root) = &request.inference_cache_dir {
                    let cache = root.join(format!("coreml-1.28.2-{}", profile::SSCD_MODEL_SHA256));
                    std::fs::create_dir_all(&cache)?;
                    // Serialize first compilation into a shared CoreML cache.
                    let lock = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(cache.join("compile.lock"))?;
                    fs2::FileExt::lock_exclusive(&lock)?;
                    _cache_lock = Some(lock);
                    provider = provider.with_model_cache_dir(cache.display());
                }
                builder = builder
                    .with_execution_providers([provider.build().error_on_failure()])
                    .map_err(|e| unavailable(e.to_string()))?;
            }
            profile::Backend::Cuda => {
                if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
                    return Err(unavailable(
                        "CUDA profiles require Linux x86-64 with an NVIDIA GPU".into(),
                    ));
                }
                crate::verify_cuda_providers(runtime)?;
                let provider = ep::CUDA::default()
                    .with_device_id(request.runtime.cuda_device_id)
                    .with_tf32(false)
                    .with_conv_algorithm_search(ep::cuda::ConvAlgorithmSearch::Heuristic)
                    .with_memory_limit(request.memory_bytes as usize);
                builder = builder.with_execution_providers([provider.build().error_on_failure()])
                    .map_err(|e| unavailable(format!("CUDA initialization failed; check the NVIDIA driver, CUDA 12 and cuDNN 9 libraries: {e}")))?;
            }
            _ => (),
        }
        let session = builder.commit_from_memory(&bytes).map_err(|error| {
            if matches!(backend, profile::Backend::Coreml | profile::Backend::Cuda) {
                unavailable(format!(
                    "{} session could not initialize: {error}",
                    backend.as_str()
                ))
            } else {
                model_error(error)
            }
        })?;
        Ok(Self {
            session,
            inference_seconds: 0.0,
        })
    }

    pub fn tensor(&mut self, input: Vec<f32>) -> Result<Vec<f32>> {
        let input =
            Tensor::from_array(([1usize, 3, profile::IMAGE_SIZE, profile::IMAGE_SIZE], input))
                .map_err(model_error)?;
        let started = Instant::now();
        let output = self
            .session
            .run(ort::inputs!["image" => input])
            .map_err(model_error)?;
        self.inference_seconds += started.elapsed().as_secs_f64();
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
    with_model(request, |model| {
        Ok(Encoded {
            family: "image".into(),
            format: extraction["format"].as_str().unwrap_or("raster").into(),
            vector: model.rgb(&rgb)?,
            extraction,
        })
    })
}

thread_local! {
    static MODEL: RefCell<Option<(String, Sscd)>> = const { RefCell::new(None) };
}

/// One immutable, verified session per worker, shared by images and video frames.
/// A changed backend/runtime/budget replaces it before allocating another model.
pub(crate) fn with_model(
    request: &Request,
    run: impl FnOnce(&mut Sscd) -> Result<Encoded>,
) -> Result<Encoded> {
    let backend = profile::inference_backend(&request.profile_id)?;
    let key = serde_json::to_string(&(
        backend,
        &request.model_dir,
        &request.runtime,
        request.memory_bytes,
        &request.inference_cache_dir,
    ))?;
    MODEL.with(|cell| {
        let mut cache = cell.borrow_mut();
        let reused = cache.as_ref().is_some_and(|(old, _)| old == &key);
        let started = Instant::now();
        if !reused {
            *cache = None;
            *cache = Some((key, Sscd::load(request)?));
        }
        let load_seconds = if reused { 0.0 } else { started.elapsed().as_secs_f64() };
        let model = &mut cache.as_mut().expect("Loaded model").1;
        let previous = model.inference_seconds;
        let mut encoded = run(model)?;
        encoded.extraction["inference"] = json!({
            "backend":backend, "model_reused":reused, "model_load_seconds":load_seconds,
            "inference_seconds":model.inference_seconds-previous,
            "intra_threads":if backend == profile::Backend::Reference { 1 } else { request.runtime.inference_threads },
            "cuda_device_id":if backend == profile::Backend::Cuda { Some(request.runtime.cuda_device_id) } else { None },
            "onnxruntime":"1.28.2", "graph_optimizations":backend != profile::Backend::Reference,
            "cpu_node_fallback_allowed":matches!(backend, profile::Backend::Coreml | profile::Backend::Cuda),
        });
        Ok(encoded)
    })
}
