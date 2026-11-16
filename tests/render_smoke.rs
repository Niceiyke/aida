use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::json;
use studio::{ImportOptions, ProjectStore, RenderOptions, import_asset, render_timeline};
use tempfile::tempdir;

#[test]
fn imports_validates_and_renders_a_small_timeline() {
    let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
        eprintln!("skipping FFmpeg integration test: ffmpeg not found");
        return;
    };
    let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
        eprintln!("skipping FFmpeg integration test: ffprobe not found");
        return;
    };

    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let clip_a = root.path().join("clip-a.mp4");
    let clip_b = root.path().join("clip-b.mp4");
    let audio = root.path().join("audio.wav");
    make_video(&ffmpeg, &clip_a, "blue");
    make_video(&ffmpeg, &clip_b, "red");
    run(
        &ffmpeg,
        [
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=70",
            "-c:a",
            "pcm_s16le",
            "-y",
        ]
        .into_iter()
        .chain([audio.to_str().unwrap()]),
    );

    let imported_a = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: clip_a,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    let imported_b = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: clip_b,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    let imported_audio = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: audio,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    assert!(imported_audio.probe.duration_seconds > 60.0);

    let malformed = root.path().join("not-media.mp4");
    fs::write(&malformed, b"this is not a media file").unwrap();
    assert!(
        import_asset(ImportOptions {
            workspace: workspace.clone(),
            file: malformed,
            ffprobe: ffprobe.clone(),
        })
        .is_err()
    );
    assert!(
        fs::read_dir(workspace.join("assets"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".import-"))
    );

    let mut tracks = vec![
        json!({
            "id": "video-main", "kind": "video", "order": 0,
            "clips": [
                video_clip("first", &imported_a.asset_id, 0),
                video_clip("second", &imported_b.asset_id, 24)
            ]
        }),
        json!({
            "id": "music", "kind": "audio", "order": 1,
            "clips": [{
                "id": "music-clip", "kind": "audio", "assetId": imported_audio.asset_id,
                "startFrame": 24, "durationFrames": 12,
                "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 48000},
                "sourceOut": {"ticks": 24000, "timebase_num": 1, "timebase_den": 48000},
                "gain": 0.2
            }]
        }),
    ];

    let font = configured_font();
    if font.is_some() {
        tracks.push(json!({
            "id": "titles", "kind": "overlay", "order": 2,
            "clips": [{
                "id": "title", "kind": "text", "startFrame": 3,
                "durationFrames": 40, "text": "Studio smoke test", "fontSize": 32,
                "color": "#FFFFFF", "x": 24, "y": 40
            }]
        }));
    }

    let mut timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
        "revision": 0,
        "canvas": {"width": 640, "height": 360, "fps": {"num": 24, "den": 1}, "background": "#101820"},
        "assets": {
            imported_a.asset_id.clone(): asset_ref(&imported_a),
            imported_b.asset_id.clone(): asset_ref(&imported_b),
            imported_audio.asset_id.clone(): asset_ref(&imported_audio)
        },
        "tracks": tracks,
        "settings": {"audioSampleRate": 48000, "colorSpace": "rec709"}
    });
    timeline["tracks"][0]["clips"][0]["crop"] =
        json!({"x": 0.25, "y": 0.1, "width": 0.5, "height": 0.8});
    let timeline_path = root.path().join("timeline.json");
    fs::write(
        &timeline_path,
        serde_json::to_vec_pretty(&timeline).unwrap(),
    )
    .unwrap();

    let mut unsupported = timeline.clone();
    unsupported["tracks"][0]["clips"][0]["effects"] = json!([{"type": "unregistered"}]);
    let unsupported_path = root.path().join("unsupported-effect.json");
    fs::write(
        &unsupported_path,
        serde_json::to_vec_pretty(&unsupported).unwrap(),
    )
    .unwrap();
    let unsupported_result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: unsupported_path,
        output: root.path().join("unsupported.mp4"),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font: font.clone(),
        timeout_seconds: 120,
    });
    assert!(
        format!("{:#}", unsupported_result.unwrap_err()).contains("JSON Schema validation failed")
    );

    let mut missing_asset = timeline.clone();
    missing_asset["tracks"][0]["clips"][0]["assetId"] =
        json!("018f2e40-4d28-7e31-8b63-611d49c43b0c");
    let missing_asset_path = root.path().join("missing-asset.json");
    fs::write(
        &missing_asset_path,
        serde_json::to_vec_pretty(&missing_asset).unwrap(),
    )
    .unwrap();
    let missing_asset_result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: missing_asset_path,
        output: root.path().join("missing-asset.mp4"),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font: font.clone(),
        timeout_seconds: 120,
    });
    assert!(format!("{:#}", missing_asset_result.unwrap_err()).contains("missing asset"));

    let failed_output = root.path().join("failed.mp4");
    let failed_render = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: timeline_path.clone(),
        output: failed_output.clone(),
        // ffprobe accepts the identity check but rejects FFmpeg's render arguments.
        ffmpeg: ffprobe.clone(),
        ffprobe: ffprobe.clone(),
        font: font.clone(),
        timeout_seconds: 120,
    });
    assert!(failed_render.is_err());
    assert!(!failed_output.exists());
    assert!(!root.path().join("failed.mp4.provenance.json").exists());
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".studio-")
    }));
    assert_eq!(fs::read_dir(workspace.join("tmp")).unwrap().count(), 0);

    let output = root.path().join("result.mp4");
    let result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: timeline_path.clone(),
        output: output.clone(),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font,
        timeout_seconds: 120,
    })
    .unwrap();

    assert!(output.is_file());
    assert!(result.provenance.is_file());
    assert_eq!(result.output_width, 640);
    assert_eq!(result.output_height, 360);
    assert_eq!(result.video_codec, "h264");
    assert_eq!(result.output_frame_count, 48);
    assert_eq!(result.output_frame_rate, "24/1");
    assert_eq!(result.audio_codec.as_deref(), Some("aac"));
    assert!((result.output_duration_seconds - 2.0).abs() <= 1.0 / 24.0);
    let provenance: serde_json::Value =
        serde_json::from_slice(&fs::read(&result.provenance).unwrap()).unwrap();
    assert_eq!(provenance["timelineSha256"], result.timeline_sha256);
    assert_eq!(provenance["outputSha256"], result.output_sha256);
    assert_eq!(provenance["assets"].as_array().unwrap().len(), 3);
    assert_eq!(provenance["ffmpegFingerprint"].as_str().unwrap().len(), 64);
    assert_eq!(provenance["frameCount"], 48);

    let decoded_audio = Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-i",
            output.to_str().unwrap(),
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "48000",
            "-acodec",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(decoded_audio.status.success());
    let first_audible_sample = decoded_audio
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .position(|bytes| i32::from(i16::from_le_bytes(*bytes)).abs() > 100)
        .expect("render should contain audible samples");
    let expected_onset = 48_000_usize;
    let one_frame_samples = 48_000_usize / 24;
    assert!(first_audible_sample.abs_diff(expected_onset) <= one_frame_samples);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let symlink_output = root.path().join("symlink-output.mp4");
        symlink(&output, &symlink_output).unwrap();
        let symlink_result = render_timeline(RenderOptions {
            workspace: workspace.clone(),
            timeline: timeline_path.clone(),
            output: symlink_output,
            ffmpeg: ffmpeg.clone(),
            ffprobe: ffprobe.clone(),
            font: configured_font(),
            timeout_seconds: 120,
        });
        assert!(
            symlink_result.is_err(),
            "renderer must reject symlink output paths"
        );
    }

    let silent_timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
        "revision": 0,
        "canvas": {"width": 640, "height": 360, "fps": {"num": 24, "den": 1}, "background": "#101820"},
        "assets": {imported_a.asset_id.clone(): asset_ref(&imported_a)},
        "tracks": [{
            "id": "video-main", "kind": "video", "order": 0,
            "clips": [video_clip("single", &imported_a.asset_id, 0)]
        }]
    });
    let silent_timeline_path = root.path().join("silent-timeline.json");
    fs::write(
        &silent_timeline_path,
        serde_json::to_vec_pretty(&silent_timeline).unwrap(),
    )
    .unwrap();
    let silent_output = root.path().join("silent.mp4");
    let silent_result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: silent_timeline_path,
        output: silent_output.clone(),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();
    assert!(silent_output.is_file());
    assert_eq!(silent_result.audio_codec, None);
    assert_eq!(silent_result.output_frame_count, 24);
    assert!((silent_result.output_duration_seconds - 1.0).abs() <= 1.0 / 24.0);

    let mut crossfade_timeline = silent_timeline.clone();
    crossfade_timeline["assets"][&imported_b.asset_id] = asset_ref(&imported_b);
    let mut incoming = video_clip("incoming", &imported_b.asset_id, 12);
    incoming["transitionIn"] = json!({"type": "crossfade", "durationFrames": 12});
    crossfade_timeline["tracks"][0]["clips"]
        .as_array_mut()
        .unwrap()
        .push(incoming);
    let crossfade_timeline_path = root.path().join("crossfade-timeline.json");
    fs::write(
        &crossfade_timeline_path,
        serde_json::to_vec_pretty(&crossfade_timeline).unwrap(),
    )
    .unwrap();
    let crossfade_output = root.path().join("crossfade.mp4");
    let crossfade_result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: crossfade_timeline_path,
        output: crossfade_output.clone(),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();
    assert_eq!(crossfade_result.output_frame_count, 36);
    assert!((crossfade_result.output_duration_seconds - 1.5).abs() <= 1.0 / 24.0);
    let midpoint = decode_frame(&ffmpeg, &crossfade_output, 18);
    let center = (180 * 640 + 320) * 3;
    assert!(midpoint[center] > 50 && midpoint[center + 2] > 50);

    let fade_timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
        "revision": 0,
        "canvas": {"width": 640, "height": 360, "fps": {"num": 24, "den": 1}, "background": "#101820"},
        "assets": {
            imported_a.asset_id.clone(): asset_ref(&imported_a),
            imported_audio.asset_id.clone(): asset_ref(&imported_audio)
        },
        "tracks": [
            {"id": "video-main", "kind": "video", "order": 0, "clips": [video_clip("fade-visual", &imported_a.asset_id, 0)]},
            {"id": "music", "kind": "audio", "order": 1, "clips": [{
                "id": "faded-audio", "kind": "audio", "assetId": imported_audio.asset_id,
                "startFrame": 0, "durationFrames": 24,
                "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 48000},
                "sourceOut": {"ticks": 48000, "timebase_num": 1, "timebase_den": 48000},
                "gain": 0.7, "fadeInFrames": 12, "fadeOutFrames": 12
            }]}
        ],
        "settings": {"audioSampleRate": 48000, "colorSpace": "rec709"}
    });
    let fade_timeline_path = root.path().join("fade-timeline.json");
    fs::write(
        &fade_timeline_path,
        serde_json::to_vec_pretty(&fade_timeline).unwrap(),
    )
    .unwrap();
    let fade_output = root.path().join("audio-fades.mp4");
    let fade_result = render_timeline(RenderOptions {
        workspace: workspace.clone(),
        timeline: fade_timeline_path,
        output: fade_output.clone(),
        ffmpeg: ffmpeg.clone(),
        ffprobe: ffprobe.clone(),
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();
    assert_eq!(fade_result.output_frame_count, 24);
    let faded_samples = decode_audio(&ffmpeg, &fade_output);
    assert!(faded_samples.len() >= 48_000);
    let fade_in_rms = samples_rms(&faded_samples[..4_800]);
    let middle_rms = samples_rms(&faded_samples[21_600..26_400]);
    let fade_out_rms = samples_rms(&faded_samples[43_200..48_000]);
    assert!(fade_in_rms < middle_rms * 0.25);
    assert!(fade_out_rms < middle_rms * 0.25);

    let overwrite = render_timeline(RenderOptions {
        workspace,
        timeline: timeline_path,
        output,
        ffmpeg,
        ffprobe,
        font: configured_font(),
        timeout_seconds: 120,
    });
    assert!(
        overwrite.is_err(),
        "renderer must not overwrite an existing export"
    );
}

#[test]
fn imports_bootstraps_crops_and_renders_still_images() {
    let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
        eprintln!("skipping still-image integration test: ffmpeg not found");
        return;
    };
    let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
        eprintln!("skipping still-image integration test: ffprobe not found");
        return;
    };

    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let image_path = root.path().join("still.png");
    run(
        &ffmpeg,
        [
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=magenta:s=320x240",
            "-frames:v",
            "1",
            "-y",
        ]
        .into_iter()
        .chain([image_path.to_str().unwrap()]),
    );

    let imported = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: image_path,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    assert_eq!(imported.kind, studio::AssetKind::Image);
    assert!(imported.probe.has_still_image);
    assert!(!imported.probe.has_video);
    assert_eq!(imported.probe.video.as_ref().unwrap().width, Some(320));

    let store = ProjectStore::open(root.path().join("projects.sqlite")).unwrap();
    let created = store
        .create_from_asset_ids(
            &workspace,
            std::slice::from_ref(&imported.asset_id),
            None,
            None,
        )
        .unwrap();
    let (_, timeline_json) = store
        .load_current_revision_json(&created.project_id)
        .unwrap();
    let timeline: serde_json::Value = serde_json::from_str(&timeline_json).unwrap();
    assert_eq!(timeline["tracks"][0]["clips"][0]["kind"], "image");
    assert_eq!(timeline["tracks"][0]["clips"][0]["durationFrames"], 150);
    let image_clip_id = timeline["tracks"][0]["clips"][0]["id"].as_str().unwrap();

    let image_add = serde_json::from_value(json!({
        "type": "image.add",
        "assetId": imported.asset_id,
        "durationSeconds": 2.0
    }))
    .unwrap();
    let crop = serde_json::from_value(json!({
        "type": "clip.crop",
        "clipId": image_clip_id,
        "crop": {"x": 0.25, "y": 0.1, "width": 0.5, "height": 0.8}
    }))
    .unwrap();
    let trim = serde_json::from_value(json!({
        "type": "clip.trim",
        "clipId": image_clip_id,
        "startFrame": 0,
        "durationFrames": 180
    }))
    .unwrap();
    let motion = serde_json::from_value(json!({
        "type": "image.motion.set",
        "clipId": image_clip_id,
        "motion": "pan_right"
    }))
    .unwrap();
    store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "append-crop-and-motion-image",
            &[image_add, crop, trim, motion],
        )
        .unwrap();
    let (_, timeline_json) = store
        .load_current_revision_json(&created.project_id)
        .unwrap();
    let edited_timeline: serde_json::Value = serde_json::from_str(&timeline_json).unwrap();
    assert_eq!(
        edited_timeline["tracks"][0]["clips"][0]["motion"],
        "pan_right"
    );
    let timeline_path = root.path().join("still-image-timeline.json");
    fs::write(&timeline_path, timeline_json).unwrap();

    let output = root.path().join("still-image-render.mp4");
    let result = render_timeline(RenderOptions {
        workspace,
        timeline: timeline_path,
        output: output.clone(),
        ffmpeg,
        ffprobe,
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();
    assert!(output.is_file());
    assert_eq!((result.output_width, result.output_height), (320, 240));
    assert_eq!(result.output_frame_count, 240);
    assert_eq!(result.output_frame_rate, "30/1");
}

#[test]
fn renders_h265_vfr_input_to_a_counted_portrait_output() {
    let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
        eprintln!("skipping H.265/VFR test: ffmpeg not found");
        return;
    };
    let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
        eprintln!("skipping H.265/VFR test: ffprobe not found");
        return;
    };
    if !ffmpeg_has_encoder(&ffmpeg, "libx265") {
        eprintln!("skipping H.265/VFR test: installed ffmpeg has no libx265 encoder");
        return;
    }

    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let source = root.path().join("portrait-vfr-hevc.mp4");
    let status = Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=240x320:r=30:d=1",
            "-vf",
            "select='not(mod(n,3))'",
            "-fps_mode",
            "vfr",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:pools=1:frame-threads=1",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success(), "could not generate H.265/VFR fixture");

    let imported = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: source,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    assert!(imported.probe.video.is_some());
    assert!(imported.probe.duration_seconds > 0.9);
    assert_ne!(
        imported
            .probe
            .video
            .as_ref()
            .and_then(|video| video.frame_rate.as_deref()),
        Some("30/1"),
        "fixture should retain a variable source cadence"
    );

    let timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
        "revision": 0,
        "canvas": {"width": 360, "height": 640, "fps": {"num": 24, "den": 1}, "background": "#101820"},
        "assets": {imported.asset_id.clone(): asset_ref(&imported)},
        "tracks": [{
            "id": "video-main", "kind": "video", "order": 0,
            "clips": [{
                "id": "portrait-vfr-clip", "kind": "video", "assetId": imported.asset_id,
                "startFrame": 0, "durationFrames": 22,
                "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 1000000},
                "sourceOut": {"ticks": 933333, "timebase_num": 1, "timebase_den": 1000000}
            }]
        }]
    });
    let timeline_path = root.path().join("timeline.json");
    fs::write(
        &timeline_path,
        serde_json::to_vec_pretty(&timeline).unwrap(),
    )
    .unwrap();
    let output = root.path().join("portrait.mp4");
    let result = render_timeline(RenderOptions {
        workspace,
        timeline: timeline_path,
        output,
        ffmpeg,
        ffprobe,
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();

    assert_eq!((result.output_width, result.output_height), (360, 640));
    assert_eq!(result.video_codec, "h264");
    assert_eq!(result.output_frame_count, 22);
    assert_eq!(result.output_frame_rate, "24/1");
}

fn make_video(ffmpeg: &Path, output: &Path, color: &str) {
    let source = format!("color=c={color}:s=320x240:r=24:d=1");
    run(
        ffmpeg,
        [
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &source,
            "-an",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ]
        .into_iter()
        .chain([output.to_str().unwrap()]),
    );
}

fn run<'a>(ffmpeg: &Path, args: impl IntoIterator<Item = &'a str>) {
    let status = Command::new(ffmpeg).args(args).status().unwrap();
    assert!(status.success(), "fixture ffmpeg process failed: {status}");
}

