import "./style.css";
import "@hotwired/turbo";
import { Application, Controller } from "@hotwired/stimulus";
import { createShare, openShare, type ShareProgress } from "./share";
import { brandMarkHtml, galaxyHtml, mascotHtml } from "./art";

const paths: Record<string, string> = {
  lock: '<rect x="5" y="10" width="14" height="11" rx="3"/><path d="M8 10V7a4 4 0 0 1 8 0v3m-4 5v2"/>',
  upload: '<path d="M12 16V4m-4 4 4-4 4 4"/><path d="M4 16v2a3 3 0 0 0 3 3h10a3 3 0 0 0 3-3v-2"/>',
  file: '<path d="M6 3h8l4 4v14H6z"/><path d="M14 3v5h5M9 13h6m-6 4h6"/>',
  arrow: '<path d="M5 12h14m-5-5 5 5-5 5"/>',
  copy: '<rect x="8" y="8" width="11" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h2"/>',
  x: '<path d="m6 6 12 12M18 6 6 18"/>',
  download:
    '<path d="M12 3v12m-4-4 4 4 4-4"/><path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
};
const icon = (name: string, size = 20) =>
  `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths[name]}</svg>`;

document.querySelector<HTMLDivElement>("#app")!.innerHTML = `
<div class="app-shell" data-controller="share">
  <div class="sky" aria-hidden="true"><span class="shooting-star shooting-one"></span><span class="shooting-star shooting-two"></span></div>
  <main>
    <section class="hero wrap" id="send">
      <div class="hero-copy">
        <a class="brand" href="/" data-turbo="false" aria-label="Hokan home">${brandMarkHtml}<span>hokan<span class="brand-period">.</span></span></a>
        <div class="orbit-art" aria-hidden="true">${galaxyHtml}<span class="art-star star-a">✦</span><span class="art-star star-b">✧</span><span class="art-star star-c">✧</span><span class="art-star star-d">✦</span><span class="space-dust dust-a"></span><span class="space-dust dust-b"></span><span class="space-dust dust-c"></span><span class="space-dust dust-d"></span>${mascotHtml}</div>
      </div>
      <div class="transfer-card">
        <div data-share-target="sendView">
          <h1>Share a file</h1>
          <p class="card-description">Choose a file to create an encrypted share link.</p>
          <div class="drop-zone" data-share-target="dropZone" data-action="dragover->share#dragOver dragleave->share#dragLeave drop->share#drop">
            <div class="upload-icon">${icon("upload", 25)}</div>
            <strong>Drop a file here</strong><p>or</p><label class="browse-label" for="file-input">Browse files</label><span class="drop-hint">One file, up to 24 MiB</span>
            <input id="file-input" class="sr-only" type="file" data-share-target="input" data-action="change->share#select" />
          </div>
          <div class="selected-file" data-share-target="selected" hidden><span class="selected-icon">${icon("file", 19)}</span><span class="file-detail"><strong data-share-target="fileName"></strong><small data-share-target="fileSize"></small></span><button type="button" aria-label="Remove selected file" data-share-target="removeButton" data-action="share#remove">${icon("x", 18)}</button><div class="progress" data-share-target="progress" role="progressbar" aria-label="Upload progress" aria-valuemin="0" aria-valuemax="100" hidden><div class="progress-fill" data-share-target="progressFill"></div></div></div>
          <div class="delivery-details"><label for="expiry"><span class="detail-icon">${icon("clock", 17)}</span><span>Link expires</span></label><select id="expiry" name="expiry" data-share-target="expiry"><option value="3600">In 1 hour</option><option value="86400" selected>In 1 day</option><option value="604800">In 7 days</option><option value="0">Never</option></select></div>
          <button class="primary-button" type="button" data-share-target="submit" data-action="share#send" disabled><span data-share-target="submitText">Create a private link</span>${icon("arrow", 19)}</button>
          <p class="card-footnote">${icon("lock", 13)} Your file is encrypted before it leaves this device.</p>
        </div>
        <div class="result-view" data-share-target="result" hidden><h1>Share link ready</h1><p data-share-target="resultNote"></p><div class="link-field"><input readonly aria-label="Private share link" data-share-target="link"/></div><button class="primary-button" type="button" data-action="share#copy">Copy private link ${icon("copy", 18)}</button><button class="text-button" type="button" data-action="share#reset">Send another file</button></div>
        <div class="result-view" data-share-target="receive" hidden><h1>Download a file</h1><p data-share-target="receiveStatus" role="status" aria-live="polite">Opening the shared file…</p><a class="primary-button" data-share-target="download" download hidden>Download your file ${icon("download", 18)}</a><a class="text-button" href="/" data-turbo="false">Share a file</a></div>
        <p class="status-message" data-share-target="status" role="status" aria-live="polite"></p>
      </div>
    </section>
  </main>
</div>`;

