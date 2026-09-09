use crate::raster::Sscd;
use filetwin_core::{
    Error, ErrorCode, Result, profile,
    worker_protocol::{Encoded, Request},
};
use rustfft::{FftPlanner, num_complex::Complex};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::OnceLock,
};

fn decode_error(e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::DecodeFailed, "media", e.to_string())
}

/// Pipes are drained concurrently and retained bytes are capped. Decoder stderr
/// can never fill the pipe or escape into the public CLI stream. The coordinator
/// bounds wall time and kills the entire worker/decoder process group.
fn capture(command: &mut Command, max_stdout: usize) -> Result<(Vec<u8>, String)> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(decode_error)?;
    fn drain(mut r: impl Read, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        let mut overflow = false;
        loop {
            let n = r.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            let retain = n.min(max.saturating_sub(data.len()));
            data.extend_from_slice(&buffer[..retain]);
            overflow |= retain < n;
        }
        Ok((data, overflow))
    }
    let stderr = child.stderr.take().expect("stderr pipe");
    let logs = std::thread::spawn(move || drain(stderr, 64 * 1024));
    let out = drain(child.stdout.take().expect("stdout pipe"), max_stdout);
    if out.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let (diagnostic, overflow) = logs
        .join()
        .map_err(|_| decode_error("Decoder log reader failed"))??;
    let (output, output_overflow) = out?;
    let diagnostic = String::from_utf8_lossy(&diagnostic).into_owned();
    if !status.success() || overflow || output_overflow {
        let mut e = decode_error(if overflow || output_overflow {
            "Decoder output exceeded its bounded buffer".into()
        } else {
            format!("Decoder exited with {status}")
        });
        e.details =
            Box::new(json!({"diagnostic":diagnostic.chars().take(2048).collect::<String>()}));
        return Err(e);
    }
    Ok((output, diagnostic))
}

fn decoder(request: &Request, probe: bool) -> Result<Command> {
    let path = if probe {
        &request.runtime.ffprobe_path
    } else {
        &request.runtime.ffmpeg_path
    };
    let mut c = Command::new(path.as_ref().ok_or_else(|| {
        Error::new(
            ErrorCode::RuntimeUnavailable,
            "media",
            "FFmpeg/ffprobe path missing",
        )
    })?);
    c.stdin(Stdio::from(File::open(&request.path)?));
    if !probe {
        c.args([
            "-nostdin",
            "-filter_threads",
            "1",
            "-filter_complex_threads",
            "1",
        ]);
    }
    c.args([
        "-hide_banner",
        "-threads",
        "1",
        "-max_alloc",
        &(request.memory_bytes / 4)
            .min(128 * 1024 * 1024)
            .to_string(),
        "-protocol_whitelist",
        "fd",
        "-fd",
        "0",
        "-probesize",
        "8388608",
        "-analyzeduration",
        "10000000",
    ]);
    Ok(c)
}

fn version(path: &Path) -> Result<String> {
    let (bytes, _) = capture(
        Command::new(path).arg("-version").stdin(Stdio::null()),
        16 * 1024,
    )?;
    let line = String::from_utf8_lossy(&bytes)
        .lines()
        .next()
        .unwrap_or("")
        .to_owned();
    if !line
        .split_whitespace()
        .nth(2)
        .is_some_and(|s| s.starts_with("9."))
    {
        return Err(Error::new(
            ErrorCode::RuntimeUnavailable,
            "media",
            "This experimental media profile requires FFmpeg and ffprobe major version 9",
        ));
    }
    Ok(line)
}

