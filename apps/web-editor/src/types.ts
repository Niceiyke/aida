export interface StreamInfo {
  codec: string;
  durationSeconds?: number;
  frameRate?: string;
  width?: number;
  height?: number;
  sampleRate?: number;
  channels?: number;
}

export interface MediaProbe {
  formatName: string;
  duration: string;
  durationSeconds: number;
  hasVideo: boolean;
  hasAudio: boolean;
  hasStillImage: boolean;
  video?: StreamInfo;
  audio?: StreamInfo;
}

export interface ImportedAsset {
  assetId: string;
  uri?: string;
  sha256: string;
  kind: "video" | "audio" | "image";
  bytes: number;
  fileName?: string;
  mimeType?: string;
  contentUrl?: string;
  probe: MediaProbe;
}

export interface ProjectSummary {
  projectId: string;
  currentRevision: number;
  createdAt: string;
  updatedAt: string;
  name?: string;
}

export interface SourceTimestamp {
  ticks: number;
  timebase_num: number;
  timebase_den: number;
}

export interface CropRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface VideoTransition {
  type: "crossfade";
  durationFrames: number;
}

export interface VideoClip {
  id: string;
  kind: "video";
  assetId: string;
  startFrame: number;
  durationFrames: number;
  sourceIn: SourceTimestamp;
  sourceOut: SourceTimestamp;
  crop?: CropRect;
  transitionIn?: VideoTransition;
}

export interface ImageClip {
  id: string;
  kind: "image";
  assetId: string;
  startFrame: number;
  durationFrames: number;
  motion?: ImageMotion;
  crop?: CropRect;
  transitionIn?: VideoTransition;
}

export type ImageMotion =
  | "static"
  | "zoom_in"
  | "zoom_out"
  | "pan_left"
  | "pan_right"
  | "pan_up"
  | "pan_down";

export interface AudioClip {
  id: string;
  kind: "audio";
  assetId: string;
  startFrame: number;
  durationFrames: number;
  sourceIn: SourceTimestamp;
  sourceOut: SourceTimestamp;
  gain: number;
  fadeInFrames?: number;
  fadeOutFrames?: number;
}

export interface TextClip {
  id: string;
  kind: "text";
  startFrame: number;
  durationFrames: number;
  text: string;
  fontSize: number;
  color: string;
  x: number;
  y: number;
}

export type TimelineClip = VideoClip | ImageClip | AudioClip | TextClip;
export type TrackKind = "video" | "audio" | "overlay";

export interface TimelineTrack {
  id: string;
  kind: TrackKind;
  order: number;
  enabled?: boolean;
  locked?: boolean;
  clips: TimelineClip[];
}

export interface TimelineDocument {
  schemaVersion: string;
  projectId: string;
  revision: number;
  canvas: {
    width: number;
    height: number;
    fps: { num: number; den: number };
    background: string;
  };
  assets: Record<string, { kind: "video" | "audio" | "image"; uri: string; sha256: string }>;
  tracks: TimelineTrack[];
}

export interface RevisionSummary {
  revision: number;
  createdAt: string;
  isCurrent: boolean;
}

export interface ProjectRevisionHistory {
  projectId: string;
  currentRevision: number;
  revisions: RevisionSummary[];
}

export type EditOperation =
  | { type: "clip.add"; trackId: string; clip: TimelineClip; index?: number }
  | { type: "clip.append"; assetId: string; durationSeconds?: number; sourceInSeconds?: number }
  | { type: "image.add"; assetId: string; durationSeconds?: number }
  | { type: "image.motion.set"; clipId: string; motion: ImageMotion }
  | { type: "clip.transition.set"; clipId: string; durationFrames: number }
  | { type: "clip.transition.remove"; clipId: string }
  | { type: "clip.remove"; clipId: string }
  | { type: "clip.move"; clipId: string; startFrame: number }
  | { type: "clip.reorder"; clipId: string; destinationIndex: number }
  | {
      type: "clip.trim";
      clipId: string;
      startFrame: number;
      durationFrames: number;
      sourceIn?: SourceTimestamp;
      sourceOut?: SourceTimestamp;
    }
  | { type: "clip.split"; clipId: string; splitFrame: number }
  | {
      type: "text.update";
      clipId: string;
      text?: string;
      fontSize?: number;
      color?: string;
      x?: number;
      y?: number;
    }
  | { type: "audio.gain.set"; clipId: string; gain: number }
  | { type: "audio.fade.set"; clipId: string; fadeInFrames: number; fadeOutFrames: number };

export type RenderStatus = "queued" | "running" | "succeeded" | "failed" | "cancelled";
export type RenderPhase = "queued" | "preparing" | "encoding" | "validating" | "publishing" | "complete";

export interface RenderJob {
  jobId: string;
  projectId: string;
  revision: number;
  presetId: "export" | "preview";
  status: RenderStatus;
  phase: RenderPhase;
  progressPercent: number;
  attempt: number;
  cancelRequested: boolean;
  createdAt: string;
  startedAt?: string;
  finishedAt?: string;
  outputAssetId?: string;
  error?: string;
  result?: {
    outputDurationSeconds: number;
    outputWidth: number;
    outputHeight: number;
    outputFrameRate: string;
    outputSha256: string;
  };
}

export interface RenderSubmission {
  job: RenderJob;
  replayed: boolean;
}

export interface UploadSession {
  uploadId: string;
  expectedBytes: number;
  uploadUrl: string;
  completeUrl: string;
}
