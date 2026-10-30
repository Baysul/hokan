// The server receives only this versioned envelope: version, IV, and ciphertext.
// The filename and MIME type are inside the authenticated ciphertext; the key stays in the URL fragment.
const MAX_FILE_SIZE = 24 * 1024 * 1024;
const MAX_ENVELOPE_SIZE = 25 * 1024 * 1024;
const MAX_METADATA_SIZE = 16 * 1024;
const VERSION = 1;
const IV_LENGTH = 12;
const KEY_LENGTH = 32;
const DEFAULT_TYPE = "application/octet-stream";

function cryptoApi(): Crypto {
  if (!globalThis.crypto?.subtle || !globalThis.isSecureContext) {
    throw new Error("Encryption requires HTTPS or localhost in a supported browser.");
  }
  return globalThis.crypto;
}

function encodeKey(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

function decodeKey(value: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]{43}$/.test(value)) throw new Error("This share link has an invalid key.");
  const decoded = atob(value.replaceAll("-", "+").replaceAll("_", "/") + "=");
  const bytes = Uint8Array.from(decoded, (character) => character.charCodeAt(0));
  if (bytes.length !== KEY_LENGTH || encodeKey(bytes) !== value)
    throw new Error("This share link has an invalid key.");
  return bytes;
}

function safeName(name: string): string {
  return (
    Array.from(name, (character) => {
      const code = character.codePointAt(0)!;
      // Direction controls can make an executable filename look like a document.
      const directionControl =
        code === 0x061c ||
        (code >= 0x200e && code <= 0x200f) ||
        (code >= 0x202a && code <= 0x202e) ||
        (code >= 0x2066 && code <= 0x2069);
      return code < 32 ||
        code === 127 ||
        directionControl ||
        character === "/" ||
        character === "\\"
        ? "_"
        : character;
    })
      .join("")
      .slice(0, 255) || "download"
  );
}

// The type comes from the sender, so anything that is not a plain type/subtype is discarded.
function safeType(type: unknown): string {
  return typeof type === "string" &&
    type.length <= 255 &&
    /^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+$/i.test(type)
    ? type.toLowerCase()
    : DEFAULT_TYPE;
}

const CONNECT_ERROR = "Could not connect to the server. Check that it is running and try again.";

export type ShareProgress =
  | { stage: "encrypting" }
  | { stage: "uploading"; loaded: number; total: number }
  | { stage: "saving" };

async function requestFile(url: string, options: RequestInit): Promise<Response> {
  try {
    return await fetch(url, options);
  } catch {
    throw new Error(CONNECT_ERROR);
  }
}

async function readEnvelope(response: Response): Promise<Uint8Array> {
  const declaredSize = response.headers.get("content-length");
  if (declaredSize !== null && Number(declaredSize) > MAX_ENVELOPE_SIZE)
    throw new Error("This encrypted file is invalid or unsupported.");
  if (!response.body) throw new Error("This encrypted file is invalid or unsupported.");
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > MAX_ENVELOPE_SIZE)
        throw new Error("This encrypted file is invalid or unsupported.");
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel().catch(() => undefined);
    throw error;
  }
  const envelope = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    envelope.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return envelope;
}

// fetch cannot report upload progress, so uploads use XMLHttpRequest.
function sendEnvelope(
  envelope: Uint8Array<ArrayBuffer>,
  expirySeconds: number,
  onProgress?: (progress: ShareProgress) => void,
): Promise<XMLHttpRequest> {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("POST", "/api/files");
    request.setRequestHeader("Content-Type", "application/octet-stream");
    request.setRequestHeader("X-Hokan-Expiry-Seconds", String(expirySeconds));
    request.upload.onprogress = (event) => {
      if (event.lengthComputable)
        onProgress?.({ stage: "uploading", loaded: event.loaded, total: event.total });
    };
    request.upload.onload = () => onProgress?.({ stage: "saving" });
    request.onload = () => resolve(request);
    request.onerror = request.ontimeout = request.onabort = () => reject(new Error(CONNECT_ERROR));
    request.send(envelope);
  });
}

function uploadError(request: XMLHttpRequest): Error {
  const { status } = request;
  const isText = request.getResponseHeader("Content-Type")?.startsWith("text/plain");
  // The server explains its own refusals in plain text; proxies and dev servers do not.
  const reason = isText ? request.responseText.trim().slice(0, 120) : "";
  if (status === 405)
    return new Error("The server does not support uploads. Restart it with the latest version.");
  if (status === 413) return new Error("The encrypted file is larger than the server accepts.");
  if (!reason && (status === 0 || (status >= 502 && status <= 504)))
    return new Error(CONNECT_ERROR);
  return new Error(`The server could not store the file (${reason || `error ${status}`}).`);
}

