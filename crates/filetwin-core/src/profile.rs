//! The experimental representation is immutable. Its manifest deliberately uses
//! only ASCII keys/strings, booleans, and small integers: sorted serde_json output
//! is RFC 8785 canonical JSON for this restricted manifest vocabulary.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub const DIMENSIONS: usize = 4096;
pub const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
pub const FNV_PRIME: u64 = 1_099_511_628_211;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    pub profile_id: String,
    pub name: String,
    pub family: String,
    pub dimensions: usize,
    pub dtype: String,
    pub status: String,
    pub calibration_status: String,
    pub default_threshold: Option<f64>,
    pub requires_model: bool,
    pub manifest: Value,
}

pub fn text_profile() -> Profile {
    let manifest = json!({
        "profile_schema_version":1,
        "family":"text",
        "matching_objective":"near_copy_text",
        "content_scope":"complete_utf8_source_text",
        "reader":"filetwin_utf8_v1",
        "preprocessing":{
            "utf8":"strict", "bom":"remove_one_leading_utf8_bom",
            "newlines":"crlf_and_cr_to_lf", "unicode":"NFC",
            "unicode_version": format!("{}.{}.{}", unicode_normalization::UNICODE_VERSION.0, unicode_normalization::UNICODE_VERSION.1, unicode_normalization::UNICODE_VERSION.2),
            "case":"preserve", "punctuation":"preserve",
            "max_consecutive_nonstarters":1024, "nul":"reject"
        },
        "features":{
            "kind":"unicode_scalar_character_ngrams", "lengths":[1,2,3,4,5],
            "weights":[1,1,1,1,1], "hash":"fnv1a64",
            "offset_basis_decimal":FNV_OFFSET.to_string(),
            "input":"length_byte_then_utf8_scalars", "bucket":"low_12_bits",
            "sign":"high_bit_set_negative", "counts":"signed_f64_exact_integer",
            "max_features_decimal":"9007199254740991", "idf":"none"
        },
        "dimensions":DIMENSIONS,"dtype":"float32_le",
        "normalization":"f64_l2_then_round_to_f32",
        "metric":"cosine", "scoring":"filetwin_cosine_f64_v1"
    });
    let bytes = serde_json::to_vec(&manifest).expect("Static profile is serializable");
    Profile {
        profile_id: format!("sha256:{}", digest_hex(&bytes)),
        name: "experimental-text-v1".into(),
        family: "text".into(),
        dimensions: DIMENSIONS,
        dtype: "float32_le".into(),
        status: "experimental".into(),
        calibration_status: "uncalibrated".into(),
        default_threshold: None,
        requires_model: false,
        manifest,
    }
}

pub(crate) fn digest_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub(crate) fn decode_vector(bytes: &[u8], dimensions: usize) -> crate::Result<Vec<f32>> {
    if bytes.len() != dimensions * 4 {
        return Err(crate::Error::new(
            crate::ErrorCode::InvalidVectorFile,
            "vector",
            "Invalid vector byte length",
        ));
    }
    let v: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let norm: f64 = v.iter().map(|x| f64::from(*x).powi(2)).sum();
    if !norm.is_finite() || (norm - 1.0).abs() > 1e-5 {
        return Err(crate::Error::new(
            crate::ErrorCode::InvalidVectorFile,
            "vector",
            "Vector has invalid components or normalization",
        ));
    }
    Ok(v)
}

pub const IMAGE_SIZE: usize = 320;
pub const IMAGE_DIMENSIONS: usize = 512;
pub const AUDIO_DIMENSIONS: usize = 4096;
pub const SSCD_MODEL_FILE: &str = "sscd_disc_mixup.onnx";
pub const SSCD_MODEL_SHA256: &str =
    "5e51a088552434c603ff3d5259991aea2b1d87aab1ca7fa4d48e636dc46738f4";
pub const MAX_MEDIA_WINDOWS: usize = 32;
pub const AUDIO_WINDOW_SECONDS: f64 = 8.0;

