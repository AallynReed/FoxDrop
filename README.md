# FoxDrop

Self-hosted file sharing. Upload a file, or pick one already in a folder, and it gets a download link at a random UUID or a custom name.

- **Admin page:** drag-and-drop uploads with progress, a list of the folder's files to share, and links with download counts, optional expiry, and enable/disable/delete.
- **Downloads:** `/<link>`, with resumable Range requests and the original filename.
- **File info:** `/info/<link>` shows the size and MD5, SHA-1, SHA-256 and SHA-512. Add `?json=1` for JSON. Hashes are calculated in the background and cached.

## How it runs

One Rust binary with two listeners:

| Listener | Default | Serves |
|---|---|---|
| Public | `0.0.0.0:8080` | `/<link>`, `/info/<link>` |
| Admin | `0.0.0.0:8081` | `/admin/`, `/api/*` |

The admin listener has no login of its own. Put it behind an authenticating proxy (this deployment uses Authelia) and never expose it directly. The public listener has no admin routes at all.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `FOXDROP_FILES` | `/files` | Folder whose files can be shared; uploads land here |
| `FOXDROP_DATA` | `/data` | `links.json` and `hashes.json` |
| `FOXDROP_PUBLIC_URL` | `http://localhost:8080` | Base URL used for links |
| `FOXDROP_BACKUP_URL` | unset | Optional second host serving the same links |
| `FOXDROP_PUBLIC_ADDR` | `0.0.0.0:8080` | Public listen address |
| `FOXDROP_ADMIN_ADDR` | `0.0.0.0:8081` | Admin listen address |

## Build

```sh
docker build -t foxdrop .
```

The image is a static musl binary on `scratch`. `deploy/` holds the compose file and nginx config this instance runs with.

The proxy in front of the admin listener must allow unlimited request bodies at server level (`client_max_body_size 0` in nginx), so that an auth subrequest doesn't reject large uploads.
