import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apiRequest, ApiError, downloadAsset, uploadMedia } from "./api";
import Icon from "./Icon";
import type {
  AudioClip,
  EditOperation,
  ImageClip,
  ImageMotion,
  ImportedAsset,
  ProjectRevisionHistory,
  ProjectSummary,
  RenderJob,
  RenderSubmission,
  SourceTimestamp,
  TextClip,
  TimelineClip,
  TimelineDocument,
  TimelineTrack,
  VideoClip,
} from "./types";

const TOKEN_KEY = "avida-api-token";
const MAX_UPLOAD_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_PROJECT_SECONDS = 300;
const MAX_CROSSFADE_SECONDS = 5;
const MAX_AUDIO_FADE_SECONDS = 5;
const TERMINAL_JOB_STATES = new Set<RenderJob["status"]>([
  "succeeded",
  "failed",
  "cancelled",
]);

type RenderPreset = "preview" | "export";

function readableError(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 401) return "The local API token was not accepted.";
    return error.message;
  }
  if (error instanceof Error) return error.message;
  return "Something went wrong. Please try again.";
}

function formatDuration(seconds: number): string {
  const safe = Math.max(0, Math.floor(seconds));
  return `${Math.floor(safe / 60)}:${String(safe % 60).padStart(2, "0")}`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (let index = 1; value >= 1024 && index < units.length; index += 1) {
    value /= 1024;
    unit = units[index];
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} ${unit}`;
}

function timestampSeconds(timestamp: SourceTimestamp): number {
  return (timestamp.ticks * timestamp.timebase_num) / timestamp.timebase_den;
}

function timestampFromSeconds(seconds: number): SourceTimestamp {
  return {
    ticks: Math.round(seconds * 1_000_000),
    timebase_num: 1,
    timebase_den: 1_000_000,
  };
}

function frameRate(timeline: TimelineDocument): number {
  return timeline.canvas.fps.num / timeline.canvas.fps.den;
}

function isVisualClip(clip: TimelineClip | null | undefined): clip is VideoClip | ImageClip {
  return clip?.kind === "video" || clip?.kind === "image";
}

function clipName(clip: TimelineClip, assets: ImportedAsset[]): string {
  if (clip.kind === "text") return clip.text || "Title";
  const asset = assets.find((item) => item.assetId === clip.assetId);
  return asset?.fileName ?? `${clip.kind} ${clip.assetId.slice(0, 8)}`;
}

function projectName(
  project: ProjectSummary,
  timeline: TimelineDocument | null,
  assets: ImportedAsset[],
): string {
  if (project.name) return project.name;
  if (timeline?.projectId === project.projectId) {
    const firstVisual = timeline.tracks
      .flatMap((track) => track.clips)
      .find(isVisualClip);
    if (firstVisual) {
      const name = assets.find((asset) => asset.assetId === firstVisual.assetId)?.fileName;
      if (name) return name.replace(/\.[^.]+$/, "");
    }
  }
  return `Project ${project.projectId.slice(0, 8)}`;
}

function clipStart(clip: TimelineClip): number {
  return clip.startFrame;
}

function clipDuration(clip: TimelineClip): number {
  return clip.durationFrames;
}

function trackTitle(track: TimelineTrack): string {
  if (track.kind === "video") return "V1 · VIDEO";
  if (track.kind === "overlay") return "T1 · TITLES";
  return track.id === "audio-2" ? "A2 · AUDIO" : "A1 · AUDIO";
}

function App() {
  const [token, setToken] = useState("");
  const [tokenDraft, setTokenDraft] = useState("");
  const [connection, setConnection] = useState<"disconnected" | "connecting" | "connected">("disconnected");
  const [connectionError, setConnectionError] = useState("");
  const [error, setError] = useState("");
  const [projects, setProjects] = useState<ProjectSummary[]>([]);
  const [assets, setAssets] = useState<ImportedAsset[]>([]);
  const [activeProjectId, setActiveProjectId] = useState("");
  const [timeline, setTimeline] = useState<TimelineDocument | null>(null);
  const [history, setHistory] = useState<ProjectRevisionHistory | null>(null);
  const [selectedClipId, setSelectedClipId] = useState("");
  const [selectedAssetId, setSelectedAssetId] = useState("");
  const [playhead, setPlayhead] = useState(0);
  const [undoStack, setUndoStack] = useState<number[]>([]);
  const [redoStack, setRedoStack] = useState<number[]>([]);
  const [busy, setBusy] = useState(false);
  const [importProgress, setImportProgress] = useState<{ fileName: string; percent: number } | null>(null);
  const [renderJob, setRenderJob] = useState<RenderJob | null>(null);
  const [renderedUrl, setRenderedUrl] = useState("");
  const [sourceUrl, setSourceUrl] = useState("");
  const [audioTrackId, setAudioTrackId] = useState("audio-1");
  const [sourceInDraft, setSourceInDraft] = useState("");
  const [sourceOutDraft, setSourceOutDraft] = useState("");
  const [durationFramesDraft, setDurationFramesDraft] = useState(1);
  const [transitionFramesDraft, setTransitionFramesDraft] = useState(24);
  const [imageMotionDraft, setImageMotionDraft] = useState<ImageMotion>("static");
  const [gainDraft, setGainDraft] = useState(1);
  const [audioFadeInDraft, setAudioFadeInDraft] = useState(0);
  const [audioFadeOutDraft, setAudioFadeOutDraft] = useState(0);
  const [moveFrameDraft, setMoveFrameDraft] = useState(0);
  const [titleDraft, setTitleDraft] = useState("");
  const [fontSizeDraft, setFontSizeDraft] = useState(48);
  const [colorDraft, setColorDraft] = useState("#FFFFFF");
  const [titleXDraft, setTitleXDraft] = useState(0);
  const [titleYDraft, setTitleYDraft] = useState(80);
  const importInput = useRef<HTMLInputElement>(null);
  const autoSelectFirstProject = useRef(true);

  const connect = useCallback(async (value: string, remember = true) => {
    const candidate = value.trim();
    if (!candidate) {
      setConnectionError("Paste the token shown in the API server terminal.");
      return;
    }
    setConnection("connecting");
    setConnectionError("");
    try {
      await apiRequest<{ status: string }>(candidate, "/v1/health");
      setToken(candidate);
      setConnection("connected");
      autoSelectFirstProject.current = true;
      if (remember) window.sessionStorage.setItem(TOKEN_KEY, candidate);
    } catch (connectError) {
      setConnection("disconnected");
      setConnectionError(readableError(connectError));
    }
  }, []);

  const loadLibrary = useCallback(async () => {
    if (!token) return;
    try {
      const [projectList, assetResponse] = await Promise.all([
        apiRequest<ProjectSummary[]>(token, "/v1/projects"),
        apiRequest<{ assets: ImportedAsset[] }>(token, "/v1/assets"),
      ]);
      setProjects(projectList);
      setAssets(assetResponse.assets);
      if (autoSelectFirstProject.current && projectList.length > 0) {
        setActiveProjectId(projectList[0].projectId);
        autoSelectFirstProject.current = false;
      }
    } catch (loadError) {
      setError(`Could not load the local library: ${readableError(loadError)}`);
    }
  }, [token]);

  const loadProject = useCallback(async (projectId: string, isCurrent?: () => boolean) => {
    if (!token || !projectId) return;
    const [nextTimeline, nextHistory, nextSummary] = await Promise.all([
      apiRequest<TimelineDocument>(token, `/v1/projects/${projectId}/timeline`),
      apiRequest<ProjectRevisionHistory>(token, `/v1/projects/${projectId}/revisions`),
      apiRequest<ProjectSummary>(token, `/v1/projects/${projectId}`),
    ]);
    if (isCurrent && !isCurrent()) return;
    setTimeline(nextTimeline);
    setHistory(nextHistory);
    setProjects((current) =>
      current.map((project) => (project.projectId === projectId ? nextSummary : project)),
    );
    setSelectedClipId((current) => {
      const exists = nextTimeline.tracks.some((track) => track.clips.some((clip) => clip.id === current));
      if (exists) return current;
      return nextTimeline.tracks.find((track) => track.kind === "video")?.clips[0]?.id ?? "";
    });
    setPlayhead(0);
  }, [token]);

  useEffect(() => {
    const storedToken = window.sessionStorage.getItem(TOKEN_KEY);
    if (storedToken) {
      setTokenDraft(storedToken);
      void connect(storedToken, false);
    }
  }, [connect]);

  useEffect(() => {
    if (!token) return;
    void loadLibrary();
  }, [token, loadLibrary]);

  useEffect(() => {
    if (!activeProjectId || !token) {
      setTimeline(null);
      setHistory(null);
      return;
    }
    let disposed = false;
    void loadProject(activeProjectId, () => !disposed).catch((loadError) => {
      if (!disposed) setError(`Could not open project: ${readableError(loadError)}`);
    });
    return () => {
      disposed = true;
    };
  }, [activeProjectId, loadProject, token]);

  useEffect(() => {
    const audioTrack = timeline?.tracks.find((track) => track.kind === "audio");
    if (audioTrack && !timeline?.tracks.some((track) => track.id === audioTrackId && track.kind === "audio")) {
      setAudioTrackId(audioTrack.id);
    }
  }, [audioTrackId, timeline]);

  const allClips = useMemo(
    () => timeline?.tracks.flatMap((track) => track.clips) ?? [],
    [timeline],
  );
  const selectedClip = allClips.find((clip) => clip.id === selectedClipId) ?? null;
  const videoTrack = timeline?.tracks.find((track) => track.kind === "video");
  const visualClips = videoTrack?.clips.filter(isVisualClip) ?? [];
  const firstVisualClip = visualClips[0];
  const selectedVisualIndex = isVisualClip(selectedClip)
    ? visualClips.findIndex((clip) => clip.id === selectedClip.id)
    : -1;
  const totalFrames = videoTrack?.clips.reduce((end, clip) => Math.max(end, clip.startFrame + clip.durationFrames), 0) ?? 0;
  const fps = timeline ? frameRate(timeline) : 30;
  const selectedProject = projects.find((project) => project.projectId === activeProjectId) ?? null;
  const activeClipAssetId = selectedClip && selectedClip.kind !== "text" ? selectedClip.assetId : "";
  const monitorAssetId = activeClipAssetId || selectedAssetId || firstVisualClip?.assetId || "";
  const monitorAsset = assets.find((asset) => asset.assetId === monitorAssetId);
  const renderIsCurrent = Boolean(
    timeline && renderJob?.status === "succeeded" && renderJob.revision === timeline.revision && renderedUrl,
  );
  const name = selectedProject ? projectName(selectedProject, timeline, assets) : "New project";

  useEffect(() => {
    if (!token || !monitorAssetId) {
      setSourceUrl("");
      return;
    }
    let disposed = false;
    let objectUrl = "";
    void downloadAsset(token, monitorAssetId)
      .then((blob) => {
        objectUrl = URL.createObjectURL(blob);
        if (disposed) URL.revokeObjectURL(objectUrl);
        else setSourceUrl(objectUrl);
      })
      .catch(() => {
        if (!disposed) setSourceUrl("");
      });
    return () => {
      disposed = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [monitorAssetId, token]);

  useEffect(() => {
    if (!token || !renderJob?.outputAssetId || renderJob.status !== "succeeded") {
      setRenderedUrl("");
      return;
    }
    let disposed = false;
    let objectUrl = "";
    void downloadAsset(token, renderJob.outputAssetId)
      .then((blob) => {
        objectUrl = URL.createObjectURL(blob);
        if (disposed) URL.revokeObjectURL(objectUrl);
        else setRenderedUrl(objectUrl);
      })
      .catch((renderError) => {
        if (!disposed) setError(`Could not load rendered preview: ${readableError(renderError)}`);
      });
    return () => {
      disposed = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [renderJob?.outputAssetId, renderJob?.status, token]);

  useEffect(() => {
    if (!token || !renderJob || TERMINAL_JOB_STATES.has(renderJob.status)) return;
    let stopped = false;
    let timeoutId: number | undefined;
    const poll = async () => {
      try {
        const latest = await apiRequest<RenderJob>(token, `/v1/jobs/${renderJob.jobId}`);
        if (stopped) return;
        setRenderJob(latest);
        if (!TERMINAL_JOB_STATES.has(latest.status)) timeoutId = window.setTimeout(poll, 1_500);
      } catch (pollError) {
        if (stopped) return;
        setError(`Unable to refresh render status: ${readableError(pollError)}`);
        timeoutId = window.setTimeout(poll, 4_000);
      }
    };
    timeoutId = window.setTimeout(poll, 900);
    return () => {
      stopped = true;
      if (timeoutId !== undefined) window.clearTimeout(timeoutId);
    };
  }, [renderJob?.jobId, renderJob?.status, token]);

  useEffect(() => {
    if (!selectedClip) return;
    setMoveFrameDraft(selectedClip.startFrame);
    setDurationFramesDraft(selectedClip.durationFrames);
    if (isVisualClip(selectedClip)) {
      setTransitionFramesDraft(selectedClip.transitionIn?.durationFrames ?? Math.max(1, Math.round(fps)));
    }
    if (selectedClip.kind === "image") {
      setImageMotionDraft(selectedClip.motion ?? "static");
    } else if (selectedClip.kind === "text") {
      setTitleDraft(selectedClip.text);
      setFontSizeDraft(selectedClip.fontSize);
      setColorDraft(selectedClip.color);
      setTitleXDraft(selectedClip.x);
      setTitleYDraft(selectedClip.y);
    } else {
      setSourceInDraft(timestampSeconds(selectedClip.sourceIn).toFixed(3));
      setSourceOutDraft(timestampSeconds(selectedClip.sourceOut).toFixed(3));
      if (selectedClip.kind === "audio") {
        setGainDraft(selectedClip.gain);
        setAudioFadeInDraft(selectedClip.fadeInFrames ?? 0);
        setAudioFadeOutDraft(selectedClip.fadeOutFrames ?? 0);
      }
    }
  }, [selectedClip?.id, timeline?.revision, fps]);

  const refreshLibrary = async () => {
    if (!token) return;
    const [projectList, assetResponse] = await Promise.all([
      apiRequest<ProjectSummary[]>(token, "/v1/projects"),
      apiRequest<{ assets: ImportedAsset[] }>(token, "/v1/assets"),
    ]);
    setProjects(projectList);
    setAssets(assetResponse.assets);
  };

  const changeProject = (projectId: string) => {
    autoSelectFirstProject.current = false;
    setError("");
    setTimeline(null);
    setHistory(null);
    setSelectedClipId("");
    setUndoStack([]);
    setRedoStack([]);
    setRenderJob(null);
    setActiveProjectId(projectId);
  };

  const importFiles = async (files: FileList | null) => {
    if (!token || !files?.length) return;
    setError("");
    setBusy(true);
    for (const file of Array.from(files)) {
      if (file.size <= 0 || file.size > MAX_UPLOAD_BYTES) {
        setError(`${file.name}: choose a non-empty file up to 2 GB.`);
        continue;
      }
      try {
        setImportProgress({ fileName: file.name, percent: 0 });
        await uploadMedia(token, file, (percent) => setImportProgress({ fileName: file.name, percent }));
      } catch (uploadError) {
        setError(`${file.name}: ${readableError(uploadError)}`);
      }
    }
    setImportProgress(null);
    try {
      await refreshLibrary();
      autoSelectFirstProject.current = false;
    } catch (loadError) {
      setError(readableError(loadError));
    }
    setBusy(false);
    if (importInput.current) importInput.current.value = "";
  };

  const createProjectFromAsset = async (asset: ImportedAsset) => {
    if (!token || busy) return;
    setBusy(true);
    setError("");
    try {
      const created = await apiRequest<{ projectId: string; revision: number }>(token, "/v1/projects", {
        method: "POST",
        body: JSON.stringify({ assetIds: [asset.assetId] }),
      });
      await refreshLibrary();
      setUndoStack([]);
      setRedoStack([]);
      setRenderJob(null);
      autoSelectFirstProject.current = false;
      setActiveProjectId(created.projectId);
    } catch (createError) {
      setError(readableError(createError));
    } finally {
      setBusy(false);
    }
  };

  const commitOperations = async (operations: EditOperation[], selectAfter?: string | null) => {
    if (!token || !timeline || busy || operations.length === 0) return;
    const previousRevision = timeline.revision;
    setBusy(true);
    setError("");
    try {
      await apiRequest(token, `/v1/projects/${timeline.projectId}/operations`, {
        method: "POST",
        body: JSON.stringify({
          expectedRevision: previousRevision,
          idempotencyKey: crypto.randomUUID(),
          operations,
        }),
      });
      setUndoStack((stack) => [...stack, previousRevision]);
      setRedoStack([]);
      setRenderJob(null);
      if (selectAfter !== undefined) setSelectedClipId(selectAfter ?? "");
      await loadProject(timeline.projectId);
    } catch (editError) {
      setError(readableError(editError));
      if (editError instanceof ApiError && editError.status === 409) {
        await loadProject(timeline.projectId).catch(() => undefined);
      }
    } finally {
      setBusy(false);
    }
  };

  const restoreRevision = async (targetRevision: number, mode: "undo" | "redo" | "history") => {
    if (!token || !timeline || busy || targetRevision < 1) return;
    const previousRevision = timeline.revision;
    setBusy(true);
    setError("");
    try {
      await apiRequest(token, `/v1/projects/${timeline.projectId}/restore`, {
        method: "POST",
        body: JSON.stringify({
          targetRevision,
          expectedRevision: previousRevision,
          idempotencyKey: crypto.randomUUID(),
        }),
      });
      if (mode === "undo") {
        setUndoStack((stack) => stack.slice(0, -1));
        setRedoStack((stack) => [...stack, previousRevision]);
      } else if (mode === "redo") {
        setRedoStack((stack) => stack.slice(0, -1));
        setUndoStack((stack) => [...stack, previousRevision]);
      } else {
        setUndoStack((stack) => [...stack, previousRevision]);
        setRedoStack([]);
      }
      setRenderJob(null);
      await loadProject(timeline.projectId);
    } catch (restoreError) {
      setError(readableError(restoreError));
      if (restoreError instanceof ApiError && restoreError.status === 409) {
        await loadProject(timeline.projectId).catch(() => undefined);
      }
    } finally {
      setBusy(false);
    }
  };

  const addVideoClip = async (asset: ImportedAsset) => {
    if (!timeline || !videoTrack) return;
    const remainingFrames = Math.floor(MAX_PROJECT_SECONDS * fps) - totalFrames;
    if (remainingFrames <= 0) {
      setError("This project has reached the five-minute timeline limit.");
      return;
    }
    const operation: EditOperation = asset.kind === "image"
      ? { type: "image.add", assetId: asset.assetId }
      : { type: "clip.append", assetId: asset.assetId };
    await commitOperations([operation]);
  };

  const addAudioClip = async (asset: ImportedAsset) => {
    if (!timeline || !asset.probe.hasAudio) return;
    const track = timeline.tracks.find((item) => item.id === audioTrackId && item.kind === "audio")
      ?? timeline.tracks.find((item) => item.kind === "audio");
    if (!track) {
      setError("This project has no available audio track.");
      return;
    }
    const startFrame = Math.min(playhead, Math.max(0, totalFrames - 1));
    const remainingFrames = totalFrames - startFrame;
    const sourceDuration = Math.min(
      asset.probe.audio?.durationSeconds ?? asset.probe.durationSeconds,
      MAX_PROJECT_SECONDS,
    );
    const durationFrames = Math.min(remainingFrames, Math.max(1, Math.round(sourceDuration * fps)));
    const clipSeconds = Math.min(sourceDuration, durationFrames / fps);
    const clip: AudioClip = {
      id: crypto.randomUUID(),
      kind: "audio",
      assetId: asset.assetId,
      startFrame,
      durationFrames,
      sourceIn: timestampFromSeconds(0),
      sourceOut: timestampFromSeconds(clipSeconds),
      gain: 1,
    };
    await commitOperations([{ type: "clip.add", trackId: track.id, clip }], clip.id);
  };

  const addTitle = async () => {
    if (!timeline || totalFrames <= 0) return;
    const track = timeline.tracks.find((item) => item.kind === "overlay");
    if (!track) {
      setError("This project has no title track.");
      return;
    }
    const startFrame = Math.min(playhead, Math.max(0, totalFrames - 1));
    const durationFrames = Math.min(Math.max(1, Math.round(fps * 3)), totalFrames - startFrame);
    const clip: TextClip = {
      id: crypto.randomUUID(),
      kind: "text",
      startFrame,
      durationFrames,
      text: "Your title",
      fontSize: 48,
      color: "#FFFFFF",
      x: 40,
      y: Math.round(timeline.canvas.height * 0.72),
    };
    await commitOperations([{ type: "clip.add", trackId: track.id, clip }], clip.id);
  };

  const splitSelected = async () => {
    if (!selectedClip || playhead <= selectedClip.startFrame || playhead >= selectedClip.startFrame + selectedClip.durationFrames) {
      setError("Move the playhead inside the selected clip before splitting.");
      return;
    }
    await commitOperations([{ type: "clip.split", clipId: selectedClip.id, splitFrame: playhead }]);
  };

  const applyTrim = async () => {
    if (!selectedClip || selectedClip.kind === "text" || selectedClip.kind === "image") return;
    const sourceInSeconds = Number(sourceInDraft);
    const sourceOutSeconds = Number(sourceOutDraft);
    if (!Number.isFinite(sourceInSeconds) || !Number.isFinite(sourceOutSeconds) || sourceInSeconds < 0 || sourceOutSeconds <= sourceInSeconds) {
      setError("Enter a valid source range where source out is after source in.");
      return;
    }
    const durationFrames = Math.max(1, Math.round((sourceOutSeconds - sourceInSeconds) * fps));
    await commitOperations([{
      type: "clip.trim",
      clipId: selectedClip.id,
      startFrame: selectedClip.startFrame,
      durationFrames,
      sourceIn: timestampFromSeconds(sourceInSeconds),
      sourceOut: timestampFromSeconds(sourceOutSeconds),
    }]);
  };

  const applyImageDuration = async () => {
    if (selectedClip?.kind !== "image") return;
    if (!Number.isSafeInteger(durationFramesDraft) || durationFramesDraft <= 0) {
      setError("Image duration must be a positive whole number of frames.");
      return;
    }
    await commitOperations([{
      type: "clip.trim",
      clipId: selectedClip.id,
      startFrame: selectedClip.startFrame,
      durationFrames: durationFramesDraft,
    }]);
  };

  const applyImageMotion = async () => {
    if (selectedClip?.kind !== "image") return;
    await commitOperations([{
      type: "image.motion.set",
      clipId: selectedClip.id,
      motion: imageMotionDraft,
    }]);
  };

  const applyCrossfade = async () => {
    if (!isVisualClip(selectedClip)) return;
    if (selectedVisualIndex <= 0) {
      setError("The first visual clip cannot have an incoming crossfade.");
      return;
    }
    if (
      !Number.isSafeInteger(transitionFramesDraft)
      || transitionFramesDraft <= 0
      || transitionFramesDraft > Math.floor(MAX_CROSSFADE_SECONDS * fps)
    ) {
      setError(`Crossfade duration must be 1–${Math.floor(MAX_CROSSFADE_SECONDS * fps)} frames (up to five seconds).`);
      return;
    }
    await commitOperations([{
      type: "clip.transition.set",
      clipId: selectedClip.id,
      durationFrames: transitionFramesDraft,
    }]);
  };

  const removeCrossfade = async () => {
    if (!isVisualClip(selectedClip) || !selectedClip.transitionIn) return;
    await commitOperations([{ type: "clip.transition.remove", clipId: selectedClip.id }]);
  };

  const applyAudioFades = async () => {
    if (selectedClip?.kind !== "audio") return;
    const maxFadeFrames = Math.floor(MAX_AUDIO_FADE_SECONDS * fps);
    if (
      !Number.isSafeInteger(audioFadeInDraft)
      || !Number.isSafeInteger(audioFadeOutDraft)
      || audioFadeInDraft < 0
      || audioFadeOutDraft < 0
      || audioFadeInDraft > maxFadeFrames
      || audioFadeOutDraft > maxFadeFrames
      || audioFadeInDraft + audioFadeOutDraft > selectedClip.durationFrames
    ) {
      setError(`Each audio fade must be 0–${maxFadeFrames} frames, and together they cannot exceed the clip duration.`);
      return;
    }
    await commitOperations([{
      type: "audio.fade.set",
      clipId: selectedClip.id,
      fadeInFrames: audioFadeInDraft,
      fadeOutFrames: audioFadeOutDraft,
    }]);
  };

  const applyClipPosition = async () => {
    if (!selectedClip || isVisualClip(selectedClip)) return;
    await commitOperations([{ type: "clip.move", clipId: selectedClip.id, startFrame: Math.max(0, moveFrameDraft) }]);
  };

  const applyClipProperties = async () => {
    if (!selectedClip) return;
    if (selectedClip.kind === "audio") {
      await commitOperations([{ type: "audio.gain.set", clipId: selectedClip.id, gain: gainDraft }]);
    } else if (selectedClip.kind === "text") {
      await commitOperations([{
        type: "text.update",
        clipId: selectedClip.id,
        text: titleDraft,
        fontSize: fontSizeDraft,
        color: colorDraft,
        x: titleXDraft,
        y: titleYDraft,
      }]);
    }
  };

  const render = async (preset: RenderPreset) => {
    if (!token || !timeline || busy || (renderJob && !TERMINAL_JOB_STATES.has(renderJob.status))) return;
    setBusy(true);
    setError("");
    try {
      const response = await apiRequest<RenderSubmission>(token, `/v1/projects/${timeline.projectId}/renders`, {
        method: "POST",
        body: JSON.stringify({
          revision: timeline.revision,
          presetId: preset,
          idempotencyKey: crypto.randomUUID(),
        }),
      });
      setRenderJob(response.job);
    } catch (renderError) {
      setError(readableError(renderError));
    } finally {
      setBusy(false);
    }
  };

  const cancelRender = async () => {
    if (!token || !renderJob) return;
    try {
      setRenderJob(await apiRequest<RenderJob>(token, `/v1/jobs/${renderJob.jobId}/cancel`, { method: "POST" }));
    } catch (cancelError) {
      setError(readableError(cancelError));
    }
  };

  const retryRender = async () => {
    if (!token || !renderJob || busy) return;
    setBusy(true);
    try {
      const response = await apiRequest<RenderSubmission>(token, `/v1/jobs/${renderJob.jobId}/retry`, {
        method: "POST",
        body: JSON.stringify({ idempotencyKey: crypto.randomUUID() }),
      });
      setRenderJob(response.job);
    } catch (retryError) {
      setError(readableError(retryError));
    } finally {
      setBusy(false);
    }
  };

  const downloadRender = async () => {
    if (!token || !renderJob?.outputAssetId) return;
    try {
      const blob = await downloadAsset(token, renderJob.outputAssetId);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `avida-${renderJob.presetId}-${timeline?.projectId.slice(0, 8) ?? "render"}.mp4`;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
    } catch (downloadError) {
      setError(readableError(downloadError));
    }
  };

  const disconnect = () => {
    window.sessionStorage.removeItem(TOKEN_KEY);
    setToken("");
    setTokenDraft("");
    setConnection("disconnected");
    setProjects([]);
    setAssets([]);
    setTimeline(null);
    setActiveProjectId("");
    setHistory(null);
    setRenderJob(null);
  };

  const handleDropOnVideo = (event: React.DragEvent<HTMLButtonElement>, destinationIndex: number) => {
    event.preventDefault();
    const clipId = event.dataTransfer.getData("text/avida-clip");
    if (!clipId || !timeline) return;
    void commitOperations([{ type: "clip.reorder", clipId, destinationIndex }]);
  };

  const monitorUrl = renderIsCurrent ? renderedUrl : sourceUrl;
  const monitorLabel = renderIsCurrent ? `RENDERED REVISION ${timeline?.revision}` : "SOURCE PREVIEW · COMPOSITE APPROXIMATE";
  const monitorIsImage = !renderIsCurrent && monitorAsset?.kind === "image";

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand-lockup"><div className="brand-mark"><span /></div><span>avida</span><span className="brand-edition">STUDIO</span></div>
        <div className="sidebar-workspace"><span className="workspace-avatar">A</span><div><strong>Personal studio</strong><span>Local workspace</span></div><Icon name="chevron-down" size={15} /></div>
        <nav className="side-nav" aria-label="Workspace navigation">
          <div className="nav-label">WORKSPACE</div>
          <button className="nav-item active" type="button" onClick={() => changeProject("")} disabled={busy}><Icon name="plus" size={17} /><span>New project</span><span className="nav-item-shortcut">N</span></button>
          {selectedProject && <div className="nav-project"><span className="nav-project-thumb"><Icon name="film" size={15} /></span><span className="nav-project-copy"><strong>{name}</strong><small>Revision {timeline?.revision ?? selectedProject.currentRevision}</small></span><span className="nav-project-dot" /></div>}
        </nav>
        <div className="sidebar-bottom">
          <div className="privacy-card"><div className="privacy-icon"><Icon name="lock" size={15} /></div><div><strong>Your media stays here</strong><span>Files never leave this machine.</span></div></div>
          <div className="sidebar-version"><span className="online-dot" /> LOCAL MODE <span>v0.2</span></div>
        </div>
      </aside>

      <div className="app-main">
        <header className="topbar">
          <div className="breadcrumbs"><span>Workspace</span><b>/</b><strong>{selectedProject ? name : "New project"}</strong></div>
          <div className="topbar-actions">
            <div className={`connection-pill ${connection}`}><span className="connection-dot" />{connection === "connected" ? "API connected" : connection === "connecting" ? "Connecting" : "API offline"}</div>
            {connection === "connected" && <button className="quiet-button" type="button" onClick={disconnect}>Disconnect</button>}
            <div className="user-avatar">A</div>
          </div>
        </header>

        {!token ? (
          <main className="page-content">
            <div className="page-heading"><div><div className="eyebrow"><span className="eyebrow-line" /> YOUR LOCAL VIDEO STUDIO</div><h1>Your edit suite.<br />Your machine.</h1><p>Connect to the local API to open your media library and edit without sending files anywhere.</p></div><div className="heading-badge"><Icon name="spark" size={16} /> Made for your machine</div></div>
            <section className="connect-card">
              <div className="connect-illustration"><div className="connect-orbit orbit-one" /><div className="connect-orbit orbit-two" /><div className="connect-center"><div className="brand-mark large"><span /></div></div><div className="connect-node node-top"><Icon name="lock" size={16} /></div><div className="connect-node node-side"><Icon name="activity" size={16} /></div></div>
              <div className="connect-content"><div className="section-kicker">CONNECT YOUR STUDIO</div><h2>Your edit suite.<br />Your machine.</h2><p>Start the Avida API in a terminal, then paste its local access token. Your media and renders stay on this computer.</p><label className="field-label" htmlFor="api-token">LOCAL API TOKEN</label><div className="token-field"><Icon name="lock" size={16} /><input id="api-token" value={tokenDraft} onChange={(event) => setTokenDraft(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") void connect(tokenDraft); }} placeholder="Paste token from your API terminal" type="password" autoComplete="off" spellCheck={false} /><button className="button button-primary button-compact" disabled={connection === "connecting"} onClick={() => void connect(tokenDraft)} type="button">{connection === "connecting" ? <Icon name="loader" className="spin" size={15} /> : "Connect"}<Icon name="arrow-up-right" size={15} /></button></div>{connectionError && <div className="inline-error"><Icon name="info" size={15} />{connectionError}</div>}<div className="connect-footnote"><Icon name="lock" size={13} /> Token is kept only for this browser session.</div></div>
            </section>
          </main>
        ) : (
          <main className="editor-page">
            <div className="editor-heading">
                  <div><div className="eyebrow"><span className="eyebrow-line" /> LOCAL VIDEO STUDIO</div><h1>{selectedProject ? name : "Build a new cut"}</h1><p>{timeline ? "Edit a cut, add crossfades, balance audio, add a title, then render a revision." : "Import video, still images, and audio into your local media bin, then start a project from a visual asset."}</p></div>
              {timeline && <div className="editor-revision-actions"><button className="button button-subtle" type="button" onClick={() => void restoreRevision(undoStack[undoStack.length - 1], "undo")} disabled={busy || undoStack.length === 0} title="Undo last edit"><Icon name="refresh" size={15} /> Undo</button><button className="button button-subtle" type="button" onClick={() => void restoreRevision(redoStack[redoStack.length - 1], "redo")} disabled={busy || redoStack.length === 0} title="Redo last edit"><Icon name="refresh" size={15} /> Redo</button><span className="revision-label">REV {timeline.revision}</span></div>}
            </div>

            {error && <div className="error-banner" role="alert"><Icon name="info" size={17} /><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError("")}><Icon name="x" size={15} /></button></div>}

            <div className="editor-layout">
              <aside className="editor-rail">
                <section className="editor-card media-card">
                  <div className="editor-card-heading"><div><div className="section-kicker">MEDIA BIN</div><h2>Media library</h2></div><button className="icon-button" type="button" onClick={() => importInput.current?.click()} disabled={busy} aria-label="Import media"><Icon name="plus" size={17} /></button></div>
                  <input ref={importInput} className="visually-hidden" type="file" multiple accept="video/*,audio/*,image/png,image/jpeg,image/webp,image/bmp,image/tiff,.mp4,.mov,.m4v,.mkv,.mp3,.wav,.aac,.m4a,.png,.jpg,.jpeg,.webp,.bmp,.tif,.tiff" onChange={(event) => void importFiles(event.target.files)} />
                  <button className="media-import-button" type="button" onClick={() => importInput.current?.click()} disabled={busy}><Icon name="upload" size={15} /> Import media</button>
                  {timeline && <label className="media-track-select">Add audio to<select value={audioTrackId} onChange={(event) => setAudioTrackId(event.target.value)}>{timeline.tracks.filter((track) => track.kind === "audio").map((track) => <option key={track.id} value={track.id}>{trackTitle(track)}</option>)}</select></label>}
                  {importProgress && <div className="import-progress-copy"><span>{importProgress.fileName}</span><span>{importProgress.percent}%</span><div className="upload-progress"><div style={{ width: `${importProgress.percent}%` }} /></div></div>}
                  <div className="media-list">
                    {assets.length === 0 ? <div className="empty-library">Your imported media will appear here.</div> : assets.map((asset) => (
                      <div className={`media-item ${selectedAssetId === asset.assetId ? "selected" : ""}`} key={asset.assetId}>
                        <button type="button" className="media-select" onClick={() => setSelectedAssetId(asset.assetId)}><span className={`media-type-icon ${asset.kind}`}><Icon name={asset.kind === "audio" ? "activity" : "film"} size={16} /></span><span className="media-copy"><strong title={asset.fileName}>{asset.fileName ?? `${asset.kind} ${asset.assetId.slice(0, 8)}`}</strong><small>{asset.kind === "image" ? `${asset.probe.video?.width ?? "?"} × ${asset.probe.video?.height ?? "?"} · ${formatBytes(asset.bytes)}` : `${formatDuration(asset.probe.durationSeconds)} · ${formatBytes(asset.bytes)}`}</small></span></button>
                        {timeline && <div className="media-item-actions">{(asset.kind === "video" || asset.kind === "image") && <button type="button" title={asset.kind === "image" ? "Add image to end of V1 (5 seconds)" : "Add video to end of V1"} onClick={() => void addVideoClip(asset)} disabled={busy || totalFrames >= Math.floor(MAX_PROJECT_SECONDS * fps)}><Icon name="plus" size={14} /> V1</button>}{asset.probe.hasAudio && <button type="button" title="Add audio at the playhead" onClick={() => void addAudioClip(asset)} disabled={busy || totalFrames === 0}><Icon name="plus" size={14} /> A</button>}</div>}
                        {!timeline && asset.kind !== "audio" && <button className="media-create-project" type="button" onClick={() => void createProjectFromAsset(asset)} disabled={busy}>Create project</button>}
                      </div>
                    ))}
                  </div>
                </section>

                <section className="editor-card project-list-card">
                  <div className="editor-card-heading"><div><div className="section-kicker">PROJECTS</div><h2>Recent edits</h2></div><button className="icon-button" type="button" onClick={() => changeProject("")} aria-label="Start a new project"><Icon name="plus" size={17} /></button></div>
                  {projects.length === 0 ? <div className="empty-library">Projects you create are saved locally.</div> : projects.map((project) => <button className={`project-list-item ${project.projectId === activeProjectId ? "selected" : ""}`} key={project.projectId} type="button" onClick={() => changeProject(project.projectId)}><span className="nav-project-thumb"><Icon name="film" size={15} /></span><span><strong>{projectName(project, timeline, assets)}</strong><small>Revision {project.currentRevision}</small></span></button>)}
                </section>

                {history && timeline && <section className="editor-card revisions-card"><div className="editor-card-heading"><div><div className="section-kicker">HISTORY</div><h2>Revisions</h2></div></div><div className="revision-list">{history.revisions.slice(0, 8).map((revision) => <div className="revision-item" key={revision.revision}><span className={revision.isCurrent ? "revision-current-dot" : "revision-dot"} /><span>Revision {revision.revision}{revision.isCurrent ? " · current" : ""}</span>{!revision.isCurrent && <button type="button" onClick={() => void restoreRevision(revision.revision, "history")} disabled={busy}>Restore</button>}</div>)}</div></section>}
              </aside>

              <section className="editor-center">
                {!timeline ? <div className="editor-empty-state"><div className="empty-state-mark"><Icon name="film" size={24} /></div><div className="section-kicker">READY WHEN YOU ARE</div><h2>Start with a visual</h2><p>Import videos, still images, or audio. Choose a video or image in the media bin to create a project; images hold for five seconds by default.</p><button className="button button-primary" type="button" onClick={() => importInput.current?.click()} disabled={busy}><Icon name="upload" size={16} /> Import media</button></div> : <>
                  <section className="editor-card monitor-card">
                    <div className="editor-card-heading"><div><div className="section-kicker">PREVIEW MONITOR</div><h2>{renderIsCurrent ? "Rendered cut" : selectedClip ? clipName(selectedClip, assets) : "Source media"}</h2></div><span className={`monitor-badge ${renderIsCurrent ? "rendered" : "approximate"}`}><span />{monitorLabel}</span></div>
                  <div className="monitor-frame">{monitorUrl ? monitorIsImage ? <img key={monitorUrl} src={monitorUrl} alt={monitorAsset?.fileName ?? "Selected still image"} /> : <video key={monitorUrl} src={monitorUrl} controls playsInline preload="metadata" /> : <div className="monitor-placeholder"><Icon name="video" size={30} /><span>Select media to preview</span></div>}</div>
                    <div className="monitor-caption"><Icon name="info" size={14} />{renderIsCurrent ? `Rendered revision ${timeline.revision} · ${renderJob?.presetId} preset` : "Source playback is approximate; use Render preview to inspect the composited timeline."}</div>
                  </section>

                  <section className="editor-card timeline-editor-card">
                    <div className="editor-card-heading timeline-heading"><div><div className="section-kicker">EDITABLE TIMELINE</div><h2>Cut & arrange</h2></div><div className="timeline-tools"><button type="button" className="button button-subtle button-small" onClick={() => void splitSelected()} disabled={busy || !selectedClip}><span className="split-symbol">╫</span> Split</button><button type="button" className="button button-subtle button-small" onClick={() => void addTitle()} disabled={busy || totalFrames <= 0}><Icon name="plus" size={14} /> Title</button><button type="button" className="button button-subtle button-small danger-action" onClick={() => selectedClip && void commitOperations([{ type: "clip.remove", clipId: selectedClip.id }], null)} disabled={busy || !selectedClip || (isVisualClip(selectedClip) && (videoTrack?.clips.length ?? 0) <= 1)}><Icon name="x" size={14} /> Remove</button></div></div>
                    <div className="playhead-control"><span>PLAYHEAD</span><input type="range" min={0} max={Math.max(0, totalFrames)} value={Math.min(playhead, totalFrames)} onChange={(event) => setPlayhead(Number(event.target.value))} disabled={totalFrames === 0} /><output>{formatDuration(playhead / fps)} <small>F{playhead}</small></output></div>
                    <div className="timeline-scroll"><div className="timeline-canvas" style={{ minWidth: `${Math.max(620, totalFrames * 2.2)}px` }}>
                      <div className="timeline-ruler"><span className="track-label-spacer" />{Array.from({ length: 6 }, (_, index) => <span key={index}>{formatDuration((totalFrames / fps) * index / 5)}</span>)}</div>
                      {timeline.tracks.filter((track) => track.kind !== "overlay" || track.clips.length > 0).map((track) => <div className={`editor-track-row track-${track.kind}`} key={track.id}>
                        <div className="editor-track-label"><span>{track.kind === "video" ? <Icon name="video" size={15} /> : track.kind === "audio" ? <Icon name="activity" size={15} /> : <Icon name="file" size={15} />}</span><strong>{trackTitle(track)}</strong></div>
                        <div className="editor-track-lane" onClick={(event) => { const rect = event.currentTarget.getBoundingClientRect(); const ratio = Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width)); setPlayhead(Math.round(ratio * totalFrames)); }}>
                          {track.clips.map((clip, index) => {
                            const left = totalFrames ? (clipStart(clip) / totalFrames) * 100 : 0;
                            const width = totalFrames ? Math.max(1.3, (clipDuration(clip) / totalFrames) * 100) : 0;
                            const title = clipName(clip, assets);
                           return <button key={clip.id} type="button" draggable={isVisualClip(clip)} onDragStart={(event) => { if (isVisualClip(clip)) { event.dataTransfer.setData("text/avida-clip", clip.id); event.dataTransfer.effectAllowed = "move"; } }} onDragOver={(event) => { if (isVisualClip(clip)) event.preventDefault(); }} onDrop={(event) => { if (isVisualClip(clip)) handleDropOnVideo(event, index); }} onClick={(event) => { event.stopPropagation(); setSelectedClipId(clip.id); setSelectedAssetId(clip.kind === "text" ? "" : clip.assetId); setPlayhead(clip.startFrame); }} className={`timeline-clip clip-${clip.kind} ${isVisualClip(clip) && clip.transitionIn ? "has-incoming-transition" : ""} ${selectedClipId === clip.id ? "selected" : ""}`} style={{ left: `${left}%`, width: `${width}%` }} title={`${title} · ${clip.durationFrames} frames${isVisualClip(clip) && clip.transitionIn ? ` · ${clip.transitionIn.durationFrames}f crossfade` : ""}`}><span>{title}</span>{isVisualClip(clip) && clip.transitionIn && <small className="transition-mark">↔</small>}{clip.kind === "audio" && <small>{Math.round(clip.gain * 100)}%</small>}</button>;
                          })}
                          <span className="timeline-playhead" style={{ left: `${totalFrames ? (playhead / totalFrames) * 100 : 0}%` }} />
                        </div>
                      </div>)}
                    </div></div>
                    <div className="timeline-footer"><span><Icon name="clock" size={14} /> {formatDuration(totalFrames / fps)}</span><span>{timeline.canvas.width} × {timeline.canvas.height}</span><span>{timeline.canvas.fps.num}/{timeline.canvas.fps.den} FPS</span><span>{allClips.length} clips</span></div>
                  </section>
                </>}
              </section>

              <aside className="editor-inspector">
                {timeline && <>
                  <section className="editor-card inspector-card">
                    <div className="editor-card-heading"><div><div className="section-kicker">INSPECTOR</div><h2>{selectedClip ? "Clip properties" : "Project"}</h2></div></div>
                    {selectedClip ? <>
                      <div className="inspector-clip-name"><span className={`media-type-icon ${selectedClip.kind}`}>{selectedClip.kind === "audio" ? <Icon name="activity" size={15} /> : selectedClip.kind === "text" ? <Icon name="file" size={15} /> : <Icon name="film" size={15} />}</span><span>{clipName(selectedClip, assets)}</span></div>
                      <div className="inspector-field-row"><label>Start frame</label>{isVisualClip(selectedClip) ? <output>{selectedClip.startFrame}</output> : <div className="inline-number"><input type="number" min={0} max={Math.max(0, totalFrames - selectedClip.durationFrames)} value={moveFrameDraft} onChange={(event) => setMoveFrameDraft(Number(event.target.value))} /><button type="button" onClick={() => void applyClipPosition()} disabled={busy}>Move</button></div>}</div>
                      <div className="inspector-field-row"><label>Duration</label><output>{(selectedClip.durationFrames / fps).toFixed(2)}s · {selectedClip.durationFrames}f</output></div>
                      {isVisualClip(selectedClip) && <>
                        <div className="inspector-section-label">INCOMING TRANSITION</div>
                        {selectedVisualIndex <= 0 ? <p className="inspector-help">The first visual clip cannot have an incoming crossfade.</p> : <>
                          <div className="inspector-field-row"><label htmlFor="crossfade-frames">Crossfade (frames)</label><input id="crossfade-frames" type="number" min={1} max={Math.floor(MAX_CROSSFADE_SECONDS * fps)} step={1} value={transitionFramesDraft} onChange={(event) => setTransitionFramesDraft(Number(event.target.value))} /></div>
                          <p className="inspector-help">Up to five seconds; the duration must fit both adjacent clips. The overlap shortens the total cut.</p>
                          <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyCrossfade()} disabled={busy}>Apply crossfade</button>
                          {selectedClip.transitionIn && <button className="button button-subtle inspector-apply" type="button" onClick={() => void removeCrossfade()} disabled={busy}>Remove crossfade</button>}
                        </>}
                      </>}
                      {selectedClip.kind === "image" && <>
                        <div className="inspector-section-label">IMAGE MOTION</div>
                        <label className="inspector-label" htmlFor="image-motion">Movement</label>
                        <select id="image-motion" value={imageMotionDraft} onChange={(event) => setImageMotionDraft(event.target.value as ImageMotion)}>
                          <option value="static">Static</option>
                          <option value="zoom_in">Gentle zoom in</option>
                          <option value="zoom_out">Gentle zoom out</option>
                          <option value="pan_left">Pan left</option>
                          <option value="pan_right">Pan right</option>
                          <option value="pan_up">Pan up</option>
                          <option value="pan_down">Pan down</option>
                        </select>
                        <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyImageMotion()} disabled={busy}>Apply motion</button>
                        <div className="inspector-section-label">IMAGE HOLD</div>
                        <div className="inspector-field-row"><label htmlFor="image-duration-frames">Duration (frames)</label><input id="image-duration-frames" type="number" min={1} max={Math.floor(MAX_PROJECT_SECONDS * fps)} step={1} value={durationFramesDraft} onChange={(event) => setDurationFramesDraft(Number(event.target.value))} /></div>
                        <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyImageDuration()} disabled={busy}>Apply duration</button>
                      </>}
                      {selectedClip.kind !== "text" && selectedClip.kind !== "image" && <>
                        <div className="inspector-section-label">SOURCE TRIM</div>
                        <div className="inspector-field-row"><label htmlFor="source-in">In (seconds)</label><input id="source-in" type="number" min={0} step={0.001} value={sourceInDraft} onChange={(event) => setSourceInDraft(event.target.value)} /></div>
                        <div className="inspector-field-row"><label htmlFor="source-out">Out (seconds)</label><input id="source-out" type="number" min={0} step={0.001} value={sourceOutDraft} onChange={(event) => setSourceOutDraft(event.target.value)} /></div>
                        <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyTrim()} disabled={busy}>Apply trim</button>
                      </>}
                      {selectedClip.kind === "audio" && <>
                        <div className="inspector-section-label">AUDIO</div>
                        <div className="gain-line"><label htmlFor="clip-gain">Gain</label><output>{gainDraft.toFixed(2)}×</output></div><input className="gain-range" id="clip-gain" type="range" min={0} max={4} step={0.05} value={gainDraft} onChange={(event) => setGainDraft(Number(event.target.value))} /><button className="button button-subtle inspector-apply" type="button" onClick={() => void applyClipProperties()} disabled={busy}>Apply gain</button>
                        <div className="inspector-section-label">LINEAR FADES</div>
                        <div className="inspector-field-row"><label htmlFor="audio-fade-in">Fade in (frames)</label><input id="audio-fade-in" type="number" min={0} max={Math.floor(MAX_AUDIO_FADE_SECONDS * fps)} step={1} value={audioFadeInDraft} onChange={(event) => setAudioFadeInDraft(Number(event.target.value))} /></div>
                        <div className="inspector-field-row"><label htmlFor="audio-fade-out">Fade out (frames)</label><input id="audio-fade-out" type="number" min={0} max={Math.floor(MAX_AUDIO_FADE_SECONDS * fps)} step={1} value={audioFadeOutDraft} onChange={(event) => setAudioFadeOutDraft(Number(event.target.value))} /></div>
                        <p className="inspector-help">Each fade is up to five seconds. Fade durations cannot overlap or exceed the clip length.</p>
                        <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyAudioFades()} disabled={busy}>Apply fades</button>
                      </>}
                      {selectedClip.kind === "text" && <>
                        <div className="inspector-section-label">TITLE</div>
                        <label className="inspector-label" htmlFor="title-text">Text</label><textarea id="title-text" rows={3} maxLength={4096} value={titleDraft} onChange={(event) => setTitleDraft(event.target.value)} />
                        <div className="inspector-field-row"><label htmlFor="title-size">Font size</label><input id="title-size" type="number" min={1} max={256} value={fontSizeDraft} onChange={(event) => setFontSizeDraft(Number(event.target.value))} /></div>
                        <div className="inspector-field-row"><label htmlFor="title-color">Color</label><input id="title-color" type="color" value={colorDraft} onChange={(event) => setColorDraft(event.target.value.toUpperCase())} /></div>
                        <div className="inspector-two-fields"><label>X<input type="number" min={0} max={timeline.canvas.width - 1} value={titleXDraft} onChange={(event) => setTitleXDraft(Number(event.target.value))} /></label><label>Y<input type="number" min={0} max={timeline.canvas.height - 1} value={titleYDraft} onChange={(event) => setTitleYDraft(Number(event.target.value))} /></label></div>
                        <button className="button button-subtle inspector-apply" type="button" onClick={() => void applyClipProperties()} disabled={busy}>Apply title</button>
                      </>}
                    </> : <div className="project-inspector-summary"><strong>{name}</strong><span>{totalFrames} frames · {timeline.canvas.fps.num}/{timeline.canvas.fps.den} fps</span><span>{timeline.canvas.width} × {timeline.canvas.height}</span><span>Revision {timeline.revision} is immutable; edits create a new revision.</span></div>}
                  </section>

                  <section className="editor-card render-editor-card">
                    <div className="editor-card-heading"><div><div className="section-kicker">EXPORT</div><h2>Render this revision</h2></div><span className="render-heading-icon"><Icon name="play" size={16} /></span></div>
                    <p>Preview and export run through the local render worker.</p>
                    <button className="render-choice" type="button" onClick={() => void render("preview")} disabled={busy || Boolean(renderJob && !TERMINAL_JOB_STATES.has(renderJob.status))}><span className="render-choice-icon preview-icon"><Icon name="play" size={15} /></span><span className="render-choice-copy"><strong>Render preview</strong><small>640 px · revision {timeline.revision}</small></span><Icon name="arrow-up-right" size={14} /></button>
                    <button className="render-choice" type="button" onClick={() => void render("export")} disabled={busy || Boolean(renderJob && !TERMINAL_JOB_STATES.has(renderJob.status))}><span className="render-choice-icon export-icon"><Icon name="arrow-down" size={15} /></span><span className="render-choice-copy"><strong>Final export</strong><small>{timeline.canvas.width} × {timeline.canvas.height} · H.264</small></span><Icon name="arrow-up-right" size={14} /></button>
                     {renderJob && <>
                       <div className={`render-state render-${renderJob.status}`}><span><strong>{renderJob.status === "succeeded" ? "Render ready" : renderJob.status === "failed" ? "Render failed" : renderJob.status === "cancelled" ? "Render cancelled" : `${renderJob.presetId} ${renderJob.status}`}</strong><small>{renderJob.error ?? `Revision ${renderJob.revision} · ${renderJob.phase.replaceAll("_", " ")}${renderJob.status === "running" ? ` · ${renderJob.progressPercent}%` : ""}`}</small></span><div>{renderJob.status === "succeeded" && <button type="button" onClick={() => void downloadRender()}>Download</button>}{(renderJob.status === "queued" || renderJob.status === "running") && <button type="button" onClick={() => void cancelRender()}>Cancel</button>}{renderJob.status === "failed" && <button type="button" onClick={() => void retryRender()} disabled={busy}>Retry</button>}</div></div>
                       {(renderJob.status === "queued" || renderJob.status === "running") && <div className="render-progress" role="progressbar" aria-label={`Render ${renderJob.phase}`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={renderJob.progressPercent}><div style={{ width: `${renderJob.progressPercent}%` }} /></div>}
                     </>}
                    <div className="render-security"><Icon name="lock" size={13} /><span>Private render · stored in your workspace</span></div>
                  </section>
                </>}
              </aside>
            </div>
            <footer className="page-footer"><span>AVIDA LOCAL STUDIO</span><span><Icon name="lock" size={12} /> PRIVATE BY DESIGN</span><span>ALL PROCESSING STAYS ON THIS DEVICE</span></footer>
          </main>
        )}
      </div>
    </div>
  );
}

export default App;