fn probe(request: &Request) -> Result<(Value, String)> {
    let ffmpeg = version(
        request
            .runtime
            .ffmpeg_path
            .as_ref()
            .ok_or_else(|| decode_error("FFmpeg missing"))?,
    )?;
    version(
        request
            .runtime
            .ffprobe_path
            .as_ref()
            .ok_or_else(|| decode_error("ffprobe missing"))?,
    )?;
    let mut c = decoder(request, true)?;
    c.args(["-v", "error", "-show_entries", "format=duration,start_time,format_name:stream=index,codec_type,codec_name,duration,start_time,width,height,nb_frames,color_space,color_transfer,color_primaries,sample_aspect_ratio:stream_disposition=attached_pic", "-of", "json", "fd:"]);
    let (bytes, _) = capture(&mut c, 64 * 1024)?;
    let metadata: Value = serde_json::from_slice(&bytes).map_err(decode_error)?;
    Ok((metadata, ffmpeg))
}

/// HEIF/AVIF items are still images. They need neither a duration nor the
/// temporal sampling used by video, even though FFmpeg describes them as video
/// streams. An explicit primary-item selection avoids embedding a thumbnail.
pub fn still_image(request: &Request) -> Result<(image::RgbImage, Value)> {
    let ffmpeg = version(
        request
            .runtime
            .ffmpeg_path
            .as_ref()
            .ok_or_else(|| decode_error("FFmpeg missing"))?,
    )?;
    version(
        request
            .runtime
            .ffprobe_path
            .as_ref()
            .ok_or_else(|| decode_error("ffprobe missing"))?,
    )?;
    let mut command = decoder(request, true)?;
    command.args([
        "-v",
        "error",
        "-show_streams",
        "-show_stream_groups",
        "-of",
        "json",
        "fd:",
    ]);
    let (bytes, _) = capture(&mut command, 1024 * 1024)?;
    let metadata: Value = serde_json::from_slice(&bytes).map_err(decode_error)?;
    let target = still_target(&metadata)?;
    let mut pixels = target
        .width
        .checked_mul(target.height)
        .ok_or_else(|| decode_error("Invalid image dimensions"))?;
    for stream in &target.streams {
        let width = stream["width"].as_u64().unwrap_or(0);
        let height = stream["height"].as_u64().unwrap_or(0);
        if width == 0 || height == 0 || width > 32768 || height > 32768 {
            return Err(decode_error("Invalid HEIF/AVIF item dimensions"));
        }
        pixels = pixels
            .checked_add(width * height)
            .ok_or_else(|| decode_error("Invalid image dimensions"))?;
        if matches!(
            stream["color_transfer"].as_str(),
            Some("smpte2084" | "arib-std-b67")
        ) {
            return Err(Error::new(
                ErrorCode::UnsupportedFormat,
                "image",
                "PQ/HLG HEIF/AVIF requires a qualified tone-mapping profile",
            ));
        }
    }
    if target.width > 32768
        || target.height > 32768
        || pixels
            .checked_mul(32)
            .is_none_or(|bytes| bytes > request.memory_bytes / 2)
    {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "image",
            "HEIF/AVIF decoded image and tiles exceed memory allowance",
        ));
    }
    let output_limit = (request.memory_bytes / 8).min(128 * 1024 * 1024);
    let mut command = decoder(request, false)?;
    command.args([
        "-v",
        "error",
        "-i",
        "fd:",
        "-map",
        &target.mapping,
        "-frames:v",
        "1",
        "-an",
        "-sn",
        "-dn",
        "-pix_fmt",
        "rgba",
        "-c:v",
        "png",
        "-threads",
        "1",
        "-f",
        "image2pipe",
        "-protocol_whitelist",
        "pipe",
        "pipe:1",
    ]);
    let (png, _) = capture(&mut command, output_limit as usize)?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(output_limit);
    reader.limits(limits);
    let rgba = reader.decode().map_err(decode_error)?.into_rgba8();
    let rgb =
        image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            let pixel = rgba.get_pixel(x, y).0;
            image::Rgb([0, 1, 2].map(|channel| {
                ((u16::from(pixel[channel]) * u16::from(pixel[3]) + 127) / 255) as u8
            }))
        });
    Ok((
        rgb,
        json!({"coverage":"primary_image", "format":"HEIF/AVIF",
        "source_width":target.width,"source_height":target.height,"decoded_width":rgba.width(),"decoded_height":rgba.height(),
        "selection":target.mapping,"tile_grid":target.grid,"decoder":ffmpeg,
        "orientation":"ffmpeg_autorotate_and_crop","color_policy":"ffmpeg_rgba8_alpha_on_black_ignore_icc",
        "auxiliary_images":"unselected_items_and_depth_ignored","hdr_tone_mapping":false}),
    ))
}