fn experimental(
    name: &str,
    family: &str,
    dimensions: usize,
    model: bool,
    mut manifest: Value,
) -> Profile {
    manifest["profile_schema_version"] = json!(1);
    manifest["family"] = json!(family);
    manifest["dimensions"] = json!(dimensions);
    manifest["dtype"] = json!("float32_le");
    manifest["metric"] = json!("cosine");
    manifest["scoring"] = json!("filetwin_cosine_f64_v1");
    Profile {
        profile_id: format!(
            "sha256:{}",
            digest_hex(&serde_json::to_vec(&manifest).expect("Static manifest"))
        ),
        name: name.into(),
        family: family.into(),
        dimensions,
        dtype: "float32_le".into(),
        status: "experimental".into(),
        calibration_status: "uncalibrated".into(),
        default_threshold: None,
        requires_model: model,
        manifest,
    }
}

/// Plain UTF-8, DOCX body text, and PDF text share this representation. The old
/// plain-text-only profile stays available, so old snapshots never change meaning.
pub fn document_profile() -> Profile {
    let mut manifest = text_profile().manifest;
    manifest["content_scope"] = json!("complete_extracted_body_text");
    manifest["reader"] = json!({
        "policy":"filetwin_document_v1", "utf8":"filetwin_utf8_v1",
        "docx":"word/document.xml_transitional_or_strict_body_text_in_document_order",
        "docx_paragraphs":"lf", "docx_breaks":"lf", "docx_tabs":"tab",
        "docx_excluded":"deleted_text_instructions_headers_footers_comments_footnotes_textboxes",
        "docx_xml":"quick_xml_0_42_utf8_no_dtd_no_external_entities",
        "pdf":"pdfium_chromium_8044_page_bounds_content_order_crlf_to_lf",
        "pdf_page_separator":"lf", "ocr":false,
        "max_extracted_utf8_bytes":33554432,"max_pdf_pages":10000,
        "max_xml_depth":256,"max_zip_entries":4096,"zip64":"reject","max_zip_directory_bytes":8388608
    });
    experimental(
        "experimental-documents-v1",
        "text",
        DIMENSIONS,
        false,
        manifest,
    )
}

fn image_representation() -> Value {
    json!({
        "model":"sscd_disc_mixup", "architecture":"resnet50",
        "source_sha256":"9f26bd4c848cc19b73d2ae92eea6e04886f61a7b764ceb7a13aeee62e6a6db56",
        "onnx_sha256":SSCD_MODEL_SHA256,"runtime":"onnxruntime_cpu_1_28_2",
        "inference":"float32_single_thread_sequential_graph_optimization_disabled",
        "resize":"image_rs_0_25_triangle_direct_square_320_no_crop",
        "rgb":"rgb8_alpha_composite_black_assume_srgb_ignore_icc",
        "orientation":"apply_decoder_exif_orientation",
        "mean_decimal":["0.485","0.456","0.406"],
        "std_decimal":["0.229","0.224","0.225"],
        "layout":"NCHW_1_3_320_320", "normalization":"f64_l2_then_round_to_f32"
    })
}

/// Retained verbatim for cached vectors and immutable snapshots created by v1.
pub fn image_profile_v1() -> Profile {
    experimental(
        "experimental-image-sscd-v1",
        "image",
        IMAGE_DIMENSIONS,
        true,
        json!({"matching_objective":"near_copy_image", "representation":image_representation(),
            "decoder":"image_rs_0_25_jpeg_png_gif_webp_tiff_bmp_ico_pnm_tga_dds_qoi_farbfeld_hdr_exr",
            "animation":"first_frame", "multipage":"first_image"}),
    )
}

pub fn image_profile_v2() -> Profile {
    let mut manifest = image_profile_v1().manifest;
    manifest["decoder"] = json!({
        "raster":"image_rs_0_25_jpeg_png_gif_webp_tiff_bmp_ico_pnm_tga_dds_qoi_farbfeld_hdr_exr",
        "heif_avif":"ffmpeg_9_primary_item_or_primary_tile_grid_png_rgba8_autorotate",
        "primary_selection":"default_disposition_then_first_item_or_grid",
        "auxiliary_images":"ignore_depth_thumbnails_and_unselected_items",
        "alpha":"retain_when_exposed_by_ffmpeg_rgba_output",
        "hdr":"reject_pq_hlg_until_tone_mapping_is_qualified",
        "max_items":256,"max_dimension":32768,
        "decoded_memory":"canvas_plus_selected_item_pixels_times_32_at_most_half_worker_allowance"
    });
    experimental(
        "experimental-image-sscd-v2",
        "image",
        IMAGE_DIMENSIONS,
        true,
        manifest,
    )
}

