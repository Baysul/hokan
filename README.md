# Hokan

Hokan shares files with a link. Your browser encrypts each file, up to 5 GiB, before it is uploaded, and the decryption key stays in the link. Anyone with the full link can open the file, so keep links private. Links expire after 1 hour, 1 day, or 7 days, and any file that goes 7 days without a download is deleted.

Uploads survive interruptions. If the connection drops or the server restarts, the browser keeps retrying and continues where it stopped. If the tab closes, choose the same file again within 24 hours to resume. Downloads that are cut off continue from where they stopped, too.

## Run with Docker

```sh
docker build -t hokan .
docker run --rm -p 3000:3000 hokan
```

Open <http://localhost:3000>, choose a file, and copy the link. Files are kept in memory by default and are lost when the server stops.

Deploy behind HTTPS. Browsers only allow encryption in a secure context, though localhost works for testing.

## Develop locally

You need [Vite+](https://viteplus.dev/guide/) and Rust.

```sh
vp install
vp run website#build
cargo run --manifest-path apps/server/Cargo.toml
```

Open <http://127.0.0.1:3000>. To work on the website with live reload, keep the server running and start `vp run dev` in another terminal. Run all checks and tests with `vp run ready`.

## Configuration

Set these environment variables on the server, for example with `docker run --env-file .env`:

| Variable                     | Default          | Purpose                                                 |
| ---------------------------- | ---------------- | ------------------------------------------------------- |
| `HOKAN_ADDRESS`              | `127.0.0.1:3000` | Listening address (`0.0.0.0:3000` in Docker)            |
| `HOKAN_STORAGE`              | `memory`         | `memory`, or `s3`, `r2`, `b2` to keep files in a bucket |
| `HOKAN_MEMORY_LIMIT`         | `6GiB`           | Total size of files held in memory storage              |
| `HOKAN_DAILY_UPLOAD_LIMIT`   | `50GiB`          | Upload data accepted per 24 hours                       |
| `HOKAN_S3_BUCKET`            |                  | Bucket name                                             |
| `HOKAN_S3_REGION`            |                  | Bucket region; `auto` for R2                            |
| `HOKAN_S3_ENDPOINT`          |                  | HTTPS S3 API URL; required for R2 and B2                |
| `HOKAN_S3_ACCESS_KEY_ID`     |                  | Access key ID                                           |
| `HOKAN_S3_SECRET_ACCESS_KEY` |                  | Secret access key                                       |
| `HOKAN_S3_SESSION_TOKEN`     |                  | Session token, if using temporary credentials           |

Keep the bucket private. The server only needs permissions for objects and multipart uploads (including listing and aborting them) under the `hokan/` prefix.

## Limits

The server accepts 240 new uploads per 24 hours and receives at most 8 parts of 16 MiB at a time. With memory storage, a server restart also loses unfinished uploads, so the browser starts them again from the beginning. Hokan is a prototype, not an audited security product: it has no accounts, so public deployments also need abuse controls at the reverse proxy.