struct StillTarget<'a> {
    mapping: String,
    width: u64,
    height: u64,
    grid: bool,
    streams: Vec<&'a Value>,
}

fn still_target(metadata: &Value) -> Result<StillTarget<'_>> {
    let streams = metadata["streams"]
        .as_array()
        .ok_or_else(|| decode_error("No image items"))?;
    let groups = metadata["stream_groups"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if streams.len() > 256 || groups.len() > 256 {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "image",
            "HEIF/AVIF exceeds 256 items",
        ));
    }
    let mut targets = Vec::new();
    let mut tiled = BTreeSet::new();
    for group in groups {
        if group["type"].as_str() != Some("Tile Grid") {
            continue;
        }
        let component = &group["components"][0];
        let tiles = group["streams"]
            .as_array()
            .ok_or_else(|| decode_error("Tile grid has no streams"))?;
        let index = group["index"]
            .as_u64()
            .ok_or_else(|| decode_error("Tile grid has no index"))?;
        if tiles.is_empty() || tiles.len() > 256 {
            return Err(decode_error("Invalid tile grid"));
        }
        for tile in tiles {
            if let Some(index) = tile["index"].as_u64() {
                tiled.insert(index);
            }
        }
        // FFmpeg 9 reconstructs a multi-tile group into this internal filter
        // output, including grid cropping and orientation. Mapping individual
        // tile streams would encode only part of the picture.
        let mapping = if component["nb_tiles"].as_u64().unwrap_or(0) > 1 {
            format!("[0:g:{index}]")
        } else {
            format!(
                "0:{}",
                tiles[0]["index"]
                    .as_u64()
                    .ok_or_else(|| decode_error("Missing tile index"))?
            )
        };
        targets.push((
            group["disposition"]["default"].as_u64() == Some(1),
            StillTarget {
                mapping,
                width: component["width"].as_u64().unwrap_or(0),
                height: component["height"].as_u64().unwrap_or(0),
                grid: true,
                streams: tiles.iter().collect(),
            },
        ));
    }
    for stream in streams {
        let index = stream["index"]
            .as_u64()
            .ok_or_else(|| decode_error("Missing image item index"))?;
        if stream["codec_type"].as_str() != Some("video") || tiled.contains(&index) {
            continue;
        }
        targets.push((
            stream["disposition"]["default"].as_u64() == Some(1),
            StillTarget {
                mapping: format!("0:{index}"),
                width: stream["width"].as_u64().unwrap_or(0),
                height: stream["height"].as_u64().unwrap_or(0),
                grid: false,
                streams: vec![stream],
            },
        ));
    }
    let selected = targets
        .iter()
        .position(|(primary, _)| *primary)
        .unwrap_or(0);
    if targets.is_empty() {
        return Err(Error::new(
            ErrorCode::InsufficientContent,
            "image",
            "No usable HEIF/AVIF image item",
        ));
    }
    let target = targets.swap_remove(selected).1;
    if target.width == 0 || target.height == 0 {
        return Err(decode_error("Invalid HEIF/AVIF image dimensions"));
    }
    Ok(target)
}

fn stream<'a>(metadata: &'a Value, family: &str) -> Result<&'a Value> {
    metadata["streams"]
        .as_array()
        .and_then(|s| {
            s.iter().find(|s| {
                s["codec_type"].as_str() == Some(family)
                    && s["disposition"]["attached_pic"].as_u64().unwrap_or(0) == 0
            })
        })
        .ok_or_else(|| {
            Error::new(
                ErrorCode::InsufficientContent,
                "media",
                format!("No usable {family} stream"),
            )
        })
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}
fn duration(metadata: &Value, stream: &Value) -> Result<f64> {
    let offset = stream_offset(metadata, stream);
    number(&stream["duration"])
        .or_else(|| number(&metadata["format"]["duration"]).map(|n| n - offset))
        .filter(|s| *s > 0.0 && *s <= 365.0 * 24.0 * 3600.0)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::InsufficientContent,
                "media",
                "A finite positive media duration is required for whole-timeline sampling",
            )
        })
}

