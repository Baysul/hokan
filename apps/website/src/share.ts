import { MAX_RETRY_AFTER, backoff, parseRetryAfter, pause, retryableStatus, timing } from "./retry";

// The server receives only a versioned envelope; the key stays in the URL fragment.
// The plaintext starts with the filename and MIME type as length-prefixed JSON, so they are
// encrypted and authenticated along with the file.
//
// Version 2 is a version byte and a random nonce prefix, followed by the plaintext encrypted in
// 1 MiB chunks. Each chunk's nonce holds its index and whether it is the last chunk, so chunks
// cannot be reordered, dropped or cut off without decryption failing.
// Version 1 links, a single AES-GCM message of up to 25 MiB, can still be opened.
export const MAX_FILE_SIZE = 5 * 1024 * 1024 * 1024;
const MAX_ENVELOPE_SIZE = MAX_FILE_SIZE + 1024 * 1024;
const MAX_V1_ENVELOPE_SIZE = 25 * 1024 * 1024;
const MAX_METADATA_SIZE = 16 * 1024;
const VERSION = 2;
const V1 = 1;
const IV_LENGTH = 12;
const NONCE_PREFIX_LENGTH = 7;
const ENVELOPE_HEADER_LENGTH = 1 + NONCE_PREFIX_LENGTH;
const CHUNK_SIZE = 1024 * 1024;
const TAG_LENGTH = 16;
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

function chunkNonce(prefix: Uint8Array, index: number, last: boolean): Uint8Array<ArrayBuffer> {
  const nonce = new Uint8Array(IV_LENGTH);
  nonce.set(prefix);
  new DataView(nonce.buffer).setUint32(NONCE_PREFIX_LENGTH, index);
  nonce[IV_LENGTH - 1] = last ? 1 : 0;
  return nonce;
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

// Returns the sender's filename and type, and where the file's bytes begin in the plaintext.
function readMetadata(plaintext: Uint8Array): { name: string; type: string; offset: number } {
  if (plaintext.byteLength < 4) throw new Error("This file's metadata is invalid.");
  const length = new DataView(plaintext.buffer, plaintext.byteOffset).getUint32(0);
  if (length > MAX_METADATA_SIZE || 4 + length > plaintext.byteLength)
    throw new Error("This file's metadata is invalid.");
  let metadata: unknown;
  try {
    metadata = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(plaintext.subarray(4, 4 + length)),
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
    offset: 4 + length,
  };
}

export type ShareSecrets = {
  rawKey: Uint8Array<ArrayBuffer>;
  noncePrefix: Uint8Array<ArrayBuffer>;
};

export function newSecrets(): ShareSecrets {
  const crypto = cryptoApi();
  return {
    rawKey: crypto.getRandomValues(new Uint8Array(KEY_LENGTH)),
    noncePrefix: crypto.getRandomValues(new Uint8Array(NONCE_PREFIX_LENGTH)),
  };
}

export function shareLink(id: string, rawKey: Uint8Array): string {
  return `${location.origin}/share/${id}#${encodeKey(rawKey)}`;
}

export class FileChangedError extends Error {
  constructor() {
    super("The file changed or could not be read while it was being uploaded. Choose it again.");
  }
}

// Called with each chunk's index and plaintext before the chunk is encrypted.
export type ChunkCheck = (index: number, plaintext: Uint8Array<ArrayBuffer>) => Promise<void>;

export type EnvelopePart = { number: number; bytes: Uint8Array<ArrayBuffer> };

// Encrypts a file into the envelope format, one upload part at a time.
export class Envelope {
  readonly size: number;
  private readonly file: File;
  private readonly secrets: ShareSecrets;
  private readonly header: Uint8Array;
  private readonly plaintextSize: number;
  private readonly chunks: number;

  constructor(file: File, secrets: ShareSecrets) {
    if (file.size > MAX_FILE_SIZE) throw new Error("Choose a file no larger than 5 GiB.");
    const metadata = new TextEncoder().encode(
      JSON.stringify({ name: safeName(file.name), type: safeType(file.type) }),
    );
    if (metadata.length > MAX_METADATA_SIZE) throw new Error("The file name is too long.");
    this.file = file;
    this.secrets = secrets;
    this.header = new Uint8Array(4 + metadata.length);
    new DataView(this.header.buffer).setUint32(0, metadata.length);
    this.header.set(metadata, 4);
    this.plaintextSize = this.header.length + file.size;
    this.chunks = Math.ceil(this.plaintextSize / CHUNK_SIZE);
    this.size = ENVELOPE_HEADER_LENGTH + this.plaintextSize + this.chunks * TAG_LENGTH;
  }

  // Yields the parts from number `first` on. Parts are numbered from 1, and every part but the
  // last is `partSize` bytes long. Each chunk is encrypted once per call, in order.
  async *parts(partSize: number, first: number, check: ChunkCheck): AsyncGenerator<EnvelopePart> {
    let number = first;
    let start = (number - 1) * partSize;
    if (start >= this.size) return;
    let part = new Uint8Array(Math.min(partSize, this.size - start));
    const sealedSize = CHUNK_SIZE + TAG_LENGTH;
    const firstChunk =
      start < ENVELOPE_HEADER_LENGTH
        ? 0
        : Math.floor((start - ENVELOPE_HEADER_LENGTH) / sealedSize);
    for await (const { offset, bytes } of this.pieces(firstChunk, check)) {
      // The first piece may begin before this part does.
      let used = Math.max(start - offset, 0);
      while (used < bytes.length) {
        const at = offset + used - start;
        const count = Math.min(bytes.length - used, part.length - at);
        part.set(bytes.subarray(used, used + count), at);
        used += count;
        if (at + count < part.length) continue;
        yield { number, bytes: part };
        number++;
        start += partSize;
        if (start >= this.size) return;
        part = new Uint8Array(Math.min(partSize, this.size - start));
      }
    }
  }

  // Yields the envelope header and the sealed chunks from `first` on, with their offsets.
  private async *pieces(first: number, check: ChunkCheck) {
    const crypto = cryptoApi();
    const key = await crypto.subtle.importKey("raw", this.secrets.rawKey, "AES-GCM", false, [
      "encrypt",
    ]);
    if (first === 0)
      yield { offset: 0, bytes: Uint8Array.of(VERSION, ...this.secrets.noncePrefix) };
    for (let index = first; index < this.chunks; index++) {
      const start = index * CHUNK_SIZE;
      const end = Math.min(start + CHUNK_SIZE, this.plaintextSize);
      const plaintext = new Uint8Array(end - start);
      if (start < this.header.length) plaintext.set(this.header.subarray(start, end));
      const fileStart = Math.max(start - this.header.length, 0);
      const fileEnd = end - this.header.length;
      if (fileEnd > fileStart) {
        let bytes: ArrayBuffer;
        try {
          bytes = await this.file.slice(fileStart, fileEnd).arrayBuffer();
        } catch {
          // Browsers refuse to read a selected file that has since been changed.
          throw new FileChangedError();
        }
        if (bytes.byteLength !== fileEnd - fileStart) throw new FileChangedError();
        plaintext.set(new Uint8Array(bytes), fileStart + this.header.length - start);
      }
      await check(index, plaintext);
      const iv = chunkNonce(this.secrets.noncePrefix, index, index === this.chunks - 1);
      yield {
        offset: ENVELOPE_HEADER_LENGTH + index * (CHUNK_SIZE + TAG_LENGTH),
        bytes: new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, plaintext)),
      };
    }
  }
}