fn decode_frame(ffmpeg: &Path, video: &Path, frame: u32) -> Vec<u8> {
    let selection = format!("select=eq(n\\,{frame})");
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(video)
        .args([
            "-vf",
            &selection,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "could not decode crossfade frame");
    output.stdout
}

fn decode_audio(ffmpeg: &Path, video: &Path) -> Vec<i16> {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(video)
        .args([
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "48000",
            "-acodec",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "could not decode faded audio");
    output
        .stdout
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
        .collect()
}

fn samples_rms(samples: &[i16]) -> f64 {
    let energy = samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>();
    (energy / samples.len() as f64).sqrt()
}

fn configured_tool(variable: &str, fallback: &str) -> Option<PathBuf> {
    if let Ok(value) = std::env::var(variable) {
        let path = PathBuf::from(value);
        assert!(
            path.is_file(),
            "configured tool does not exist: {}",
            path.display()
        );
        return Some(path);
    }
    find_on_path(fallback)
}

fn ffmpeg_has_encoder(ffmpeg: &Path, encoder: &str) -> bool {
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .unwrap();
    if !output.status.success() {
        return false;
    }
    let listing = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    listing
        .lines()
        .any(|line| line.split_whitespace().any(|field| field == encoder))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = directory.join(format!("{name}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    })
}

fn configured_font() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("STUDIO_TEST_FONT") {
        let path = PathBuf::from(value);
        assert!(
            path.is_file(),
            "configured font does not exist: {}",
            path.display()
        );
        return Some(path);
    }
    [
        PathBuf::from(r"C:\Windows\Fonts\arial.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn asset_ref(asset: &studio::ImportedAsset) -> serde_json::Value {
    json!({
        "kind": asset.kind,
        "uri": asset.uri,
        "sha256": asset.sha256
    })
}

fn video_clip(id: &str, asset_id: &str, start_frame: u32) -> serde_json::Value {
    json!({
        "id": id,
        "kind": "video",
        "assetId": asset_id,
        "startFrame": start_frame,
        "durationFrames": 24,
        "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
        "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24},
        "transform": {"x": 0, "y": 0, "scaleX": 1, "scaleY": 1, "rotationDeg": 0, "opacity": 1},
        "effects": []
    })
}