fn stream_offset(metadata: &Value, stream: &Value) -> f64 {
    let container = number(&metadata["format"]["start_time"]).unwrap_or(0.0);
    (number(&stream["start_time"]).unwrap_or(container) - container).max(0.0)
}

pub fn encode(request: &Request) -> Result<Encoded> {
    let (metadata, ffmpeg) = probe(request)?;
    let family = if stream(&metadata, "video").is_ok() {
        "video"
    } else {
        "audio"
    };
    let Some(id) = request.profiles.get(family) else {
        let mut e = Error::new(
            ErrorCode::UnselectedFamily,
            "media",
            "Detected media family was not selected",
        );
        e.details = Box::new(json!({"family":family}));
        return Err(e);
    };
    if profile::find(id)?.family != family {
        return Err(decode_error("Media profile belongs to another family"));
    }
    if family == "video" {
        video_with_probe(request, metadata, ffmpeg)
    } else {
        audio_with_probe(request, metadata, ffmpeg)
    }
}

pub fn audio(request: &Request) -> Result<Encoded> {
    let (metadata, ffmpeg) = probe(request)?;
    audio_with_probe(request, metadata, ffmpeg)
}

fn audio_with_probe(request: &Request, metadata: Value, ffmpeg: String) -> Result<Encoded> {
    let stream = stream(&metadata, "audio")?;
    let duration = duration(&metadata, stream)?;
    let offset = stream_offset(&metadata, stream);
    let n = (duration / profile::AUDIO_WINDOW_SECONDS)
        .ceil()
        .clamp(1.0, profile::MAX_MEDIA_WINDOWS as f64) as usize;
    let interval = duration / n as f64;
    let length = profile::AUDIO_WINDOW_SECONDS.min(interval);
    let mut counts = vec![0.0f64; profile::AUDIO_DIMENSIONS];
    let mut landmarks = 0u64;
    let mut windows = Vec::new();
    let mut sample_count = 0;
    for i in 0..n {
        let start = ((i as f64 + 0.5) * interval - length / 2.0).max(0.0);
        let mut c = decoder(request, false)?;
        c.args([
            "-v",
            "error",
            "-xerror",
            "-ss",
            &format!("{:.9}", start + offset),
            "-i",
            "fd:",
            "-map",
            &format!("0:{}", stream["index"]),
            "-vn",
            "-sn",
            "-dn",
            "-t",
            &format!("{length:.9}"),
            "-ac",
            "1",
            "-ar",
            "16000",
            "-threads",
            "1",
            "-f",
            "f32le",
            "-protocol_whitelist",
            "pipe",
            "pipe:1",
        ]);
        let (bytes, _) = capture(&mut c, 16000 * 4 * 9)?;
        if !bytes.len().is_multiple_of(4) {
            return Err(decode_error("Truncated PCM sample"));
        }
        let samples: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if samples.iter().any(|v| !v.is_finite()) {
            return Err(decode_error("Nonfinite PCM samples"));
        }
        if samples.len() < 1024 {
            return Err(Error::new(
                ErrorCode::InsufficientContent,
                "audio",
                "A sampled audio interval has fewer than 1024 decoded samples",
            ));
        }
        landmarks += fingerprint(&samples, &mut counts);
        sample_count += samples.len();
        windows.push(json!({"start_seconds":start,"decoder_seek_seconds":start+offset,"requested_duration_seconds":length,"decoded_samples":samples.len()}));
    }
    if landmarks < 32 {
        return Err(Error::new(
            ErrorCode::InsufficientContent,
            "audio",
            "Recording is silent, too short, or has too few spectral landmarks",
        ));
    }
    let vector = profile::normalized_f64(
        &counts
            .into_iter()
            .map(|n| n.signum() * n.abs().sqrt())
            .collect::<Vec<_>>(),
    )?;
    Ok(Encoded {
        family: "audio".into(),
        format: metadata["format"]["format_name"]
            .as_str()
            .unwrap_or("audio")
            .into(),
        vector,
        extraction: json!({"coverage":if n as f64 * length + 1e-6 >= duration {"complete_audio_timeline"} else {"sampled_audio_timeline"},
            "duration_seconds":duration,"stream_index":stream["index"],"codec":stream["codec_name"],"sample_rate":16000,"decoded_samples":sample_count,"landmarks":landmarks,"windows":windows,"decoder":ffmpeg}),
    })
}