type Spool = {
  write(chunk: Uint8Array<ArrayBuffer>): Promise<void>;
  close(): Promise<Blob>;
  discard(): Promise<void>;
};

const SPOOL_DIRECTORY = "hokan-spool";
const SPOOL_MAX_AGE = 24 * 60 * 60 * 1000;

function storageError(error: unknown): never {
  if (error instanceof DOMException && error.name === "QuotaExceededError")
    throw new Error("Your browser does not have enough free storage space for this file.");
  throw error;
}

// Decrypted downloads can be several gigabytes, so they are written to the origin private file
// system when the browser offers it, and kept in memory otherwise. Files stay there after a
// download so the user can still save it; old ones are removed later.
async function openSpool(): Promise<Spool> {
  try {
    const root = await navigator.storage.getDirectory();
    const directory = await root.getDirectoryHandle(SPOOL_DIRECTORY, { create: true });
    for await (const [name, entry] of directory.entries()) {
      if (
        entry.kind === "file" &&
        Date.now() - (await (entry as FileSystemFileHandle).getFile()).lastModified > SPOOL_MAX_AGE
      )
        await directory.removeEntry(name).catch(() => undefined);
    }
    const name = crypto.randomUUID();
    const handle = await directory.getFileHandle(name, { create: true });
    const writable = await handle.createWritable();
    return {
      write: (chunk) => writable.write(chunk).catch(storageError),
      async close() {
        await writable.close().catch(storageError);
        return handle.getFile();
      },
      async discard() {
        await writable.abort().catch(() => undefined);
        await directory.removeEntry(name).catch(() => undefined);
      },
    };
  } catch {
    const parts: BlobPart[] = [];
    return {
      write: async (chunk) => void parts.push(chunk),
      close: async () => new Blob(parts),
      discard: async () => void parts.splice(0),
    };
  }
}

