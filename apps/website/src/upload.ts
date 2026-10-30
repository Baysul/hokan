import {
  Envelope,
  FileChangedError,
  newSecrets,
  shareLink,
  type ChunkCheck,
  type ShareSecrets,
} from "./share";
import {
  MAX_RETRY_AFTER,
  abortReason,
  backoff,
  parseRetryAfter,
  pause,
  retryableStatus,
  timing,
} from "./retry";

// The server stores uploads one part at a time, so an interrupted upload continues from where
// it stopped instead of starting over:
// - Failed requests are retried with growing pauses for as long as the page stays open, and
//   right away when the browser comes back online. Parts the server has are never sent again.
// - If the page closes, choosing the same file again resumes the upload. Until it finishes,
//   IndexedDB keeps the key and a digest of each chunk's plaintext. A chunk may then have to be
//   encrypted a second time, and AES-GCM must never use a nonce for two different plaintexts,
//   so its digest has to match before it is.

export type ShareProgress =
  | { stage: "preparing" }
  | { stage: "uploading"; loaded: number; total: number }
  | { stage: "reconnecting"; retryAt: number; offline: boolean }
  | { stage: "saving" };

export type ShareOptions = {
  onProgress?: (progress: ShareProgress) => void;
  signal?: AbortSignal;
};

export type PendingShare = { name: string; size: number; expirySeconds: number };

const PARALLEL_PARTS = 3;
const MAX_RESTARTS = 3;
const PENDING_MAX_AGE = 7 * 24 * 60 * 60 * 1000;
const CONNECT_ERROR = "Could not connect to the server. Check that it is running and try again.";

type Reply = { status: number; text: string; type: string | null; retryAfter: number | null };

// The request got no answer: the connection failed, stalled or was cut off.
class NetworkError extends Error {}

class HttpError extends Error {
  readonly reply: Reply;

  constructor(reply: Reply) {
    // The server explains its own refusals in plain text; proxies and dev servers do not.
    const reason = reply.type?.startsWith("text/plain") ? reply.text.trim().slice(0, 120) : "";
    super(
      reply.status === 405
        ? "The server does not support uploads. Restart it with the latest version."
        : reply.status === 413
          ? "The file is larger than the server accepts."
          : reply.status === 429
            ? "The server's daily upload limit has been reached. Try again later to continue this upload."
            : `The server could not store the file (${reason || `error ${reply.status}`}).`,
    );
    this.reply = reply;
  }
}

// The server no longer has the upload, for example because it restarted with memory storage.
class SessionLost extends Error {}

// fetch cannot report upload progress, so requests use XMLHttpRequest.
function send(
  method: string,
  url: string,
  options: {
    signal: AbortSignal;
    headers?: Record<string, string>;
    body?: Uint8Array<ArrayBuffer>;
    onProgress?: (loaded: number) => void;
  },
): Promise<Reply> {
  return new Promise((resolve, reject) => {
    if (options.signal.aborted) return reject(abortReason(options.signal));
    const request = new XMLHttpRequest();
    let timer: ReturnType<typeof setTimeout> | undefined;
    // A request that stops making progress is abandoned, so that it can be retried.
    const watch = (ms: number) => {
      clearTimeout(timer);
      timer = setTimeout(() => request.abort(), ms);
    };
    const cancel = () => request.abort();
    const settle = () => {
      clearTimeout(timer);
      options.signal.removeEventListener("abort", cancel);
    };
    request.open(method, url);
    for (const [name, value] of Object.entries(options.headers ?? {}))
      request.setRequestHeader(name, value);
    request.upload.onprogress = (event) => {
      watch(timing.stall);
      options.onProgress?.(event.loaded);
    };
    request.upload.onload = () => watch(timing.response);
    request.onload = () => {
      settle();
      resolve({
        status: request.status,
        text: request.responseText,
        type: request.getResponseHeader("Content-Type"),
        retryAfter: parseRetryAfter(request.getResponseHeader("Retry-After")),
      });
    };
    request.onerror =
      request.ontimeout =
      request.onabort =
        () => {
          settle();
          reject(
            options.signal.aborted ? abortReason(options.signal) : new NetworkError(CONNECT_ERROR),
          );
        };
    options.signal.addEventListener("abort", cancel);
    watch(options.body ? timing.stall : timing.response);
    request.send(options.body ?? null);
  });
}