fn fingerprint(samples: &[f32], counts: &mut [f64]) -> u64 {
    const N: usize = 1024;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
    static HANN: OnceLock<Vec<f32>> = OnceLock::new();
    let hann = HANN.get_or_init(|| {
        (0..N)
            .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / N as f64).cos()) as f32)
            .collect()
    });
    let mut history: VecDeque<Vec<u8>> = VecDeque::new();
    let mut landmarks = 0;
    let mut buffer = vec![Complex::new(0.0, 0.0); N];
    for frame in samples.windows(N).step_by(256) {
        for (i, sample) in frame.iter().enumerate() {
            buffer[i] = Complex::new(*sample * hann[i], 0.0);
        }
        fft.process(&mut buffer);
        let powers: Vec<f32> = buffer[..N / 2].iter().map(|c| c.norm_sqr()).collect();
        let maximum = powers[6..=256].iter().copied().fold(0.0f32, f32::max);
        let mut peaks: Vec<usize> = (6usize..=256)
            .filter(|&k| {
                maximum >= 1e-8
                    && powers[k] >= maximum * 0.03
                    && (k - 4..=k + 4).all(|j| {
                        j == k || powers[j] < powers[k] || (powers[j] == powers[k] && j > k)
                    })
            })
            .collect();
        peaks.sort_by(|&a, &b| powers[b].total_cmp(&powers[a]).then(a.cmp(&b)));
        peaks.truncate(5);
        let peaks: Vec<u8> = peaks.into_iter().map(|k| (k / 2) as u8).collect();
        for (pattern, (far, near)) in [(7usize, 3usize), (15, 7), (31, 15), (63, 31)]
            .into_iter()
            .enumerate()
        {
            if history.len() < far {
                continue;
            }
            for &a in &history[history.len() - far] {
                for &b in &history[history.len() - near] {
                    for &c in &peaks {
                        let mut hash = profile::FNV_OFFSET;
                        for byte in [pattern as u8, a, b, c] {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(profile::FNV_PRIME);
                        }
                        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
                        hash ^= hash >> 31;
                        counts[(hash as usize) & (profile::AUDIO_DIMENSIONS - 1)] +=
                            if hash >> 63 == 0 { 1.0 } else { -1.0 };
                        landmarks += 1;
                    }
                }
            }
        }
        history.push_back(peaks);
        if history.len() > 63 {
            history.pop_front();
        }
    }
    landmarks
}

pub fn video(request: &Request) -> Result<Encoded> {
    let (metadata, ffmpeg) = probe(request)?;
    video_with_probe(request, metadata, ffmpeg)
}

