#!/usr/bin/env bash
set -euo pipefail

image="${1:-hokan:ci}"
container=""
temp_dir="$(mktemp -d)"

cleanup() {
  status=$?
  if [[ -n "$container" ]]; then
    if (( status != 0 )); then
      docker logs "$container" || true
    fi
    docker rm -f "$container" >/dev/null || true
  fi
  rm -rf "$temp_dir"
}
trap cleanup EXIT

container="$(docker run -d -p 127.0.0.1::3000 "$image")"
port="$(docker port "$container" 3000/tcp)"
url="http://127.0.0.1:${port##*:}"

test "$(curl --fail --silent --show-error --retry 30 --retry-delay 1 --retry-all-errors "$url/health")" = "ok"
curl --fail --silent --show-error "$url/" | grep -q '<title>Hokan'

printf 'container smoke test' > "$temp_dir/upload"
upload="$(curl --fail --silent --show-error -X POST -H "X-Hokan-Upload-Length: $(wc -c < "$temp_dir/upload")" "$url/api/uploads" | jq -er '.upload | select(type == "string" and test("^[0-9a-f.-]+$"))')"
curl --fail --silent --show-error -X PUT -H 'Content-Type: application/octet-stream' --data-binary "@$temp_dir/upload" "$url/api/uploads/$upload/parts/1"
id="$(curl --fail --silent --show-error -X POST "$url/api/uploads/$upload/complete" | jq -er '.id | select(type == "string" and test("^[0-9a-f-]+$"))')"
curl --fail --silent --show-error "$url/api/files/$id" > "$temp_dir/download"
cmp "$temp_dir/upload" "$temp_dir/download"
echo 'Docker image smoke test passed.'