async function call(...args: Parameters<typeof send>): Promise<Reply> {
  const reply = await send(...args);
  if (reply.status < 200 || reply.status >= 300) throw new HttpError(reply);
  return reply;
}

function json(reply: Reply): unknown {
  try {
    return JSON.parse(reply.text);
  } catch {
    return null;
  }
}

function retryable(error: unknown): boolean {
  if (error instanceof NetworkError) return true;
  if (!(error instanceof HttpError)) return false;
  const { status, retryAfter } = error.reply;
  return (retryAfter === null || retryAfter <= MAX_RETRY_AFTER) && retryableStatus(status);
}

// Called with the time of the next attempt, and with null once it starts.
type Waiting = (retryAt: number | null) => void;

async function retrying<T>(
  attempt: () => Promise<T>,
  signal: AbortSignal,
  onWait: Waiting,
): Promise<T> {
  for (let failures = 0; ; failures++) {
    try {
      return await attempt();
    } catch (error) {
      if (signal.aborted || !retryable(error)) throw error;
      const delay = backoff(failures, error instanceof HttpError ? error.reply.retryAfter : null);
      onWait(Date.now() + delay);
      try {
        await pause(delay, signal);
      } finally {
        onWait(null);
      }
    }
  }
}

type SavedUpload = {
  token: string;
  partSize: number;
  rawKey: Uint8Array<ArrayBuffer>;
  noncePrefix: Uint8Array<ArrayBuffer>;
  expirySeconds: number;
  started: number;
  file: { name: string; size: number; lastModified: number };
};

const DATABASE = "hokan";
const UPLOADS = "uploads";
const DIGESTS = "digests";
const CURRENT = "current";

function result<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function committed(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = transaction.onabort = () =>
      reject(transaction.error ?? new Error("Browser storage failed."));
  });
}

let database: Promise<IDBDatabase | null> | undefined;

// Resolves to null when the browser cannot store data, as in some private windows.
function openDatabase(): Promise<IDBDatabase | null> {
  database ??= new Promise((resolve) => {
    if (typeof indexedDB === "undefined") return resolve(null);
    let opening: IDBOpenDBRequest;
    try {
      opening = indexedDB.open(DATABASE, 1);
    } catch {
      return resolve(null);
    }
    opening.onupgradeneeded = () => {
      opening.result.createObjectStore(UPLOADS);
      opening.result.createObjectStore(DIGESTS);
    };
    opening.onsuccess = () => resolve(opening.result);
    opening.onerror = opening.onblocked = () => resolve(null);
  });
  return database;
}

async function loadSaved(): Promise<SavedUpload | null> {
  try {
    const db = await openDatabase();
    if (!db) return null;
    const store = db.transaction(UPLOADS, "readonly").objectStore(UPLOADS);
    return ((await result(store.get(CURRENT))) as SavedUpload | undefined) ?? null;
  } catch {
    return null;
  }
}

async function loadDigests(token: string): Promise<Map<number, Uint8Array>> {
  const db = await openDatabase();
  if (!db) throw new Error("Browser storage is unavailable.");
  const store = db.transaction(DIGESTS, "readonly").objectStore(DIGESTS);
  const range = IDBKeyRange.bound([token, 0], [token, Infinity]);
  const [keys, values] = await Promise.all([
    result(store.getAllKeys(range)),
    result(store.getAll(range)),
  ]);
  return new Map(keys.map((key, index) => [(key as [string, number])[1], values[index]]));
}

// Only one upload is kept, so saving one replaces whatever was saved before.
async function saveUpload(saved: SavedUpload): Promise<boolean> {
  try {
    const db = await openDatabase();
    if (!db) return false;
    const transaction = db.transaction([UPLOADS, DIGESTS], "readwrite");
    transaction.objectStore(DIGESTS).clear();
    transaction.objectStore(UPLOADS).put(saved, CURRENT);
    await committed(transaction);
    return true;
  } catch {
    return false;
  }
}

