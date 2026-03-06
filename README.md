# proxymore

[![CI](https://github.com/sigoden/proxymore/actions/workflows/ci.yaml/badge.svg)](https://github.com/sigoden/proxymore/actions/workflows/ci.yaml)
[![Crates](https://img.shields.io/crates/v/proxymore.svg)](https://crates.io/crates/proxymore)
[![Docker Pulls](https://img.shields.io/docker/pulls/sigoden/proxymore)](https://hub.docker.com/r/sigoden/proxymore)

A powerful and flexible proxy CLI for capturing and inspecting HTTP(S) and WS(S) traffic.

## Features

- Supports forward/reverse proxy
- Provides TUI & WebUI
- Enables filtering & searching
- Handles HTTP/HTTPS/WS/WSS protocols
- Comes with a tool for installing CA certificates
- Allows export in Markdown, cURL, or HAR formats
- Captures request/response in a non-blocking, streaming way
- Offers a single-file portable executable for use across Windows/macOS/Linux

## Screenshots

**Terminal User Interface (TUI)**
![proxymore-tui](https://github.com/user-attachments/assets/87a93e09-4783-4273-85b6-002762909fc3)

**Web User Interface (WebUI)**
![proxymore-webui](https://github.com/user-attachments/assets/4f1f921a-95ec-44e0-8a2f-671614c0b934)

## Installation

### With cargo

```
cargo install proxymore
```

### With docker

```
docker run -v ~/.proxymore:/.proxymore -p 8080:8080 --rm sigoden/proxymore --web 
```

### Binaries on macOS, Linux, Windows

Download from [Github Releases](https://github.com/sigoden/proxymore/releases), unzip and add proxymore to your $PATH.

## Proxy Modes Explained

### Forward Proxy

In this mode, your client applications (e.g., web browsers, curl) are configured to send their requests to `proxymore`, which then forwards them to the target servers. You would configure your client to use a proxy at `http://127.0.0.1:8080`.

```bash
proxymore
curl -x http://127.0.0.1:8080 httpbin.org/ip
```

### Reverse Proxy

In reverse proxy mode, `proxymore` sits in front of a target server. Clients access `proxymore` and it forwards the requests to the defined URL. This mode is ideal when clients cannot be configured to use a proxy.

```bash
proxymore https://httpbin.org
curl http://127.0.0.1:8080/ip
```

## Command Line Interface (CLI)

```
Usage: proxymore [OPTIONS] [URL]

Arguments:
  [URL]  Reverse proxy url

Options:
  -l, --listen <ADDR>         Listening ip and port address [default: 0.0.0.0:8080]
  -f, --filters <REGEX>       Only inspect http(s) traffic whose `{method} {uri}` matches the regex
  -m, --mime-filters <VALUE>  Only inspect http(s) traffic whose content-type matches the value
  -W, --web                   Enable user-friendly web interface
  -T, --tui                   Eenter TUI
  -D, --dump                  Dump all traffics
  -h, --help                  Print help
  -V, --version               Print version
```

### Choosing User Interface

`proxymore` provides several ways to interact with captured traffic:

```sh
proxymore                   # Enter TUI, equal to `proxymore --tui`
proxymore --web             # Serve WebUI
proxymore --web --tui       # Serve WebUI + Enter TUI
proxymore --dump            # Dump all traffics to console
proxymore > proxymore.md     # Dump all traffics to markdown file
```

### Specifying Address and Port

Customize the listening address and port:

```sh
proxymore -l 8081
proxymore -l 127.0.0.1
proxymore -l 127.0.0.1:8081
```

### Filtering Traffic

Apply regex filters to limit captured traffic based on method and URI:

```sh
proxymore -f httpbin.org/ip -f httpbin.org/anything
proxymore -f '/^(get|post) https:\/\/httpbin.org/'
```

Filter based on MIME types:

```sh
proxymore -m application/json -m application/ld+json
proxymore -m text/
```

## CA Certificate Installation

To decrypt HTTPS traffic, you must install `proxymore`'s CA certificate on your device. The easiest way to do this is to use the built-in certificate installation app.

1. Start `proxymore` with desired proxy settings.
2. On your target device, configure the device to use `proxymore` as the proxy.
3. Open a web browser on the target device and navigate to [proxymore.local](http://proxymore.local).
4. Follow the on-screen instructions to download and install the CA certificate.

![proxymore.local](https://github.com/sigoden/proxymore/assets/4012553/a5276872-8ab1-4794-9e97-ac7038ca5e4a)

## License

Copyright (c) 2024-∞ proxymore-developers.

proxymore is made available under the terms of either the MIT License or the Apache License 2.0, at your option.

See the LICENSE-APACHE and LICENSE-MIT files for license details.