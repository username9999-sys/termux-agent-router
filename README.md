# Termux Agent Router

Proxy OpenAI-compatible yang ringan untuk Termux. Proyek ini terinspirasi oleh
kebutuhan menjalankan beberapa provider dari satu endpoint lokal; proyek ini
mandiri dan tidak berafiliasi dengan proyek lain.

## Instalasi di Termux

```sh
pkg install rust
git clone <url-repository-ini> termux-agent-router
cd termux-agent-router
cargo build --release
install -Dm755 target/release/tar "$PREFIX/bin/tar"
```

## Konfigurasi

Buat contoh konfigurasi (tanpa secret):

```sh
tar config init
${EDITOR:-vi} ~/.config/termux-agent-router/config.toml
export EXAMPLE_API_KEY='...'
export BACKUP_API_KEY='...'
```

Path mengikuti XDG melalui `~/.config/termux-agent-router/config.toml`.
Jangan menaruh API key di file konfigurasi; `api_key_env` hanya menunjuk nama
environment variable.

## Menjalankan proxy

```sh
tar serve
curl http://127.0.0.1:8787/health
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"one-model","messages":[{"role":"user","content":"Halo"}]}'
```

Model pada request dipakai jika tersedia. Jika tidak ada, router memakai
`default_model`, lalu `fallback_models`. Fallback hanya terjadi pada kegagalan
transport, HTTP 5xx, atau 429.

## Menjalankan client

`run` tidak memakai shell: command dan argumen dieksekusi langsung, sehingga
quoting shell tidak diproses.

```sh
tar run -- my-client --model one-model
tar run -- claude --print "Halo"
```

Environment `OPENAI_BASE_URL`, `OPENAI_API_BASE`, dan
`TERMUX_AGENT_ROUTER_BASE_URL` diarahkan ke proxy lokal. `OPENAI_API_KEY` diisi
dengan placeholder lokal; key provider tetap dibaca oleh proxy dari environment.

## Pengembangan

```sh
cargo fmt -- --check
cargo check
cargo test
```