async function saveDigests(token: string, entries: Array<[number, Uint8Array]>): Promise<void> {
  const db = await openDatabase();
  if (!db) throw new Error("Browser storage is unavailable.");
  const transaction = db.transaction(DIGESTS, "readwrite");
  const store = transaction.objectStore(DIGESTS);
  for (const [index, digest] of entries) store.put(digest, [token, index]);
  await committed(transaction);
}

async function forgetSaved(): Promise<void> {
  try {
    const db = await openDatabase();
    if (!db) return;
    const transaction = db.transaction([UPLOADS, DIGESTS], "readwrite");
    transaction.objectStore(UPLOADS).clear();
    transaction.objectStore(DIGESTS).clear();
    await committed(transaction);
  } catch {
    // Nothing more can be done; a saved upload that cannot be resumed is discarded later.
  }
}

// Forgets an upload and asks the server to drop it. The server also drops abandoned uploads
// by itself after a day, so failures are ignored.
async function abandon(token: string): Promise<void> {
  await forgetSaved();
  await send("DELETE", `/api/uploads/${token}`, { signal: AbortSignal.timeout(10_000) }).catch(
    () => undefined,
  );
}

// Only one tab uploads at a time, since only one upload is saved for resuming.
async function exclusively<T>(task: () => Promise<T>): Promise<T> {
  if (!("locks" in navigator)) return task();
  return navigator.locks.request("hokan-upload", { ifAvailable: true }, async (lock) => {
    if (!lock) throw new Error("Another tab is uploading a file. Wait for it to finish first.");
    return task();
  });
}

type Session = { token: string; partSize: number };

class Upload {
  readonly envelope: Envelope;
  readonly session: Session;
  readonly secrets: ShareSecrets;
  private readonly digests: Map<number, Uint8Array>;
  private readonly unsaved: Array<[number, Uint8Array]> = [];
  private persistent: boolean;
  private saving: Promise<void> = Promise.resolve();

  constructor(
    envelope: Envelope,
    session: Session,
    secrets: ShareSecrets,
    digests: Map<number, Uint8Array>,
    persistent: boolean,
  ) {
    this.envelope = envelope;
    this.session = session;
    this.secrets = secrets;
    this.digests = digests;
    this.persistent = persistent;
  }

  // Records each chunk's digest the first time, and requires the same digest after that.
  readonly check: ChunkCheck = async (index, plaintext) => {
    const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", plaintext));
    const known = this.digests.get(index);
    if (!known) {
      this.digests.set(index, digest);
      if (this.persistent) this.unsaved.push([index, digest]);
    } else if (known.length !== digest.length || known.some((byte, i) => byte !== digest[i])) {
      throw new FileChangedError();
    }
  };

  // Stores the digests of the chunks encrypted so far. This must finish before their
  // ciphertext is sent.
  flush(): Promise<void> {
    const entries = this.unsaved.splice(0);
    this.saving = this.saving.then(async () => {
      if (!this.persistent || entries.length === 0) return;
      try {
        await saveDigests(this.session.token, entries);
      } catch {
        // Without its digests, the upload must not be resumed after a reload.
        this.persistent = false;
        await forgetSaved();
      }
    });
    return this.saving;
  }
}

type Context = { signal: AbortSignal; report: (progress: ShareProgress) => void; onWait: Waiting };

const UPLOAD_TOKEN = /^[0-9a-f-]{36}\.\d+\.[0-9a-f]+$/;

async function startSession(size: number, expirySeconds: number, ctx: Context): Promise<Session> {
  const reply = await retrying(
    () =>
      call("POST", "/api/uploads", {
        signal: ctx.signal,
        headers: {
          "X-Hokan-Upload-Length": String(size),
          "X-Hokan-Expiry-Seconds": String(expirySeconds),
        },
      }),
    ctx.signal,
    ctx.onWait,
  );
  const body = json(reply);
  if (
    !body ||
    typeof body !== "object" ||
    !("upload" in body) ||
    typeof body.upload !== "string" ||
    !UPLOAD_TOKEN.test(body.upload) ||
    !("part_size" in body) ||
    typeof body.part_size !== "number" ||
    !Number.isSafeInteger(body.part_size) ||
    body.part_size < 1
  )
    throw new Error("The server returned an invalid upload.");
  return { token: body.upload, partSize: body.part_size };
}

