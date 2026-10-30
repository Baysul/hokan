import { afterEach, expect, test, vi } from "vite-plus/test";
import { createShare, openShare, type ShareProgress } from "../src/share.ts";

afterEach(() => vi.unstubAllGlobals());

type Sent = { body: Uint8Array; headers: Record<string, string> };
type Reply = { status: number; body?: string; type?: string } | "network-error";

function stubUpload(reply: (sent: Sent) => Reply) {
  class FakeRequest {
    headers: Record<string, string> = {};
    status = 0;
    responseText = "";
    type = "";
    upload: {
      onprogress?: (event: { lengthComputable: boolean; loaded: number; total: number }) => void;
      onload?: () => void;
    } = {};
    onload?: () => void;
    onerror?: () => void;
    ontimeout?: () => void;
    onabort?: () => void;
    open() {}
    setRequestHeader(name: string, value: string) {
      this.headers[name.toLowerCase()] = value;
    }
    getResponseHeader(name: string) {
      return name.toLowerCase() === "content-type" ? this.type : null;
    }
    send(body: Uint8Array) {
      const result = reply({ body, headers: this.headers });
      queueMicrotask(() => {
        if (result === "network-error") return this.onerror?.();
        for (const loaded of [body.length / 2, body.length])
          this.upload.onprogress?.({ lengthComputable: true, loaded, total: body.length });
        this.upload.onload?.();
        this.status = result.status;
        this.responseText = result.body ?? "";
        this.type = result.type ?? "";
        this.onload?.();
      });
    }
  }
  vi.stubGlobal("XMLHttpRequest", FakeRequest);
}

test("shares an encrypted file with the key only in the fragment and decrypts it", async () => {
  const id = "12345678-1234-4234-8234-123456789abc";
  let uploaded = new Uint8Array();
  let expiry: string | undefined;
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL("https://example.test/"));
  stubUpload(({ body, headers }) => {
    uploaded = new Uint8Array(body);
    expiry = headers["x-hokan-expiry-seconds"];
    return { status: 201, body: JSON.stringify({ id }), type: "application/json" };
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      expect(url).toBe(`/api/files/${id}`);
      return new Response(uploaded);
    }),
  );
  const file = new File(["a private message"], "secret.txt", { type: "text/plain" });
  const progress: ShareProgress[] = [];
  const link = await createShare(file, 86400, (event) => progress.push(event));
  expect(link).toMatch(new RegExp(`^https://example.test/share/${id}#[A-Za-z0-9_-]{43}$`));
  expect(expiry).toBe("86400");
  expect(progress[0]).toEqual({ stage: "encrypting" });
  expect(progress.at(-1)).toEqual({ stage: "saving" });
  const uploading = progress.filter((event) => event.stage === "uploading");
  expect(uploading).toHaveLength(2);
  expect(uploading[1]).toEqual({
    stage: "uploading",
    loaded: uploaded.length,
    total: uploaded.length,
  });
  expect(new TextDecoder().decode(uploaded)).not.toContain("a private message");
  expect(new TextDecoder().decode(uploaded)).not.toContain("secret.txt");
  expect(new TextDecoder().decode(uploaded)).not.toContain("text/plain");
  vi.stubGlobal("location", new URL(link));
  const result = await openShare();
  expect(result?.name).toBe("secret.txt");
  expect(result?.type).toBe("text/plain");
  // The download blob never carries the sender's type, since blob URLs run in this origin.
  expect(result?.blob.type).toBe("application/octet-stream");
  expect(await result?.blob.text()).toBe("a private message");

  uploaded[uploaded.length - 1] ^= 1;
  await expect(openShare()).rejects.toThrow("Could not decrypt");
});

test("falls back to application/octet-stream when the file type is unknown", async () => {
  const id = "12345678-1234-4234-8234-123456789abc";
  let uploaded = new Uint8Array();
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL("https://example.test/"));
  stubUpload(({ body }) => {
    uploaded = new Uint8Array(body);
    return { status: 201, body: JSON.stringify({ id }), type: "application/json" };
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(uploaded)),
  );
  const link = await createShare(new File(["data"], "data"), 86400);
  vi.stubGlobal("location", new URL(link));
  expect((await openShare())?.type).toBe("application/octet-stream");
});

test("removes filename direction controls before sharing and downloading", async () => {
  const id = "12345678-1234-4234-8234-123456789abc";
  let uploaded = new Uint8Array();
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL("https://example.test/"));
  stubUpload(({ body }) => {
    uploaded = new Uint8Array(body);
    return { status: 201, body: JSON.stringify({ id }), type: "application/json" };
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(uploaded)),
  );
  const link = await createShare(new File(["data"], "invoice\u202Efdp.exe"), 86400);
  vi.stubGlobal("location", new URL(link));
  expect((await openShare())?.name).toBe("invoice_fdp.exe");
});

test("stops reading an oversized encrypted response", async () => {
  const id = "12345678-1234-4234-8234-123456789abc";
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL(`https://example.test/share/${id}#${"A".repeat(43)}`));
  let cancelled = false;
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(new Uint8Array(13 * 1024 * 1024));
      controller.enqueue(new Uint8Array(13 * 1024 * 1024));
    },
    cancel() {
      cancelled = true;
    },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(body)),
  );
  await expect(openShare()).rejects.toThrow("invalid or unsupported");
  expect(cancelled).toBe(true);
});

test("sends 0 seconds when the link should never expire", async () => {
  let expiry: string | undefined;
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL("https://example.test/"));
  stubUpload(({ headers }) => {
    expiry = headers["x-hokan-expiry-seconds"];
    return {
      status: 201,
      body: JSON.stringify({ id: "12345678-1234-4234-8234-123456789abc" }),
      type: "application/json",
    };
  });
  await createShare(new File(["hello"], "hello.txt"), 0);
  expect(expiry).toBe("0");
});

test("rejects oversized files before upload", async () => {
  vi.stubGlobal("isSecureContext", true);
  const request = vi.fn();
  vi.stubGlobal("XMLHttpRequest", request);
  const oversized = new File([new Uint8Array(24 * 1024 * 1024 + 1)], "large.bin");
  await expect(createShare(oversized, 86400)).rejects.toThrow("24 MiB");
  expect(request).not.toHaveBeenCalled();
});

test("explains why an upload failed", async () => {
  vi.stubGlobal("isSecureContext", true);
  const file = new File(["hello"], "hello.txt");
  const failWith = async (reply: Reply) => {
    stubUpload(() => reply);
    return createShare(file, 86400);
  };

  await expect(failWith("network-error")).rejects.toThrow("Could not connect to the server");
  // The Vite dev proxy answers 502 with an empty body when the Rust server is not running.
  await expect(failWith({ status: 502 })).rejects.toThrow("Could not connect to the server");
  await expect(failWith({ status: 405 })).rejects.toThrow("Restart it with the latest version");
  await expect(failWith({ status: 413 })).rejects.toThrow("larger than the server accepts");
  await expect(
    failWith({ status: 503, body: "Storage capacity reached", type: "text/plain; charset=utf-8" }),
  ).rejects.toThrow("(Storage capacity reached)");
  // A server-provided reason is not hidden behind the connection message.
  await expect(
    failWith({ status: 502, body: "Storage service unavailable", type: "text/plain" }),
  ).rejects.toThrow("(Storage service unavailable)");
  await expect(failWith({ status: 500 })).rejects.toThrow("(error 500)");
  await expect(
    failWith({ status: 201, body: "<html>not json</html>", type: "text/html" }),
  ).rejects.toThrow("invalid share ID");
});