const MAX_FILE_SIZE = 24 * 1024 * 1024;
const formatSize = (bytes: number) =>
  bytes < 1024 * 1024
    ? `${(bytes / 1024).toFixed(1)} KiB`
    : `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
class ShareController extends Controller<HTMLElement> {
  static targets = [
    "input",
    "expiry",
    "dropZone",
    "selected",
    "removeButton",
    "progress",
    "progressFill",
    "resultNote",
    "fileName",
    "fileSize",
    "submit",
    "submitText",
    "sendView",
    "result",
    "link",
    "receive",
    "receiveStatus",
    "download",
    "status",
  ];
  declare readonly inputTarget: HTMLInputElement;
  declare readonly expiryTarget: HTMLSelectElement;
  declare readonly dropZoneTarget: HTMLElement;
  declare readonly selectedTarget: HTMLElement;
  declare readonly removeButtonTarget: HTMLButtonElement;
  declare readonly progressTarget: HTMLElement;
  declare readonly progressFillTarget: HTMLElement;
  declare readonly resultNoteTarget: HTMLElement;
  declare readonly fileNameTarget: HTMLElement;
  declare readonly fileSizeTarget: HTMLElement;
  declare readonly submitTarget: HTMLButtonElement;
  declare readonly submitTextTarget: HTMLElement;
  declare readonly sendViewTarget: HTMLElement;
  declare readonly resultTarget: HTMLElement;
  declare readonly linkTarget: HTMLInputElement;
  declare readonly receiveTarget: HTMLElement;
  declare readonly receiveStatusTarget: HTMLElement;
  declare readonly downloadTarget: HTMLAnchorElement;
  declare readonly statusTarget: HTMLElement;
  private file: File | null = null;
  private received: { blob: Blob; name: string } | null = null;
  private downloadUrl: string | null = null;
  private active = false;
  private readonly hashChanged = () => {
    if (location.pathname.startsWith("/share/")) location.reload();
  };

  async connect() {
    this.active = true;
    window.addEventListener("hashchange", this.hashChanged);
    if (!location.pathname.startsWith("/share/")) return;
    this.sendViewTarget.hidden = true;
    this.receiveTarget.hidden = false;
    try {
      this.received = await openShare();
      if (!this.active) return;
      if (!this.received) throw new Error("This link is missing its decryption key.");
      this.receiveStatusTarget.textContent = `Ready to download: ${this.received.name}`;
      this.downloadUrl = URL.createObjectURL(this.received.blob);
      this.downloadTarget.href = this.downloadUrl;
      this.downloadTarget.download = this.received.name;
      this.downloadTarget.hidden = false;
    } catch (error) {
      this.receiveStatusTarget.textContent =
        error instanceof Error ? error.message : "Could not open this file.";
    }
  }
  disconnect() {
    this.active = false;
    window.removeEventListener("hashchange", this.hashChanged);
    if (this.downloadUrl) URL.revokeObjectURL(this.downloadUrl);
    this.downloadUrl = null;
  }
  navigate(event: Event) {
    if (!location.pathname.startsWith("/share/")) return;
    const link = event.currentTarget as HTMLAnchorElement;
    const target = document.getElementById(link.hash.slice(1));
    if (target) {
      event.preventDefault();
      target.scrollIntoView();
    }
  }
  select() {
    this.choose(this.inputTarget.files?.[0] ?? null);
  }
  dragOver(event: DragEvent) {
    event.preventDefault();
    this.dropZoneTarget.classList.add("dragging");
  }
  dragLeave() {
    this.dropZoneTarget.classList.remove("dragging");
  }
  drop(event: DragEvent) {
    event.preventDefault();
    this.dropZoneTarget.classList.remove("dragging");
    this.choose(event.dataTransfer?.files[0] ?? null);
  }
  choose(file: File | null) {
    this.statusTarget.textContent = "";
    if (!file) return;
    if (file.size > MAX_FILE_SIZE) {
      this.statusTarget.textContent = "Please choose a file no larger than 24 MiB.";
      this.remove();
      return;
    }
    this.file = file;
    this.fileNameTarget.textContent = file.name;
    this.fileSizeTarget.textContent = `${formatSize(file.size)} · Ready to encrypt`;
    this.selectedTarget.hidden = false;
    this.dropZoneTarget.hidden = true;
    this.submitTarget.disabled = false;
  }
  remove() {
    this.file = null;
    this.inputTarget.value = "";
    this.selectedTarget.hidden = true;
    this.dropZoneTarget.hidden = false;
    this.submitTarget.disabled = true;
  }
  async send() {
    if (!this.file) return;
    const file = this.file;
    const expiry = this.expiryTarget.selectedOptions[0]!;
    this.statusTarget.textContent = "";
    this.setSending(true);
    try {
      this.linkTarget.value = await createShare(file, Number(expiry.value), (progress) =>
        this.showProgress(progress),
      );
      this.resultNoteTarget.textContent = `Anyone with this link can decrypt your file. ${
        Number(expiry.value) === 0
          ? "It never expires."
          : `It expires ${expiry.text.toLowerCase()}.`
      }`;
      this.sendViewTarget.hidden = true;
      this.resultTarget.hidden = false;
    } catch (error) {
      this.statusTarget.textContent =
        error instanceof Error ? error.message : "Could not create your link. Please try again.";
    } finally {
      this.setSending(false);
      this.fileSizeTarget.textContent = `${formatSize(file.size)} · Ready to encrypt`;
    }
  }
  // Locks the form while a file is in flight so the upload can't be changed or repeated.
  setSending(sending: boolean) {
    this.submitTarget.disabled = sending;
    this.expiryTarget.disabled = sending;
    this.removeButtonTarget.disabled = sending;
    this.progressTarget.hidden = !sending;
    this.setProgress(0);
    this.submitTextTarget.textContent = sending ? "Encrypting…" : "Create a private link";
  }
  showProgress(progress: ShareProgress) {
    if (progress.stage === "encrypting") {
      this.fileSizeTarget.textContent = "Encrypting in your browser…";
      this.submitTextTarget.textContent = "Encrypting…";
    } else if (progress.stage === "uploading") {
      const percent = Math.floor((progress.loaded / progress.total) * 100);
      this.setProgress(percent);
      this.fileSizeTarget.textContent = `${formatSize(progress.loaded)} of ${formatSize(progress.total)} uploaded`;
      this.submitTextTarget.textContent = `Uploading ${percent}%`;
    } else {
      this.setProgress(100);
      this.fileSizeTarget.textContent = "Upload complete. Saving…";
      this.submitTextTarget.textContent = "Saving…";
    }
  }
  setProgress(percent: number) {
    this.progressTarget.setAttribute("aria-valuenow", String(percent));
    this.progressFillTarget.style.width = `${percent}%`;
  }
  async copy() {
    try {
      await navigator.clipboard.writeText(this.linkTarget.value);
      this.statusTarget.textContent = "Link copied. Share it with someone you trust.";
    } catch {
      this.linkTarget.select();
      this.statusTarget.textContent = "Select and copy the link above.";
    }
  }
  reset() {
    this.remove();
    this.linkTarget.value = "";
    this.statusTarget.textContent = "";
    this.resultTarget.hidden = true;
    this.sendViewTarget.hidden = false;
  }
}
Application.start().register("share", ShareController);

// Leans the hero art toward the pointer. Touch and reduced-motion visitors keep it still.
const orbitArt = document.querySelector<HTMLElement>(".orbit-art");
if (orbitArt && matchMedia("(pointer: fine) and (prefers-reduced-motion: no-preference)").matches) {
  window.addEventListener(
    "pointermove",
    (event) => {
      orbitArt.style.setProperty("--px", (event.clientX / innerWidth - 0.5).toFixed(3));
      orbitArt.style.setProperty("--py", (event.clientY / innerHeight - 0.5).toFixed(3));
    },
    { passive: true },
  );
}