fn video_with_probe(request: &Request, metadata: Value, ffmpeg: String) -> Result<Encoded> {
    if request.memory_bytes < 512 * 1024 * 1024 {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "video",
            "Video encoding requires at least 512 MiB",
        ));
    }
    let stream = stream(&metadata, "video")?;
    if matches!(
        stream["color_transfer"].as_str(),
        Some("smpte2084" | "arib-std-b67")
    ) {
        return Err(Error::new(
            ErrorCode::UnsupportedFormat,
            "video",
            "HDR PQ/HLG requires a separately qualified tone-mapping profile",
        ));
    }
    let duration = duration(&metadata, stream)?;
    let width = stream["width"].as_u64().unwrap_or(0);
    let height = stream["height"].as_u64().unwrap_or(0);
    if width == 0 || height == 0 {
        return Err(decode_error("Invalid video dimensions"));
    }
    if width.saturating_mul(height).saturating_mul(32) > request.memory_bytes / 2 {
        return Err(Error::new(
            ErrorCode::ResourceBudgetTooSmall,
            "video",
            "Video decoder frame buffers exceed the memory allowance",
        ));
    }
    let mut model = Sscd::load(request)?;
    let mut sum = vec![0.0f64; profile::IMAGE_DIMENSIONS];
    let mut frames = Vec::new();
    let mut seen = BTreeSet::new();
    let origin = number(&stream["start_time"])
        .or_else(|| number(&metadata["format"]["start_time"]))
        .unwrap_or(0.0);
    let offset = stream_offset(&metadata, stream);
    let mut missing = Vec::new();
    for i in 0..=profile::MAX_MEDIA_WINDOWS {
        let fallback = i == profile::MAX_MEDIA_WINDOWS;
        if fallback && !frames.is_empty() {
            break;
        }
        let target = if fallback {
            0.0
        } else {
            duration * (i as f64 + 0.5) / profile::MAX_MEDIA_WINDOWS as f64
        };
        let mut c = decoder(request, false)?;
        c.args([
            "-v",
            "info",
            "-xerror",
            "-ss",
            &format!("{:.9}", target + offset),
            "-copyts",
            "-i",
            "fd:",
            "-map",
            &format!("0:{}", stream["index"]),
            "-an",
            "-sn",
            "-dn",
            "-vf",
            "scale=320:320:flags=bilinear,setsar=1,format=rgb24,showinfo",
            "-frames:v",
            "1",
            "-fps_mode",
            "passthrough",
            "-threads",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-protocol_whitelist",
            "pipe",
            "pipe:1",
        ]);
        let (bytes, log) = capture(&mut c, 320 * 320 * 3)?;
        if bytes.is_empty() {
            missing.push(target);
            continue;
        }
        let time_base = log
            .lines()
            .find_map(|line| {
                line.split("config in time_base:")
                    .nth(1)?
                    .trim()
                    .split(',')
                    .next()
            })
            .ok_or_else(|| decode_error("Missing decoded frame time base"))?;
        let (pts, time) = log
            .lines()
            .filter(|l| l.contains("Parsed_showinfo") && l.contains("pts_time:"))
            .find_map(|line| parse_pts(line, time_base))
            .ok_or_else(|| decode_error("Decoder did not report an actual frame timestamp"))?;
        if !seen.insert(pts.clone()) {
            continue;
        }
        let rgb = image::RgbImage::from_raw(320, 320, bytes)
            .ok_or_else(|| decode_error("Incomplete RGB frame"))?;
        let vector = model.rgb(&rgb)?;
        for (total, component) in sum.iter_mut().zip(vector) {
            *total += f64::from(component);
        }
        frames.push(json!({"target_seconds":target,"decoder_seek_seconds":target+offset,"decoded_pts":pts,"decoded_pts_time_base":time_base,"decoded_pts_seconds":time,"relative_seconds":time-origin,"fallback":fallback}));
    }
    if frames.is_empty() {
        return Err(Error::new(
            ErrorCode::InsufficientContent,
            "video",
            "No sampled video frames could be decoded",
        ));
    }
    let mean: Vec<f64> = sum.into_iter().map(|n| n / frames.len() as f64).collect();
    let vector = profile::normalized_f64(&mean)?;
    Ok(Encoded {
        family: "video".into(),
        format: metadata["format"]["format_name"]
            .as_str()
            .unwrap_or("video")
            .into(),
        vector,
        extraction: json!({"coverage":"sampled_visual_timeline","duration_seconds":duration,"stream_start_seconds":origin,"stream_index":stream["index"],"codec":stream["codec_name"],
            "source_width":width,"source_height":height,"requested_frames":32,"distinct_frames":frames.len(),"frames":frames,"targets_without_frame":missing,
            "audio":"ignored","decoder":ffmpeg,"orientation":"ffmpeg_autorotate","color":"ffmpeg_default_rgb24","source_color_space":stream["color_space"],"source_transfer":stream["color_transfer"],"source_primaries":stream["color_primaries"],"source_sample_aspect_ratio":stream["sample_aspect_ratio"],"hdr_tone_mapping":false,"pooling":"mean_normalized_sscd"}),
    })
}

