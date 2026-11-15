import type { ImportedAsset, UploadSession } from "./types";

const API_BASE = (import.meta.env.VITE_API_BASE_URL ?? "/api").replace(/\/$/, "");

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code = "API_ERROR",
  ) {
    super(message);
    this.name = "ApiError";
  }
}

function apiUrl(path: string): string {
  return `${API_BASE}${path.startsWith("/") ? path : `/${path}`}`;
}

async function responseError(response: Response): Promise<ApiError> {
  try {
    const body = (await response.json()) as {
      error?: { message?: string; code?: string };
    };
    return new ApiError(
      body.error?.message ?? `The API returned ${response.status}.`,
      response.status,
      body.error?.code,
    );
  } catch {
    return new ApiError(`The API returned ${response.status}.`, response.status);
  }
}

export async function apiRequest<T>(
  token: string,
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  headers.set("Authorization", `Bearer ${token}`);
  if (typeof init.body === "string" && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  const response = await fetch(apiUrl(path), { ...init, headers });
  if (!response.ok) throw await responseError(response);
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

export async function uploadFile(
  token: string,
  session: UploadSession,
  file: File,
  onProgress: (percent: number) => void,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("PUT", apiUrl(session.uploadUrl));
    request.setRequestHeader("Authorization", `Bearer ${token}`);
    request.setRequestHeader("Content-Type", "application/octet-stream");
    request.upload.onprogress = (event) => {
      if (event.lengthComputable) {
        onProgress(Math.min(100, Math.round((event.loaded / event.total) * 100)));
      }
    };
    request.onerror = () => reject(new ApiError("The media upload was interrupted.", 0));
    request.onabort = () => reject(new ApiError("The media upload was cancelled.", 0));
    request.onload = () => {
      if (request.status >= 200 && request.status < 300) {
        onProgress(100);
        resolve();
        return;
      }
      let message = `The API returned ${request.status}.`;
      let code = "API_ERROR";
      try {
        const body = JSON.parse(request.responseText) as {
          error?: { message?: string; code?: string };
        };
        message = body.error?.message ?? message;
        code = body.error?.code ?? code;
      } catch {
        // Keep the status-based message when the response is not JSON.
      }
      reject(new ApiError(message, request.status, code));
    };
    request.send(file);
  });
}

export async function completeUpload(
  token: string,
  session: UploadSession,
): Promise<ImportedAsset> {
  const response = await apiRequest<{ asset: ImportedAsset }>(token, session.completeUrl, {
    method: "POST",
  });
  return response.asset;
}

export async function uploadMedia(
  token: string,
  file: File,
  onProgress: (percent: number) => void,
): Promise<ImportedAsset> {
  const session = await apiRequest<UploadSession>(token, "/v1/assets/uploads", {
    method: "POST",
    body: JSON.stringify({ expectedBytes: file.size, fileName: file.name }),
  });
  await uploadFile(token, session, file, onProgress);
  return completeUpload(token, session);
}

export async function downloadAsset(token: string, assetId: string): Promise<Blob> {
  const response = await fetch(apiUrl(`/v1/assets/${assetId}/content`), {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!response.ok) throw await responseError(response);
  return response.blob();
}