pub fn audio_profile() -> Profile {
    experimental(
        "experimental-audio-landmarks-v1",
        "audio",
        AUDIO_DIMENSIONS,
        false,
        json!({"matching_objective":"same_recording", "decoder":"ffmpeg_9_first_audio_stream",
            "channels":"mono_ffmpeg_default_downmix", "sample_rate":16000, "pcm":"float32_le",
            "sampling":"up_to_32_disjoint_8_second_windows_centered_in_equal_duration_intervals_short_files_complete",
            "timeline":"selected_stream_start_and_duration_fallback_container_remaining_duration",
            "features":"spectral_peak_triplets_v1", "fft":1024,"hop":256,"window":"hann_periodic",
            "frequency_bins":[6,256],"peak_neighborhood_bins":4,"peaks_per_frame":5,
            "peak_floor_relative_decimal":"0.03", "triplet_frame_offsets":[[7,3,0],[15,7,0],[31,15,0],[63,31,0]],
            "frequency_quantization":"fft_bin_div_2", "hash":"fnv1a64_then_splitmix64_finalizer",
            "input":"pattern_index_u8_frequency_old_u8_frequency_middle_u8_frequency_current_u8",
            "bucket":"low_12_bits", "sign":"high_bit_set_negative",
            "pooling":"sum_signed_f64_counts_then_signed_sqrt_abs_f64_then_l2_f64_then_round_f32",
            "silence":"skip_frames_max_power_below_1e_minus_8", "minimum_landmarks":32}),
    )
}

pub fn video_profile_v1() -> Profile {
    experimental(
        "experimental-video-sscd-v1",
        "video",
        IMAGE_DIMENSIONS,
        true,
        json!({"matching_objective":"near_copy_video_visual_only", "representation":image_representation(),
            "decoder":"ffmpeg_9_first_non_attached_video_stream", "audio":"ignored",
            "sampling":"32_equal_interval_midpoints_first_frame_at_or_after_target",
            "timeline":"selected_stream_start_and_duration_fallback_container_remaining_duration",
            "duplicate_pts":"deduplicate", "short_video":"use_available_distinct_frames",
            "frame_rgb":"ffmpeg_default_color_conversion_rgb24_autorotate_square_sar",
            "frame_resize":"ffmpeg_bilinear_to_320_square_then_sscd_input",
            "hdr":"reject_pq_and_hlg_until_tone_mapping_profile_is_qualified",
            "fallback":"first_frame_if_all_midpoint_targets_have_no_frame",
            "pooling":"f64_mean_of_l2_frame_vectors_then_f64_l2_then_round_f32"}),
    )
}

/// Execution choices have distinct profile identities; reference snapshots keep
/// their original preprocessing and inference contract.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Reference,
    #[default]
    Cpu,
    Coreml,
    Cuda,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Cpu => "cpu",
            Self::Coreml => "coreml",
            Self::Cuda => "cuda",
        }
    }
}

fn accelerated(mut p: Profile, backend: Backend) -> Profile {
    if backend == Backend::Reference {
        return p;
    }
    let representation = &mut p.manifest["representation"];
    representation["runtime"] = json!("onnxruntime_1_28_2");
    representation["execution_provider"] = json!(backend);
    representation["inference"] =
        json!("float32_io_sequential_graph_optimization_level3_bounded_threads");
    representation["precision_policy"] = json!(match backend {
        Backend::Coreml => "coreml_mlprogram_all_compute_units_no_low_precision_gpu_accumulation",
        Backend::Cuda => "cuda_tf32_disabled_heuristic_conv_search",
        _ => "cpu_float32",
    });
    if p.family == "video" {
        p.manifest["decoder_execution"] =
            json!("continuous_midpoint_crossings_up_to_60_seconds_otherwise_sparse_seeks");
        p.manifest["continuous_target_rounding"] = json!("nearest_stream_time_base_tick");
    }
    experimental(
        &format!("experimental-{}-sscd-{}-v1", p.family, backend.as_str()),
        &p.family,
        p.dimensions,
        true,
        p.manifest,
    )
}