fn parse_pts(line: &str, time_base: &str) -> Option<(String, f64)> {
    let pts = line
        .split(" pts:")
        .nth(1)?
        .split_whitespace()
        .next()?
        .to_owned();
    let (num, den) = time_base.split_once('/')?;
    let num = num.trim().parse::<f64>().ok()?;
    let den = den.trim().parse::<f64>().ok()?;
    let seconds = pts.parse::<i64>().ok()? as f64 * num / den;
    seconds.is_finite().then_some((pts, seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_images_select_the_primary_item_and_complete_grid() {
        let thumbnail = json!({"index":0,"codec_type":"video","width":32,"height":32});
        let primary = json!({"index":1,"codec_type":"video","width":128,"height":96,"disposition":{"default":1}});
        let metadata = json!({"streams":[thumbnail,primary]});
        let target = still_target(&metadata).unwrap();
        assert_eq!(target.mapping, "0:1");
        assert_eq!((target.width, target.height), (128, 96));

        let a = json!({"index":1,"codec_type":"video","width":128,"height":96});
        let b = json!({"index":2,"codec_type":"video","width":128,"height":96});
        let metadata = json!({"streams":[thumbnail,a,b],"stream_groups":[{
            "index":3,"type":"Tile Grid","disposition":{"default":1},"streams":[a,b],
            "components":[{"nb_tiles":2,"width":256,"height":96}]
        }]});
        let target = still_target(&metadata).unwrap();
        assert_eq!(target.mapping, "[0:g:3]");
        assert_eq!((target.width, target.height), (256, 96));
        assert!(target.grid);
        assert_eq!(target.streams.len(), 2);
        assert!(still_target(&json!({"streams":[]})).is_err());
    }

    #[test]
    fn silence_and_distinct_spectral_landmarks() {
        let mut a = vec![0.0; 4096];
        assert_eq!(fingerprint(&vec![0.0; 16000], &mut a), 0);
        let signal = |hz: f64| {
            (0..16000)
                .map(|i| (std::f64::consts::TAU * hz * i as f64 / 16000.0).sin() as f32)
                .collect::<Vec<_>>()
        };
        assert!(fingerprint(&signal(440.0), &mut a) > 32);
        let mut b = vec![0.0; 4096];
        assert!(fingerprint(&signal(900.0), &mut b) > 32);
        let a: Vec<f32> = a
            .iter()
            .map(|x| (x.signum() * x.abs().sqrt()) as f32)
            .collect();
        let b: Vec<f32> = b
            .iter()
            .map(|x| (x.signum() * x.abs().sqrt()) as f32)
            .collect();
        assert!(profile::cosine(&a, &b) < 0.2);
    }

    #[test]
    fn different_noise_recordings_do_not_match_as_the_same_sound_category() {
        let noise = |mut seed: u32| {
            (0..48000)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    (f64::from(seed) / f64::from(u32::MAX) - 0.5) as f32
                })
                .collect::<Vec<_>>()
        };
        let descriptor = |samples: &[f32]| {
            let mut counts = vec![0.0; profile::AUDIO_DIMENSIONS];
            assert!(fingerprint(samples, &mut counts) > 32);
            profile::normalized_f64(
                &counts
                    .iter()
                    .map(|x| x.signum() * x.abs().sqrt())
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let a = descriptor(&noise(1));
        let b = descriptor(&noise(9701));
        assert!(profile::cosine(&a, &b) < 0.3);
        assert!((profile::cosine(&a, &descriptor(&noise(1))) - 1.0).abs() < 1e-12);
    }
}