// An expirySeconds of 0 means the link never expires.
export async function createShare(
  file: File,
  expirySeconds: number,
  onProgress?: (progress: ShareProgress) => void,
): Promise<string> {
  if (file.size > MAX_FILE_SIZE) throw new Error("Choose a file no larger than 24 MiB.");
  onProgress?.({ stage: "encrypting" });
  const crypto = cryptoApi();
  const metadata = new TextEncoder().encode(
    JSON.stringify({ name: safeName(file.name), type: safeType(file.type) }),
  );
  if (metadata.length > MAX_METADATA_SIZE) throw new Error("The file name is too long.");
  const plaintext = new Uint8Array(4 + metadata.length + file.size);
  new DataView(plaintext.buffer).setUint32(0, metadata.length);
  plaintext.set(metadata, 4);
  plaintext.set(new Uint8Array(await file.arrayBuffer()), 4 + metadata.length);

  const rawKey = crypto.getRandomValues(new Uint8Array(KEY_LENGTH));
  const iv = crypto.getRandomValues(new Uint8Array(IV_LENGTH));
  const key = await crypto.subtle.importKey("raw", rawKey, "AES-GCM", false, ["encrypt"]);
  const encrypted = new Uint8Array(
    await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, plaintext),
  );
  const envelope = new Uint8Array(1 + IV_LENGTH + encrypted.length);
  envelope[0] = VERSION;
  envelope.set(iv, 1);
  envelope.set(encrypted, 1 + IV_LENGTH);
  const response = await sendEnvelope(envelope, expirySeconds, onProgress);
  if (response.status < 200 || response.status >= 300) throw uploadError(response);
  let result: unknown;
  try {
    result = JSON.parse(response.responseText);
  } catch {
    throw new Error("The server returned an invalid share ID.");
  }
  if (
    !result ||
    typeof result !== "object" ||
    !("id" in result) ||
    typeof result.id !== "string" ||
    !/^[0-9a-f-]{36}$/.test(result.id)
  ) {
    throw new Error("The server returned an invalid share ID.");
  }
  return `${location.origin}/share/${result.id}#${encodeKey(rawKey)}`;
}

// The blob stays application/octet-stream: a blob URL runs in this origin, so a sender-chosen
// type such as text/html must not reach it. The recorded type is returned separately.
export async function openShare(): Promise<{ blob: Blob; name: string; type: string } | null> {
  if (!location.pathname.startsWith("/share/")) return null;
  const id = location.pathname.slice("/share/".length);
  if (!/^[0-9a-f-]{36}$/.test(id)) throw new Error("This share link is invalid.");
  const rawKey = decodeKey(location.hash.slice(1));
  const crypto = cryptoApi();
  const response = await requestFile(`/api/files/${id}`, { cache: "no-store" });
  if (response.status === 404) throw new Error("This file has expired or is no longer available.");
  if (!response.ok) throw new Error("The encrypted file could not be retrieved.");
  const envelope = await readEnvelope(response);
  if (envelope.length < 1 + IV_LENGTH + 16 + 4 || envelope[0] !== VERSION) {
    throw new Error("This encrypted file is invalid or unsupported.");
  }
  const key = await crypto.subtle.importKey("raw", rawKey, "AES-GCM", false, ["decrypt"]);
  let plaintext: ArrayBuffer;
  try {
    plaintext = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: envelope.slice(1, 1 + IV_LENGTH) },
      key,
      envelope.slice(1 + IV_LENGTH),
    );
  } catch {
    throw new Error(
      "Could not decrypt this file. The link may be incomplete or the file may have been changed.",
    );
  }
  const length = new DataView(plaintext).getUint32(0);
  if (length > MAX_METADATA_SIZE || 4 + length > plaintext.byteLength)
    throw new Error("This file's metadata is invalid.");
  let metadata: unknown;
  try {
    metadata = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(plaintext, 4, length)),
    );
  } catch {
    throw new Error("This file's metadata is invalid.");
  }
  if (
    !metadata ||
    typeof metadata !== "object" ||
    !("name" in metadata) ||
    typeof metadata.name !== "string"
  )
    throw new Error("This file's metadata is invalid.");
  return {
    name: safeName(metadata.name),
    type: safeType("type" in metadata ? metadata.type : undefined),
    blob: new Blob([plaintext.slice(4 + length)], { type: DEFAULT_TYPE }),
  };
}
