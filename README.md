<div align="center">

<img src="assets/icon.png" width="128" alt="FoxDrop icon">

# FoxDrop

<!-- Project status -->
![GitHub Repo stars](https://img.shields.io/github/stars/AallynReed/FoxDrop?style=for-the-badge&color=gold&cacheSeconds=21600)
![GitHub last commit](https://img.shields.io/github/last-commit/AallynReed/FoxDrop?style=for-the-badge&color=green&cacheSeconds=21600)
![GitHub issues](https://img.shields.io/github/issues/AallynReed/FoxDrop?style=for-the-badge&cacheSeconds=21600)

<!-- Platforms -->
![Docker](https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white)
![Linux](https://img.shields.io/badge/Linux-FCC624?style=for-the-badge&logo=linux&logoColor=black)

<!-- Tech stack -->
![Rust](https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white)
![Axum](https://img.shields.io/badge/Axum-000000?style=for-the-badge&logo=tokio&logoColor=white)

<!-- Project meta -->
![License](https://img.shields.io/badge/License-GPL--2.0--or--later-8A2BE2?style=for-the-badge)
[![Discord](https://img.shields.io/badge/Discord-Foxhearth-5865F2?style=for-the-badge&logo=discord&logoColor=white)](https://discord.gg/VGTaqW3vwn)
[![foxhearth.com](https://img.shields.io/badge/foxhearth.com-home-E8702A?style=for-the-badge)](https://foxhearth.com)

</div>

Self-hosted file sharing. Upload a file, or pick one already in a folder, and it gets a download
link at a random UUID or a name you choose.

## Features

- Drag-and-drop uploads of any size, with progress and speed for each file
- Share files already in the folder, such as ones copied in over a network share
- Links at a random UUID or a custom name, like `cdn.example.com/my-file`
- Optional expiry: 1 hour, 1 day, 7 days or 30 days
- Download counts and the last download time for each link
- Disable a link without deleting it, delete a link, or delete the file with its links
- Downloads keep the original filename and can be paused and resumed
- `/info/<link>` shows the file size and its MD5, SHA-1, SHA-256 and SHA-512, with copy buttons;
  `?json=1` gives the same as JSON
- Hashes are calculated in the background when a link is made, once per file
- The same links on a second domain as a backup
- Light and dark themes that follow the system
- Expired or unknown links show a plain "doesn't exist" page and are never indexed by search engines

## Lightweight

FoxDrop is a single static binary in a 3 MB image, built from `scratch` with nothing else inside.
Measured on Linux with a 16-thread CPU:

| Container | Memory | CPU |
| --- | --- | --- |
| Idle | 1.4 MB | 0% |
| Serving a 115 MB download at 20 MB/s | 19 MB | about 2% of one core |

## Install

FoxDrop has two listeners. The public one (port 8080) serves only downloads and `/info` pages.
The admin one (port 8081) serves the upload page and its API, and has no login of its own: put
it behind an authenticating proxy, such as nginx with Authelia, and never expose it directly.

```yaml
services:
  foxdrop:
    build: .
    container_name: foxdrop
    restart: unless-stopped
    user: "1000:1000"
    read_only: true
    environment:
      FOXDROP_PUBLIC_URL: https://cdn.example.com
    ports:
      - 127.0.0.1:8080:8080
      - 127.0.0.1:8081:8081
    volumes:
      - /path/to/files:/files
      - ./data:/data
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `FOXDROP_FILES` | `/files` | Folder whose files can be shared; uploads land here |
| `FOXDROP_DATA` | `/data` | Where `links.json` and `hashes.json` are kept |
| `FOXDROP_PUBLIC_URL` | `http://localhost:8080` | Base URL of the links |
| `FOXDROP_BACKUP_URL` | unset | A second host serving the same links |
| `FOXDROP_PUBLIC_ADDR` | `0.0.0.0:8080` | Public listen address |
| `FOXDROP_ADMIN_ADDR` | `0.0.0.0:8081` | Admin listen address |

The proxy in front of the admin listener must allow unlimited uploads at server level
(`client_max_body_size 0` in nginx), or its auth check rejects anything over 1 MB. `deploy/`
holds the nginx and compose files of the instance at foxhearth.com.

## Build

```powershell
cargo build --release
docker build -t foxdrop .
```

## Privacy

FoxDrop makes no outgoing connections. It keeps no visitor IP addresses or user agents: each
link stores only its download count and the time of its last download.

## Licence

GPL-2.0-or-later.