pub fn image_profile() -> Profile {
    accelerated(image_profile_v2(), Backend::Cpu)
}

pub fn video_profile() -> Profile {
    accelerated(video_profile_v1(), Backend::Cpu)
}

pub fn inference_backend(id: &str) -> crate::Result<Backend> {
    let p = find(id)?;
    Ok(
        match p.manifest["representation"]["execution_provider"].as_str() {
            Some("cpu") => Backend::Cpu,
            Some("coreml") => Backend::Coreml,
            Some("cuda") => Backend::Cuda,
            _ => Backend::Reference,
        },
    )
}

pub fn experimental_profiles_for(backend: Backend) -> Vec<Profile> {
    vec![
        document_profile(),
        accelerated(image_profile_v2(), backend),
        audio_profile(),
        accelerated(video_profile_v1(), backend),
    ]
}

pub fn experimental_profiles() -> Vec<Profile> {
    experimental_profiles_for(Backend::Cpu)
}

pub fn profiles() -> Vec<Profile> {
    registry().to_vec()
}

fn registry() -> &'static [Profile] {
    // Manifests are immutable build constants. Construct and hash them once,
    // rather than once per verified vector during exhaustive comparison.
    static PROFILES: OnceLock<Vec<Profile>> = OnceLock::new();
    PROFILES.get_or_init(|| {
        let mut p = vec![
            text_profile(),
            image_profile_v1(),
            image_profile_v2(),
            video_profile_v1(),
        ];
        p.extend(experimental_profiles());
        for backend in [Backend::Coreml, Backend::Cuda] {
            p.push(accelerated(image_profile_v2(), backend));
            p.push(accelerated(video_profile_v1(), backend));
        }
        p
    })
}

pub fn find(id: &str) -> crate::Result<Profile> {
    registry()
        .iter()
        .find(|p| p.profile_id == id)
        .cloned()
        .ok_or_else(|| {
            crate::Error::new(
                crate::ErrorCode::UnknownProfile,
                "profile",
                "Profile is unavailable in this build",
            )
        })
}

#[doc(hidden)]
pub fn normalize(vector: &mut [f32]) -> crate::Result<()> {
    let norm = vector
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err(crate::Error::new(
            crate::ErrorCode::InsufficientContent,
            "encoding",
            "No finite nonzero descriptor",
        ));
    }
    for v in vector {
        *v = (f64::from(*v) / norm) as f32;
    }
    Ok(())
}

#[doc(hidden)]
pub fn normalized_f64(vector: &[f64]) -> crate::Result<Vec<f32>> {
    let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err(crate::Error::new(
            crate::ErrorCode::InsufficientContent,
            "encoding",
            "No finite nonzero descriptor",
        ));
    }
    Ok(vector.iter().map(|v| (v / norm) as f32).collect())
}

/// Reproducible reference cosine over the stored float32 components. Multiplication
/// and accumulation use f64 in index order, without fused multiply-add. Cutoffs
/// are inclusive and apply to this full score before any display rounding.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (mut dot, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (&a, &b) in a.iter().zip(b) {
        let (a, b) = (f64::from(a), f64::from(b));
        dot += a * b;
        aa += a * a;
        bb += b * b;
    }
    (dot / (aa.sqrt() * bb.sqrt())).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn image_reader_expansion_preserves_the_original_profile() {
        let old_id = "sha256:1ab3b061f7d8e5f26ab38c9cc56766ca7408cb9779290dbec6d78846e11c7b11";
        assert_eq!(image_profile_v1().profile_id, old_id);
        assert_eq!(find(old_id).unwrap().name, "experimental-image-sscd-v1");
        assert_ne!(image_profile().profile_id, old_id);
        assert!(
            experimental_profiles()
                .iter()
                .any(|p| p.profile_id == image_profile().profile_id)
        );
    }
}