// Collects response chunks so they can be taken back out in pieces of any size.
class ByteQueue {
  private chunks: Uint8Array[] = [];
  length = 0;

  push(chunk: Uint8Array) {
    this.chunks.push(chunk);
    this.length += chunk.byteLength;
  }

  take(count: number): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(count);
    let offset = 0;
    while (offset < count) {
      const chunk = this.chunks[0]!;
      const used = Math.min(chunk.byteLength, count - offset);
      bytes.set(chunk.subarray(0, used), offset);
      offset += used;
      if (used === chunk.byteLength) this.chunks.shift();
      else this.chunks[0] = chunk.subarray(used);
    }
    this.length -= count;
    return bytes;
  }
}

const INVALID_ENVELOPE = "This encrypted file is invalid or unsupported.";
const DECRYPT_ERROR =
  "Could not decrypt this file. The link may be incomplete or the file may have been changed.";

export type DownloadProgress =
  | { stage: "downloading"; loaded: number; total: number | null }
  | { stage: "reconnecting"; retryAt: number; offline: boolean };

// A failure worth retrying, with the server's suggested wait if it gave one.
class Interrupted extends Error {
  readonly retryAfter: number | null;

  constructor(retryAfter: number | null = null) {
    super("The download was interrupted.");
    this.retryAfter = retryAfter;
  }
}

// Reads the encrypted file. If the connection fails or stalls part way, it asks the server for
// the rest with a Range request, so a large download continues instead of starting over. It
// retries for as long as the page stays open, and right away when the browser comes back online.
class Download {
  received = 0;
  private total: number | null = null;
  private readonly url: string;
  private readonly onProgress?: (progress: DownloadProgress) => void;
  private reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  private controller: AbortController | null = null;
  // Bytes to drop from the next response, when a server sends the whole file again.
  private skip = 0;
  private failures = 0;
  private retryAfter: number | null = null;

  constructor(url: string, onProgress?: (progress: DownloadProgress) => void) {
    this.url = url;
    this.onProgress = onProgress;
  }

  // Returns the next bytes, or null at the end of the file.
  async read(): Promise<Uint8Array | null> {
    for (;;) {
      if (!this.reader) {
        if (this.failures > 0) {
          const delay = backoff(this.failures - 1, this.retryAfter);
          this.onProgress?.({
            stage: "reconnecting",
            retryAt: Date.now() + delay,
            offline: navigator.onLine === false,
          });
          await pause(delay);
        }
        try {
          await this.connect();
        } catch (error) {
          if (!(error instanceof Interrupted)) throw error;
          this.failed(error.retryAfter);
          continue;
        }
      }
      let result: ReadableStreamReadResult<Uint8Array>;
      try {
        result = await this.guard(this.reader!.read(), this.controller!);
      } catch {
        this.drop();
        this.failed(null);
        continue;
      }
      if (result.done) {
        this.reader = null;
        // A response that ends before the whole file has arrived is continued too.
        if (this.total !== null && this.received < this.total) {
          this.failed(null);
          continue;
        }
        return null;
      }
      let bytes = result.value;
      if (this.skip > 0) {
        const dropped = Math.min(this.skip, bytes.length);
        this.skip -= dropped;
        bytes = bytes.subarray(dropped);
        if (bytes.length === 0) continue;
      }
      this.received += bytes.length;
      if (this.total !== null && this.received > this.total) throw new Error(INVALID_ENVELOPE);
      this.failures = 0;
      this.onProgress?.({ stage: "downloading", loaded: this.received, total: this.total });
      return bytes;
    }
  }

  async cancel() {
    const reader = this.reader;
    this.reader = null;
    this.controller?.abort();
    await reader?.cancel().catch(() => undefined);
  }

  private failed(retryAfter: number | null) {
    this.failures++;
    this.retryAfter = retryAfter === null ? null : Math.min(retryAfter, MAX_RETRY_AFTER);
  }

  private drop() {
    this.controller?.abort();
    void this.reader?.cancel().catch(() => undefined);
    this.reader = null;
  }