// Returns the parts the server has, or null if it no longer has the upload.
async function receivedParts(session: Session, ctx: Context): Promise<Set<number> | null> {
  let reply: Reply;
  try {
    reply = await retrying(
      () => call("GET", `/api/uploads/${session.token}`, { signal: ctx.signal }),
      ctx.signal,
      ctx.onWait,
    );
  } catch (error) {
    if (error instanceof HttpError && error.reply.status === 404) return null;
    throw error;
  }
  const body = json(reply);
  if (
    !body ||
    typeof body !== "object" ||
    !("received" in body) ||
    !Array.isArray(body.received) ||
    !body.received.every((number) => Number.isSafeInteger(number))
  )
    throw new Error("The server returned an invalid upload.");
  return new Set(body.received as number[]);
}

// Returns the published file's ID, or why the upload could not be completed.
async function finish(
  session: Session,
  ctx: Context,
): Promise<{ id: string } | "incomplete" | "missing"> {
  let reply: Reply;
  try {
    reply = await retrying(
      () => call("POST", `/api/uploads/${session.token}/complete`, { signal: ctx.signal }),
      ctx.signal,
      ctx.onWait,
    );
  } catch (error) {
    if (error instanceof HttpError && error.reply.status === 409) return "incomplete";
    if (error instanceof HttpError && error.reply.status === 404) return "missing";
    throw error;
  }
  const body = json(reply);
  if (
    !body ||
    typeof body !== "object" ||
    !("id" in body) ||
    typeof body.id !== "string" ||
    !/^[0-9a-f-]{36}$/.test(body.id)
  )
    throw new Error("The server returned an invalid share ID.");
  return { id: body.id };
}

// Sends the parts the server does not have yet, several at a time.
async function sendParts(upload: Upload, received: Set<number>, ctx: Context): Promise<void> {
  const { token, partSize } = upload.session;
  const total = upload.envelope.size;
  const count = Math.ceil(total / partSize);
  let first = 1;
  while (received.has(first)) first++;
  if (first > count) return;
  let sent = 0;
  for (const number of received)
    if (number >= 1 && number <= count) sent += Math.min(partSize, total - (number - 1) * partSize);
  const loading = new Map<number, number>();
  const waits = new Map<number, number>();
  const report = () => {
    if (waits.size > 0) {
      ctx.report({
        stage: "reconnecting",
        retryAt: Math.min(...waits.values()),
        offline: navigator.onLine === false,
      });
      return;
    }
    let loaded = sent;
    for (const bytes of loading.values()) loaded += bytes;
    ctx.report({ stage: "uploading", loaded, total });
  };
  // A part that fails for good stops the others.
  const controller = new AbortController();
  const stop = () => controller.abort(abortReason(ctx.signal));
  ctx.signal.addEventListener("abort", stop);
  const { signal } = controller;
  const parts = upload.envelope.parts(partSize, first, upload.check);
  const worker = async () => {
    for (;;) {
      const next = await parts.next();
      if (next.done || signal.aborted) return;
      const { number, bytes } = next.value;
      if (received.has(number)) continue;
      await upload.flush();
      try {
        await retrying(
          () =>
            call("PUT", `/api/uploads/${token}/parts/${number}`, {
              signal,
              body: bytes,
              headers: { "Content-Type": "application/octet-stream" },
              onProgress: (loaded) => {
                loading.set(number, loaded);
                report();
              },
            }).catch((error: unknown) => {
              loading.delete(number);
              throw error;
            }),
          signal,
          (retryAt) => {
            if (retryAt === null) waits.delete(number);
            else waits.set(number, retryAt);
            report();
          },
        );
      } catch (error) {
        throw error instanceof HttpError && error.reply.status === 404 ? new SessionLost() : error;
      }
      loading.delete(number);
      sent += bytes.length;
      received.add(number);
      report();
    }
  };
  report();
  const results = await Promise.allSettled(
    Array.from({ length: PARALLEL_PARTS }, () =>
      worker().catch((error: unknown) => {
        controller.abort(error);
        throw error;
      }),
    ),
  );
  ctx.signal.removeEventListener("abort", stop);
  await parts.return(undefined);
  const failure = results.find((settled) => settled.status === "rejected");
  if (failure) throw failure.reason;
}

