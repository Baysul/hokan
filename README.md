# Hokan

Hokan is a prototype for sharing files with a link. Your browser encrypts each file before uploading it. Anyone with the full link can open the file, so keep links private.

## Run with Docker

Build and start the app:

```sh
docker build -t hokan .
docker run --rm -p 3000:3000 hokan
```

Open <http://localhost:3000>. By default, files are kept in memory and are lost when the container stops. See [Storage](#storage) to keep files across restarts.

To test the image locally, install Bash, curl, and jq, then run:

```sh
bash scripts/test-docker.sh hokan
```

Use HTTPS when deploying the app. Browser encryption requires a secure context; localhost works for development.

## Develop locally

You need [Vite+](https://viteplus.dev/guide/) and Rust. The project uses the stable Rust toolchain with rustfmt and Clippy.

```sh
vp install
vp run website#build
cargo run --manifest-path apps/server/Cargo.toml
```

Open <http://127.0.0.1:3000>. To work on the website, leave the server running and start this command in another terminal:

```sh
vp run dev
```

The website dev server sends `/api` requests to the Rust server on port 3000. Restart the Rust server after changing its code. If you change `HOKAN_ADDRESS`, update the proxy in `apps/website/vite.config.ts` to match.

If uploads cannot connect, check that the Rust server is running. A `405` response may mean another process is using port 3000.

Run the checks and tests with:

```sh
vp run ready
```

This checks formatting, lint, types, builds, and tests for the website and server. `vp install` also sets up the Git hooks.

## Storage

The default is `HOKAN_STORAGE=memory`. Files disappear when the server restarts. Memory storage holds up to 250 MiB; when full, it rejects new uploads.

For storage that survives restarts, use an S3-compatible bucket. Set these variables on the server:

| Variable                     | Value                                                    |
| ---------------------------- | -------------------------------------------------------- |
| `HOKAN_STORAGE`              | `s3`, `r2`, or `b2`                                      |
| `HOKAN_S3_BUCKET`            | Bucket name                                              |
| `HOKAN_S3_REGION`            | Bucket region; use `auto` for R2                         |
| `HOKAN_S3_ENDPOINT`          | S3 API URL, including `https://`; required for R2 and B2 |
| `HOKAN_S3_ACCESS_KEY_ID`     | S3 access key ID                                         |
| `HOKAN_S3_SECRET_ACCESS_KEY` | S3 secret access key                                     |
| `HOKAN_S3_SESSION_TOKEN`     | Session token, if using temporary credentials            |

To pass these settings to Docker, put them in a private file as `NAME=value`, one per line, then run:

```sh
docker run --rm -p 3000:3000 --env-file .env hokan
```

Keep credentials on the server. Do not put them in the frontend or commit them to Git. The server does not load `.env` files by itself.

For Amazon S3, use the bucket's AWS region. For Cloudflare R2, use the [R2 S3 endpoint and API credentials](https://developers.cloudflare.com/r2/get-started/s3/). For Backblaze B2, use a private bucket and a bucket-limited application key; its `keyID` is the access key ID and its `applicationKey` is the secret. See the [B2 S3 guide](https://www.backblaze.com/apidocs/introduction-to-the-s3-compatible-api).

Custom S3 endpoints must use HTTPS. Keep the bucket private and grant the server credentials only the object permissions it needs for the `hokan/` prefix.

Senders can choose links that expire after 1 hour, 1 day, or 7 days, or choose “Never.” The app blocks expired links and cleans up cloud files in the background. Bucket versioning, retention, and lifecycle rules affect when the stored bytes are actually deleted.

For B2, review its [lifecycle rules](https://www.backblaze.com/docs/cloud-storage-configure-and-manage-lifecycle-rules): deleting an object can leave old versions behind. A rule for the `hokan/` prefix can remove those versions, but it can also delete files shared with “Never.” Choose a rule that matches your retention needs.

## Server settings

| Variable           | Default                 | Purpose                                |
| ------------------ | ----------------------- | -------------------------------------- |
| `HOKAN_ADDRESS`    | `127.0.0.1:3000`        | Server listening address               |
| `HOKAN_ASSETS_DIR` | Built website directory | Directory containing the built website |
| `HOKAN_STORAGE`    | `memory`                | Storage backend                        |

The Docker image sets the listening address to `0.0.0.0:3000`. The health check endpoint is `GET /health`.

## Limits and security

- Each encrypted upload is limited to 25 MiB. The server buffers up to four uploads at once.
- The server accepts at most 240 upload attempts and 1 GiB of encrypted upload data per 24 hours per process. It rejects requests above either limit with `429`.
- API uploads require `Content-Type: application/octet-stream`, which prevents cross-site HTML forms from storing data through a visitor's browser.
- The decryption key is in the link fragment. Browsers do not send that fragment in HTTP requests.
- This is a prototype, not an audited security product. It has no accounts. The upload limits reset on restart and are separate for each server instance. Public deployments still need reverse proxy abuse controls and a storage provider quota.
- You must trust the deployed app: a compromised server or frontend can send malicious code to visitors.
