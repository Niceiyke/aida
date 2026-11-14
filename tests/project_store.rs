use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use serde_json::{Value, json};
use studio::{
    EditOperation, ImportOptions, ProjectStore, RenderRevisionOptions, import_asset,
    render_revision,
};
use tempfile::tempdir;

#[test]
fn revisioned_edits_are_atomic_idempotent_and_render_after_reopen() {
    let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
        eprintln!("skipping project store integration test: ffmpeg not found");
        return;
    };
    let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
        eprintln!("skipping project store integration test: ffprobe not found");
        return;
    };

    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let database = root.path().join("projects.sqlite");
    let video_path = root.path().join("source.mp4");
    let audio_path = root.path().join("music.wav");
    make_video(&ffmpeg, &video_path);
    make_audio(&ffmpeg, &audio_path);

    let video = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: video_path,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();
    let audio = import_asset(ImportOptions {
        workspace: workspace.clone(),
        file: audio_path,
        ffprobe: ffprobe.clone(),
    })
    .unwrap();

    let timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": "018f2e40-4d28-7e31-8b63-611d49c43b0a",
        "revision": 0,
        "canvas": {
            "width": 320,
            "height": 240,
            "fps": {"num": 24, "den": 1},
            "background": "#101820"
        },
        "assets": {
            video.asset_id.clone(): asset_ref(&video),
            audio.asset_id.clone(): asset_ref(&audio)
        },
        "tracks": [
            {
                "id": "video-main", "kind": "video", "order": 0,
                "clips": [{
                    "id": "picture", "kind": "video", "assetId": video.asset_id,
                    "startFrame": 0, "durationFrames": 24,
                    "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
                    "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24}
                }]
            },
            {
                "id": "music", "kind": "audio", "order": 1,
                "clips": [{
                    "id": "music-clip", "kind": "audio", "assetId": audio.asset_id,
                    "startFrame": 0, "durationFrames": 24,
                    "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 48000},
                    "sourceOut": {"ticks": 48000, "timebase_num": 1, "timebase_den": 48000},
                    "gain": 0.2
                }]
            }
        ]
    });
    let timeline_path = root.path().join("timeline.json");
    fs::write(
        &timeline_path,
        serde_json::to_vec_pretty(&timeline).unwrap(),
    )
    .unwrap();

    let store = ProjectStore::open(&database).unwrap();
    let created = store
        .create_from_timeline(&workspace, &timeline_path)
        .unwrap();
    assert_eq!(created.revision, 1);

    let invalid_batch = [
        EditOperation::AudioGainSet {
            clip_id: "music-clip".to_owned(),
            gain: 0.8,
        },
        EditOperation::AudioGainSet {
            clip_id: "music-clip".to_owned(),
            gain: 5.0,
        },
    ];
    let invalid_error = store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "invalid-batch",
            &invalid_batch,
        )
        .unwrap_err();
    assert!(format!("{invalid_error:#}").contains("audio gain must be between 0 and 4"));
    assert!(store.load_revision_json(&created.project_id, 2).is_err());
    let revision_one: Value =
        serde_json::from_str(&store.load_revision_json(&created.project_id, 1).unwrap()).unwrap();
    assert_eq!(revision_one["tracks"][1]["clips"][0]["gain"], 0.2);

    let valid_edit = [EditOperation::AudioGainSet {
        clip_id: "music-clip".to_owned(),
        gain: 0.5,
    }];
    let first_result = store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "set-music-gain",
            &valid_edit,
        )
        .unwrap();
    assert_eq!(first_result.revision, 2);
    assert!(!first_result.replayed);

    let retry = store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "set-music-gain",
            &valid_edit,
        )
        .unwrap();
    assert_eq!(retry.revision, 2);
    assert!(retry.replayed);

    let changed_payload = [EditOperation::AudioGainSet {
        clip_id: "music-clip".to_owned(),
        gain: 0.75,
    }];
    let reused_key_error = store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "set-music-gain",
            &changed_payload,
        )
        .unwrap_err();
    assert!(format!("{reused_key_error:#}").contains("IDEMPOTENCY_CONFLICT"));

    let stale_error = store
        .apply_edit_batch(
            &workspace,
            &created.project_id,
            1,
            "stale-write",
            &changed_payload,
        )
        .unwrap_err();
    assert!(format!("{stale_error:#}").contains("REVISION_CONFLICT"));

    let barrier = Arc::new(Barrier::new(3));
    let writers = [("race-a", 0.6), ("race-b", 0.7)].map(|(key, gain)| {
        let writer_store = store.clone();
        let writer_workspace = workspace.clone();
        let writer_project_id = created.project_id.clone();
        let writer_barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            writer_barrier.wait();
            writer_store.apply_edit_batch(
                &writer_workspace,
                &writer_project_id,
                2,
                key,
                &[EditOperation::AudioGainSet {
                    clip_id: "music-clip".to_owned(),
                    gain,
                }],
            )
        })
    });
    barrier.wait();
    let race_results = writers.map(|writer| writer.join().unwrap());
    assert_eq!(
        race_results.iter().filter(|result| result.is_ok()).count(),
        1
    );
    assert_eq!(
        race_results.iter().filter(|result| result.is_err()).count(),
        1
    );
    let winner = race_results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .unwrap();
    assert_eq!(winner.revision, 3);
    let loser_error = race_results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert!(format!("{loser_error:#}").contains("REVISION_CONFLICT"));
    drop(store);

    let reopened = ProjectStore::open(&database).unwrap();
    assert_eq!(
        reopened
            .get_project_summary(&created.project_id)
            .unwrap()
            .current_revision,
        3
    );
    let revision_two: Value =
        serde_json::from_str(&reopened.load_revision_json(&created.project_id, 2).unwrap())
            .unwrap();
    assert_eq!(revision_two["tracks"][1]["clips"][0]["gain"], 0.5);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let immutable_error = connection
        .execute(
            "UPDATE revisions SET timeline_json = '{}' WHERE project_id = ?1 AND revision = 1",
            [&created.project_id],
        )
        .unwrap_err();
    assert!(
        immutable_error
            .to_string()
            .contains("project revisions are immutable")
    );

    let output = root.path().join("revision-2.mp4");
    let rendered = render_revision(RenderRevisionOptions {
        database,
        workspace: workspace.clone(),
        project_id: created.project_id,
        revision: 2,
        output: output.clone(),
        ffmpeg,
        ffprobe,
        font: None,
        timeout_seconds: 120,
    })
    .unwrap();
    assert!(output.is_file());
    assert_eq!(rendered.output_frame_count, 24);
    assert_eq!(rendered.video_codec, "h264");
    assert_eq!(rendered.audio_codec.as_deref(), Some("aac"));
    assert_eq!(fs::read_dir(workspace.join("tmp")).unwrap().count(), 0);
}

fn make_video(ffmpeg: &Path, output: &Path) {
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x240:r=24:d=1",
            "-an",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(output)
        .status()
        .unwrap();
    assert!(status.success(), "could not generate video fixture");
}

fn make_audio(ffmpeg: &Path, output: &Path) {
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-c:a",
            "pcm_s16le",
            "-y",
        ])
        .arg(output)
        .status()
        .unwrap();
    assert!(status.success(), "could not generate audio fixture");
}

fn asset_ref(asset: &studio::ImportedAsset) -> Value {
    json!({
        "kind": asset.kind,
        "uri": asset.uri,
        "sha256": asset.sha256
    })
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