  // Gives up on a connection that makes no progress for too long, so that it can be resumed.
  private async guard<T>(promise: Promise<T>, controller: AbortController): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stalled = new Promise<never>((_, reject) => {
      timer = setTimeout(() => {
        controller.abort();
        reject(new Interrupted());
      }, timing.stall);
    });
    try {
      return await Promise.race([promise, stalled]);
    } finally {
      clearTimeout(timer);
    }
  }

  private async connect() {
    const controller = new AbortController();
    const headers: Record<string, string> =
      this.received > 0 ? { Range: `bytes=${this.received}-` } : {};
    let response: Response;
    try {
      response = await this.guard(
        fetch(this.url, { cache: "no-store", headers, signal: controller.signal }),
        controller,
      );
    } catch {
      throw new Interrupted();
    }
    if (response.status === 404)
      throw new Error("This file has expired or is no longer available.");
    if (retryableStatus(response.status))
      throw new Interrupted(parseRetryAfter(response.headers.get("retry-after")));
    let total: number | null;
    if (response.status === 206) {
      const range = /^bytes (\d+)-\d+\/(\d+)$/.exec(response.headers.get("content-range") ?? "");
      if (!range || Number(range[1]) !== this.received) throw new Error(INVALID_ENVELOPE);
      total = Number(range[2]);
      this.skip = 0;
    } else if (response.status === 200) {
      const length = response.headers.get("content-length");
      total = length === null ? null : Number(length);
      this.skip = this.received;
    } else {
      throw new Error("The encrypted file could not be retrieved.");
    }
    // A different length would mean a different file.
    if (this.total !== null && total !== null && total !== this.total)
      throw new Error(INVALID_ENVELOPE);
    this.total ??= total;
    if (this.total !== null && this.total > MAX_ENVELOPE_SIZE) throw new Error(INVALID_ENVELOPE);
    if (!response.body) throw new Error(INVALID_ENVELOPE);
    this.reader = response.body.getReader();
    this.controller = controller;
  }
}

// The blob stays application/octet-stream: a blob URL runs in this origin, so a sender-chosen
// type such as text/html must not reach it. The recorded type is returned separately.
export async function openShare(
  onProgress?: (progress: DownloadProgress) => void,
): Promise<{ blob: Blob; name: string; type: string } | null> {
  if (!location.pathname.startsWith("/share/")) return null;
  const id = location.pathname.slice("/share/".length);
  if (!/^[0-9a-f-]{36}$/.test(id)) throw new Error("This share link is invalid.");
  const rawKey = decodeKey(location.hash.slice(1));
  const crypto = cryptoApi();
  const key = await crypto.subtle.importKey("raw", rawKey, "AES-GCM", false, ["decrypt"]);
  const decrypt = async (iv: Uint8Array<ArrayBuffer>, sealed: Uint8Array<ArrayBuffer>) => {
    try {
      return new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, sealed));
    } catch {
      throw new Error(DECRYPT_ERROR);
    }
  };

  const download = new Download(`/api/files/${id}`, onProgress);
  const queue = new ByteQueue();
  let limit = MAX_ENVELOPE_SIZE;
  let done = false;
  // Reads until more than `wanted` bytes are queued or the file ends.
  const fill = async (wanted: number) => {
    while (!done && queue.length <= wanted) {
      const bytes = await download.read();
      if (!bytes) {
        done = true;
        break;
      }
      if (download.received > limit) throw new Error(INVALID_ENVELOPE);
      queue.push(bytes);
    }
  };
  let spool: Spool | null = null;
  try {
    await fill(NONCE_PREFIX_LENGTH);
    if (queue.length === 0) throw new Error(INVALID_ENVELOPE);
    const version = queue.take(1)[0];
    if (version === V1) {
      limit = MAX_V1_ENVELOPE_SIZE;
      await fill(limit);
      if (queue.length < IV_LENGTH + TAG_LENGTH + 4) throw new Error(INVALID_ENVELOPE);
      const iv = queue.take(IV_LENGTH);
      const plaintext = await decrypt(iv, queue.take(queue.length));
      const { name, type, offset } = readMetadata(plaintext);
      return { name, type, blob: new Blob([plaintext.subarray(offset)], { type: DEFAULT_TYPE }) };
    }
    if (version !== VERSION || queue.length < NONCE_PREFIX_LENGTH)
      throw new Error(INVALID_ENVELOPE);
    const noncePrefix = queue.take(NONCE_PREFIX_LENGTH);
    spool = await openSpool();
    let file: { name: string; type: string } | null = null;
    for (let index = 0; ; index++) {
      await fill(CHUNK_SIZE + TAG_LENGTH);
      const last = done && queue.length <= CHUNK_SIZE + TAG_LENGTH;
      if (queue.length <= TAG_LENGTH) throw new Error(INVALID_ENVELOPE);
      const sealed = queue.take(last ? queue.length : CHUNK_SIZE + TAG_LENGTH);
      let plaintext = await decrypt(chunkNonce(noncePrefix, index, last), sealed);
      if (!file) {
        const { name, type, offset } = readMetadata(plaintext);
        file = { name, type };
        plaintext = plaintext.subarray(offset);
      }
      await spool.write(plaintext);
      if (last) break;
    }
    return { ...file!, blob: new Blob([await spool.close()], { type: DEFAULT_TYPE }) };
  } catch (error) {
    await download.cancel();
    await spool?.discard();
    throw error;
  }
}