async function run(upload: Upload, ctx: Context): Promise<string> {
  for (let round = 0; ; round++) {
    const received = await receivedParts(upload.session, ctx);
    if (received) await sendParts(upload, received, ctx);
    ctx.report({ stage: "saving" });
    // When the server no longer lists the upload, it may have completed with the reply lost.
    const outcome = await finish(upload.session, ctx);
    if (typeof outcome === "object") return outcome.id;
    if (outcome === "missing" || !received) throw new SessionLost();
    if (round === 2) throw new Error("The server did not accept every part of the upload.");
  }
}

async function begin(file: File, expirySeconds: number, ctx: Context): Promise<Upload> {
  const secrets = newSecrets();
  const envelope = new Envelope(file, secrets);
  const session = await startSession(envelope.size, expirySeconds, ctx);
  const persistent = await saveUpload({
    ...session,
    ...secrets,
    expirySeconds,
    started: Date.now(),
    file: { name: file.name, size: file.size, lastModified: file.lastModified },
  });
  return new Upload(envelope, session, secrets, new Map(), persistent);
}

// Picks up the saved upload if it is for this file, and discards it otherwise.
async function resume(file: File, expirySeconds: number): Promise<Upload | null> {
  const saved = await loadSaved();
  if (!saved) return null;
  if (
    saved.file.name !== file.name ||
    saved.file.size !== file.size ||
    saved.file.lastModified !== file.lastModified ||
    saved.expirySeconds !== expirySeconds ||
    Date.now() - saved.started > PENDING_MAX_AGE
  ) {
    await abandon(saved.token);
    return null;
  }
  let digests: Map<number, Uint8Array>;
  try {
    digests = await loadDigests(saved.token);
  } catch {
    await abandon(saved.token);
    return null;
  }
  const secrets = { rawKey: saved.rawKey, noncePrefix: saved.noncePrefix };
  const session = { token: saved.token, partSize: saved.partSize };
  return new Upload(new Envelope(file, secrets), session, secrets, digests, true);
}

// Encrypts and uploads a file, and returns its share link. An expirySeconds of 0 means the link
// never expires.
export async function createShare(
  file: File,
  expirySeconds: number,
  options: ShareOptions = {},
): Promise<string> {
  const signal = options.signal ?? new AbortController().signal;
  const report = options.onProgress ?? (() => undefined);
  const ctx: Context = {
    signal,
    report,
    onWait: (retryAt) => {
      if (retryAt !== null)
        report({ stage: "reconnecting", retryAt, offline: navigator.onLine === false });
    },
  };
  // Checks the file before anything is sent.
  new Envelope(file, newSecrets());
  report({ stage: "preparing" });
  return exclusively(async () => {
    let upload = await resume(file, expirySeconds);
    for (let restarts = 0; ; restarts++) {
      try {
        upload ??= await begin(file, expirySeconds, ctx);
        const id = await run(upload, ctx);
        await forgetSaved();
        return shareLink(id, upload.secrets.rawKey);
      } catch (error) {
        if (error instanceof SessionLost && restarts < MAX_RESTARTS) {
          await forgetSaved();
          upload = null;
          continue;
        }
        // An upload refused only for today can be continued later.
        const later = error instanceof HttpError && error.reply.status === 429;
        if (upload && !later) await abandon(upload.session.token);
        if (error instanceof SessionLost)
          throw new Error("The server keeps losing this upload. Try again later.");
        throw error;
      }
    }
  });
}

// The upload that was interrupted when the page last closed, if any.
export async function pendingShare(): Promise<PendingShare | null> {
  const saved = await loadSaved();
  if (!saved) return null;
  if (Date.now() - saved.started > PENDING_MAX_AGE) {
    await abandon(saved.token);
    return null;
  }
  return { name: saved.file.name, size: saved.file.size, expirySeconds: saved.expirySeconds };
}

export async function discardPendingShare(): Promise<void> {
  await exclusively(async () => {
    const saved = await loadSaved();
    if (saved) await abandon(saved.token);
  });
}
